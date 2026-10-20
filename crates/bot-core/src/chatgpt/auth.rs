use super::{API_BASE, USAGE_URL};
use anyhow::{bail, ensure, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use fs2::FileExt;
use jsonwebtoken::{decode, decode_header, jwk::JwkSet, DecodingKey, Validation};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::Mutex;

const ISSUER: &str = "https://auth.openai.com";
const SCOPE: &str = "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
const DIRECT: &str = "chatgpt.tokens.use.direct";

#[derive(Clone)]
pub struct ChatGptAuthManager {
    root: PathBuf,
    client: reqwest::Client,
    gate: std::sync::Arc<Mutex<()>>,
    #[cfg(test)]
    auth_base: Option<String>,
}

// Intentionally no Debug: authorization URLs, OAuth replies and tokens are secrets.
#[derive(Default, Clone, Serialize, Deserialize)]
struct Record {
    host_id: String,
    client_id: Option<String>,
    subject: Option<String>,
    email: Option<String>,
    access_token: Option<String>,
    refresh_token: Option<String>,
    id_token: Option<String>,
    expires_at: u64,
    scopes: Vec<String>,
    #[serde(default)]
    needs_login: bool,
    #[serde(default)]
    generation: u64,
}

#[derive(Serialize)]
pub struct AuthStatus {
    pub signed_in: bool,
    pub plan_authorized: bool,
    pub needs_login: bool,
    pub email: Option<String>,
    pub subject: Option<String>,
    pub client_id: Option<String>,
    pub expires_at: Option<u64>,
    pub usage_url: &'static str,
}

pub struct LoginAttempt {
    pub authorization_url: String,
    state: String,
    nonce: String,
    verifier: String,
    redirect_uri: String,
    record: Record,
}

impl LoginAttempt {
    pub fn callback_state(&self) -> &str {
        &self.state
    }
    pub fn public_authorization_url(&self) -> Result<String> {
        let mut url = reqwest::Url::parse(&self.authorization_url)?;
        let pairs: Vec<(String, String)> = url
            .query_pairs()
            .filter(|(key, _)| key != "id_token_hint")
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        url.set_query(None);
        url.query_pairs_mut().extend_pairs(pairs);
        Ok(url.into())
    }
}

#[derive(Deserialize)]
pub struct LoginCallback {
    pub state: Option<String>,
    pub code: Option<String>,
    pub client_id: Option<String>,
    pub error: Option<String>,
}

#[derive(Deserialize)]
struct Tokens {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    expires_in: u64,
    token_type: String,
    scope: Option<String>,
}
#[derive(Deserialize)]
struct Claims {
    sub: String,
    email: Option<String>,
    nonce: Option<String>,
}
#[derive(Deserialize)]
struct Discovery {
    issuer: String,
    jwks_uri: String,
    revocation_endpoint: Option<String>,
}

impl ChatGptAuthManager {
    pub fn new(data_dir: impl AsRef<Path>) -> Self {
        Self {
            root: data_dir.as_ref().join("auth/chatgpt"),
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(30))
                .build()
                .expect("TLS client"),
            gate: Default::default(),
            #[cfg(test)]
            auth_base: None,
        }
    }
    pub fn from_env() -> Self {
        Self::new(
            std::env::var_os("REMI_DATA_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| ".remi-cat".into()),
        )
    }
    fn endpoint(&self, path: &str) -> String {
        #[cfg(test)]
        if let Some(base) = &self.auth_base {
            return format!("{base}{path}");
        }
        format!("{ISSUER}{path}")
    }
    fn load(&self) -> Result<Record> {
        let path = self.root.join("credentials.json");
        if let Ok(meta) = fs::symlink_metadata(&path) {
            ensure!(
                meta.is_file() && !meta.file_type().is_symlink(),
                "invalid ChatGPT credential file"
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::{MetadataExt, PermissionsExt};
                ensure!(
                    meta.uid() == unsafe { libc::geteuid() }
                        && meta.permissions().mode() & 0o077 == 0,
                    "ChatGPT credentials must be owned by the current user and private (0600)"
                );
            }
        }
        match fs::read(path) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).context("invalid ChatGPT credential record")
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Record::default()),
            Err(e) => Err(e).context("reading ChatGPT credentials"),
        }
    }
    fn save(&self, record: &Record) -> Result<()> {
        secure_dir(&self.root)?;
        let path = self.root.join("credentials.json");
        let temp = self.root.join(format!("{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| -> Result<()> {
            let mut file = private_file(&temp, true)?;
            file.write_all(&serde_json::to_vec(record)?)?;
            file.sync_all()?;
            drop(file);
            atomic_replace(&temp, &path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(temp);
        }
        result
    }
    async fn lock(&self) -> Result<File> {
        secure_dir(&self.root)?;
        let file = private_file(&self.root.join("session.lock"), false)?;
        let until = tokio::time::Instant::now() + Duration::from_secs(90);
        loop {
            match file.try_lock_exclusive() {
                Ok(()) => return Ok(file),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    ensure!(
                        tokio::time::Instant::now() < until,
                        "ChatGPT credential lock timed out"
                    );
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(e) => return Err(e).context("locking ChatGPT credentials"),
            }
        }
    }
    fn protocol_state_dir(&self, r: &Record) -> PathBuf {
        let binding = format!(
            "{}:{}",
            r.client_id.as_deref().unwrap_or(""),
            r.subject.as_deref().unwrap_or("")
        );
        self.root
            .join("responses")
            .join(format!("{:x}", Sha256::digest(binding.as_bytes())))
    }
    pub fn status(&self) -> Result<AuthStatus> {
        let r = self.load()?;
        Ok(AuthStatus {
            signed_in: r.access_token.is_some(),
            plan_authorized: r.scopes.iter().any(|s| s == DIRECT),
            needs_login: r.needs_login,
            email: r.email,
            subject: r.subject,
            client_id: r.client_id,
            expires_at: (r.expires_at != 0).then_some(r.expires_at),
            usage_url: USAGE_URL,
        })
    }
    pub fn ensure_configured(&self) -> Result<()> {
        let s = self.status()?;
        ensure!(
            s.signed_in && s.plan_authorized && !s.needs_login,
            "ChatGPT plan authorization required; run `remi-cat chatgpt login` for this profile"
        );
        Ok(())
    }
    pub async fn begin_login(&self, port: u16) -> Result<LoginAttempt> {
        self.begin_login_mode(port, false).await
    }
    /// Explicit replacement of the one registration after a completed local logout.
    pub async fn begin_login_new_account(&self, port: u16) -> Result<LoginAttempt> {
        self.begin_login_mode(port, true).await
    }
    async fn begin_login_mode(&self, port: u16, new_account: bool) -> Result<LoginAttempt> {
        let _gate = self.gate.lock().await;
        let _lock = self.lock().await?;
        let mut record = self.load()?;
        if record.host_id.is_empty() {
            record.host_id = format!("urn:uuid:{}", uuid::Uuid::new_v4());
            self.save(&record)?;
        }
        if new_account {
            ensure!(
                record.access_token.is_none()
                    && record.refresh_token.is_none()
                    && !record.needs_login
                    && record.scopes.is_empty(),
                "sign out before replacing the ChatGPT account"
            );
            record.client_id = None;
            record.subject = None;
            record.email = None;
            record.id_token = None;
        }
        let state = random();
        let nonce = random();
        let verifier = random();
        let redirect_uri = format!("http://127.0.0.1:{port}/auth/callback");
        let mut url = reqwest::Url::parse(&self.endpoint("/api/accounts/authorize"))?;
        {
            let mut q = url.query_pairs_mut();
            q.append_pair(
                "client_id",
                record
                    .client_id
                    .as_deref()
                    .unwrap_or("dynamic_agent_client"),
            )
            .append_pair("ext_agent_host_id", &record.host_id)
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", &redirect_uri)
            .append_pair("scope", SCOPE)
            .append_pair("resource", API_BASE)
            .append_pair("state", &state)
            .append_pair("nonce", &nonce)
            .append_pair("code_challenge_method", "S256")
            .append_pair(
                "code_challenge",
                &URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())),
            );
            if record.client_id.is_none() {
                q.append_pair("agent_name_hint", "remi-cat");
            }
            if let Some(id) = &record.id_token {
                q.append_pair("id_token_hint", id);
            }
        }
        Ok(LoginAttempt {
            authorization_url: url.into(),
            state,
            nonce,
            verifier,
            redirect_uri,
            record,
        })
    }
    pub async fn finish_login(&self, attempt: LoginAttempt, callback: LoginCallback) -> Result<()> {
        ensure!(
            callback.state.as_deref() == Some(&attempt.state),
            "invalid OAuth state"
        );
        ensure!(
            callback.error.is_none(),
            "ChatGPT authorization declined or failed"
        );
        let issued = match (&attempt.record.client_id, &callback.client_id) {
            (Some(old), Some(new)) => {
                ensure!(old == new, "OAuth client changed unexpectedly");
                old.clone()
            }
            (Some(old), None) => old.clone(),
            (None, Some(new)) if !new.is_empty() && new != "dynamic_agent_client" => new.clone(),
            _ => bail!("registration callback has no issued client ID"),
        };
        let code = callback.code.context("OAuth callback has no code")?;
        let _gate = self.gate.lock().await;
        let _lock = self.lock().await?;
        let current = self.load()?;
        ensure!(
            current.generation == attempt.record.generation,
            "ChatGPT session changed during login; retry"
        );
        if current.client_id.is_none() {
            let mut pending = current.clone();
            pending.client_id = Some(issued.clone());
            self.save(&pending)?;
        }
        let tokens = self
            .exchange(&[
                ("grant_type", "authorization_code"),
                ("client_id", &issued),
                ("code", &code),
                ("code_verifier", &attempt.verifier),
                ("redirect_uri", &attempt.redirect_uri),
                ("resource", API_BASE),
            ])
            .await?;
        let id = tokens
            .id_token
            .as_deref()
            .context("OAuth response has no ID token")?;
        let claims = self.validate_id(id, &issued, Some(&attempt.nonce)).await?;
        if let Some(subject) = attempt.record.subject.as_deref() {
            ensure!(
                subject == claims.sub,
                "different ChatGPT account; sign out and use login --new-account"
            );
        }
        let mut next = attempt.record;
        next.client_id = Some(issued);
        next.subject = Some(claims.sub);
        next.email = claims.email;
        apply_tokens(&mut next, tokens, true)?;
        ensure!(
            next.scopes.iter().any(|s| s == DIRECT),
            "ChatGPT plan usage permission was not granted"
        );
        self.save(&next)
    }
    async fn discovery(&self) -> Result<Discovery> {
        let response = self
            .client
            .get(self.endpoint("/.well-known/openid-configuration"))
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("OpenAI discovery request failed"))?;
        ensure!(
            response.status().is_success(),
            "OpenAI discovery unavailable"
        );
        let d: Discovery = response.json().await.context("invalid OpenAI discovery")?;
        ensure!(d.issuer == ISSUER, "unexpected OpenAI issuer");
        self.check_discovery_url(&d.jwks_uri)?;
        if let Some(url) = &d.revocation_endpoint {
            self.check_discovery_url(url)?;
        }
        Ok(d)
    }
    fn check_discovery_url(&self, url: &str) -> Result<()> {
        #[cfg(test)]
        if let Some(base) = &self.auth_base {
            ensure!(
                reqwest::Url::parse(url)?.origin() == reqwest::Url::parse(base)?.origin(),
                "invalid test discovery origin"
            );
            return Ok(());
        }
        trusted_auth_url(url)
    }
    async fn validate_id(&self, id: &str, client_id: &str, nonce: Option<&str>) -> Result<Claims> {
        let discovery = self.discovery().await?;
        let header = decode_header(id).context("invalid ID token header")?;
        ensure!(
            matches!(
                header.alg,
                jsonwebtoken::Algorithm::RS256 | jsonwebtoken::Algorithm::ES256
            ),
            "unsupported ID token algorithm"
        );
        let jwks: JwkSet = self
            .client
            .get(discovery.jwks_uri)
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("OpenAI signing keys unavailable"))?
            .error_for_status()
            .context("OpenAI signing keys unavailable")?
            .json()
            .await
            .context("invalid signing keys")?;
        let key = jwks
            .find(header.kid.as_deref().context("ID token missing key ID")?)
            .context("unknown ID signing key")?;
        let mut validation = Validation::new(header.alg);
        validation.set_issuer(&[ISSUER]);
        validation.set_audience(&[client_id]);
        validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        let claims = decode::<Claims>(id, &DecodingKey::from_jwk(key)?, &validation)
            .context("ID token verification failed")?
            .claims;
        ensure!(!claims.sub.is_empty(), "ID token missing subject");
        if let Some(nonce) = nonce {
            ensure!(
                claims.nonce.as_deref() == Some(nonce),
                "invalid ID token nonce"
            );
        }
        Ok(claims)
    }
    async fn exchange(&self, fields: &[(&str, &str)]) -> Result<Tokens> {
        let response = self
            .client
            .post(self.endpoint("/api/accounts/oauth/token"))
            .form(fields)
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("ChatGPT token exchange temporarily unavailable"))?;
        if !response.status().is_success() {
            let status = response.status();
            let value: serde_json::Value = response.json().await.unwrap_or_default();
            let code = value["error"]
                .as_str()
                .or(value["error"]["code"].as_str())
                .unwrap_or("unknown");
            // Never print the OAuth body: providers may include credential material.
            let terminal = matches!(
                code,
                "invalid_grant"
                    | "invalid_refresh_token"
                    | "token_expired"
                    | "refresh_token_expired"
                    | "refresh_token_invalidated"
                    | "refresh_token_reused"
            );
            return Err(anyhow::Error::new(ExchangeError {
                terminal,
                status: status.as_u16(),
            }));
        }
        response.json().await.context("invalid ChatGPT token reply")
    }
    pub async fn access_token(&self) -> Result<String> {
        Ok(self.access_grant().await?.0)
    }
    pub(crate) async fn access_grant(&self) -> Result<(String, PathBuf)> {
        let _gate = self.gate.lock().await;
        let _lock = self.lock().await?;
        let mut r = self.load()?;
        ensure!(
            r.scopes.iter().any(|s| s == DIRECT) && !r.needs_login,
            "ChatGPT authorization required; run `remi-cat chatgpt login`"
        );
        let access = r
            .access_token
            .clone()
            .context("ChatGPT is signed out; run `remi-cat chatgpt login`")?;
        if r.expires_at > now() + 60 {
            return Ok((access, self.protocol_state_dir(&r)));
        }
        let client = r
            .client_id
            .as_deref()
            .context("missing ChatGPT client ID")?;
        let refresh = r
            .refresh_token
            .as_deref()
            .context("ChatGPT refresh token missing; log in again")?;
        let tokens = match self
            .exchange(&[
                ("grant_type", "refresh_token"),
                ("client_id", client),
                ("refresh_token", refresh),
                ("resource", API_BASE),
            ])
            .await
        {
            Ok(tokens) => tokens,
            Err(e) => {
                if e.downcast_ref::<ExchangeError>()
                    .is_some_and(|e| e.terminal)
                {
                    r.access_token = None;
                    r.refresh_token = None;
                    r.id_token = None;
                    r.needs_login = true;
                    r.generation += 1;
                    self.save(&r)?;
                }
                return Err(e);
            }
        };
        if let Some(id) = &tokens.id_token {
            let claims = self.validate_id(id, client, None).await?;
            ensure!(
                r.subject.as_deref() == Some(&claims.sub),
                "ChatGPT refresh identity mismatch"
            );
        }
        apply_tokens(&mut r, tokens, false)?;
        self.save(&r)?;
        ensure!(
            r.scopes.iter().any(|s| s == DIRECT),
            "ChatGPT plan permission removed"
        );
        Ok((
            r.access_token
                .clone()
                .context("missing refreshed access token")?,
            self.protocol_state_dir(&r),
        ))
    }
    /// Returns whether remote revocation was confirmed. Local logout always clears tokens.
    pub async fn logout(&self) -> Result<bool> {
        let _gate = self.gate.lock().await;
        let _lock = self.lock().await?;
        let mut r = self.load()?;
        let mut revoked = r.refresh_token.is_none();
        if let (Some(refresh), Some(client)) = (&r.refresh_token, &r.client_id) {
            if let Ok(d) = self.discovery().await {
                if let Some(url) = d.revocation_endpoint {
                    for attempt in 0..2 {
                        if let Ok(response) = self
                            .client
                            .post(&url)
                            .form(&[
                                ("token", refresh.as_str()),
                                ("token_type_hint", "refresh_token"),
                                ("client_id", client.as_str()),
                            ])
                            .send()
                            .await
                        {
                            if response.status() == reqwest::StatusCode::OK {
                                revoked = true;
                                break;
                            }
                            if !response.status().is_server_error() {
                                break;
                            }
                        }
                        if attempt == 0 {
                            tokio::time::sleep(Duration::from_millis(250)).await;
                        }
                    }
                }
            }
        }
        r.access_token = None;
        r.refresh_token = None;
        r.id_token = None;
        r.scopes.clear();
        r.expires_at = 0;
        r.needs_login = false;
        r.generation += 1;
        self.save(&r)?;
        Ok(revoked)
    }
    pub async fn models(&self) -> Result<serde_json::Value> {
        let response = self
            .client
            .get(format!("{API_BASE}/models"))
            .bearer_auth(self.access_token().await?)
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("ChatGPT model catalog unavailable"))?;
        ensure!(
            response.status().is_success(),
            "ChatGPT model catalog returned HTTP {}",
            response.status()
        );
        let value: serde_json::Value = response.json().await.context("invalid model catalog")?;
        ensure!(
            value["models"].is_array(),
            "ChatGPT model catalog has no models array"
        );
        Ok(value)
    }
}

#[derive(Debug)]
struct ExchangeError {
    terminal: bool,
    status: u16,
}
impl std::fmt::Display for ExchangeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "ChatGPT token exchange failed (HTTP {}); {}",
            self.status,
            if self.terminal {
                "log in again"
            } else {
                "retry later or check OAuth client configuration"
            }
        )
    }
}
impl std::error::Error for ExchangeError {}
fn apply_tokens(r: &mut Record, t: Tokens, initial: bool) -> Result<()> {
    ensure!(
        t.token_type.eq_ignore_ascii_case("bearer")
            && !t.access_token.is_empty()
            && t.expires_in > 0,
        "invalid OAuth token response"
    );
    if initial {
        ensure!(
            t.refresh_token.as_ref().is_some_and(|s| !s.is_empty()),
            "OAuth refresh token missing"
        );
    }
    r.access_token = Some(t.access_token);
    if let Some(refresh) = t.refresh_token {
        ensure!(!refresh.is_empty(), "empty refresh token");
        r.refresh_token = Some(refresh);
    }
    if let Some(id) = t.id_token {
        r.id_token = Some(id);
    }
    if initial {
        r.scopes.clear();
    }
    if let Some(scope) = t.scope {
        r.scopes = scope.split_whitespace().map(str::to_owned).collect();
    }
    r.expires_at = now().saturating_add(t.expires_in);
    r.needs_login = false;
    r.generation += 1;
    Ok(())
}
fn random() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn trusted_auth_url(raw: &str) -> Result<()> {
    let url = reqwest::Url::parse(raw)?;
    ensure!(
        url.scheme() == "https"
            && url.host_str() == Some("auth.openai.com")
            && url.port_or_known_default() == Some(443)
            && url.username().is_empty()
            && url.password().is_none(),
        "untrusted OpenAI auth endpoint"
    );
    Ok(())
}
pub(super) fn secure_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)?;
        ensure!(
            !fs::symlink_metadata(path)?.file_type().is_symlink(),
            "credential directory cannot be a symlink"
        );
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(windows)]
    {
        fs::create_dir_all(path)?;
        let literal = path.to_string_lossy().replace("'", "''");
        let script = format!("$ErrorActionPreference='Stop'; $p='{literal}'; $sid=[System.Security.Principal.WindowsIdentity]::GetCurrent().User; $acl=[System.Security.AccessControl.DirectorySecurity]::new(); $acl.SetOwner($sid); $acl.SetAccessRuleProtection($true,$false); $rule=[System.Security.AccessControl.FileSystemAccessRule]::new($sid,'FullControl','ContainerInherit,ObjectInherit','None','Allow'); $acl.AddAccessRule($rule); Set-Acl -LiteralPath $p -AclObject $acl");
        let encoded: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let output = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-EncodedCommand"])
            .arg(base64::engine::general_purpose::STANDARD.encode(encoded))
            .output()?;
        ensure!(
            output.status.success(),
            "cannot protect ChatGPT credential directory ACL"
        );
    }
    Ok(())
}
fn private_file(path: &Path, create_new: bool) -> Result<File> {
    if let Ok(meta) = fs::symlink_metadata(path) {
        ensure!(
            meta.is_file() && !meta.file_type().is_symlink(),
            "invalid credential file"
        );
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    if create_new {
        options.create_new(true);
    } else {
        options.create(true).truncate(false);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}
fn atomic_replace(source: &Path, dest: &Path) -> Result<()> {
    #[cfg(not(windows))]
    fs::rename(source, dest)?;
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn MoveFileExW(a: *const u16, b: *const u16, flags: u32) -> i32;
        }
        let a: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
        let b: Vec<u16> = dest.as_os_str().encode_wide().chain(Some(0)).collect();
        ensure!(
            unsafe { MoveFileExW(a.as_ptr(), b.as_ptr(), 9) } != 0,
            "atomic ChatGPT credential replacement failed"
        );
    }
    Ok(())
}

pub(super) fn write_private_json(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut f = private_file(&tmp, true)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        atomic_replace(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}

#[cfg(test)]
pub(super) fn test_auth(dir: &Path) -> ChatGptAuthManager {
    let mut manager = ChatGptAuthManager::new(dir);
    manager.auth_base = Some("http://127.0.0.1:9".into());
    manager
        .save(&Record {
            host_id: "urn:uuid:test-host".into(),
            client_id: Some("test-client".into()),
            subject: Some("test-user".into()),
            access_token: Some("test-access".into()),
            refresh_token: Some("test-refresh".into()),
            expires_at: now() + 3600,
            scopes: vec![DIRECT.into()],
            ..Default::default()
        })
        .unwrap();
    manager
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        extract::{Form, State},
        routing::{get, post},
        Json, Router,
    };
    use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    #[derive(Clone)]
    struct Mock {
        base: String,
        calls: Arc<AtomicUsize>,
        reply: Arc<Mutex<serde_json::Value>>,
        forms: Arc<Mutex<Vec<std::collections::HashMap<String, String>>>>,
    }
    async fn mock_token(
        State(s): State<Mock>,
        Form(form): Form<std::collections::HashMap<String, String>>,
    ) -> (reqwest::StatusCode, Json<serde_json::Value>) {
        s.calls.fetch_add(1, Ordering::SeqCst);
        s.forms.lock().await.push(form);
        tokio::time::sleep(Duration::from_millis(30)).await;
        let value = s.reply.lock().await.clone();
        (
            if value.get("error").is_some() {
                reqwest::StatusCode::BAD_REQUEST
            } else {
                reqwest::StatusCode::OK
            },
            Json(value),
        )
    }
    async fn mock_discovery(State(s): State<Mock>) -> Json<serde_json::Value> {
        Json(
            serde_json::json!({"issuer":ISSUER,"jwks_uri":format!("{}/jwks",s.base),"revocation_endpoint":format!("{}/revoke",s.base)}),
        )
    }
    async fn server(dir: &Path) -> (ChatGptAuthManager, Mock, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let state = Mock {
            base: base.clone(),
            calls: Default::default(),
            reply: Arc::new(Mutex::new(
                serde_json::json!({"access_token":"new-access","refresh_token":"rotated-refresh","token_type":"Bearer","expires_in":3600}),
            )),
            forms: Default::default(),
        };
        let app = Router::new()
            .route("/api/accounts/oauth/token", post(mock_token))
            .route("/.well-known/openid-configuration", get(mock_discovery))
            .route(
                "/jwks",
                get(|| async {
                    Json(
                        serde_json::from_str::<serde_json::Value>(include_str!(
                            "fixtures/test-only-jwks.json"
                        ))
                        .unwrap(),
                    )
                }),
            )
            .route("/revoke", post(|| async { reqwest::StatusCode::OK }))
            .with_state(state.clone());
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let mut auth = ChatGptAuthManager::new(dir);
        auth.auth_base = Some(base);
        (auth, state, handle)
    }
    fn signed_id(client: &str, nonce: &str, subject: &str) -> String {
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("test-key".into());
        encode(&header,&serde_json::json!({"iss":ISSUER,"aud":client,"sub":subject,"email":"test@example.invalid","nonce":nonce,"exp":now()+3600}),&EncodingKey::from_rsa_pem(include_bytes!("fixtures/test-only-rsa.pem")).unwrap()).unwrap()
    }
    fn callback(attempt: &LoginAttempt, client: Option<&str>) -> LoginCallback {
        LoginCallback {
            state: Some(attempt.state.clone()),
            code: Some("auth-code".into()),
            client_id: client.map(str::to_owned),
            error: None,
        }
    }
    #[tokio::test]
    async fn dynamic_registration_pkce_and_verified_identity() {
        let dir = tempfile::tempdir().unwrap();
        let (auth, s, h) = server(dir.path()).await;
        let attempt = auth.begin_login(1455).await.unwrap();
        let url = reqwest::Url::parse(&attempt.authorization_url).unwrap();
        let params: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(params["client_id"], "dynamic_agent_client");
        assert_eq!(params["agent_name_hint"], "remi-cat");
        assert_eq!(
            params["code_challenge"],
            URL_SAFE_NO_PAD.encode(Sha256::digest(attempt.verifier.as_bytes()))
        );
        *s.reply.lock().await = serde_json::json!({"access_token":"access","refresh_token":"refresh","id_token":signed_id("issued-client",&attempt.nonce,"user"),"token_type":"Bearer","expires_in":3600,"scope":SCOPE});
        let cb = callback(&attempt, Some("issued-client"));
        auth.finish_login(attempt, cb).await.unwrap();
        assert_eq!(
            auth.status().unwrap().email.as_deref(),
            Some("test@example.invalid")
        );
        let forms = s.forms.lock().await;
        assert_eq!(forms[0]["client_id"], "issued-client");
        assert_eq!(
            forms[0]["redirect_uri"],
            "http://127.0.0.1:1455/auth/callback"
        );
        drop(forms);
        let returning = auth.begin_login(54321).await.unwrap();
        assert!(!returning
            .public_authorization_url()
            .unwrap()
            .contains("id_token_hint"));
        assert!(!returning.authorization_url.contains("agent_name_hint"));
        h.abort();
    }
    #[tokio::test]
    async fn reauthorization_rejects_identity_nonce_and_scope_changes_without_overwriting() {
        let dir = tempfile::tempdir().unwrap();
        let (auth, s, h) = server(dir.path()).await;
        let mut record = Record {
            host_id: "host".into(),
            client_id: Some("issued".into()),
            subject: Some("original".into()),
            access_token: Some("keep-me".into()),
            scopes: vec![DIRECT.into()],
            ..Default::default()
        };
        auth.save(&record).unwrap();
        for (subject, nonce_override, scope) in [
            ("different", false, SCOPE),
            ("original", true, SCOPE),
            ("original", false, "openid profile email"),
        ] {
            let attempt = auth.begin_login(1455).await.unwrap();
            *s.reply.lock().await = serde_json::json!({"access_token":"replace","refresh_token":"refresh","id_token":signed_id("issued",if nonce_override {"wrong"} else {&attempt.nonce},subject),"token_type":"Bearer","expires_in":3600,"scope":scope});
            let cb = callback(&attempt, None);
            assert!(auth.finish_login(attempt, cb).await.is_err());
            assert_eq!(
                auth.load().unwrap().access_token.as_deref(),
                Some("keep-me")
            );
        }
        let attempt = auth.begin_login(1455).await.unwrap();
        let mut cb = callback(&attempt, None);
        cb.state = Some("wrong".into());
        assert!(auth.finish_login(attempt, cb).await.is_err());
        let attempt = auth.begin_login(1455).await.unwrap();
        let mut cb = callback(&attempt, None);
        cb.error = Some("access_denied".into());
        assert!(auth.finish_login(attempt, cb).await.is_err());
        assert_eq!(s.calls.load(Ordering::SeqCst), 3);
        record.access_token = None;
        auth.save(&record).unwrap();
        h.abort();
    }
    #[tokio::test]
    async fn refresh_is_serialized_across_independent_managers_and_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let (a, s, h) = server(dir.path()).await;
        test_auth(dir.path());
        let mut record = a.load().unwrap();
        record.expires_at = 0;
        a.save(&record).unwrap();
        let mut b = ChatGptAuthManager::new(dir.path());
        b.auth_base = a.auth_base.clone();
        let (left, right) = tokio::join!(a.access_token(), b.access_token());
        assert_eq!(left.unwrap(), "new-access");
        assert_eq!(right.unwrap(), "new-access");
        assert_eq!(s.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            a.load().unwrap().refresh_token.as_deref(),
            Some("rotated-refresh")
        );
        assert_eq!(
            ChatGptAuthManager::new(dir.path())
                .access_token()
                .await
                .unwrap(),
            "new-access"
        );
        let form = &s.forms.lock().await[0];
        assert_eq!(form["client_id"], "test-client");
        assert!(!form.contains_key("scope"));
        h.abort();
    }
    #[tokio::test]
    async fn terminal_refresh_and_logout_invalidate_existing_clients() {
        let dir = tempfile::tempdir().unwrap();
        let (auth, s, h) = server(dir.path()).await;
        let existing = test_auth(dir.path());
        let mut r = auth.load().unwrap();
        r.expires_at = 0;
        auth.save(&r).unwrap();
        *s.reply.lock().await = serde_json::json!({"error":"invalid_grant"});
        assert!(auth.access_token().await.is_err());
        assert!(auth.status().unwrap().needs_login);
        assert!(existing.ensure_configured().is_err());
        test_auth(dir.path());
        assert!(auth.logout().await.unwrap());
        assert!(existing.access_token().await.is_err());
        let record = auth.load().unwrap();
        assert_eq!(record.client_id.as_deref(), Some("test-client"));
        assert!(!record.host_id.is_empty());
        assert!(record.id_token.is_none());
        h.abort();
    }
    #[test]
    fn tokens_never_appear_in_status_and_permissions_are_private() {
        let dir = tempfile::tempdir().unwrap();
        let auth = test_auth(dir.path());
        let status = serde_json::to_string(&auth.status().unwrap()).unwrap();
        for token in ["test-access", "test-refresh"] {
            assert!(!status.contains(token));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&auth.root).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(auth.root.join("credentials.json"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        assert!(trusted_auth_url("http://auth.openai.com/jwks").is_err());
        assert!(trusted_auth_url("https://attacker.invalid/jwks").is_err());
    }
    #[tokio::test]
    async fn id_token_requires_signature_audience_and_expiry() {
        let dir = tempfile::tempdir().unwrap();
        let (auth, _, h) = server(dir.path()).await;
        assert!(auth
            .validate_id(
                &signed_id("wrong-client", "nonce", "user"),
                "expected-client",
                Some("nonce")
            )
            .await
            .is_err());
        let mut id = signed_id("expected-client", "nonce", "user");
        let last = id.pop().unwrap();
        id.push(if last == 'a' { 'b' } else { 'a' });
        assert!(auth
            .validate_id(&id, "expected-client", Some("nonce"))
            .await
            .is_err());
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("test-key".into());
        let expired=encode(&header,&serde_json::json!({"iss":ISSUER,"aud":"expected-client","sub":"user","nonce":"nonce","exp":now()-3600}),&EncodingKey::from_rsa_pem(include_bytes!("fixtures/test-only-rsa.pem")).unwrap()).unwrap();
        assert!(auth
            .validate_id(&expired, "expected-client", Some("nonce"))
            .await
            .is_err());
        h.abort();
    }
    #[tokio::test]
    async fn temporary_refresh_error_keeps_credentials_and_registration() {
        let dir = tempfile::tempdir().unwrap();
        let (auth, s, h) = server(dir.path()).await;
        test_auth(dir.path());
        let mut r = auth.load().unwrap();
        r.expires_at = 0;
        auth.save(&r).unwrap();
        *s.reply.lock().await = serde_json::json!({"error":"temporarily_unavailable"});
        assert!(auth.access_token().await.is_err());
        let r = auth.load().unwrap();
        assert_eq!(r.refresh_token.as_deref(), Some("test-refresh"));
        assert!(!r.needs_login);
        h.abort();
    }
    #[tokio::test]
    async fn account_replacement_requires_logout_and_explicit_new_registration() {
        let dir = tempfile::tempdir().unwrap();
        let (auth, s, h) = server(dir.path()).await;
        test_auth(dir.path());
        assert!(auth.begin_login_new_account(1455).await.is_err());
        auth.logout().await.unwrap();
        let ordinary = auth.begin_login(1455).await.unwrap();
        assert!(ordinary.authorization_url.contains("client_id=test-client"));
        let host = ordinary.record.host_id.clone();
        let fresh = auth.begin_login_new_account(1455).await.unwrap();
        assert!(fresh
            .authorization_url
            .contains("client_id=dynamic_agent_client"));
        assert_eq!(fresh.record.host_id, host);
        assert_eq!(
            auth.status().unwrap().client_id.as_deref(),
            Some("test-client")
        );
        *s.reply.lock().await = serde_json::json!({"access_token":"other-access","refresh_token":"other-refresh","id_token":signed_id("new-client",&fresh.nonce,"other-user"),"token_type":"Bearer","expires_in":3600,"scope":SCOPE});
        let cb = callback(&fresh, Some("new-client"));
        auth.finish_login(fresh, cb).await.unwrap();
        assert_eq!(
            auth.status().unwrap().subject.as_deref(),
            Some("other-user")
        );
        assert_eq!(
            auth.status().unwrap().client_id.as_deref(),
            Some("new-client")
        );
        h.abort();
    }
}
