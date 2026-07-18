use std::borrow::Cow;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context};
use remi_agentloop::prelude::AgentError;

pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const BUILTIN_SENTRY_DSN: Option<&str> = option_env!("REMI_SENTRY_DSN");
pub const BUILTIN_SENTRY_FEEDBACK_DSN: Option<&str> = option_env!("REMI_SENTRY_FEEDBACK_DSN");
const FLUSH_TIMEOUT: Duration = Duration::from_secs(2);
const FEEDBACK_FLUSH_TIMEOUT: Duration = Duration::from_secs(10);

pub fn builtin_dsn() -> Option<&'static str> {
    BUILTIN_SENTRY_DSN
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

pub fn builtin_feedback_dsn() -> Option<&'static str> {
    BUILTIN_SENTRY_FEEDBACK_DSN
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn options(dsn: &str, app_id: &str) -> anyhow::Result<sentry::ClientOptions> {
    let dsn = dsn.parse().with_context(|| {
        "invalid Sentry DSN supplied at build time or through ApplicationBuilder"
    })?;
    let app_id = app_id.to_string();
    Ok(sentry::ClientOptions {
        dsn: Some(dsn),
        release: Some(Cow::Owned(format!("remi-cat@{APP_VERSION}"))),
        before_send: Some(Arc::new(move |mut event| {
            // Automatic diagnostics must not carry raw HTTP/user data or
            // breadcrumbs that can contain prompts and tool arguments.
            event.request = None;
            event.user = None;
            event.breadcrumbs.values.clear();
            event.tags.entry("app.id".into()).or_insert(app_id.clone());
            event
                .tags
                .entry("app.version".into())
                .or_insert(APP_VERSION.into());
            Some(event)
        })),
        shutdown_timeout: FLUSH_TIMEOUT,
        send_default_pii: false,
        ..Default::default()
    })
}

pub struct CliTelemetryGuard {
    _guard: sentry::ClientInitGuard,
}

impl CliTelemetryGuard {
    pub fn init(dsn: &str, profile: &str, surface: &str) -> anyhow::Result<Self> {
        let guard = sentry::init(options(dsn, "remi-cat")?);
        sentry::configure_scope(|scope| {
            scope.set_tag("app.id", "remi-cat");
            scope.set_tag("app.version", APP_VERSION);
            scope.set_tag("profile", profile);
            scope.set_tag("surface", surface);
        });
        Ok(Self { _guard: guard })
    }
}

pub struct ApplicationTelemetry {
    hub: Arc<sentry::Hub>,
    client: Arc<sentry::Client>,
}

impl ApplicationTelemetry {
    pub fn new(dsn: &str, app_id: &str) -> anyhow::Result<Self> {
        let options = sentry::apply_defaults(options(dsn, app_id)?);
        let client = Arc::new(sentry::Client::from_config(options));
        if !client.is_enabled() {
            return Err(anyhow!("Sentry client is disabled for the supplied DSN"));
        }
        let hub = Arc::new(sentry::Hub::new(
            Some(client.clone()),
            Arc::new(sentry::Scope::default()),
        ));
        hub.configure_scope(|scope| {
            scope.set_tag("app.id", app_id);
            scope.set_tag("app.version", APP_VERSION);
            scope.set_tag("surface", "application");
        });
        Ok(Self { hub, client })
    }

    pub fn run<R>(&self, f: impl FnOnce() -> R) -> R {
        sentry::Hub::run(self.hub.clone(), f)
    }

    pub fn capture_system_error(&self, message: impl Into<String>, chain: &'static str) {
        self.hub.capture_event(system_error_event(message, chain));
    }

    pub fn capture_agent_error(&self, error: &AgentError, chain: &'static str) {
        self.hub.capture_event(agent_error_event(error, chain));
    }

    pub fn flush(&self) -> bool {
        self.client.flush(Some(FLUSH_TIMEOUT))
    }
}

impl Drop for ApplicationTelemetry {
    fn drop(&mut self) {
        self.client.close(Some(FLUSH_TIMEOUT));
    }
}

pub fn capture_system_error(message: impl Into<String>, chain: &'static str) {
    sentry::capture_event(system_error_event(message, chain));
}

pub fn capture_agent_error(error: &AgentError, chain: &'static str) {
    sentry::capture_event(agent_error_event(error, chain));
}

pub fn capture_anyhow_if_system(error: &anyhow::Error, chain: &'static str) {
    if let Some(agent) = error.downcast_ref::<AgentError>() {
        capture_agent_error(agent, chain);
        return;
    }
    let system = error.chain().any(|source| {
        source.is::<std::io::Error>()
            || source.is::<reqwest::Error>()
            || source.is::<serde_json::Error>()
            || source.is::<serde_yaml::Error>()
            || source.is::<git2::Error>()
            || source.is::<tokio::task::JoinError>()
    });
    let mut event = system_error_event(format!("{error:#}"), chain);
    if !system {
        event.tags.insert("error.kind".into(), "internal".into());
    }
    sentry::capture_event(event);
}

fn agent_error_event(error: &AgentError, chain: &'static str) -> sentry::protocol::Event<'static> {
    let mut event = system_error_event(error.to_string(), chain);
    event.tags.insert(
        "error.kind".into(),
        if is_system_agent_error(error) {
            "system"
        } else {
            "operational"
        }
        .into(),
    );
    event
}

fn system_error_event(
    message: impl Into<String>,
    chain: &'static str,
) -> sentry::protocol::Event<'static> {
    let mut event = sentry::protocol::Event {
        level: sentry::protocol::Level::Error,
        message: Some(message.into()),
        ..Default::default()
    };
    event.tags.insert("error.chain".into(), chain.into());
    event.tags.insert("error.kind".into(), "system".into());
    event
}

pub fn is_system_agent_error(error: &AgentError) -> bool {
    matches!(
        error,
        AgentError::HttpTransport(_)
            | AgentError::Json(_)
            | AgentError::SseParse { .. }
            | AgentError::ToolNotFound(_)
            | AgentError::Store(_)
            | AgentError::Io(_)
            | AgentError::Other(_)
    )
}

pub fn capture_feedback(
    title: &str,
    body: &str,
    labels: &[String],
    profile: &str,
) -> anyhow::Result<sentry::types::Uuid> {
    let dsn = builtin_feedback_dsn().ok_or_else(|| {
        anyhow!("feedback is unavailable because this binary has no embedded feedback Sentry DSN")
    })?;
    let client = Arc::new(sentry::Client::from_config(sentry::apply_defaults(
        options(dsn, "remi-cat")?,
    )));
    if !client.is_enabled() {
        return Err(anyhow!("feedback Sentry client is disabled"));
    }
    let hub = Arc::new(sentry::Hub::new(
        Some(client.clone()),
        Arc::new(sentry::Scope::default()),
    ));
    let mut event = sentry::protocol::Event {
        level: sentry::protocol::Level::Info,
        message: Some(title.to_string()),
        ..Default::default()
    };
    event.tags.insert("kind".into(), "user-feedback".into());
    event.tags.insert("profile".into(), profile.into());
    event
        .extra
        .insert("body".into(), serde_json::Value::String(body.to_string()));
    event.extra.insert(
        "labels".into(),
        serde_json::Value::Array(
            labels
                .iter()
                .cloned()
                .map(serde_json::Value::String)
                .collect(),
        ),
    );
    let id = hub.capture_event(event);
    let flushed = client.flush(Some(FEEDBACK_FLUSH_TIMEOUT));
    if !flushed {
        return Err(anyhow!(
            "Sentry did not confirm feedback delivery before timeout"
        ));
    }
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::{is_system_agent_error, options, APP_VERSION};
    use remi_agentloop::prelude::AgentError;

    #[test]
    fn expected_agent_failures_are_not_system_telemetry() {
        assert!(!is_system_agent_error(&AgentError::tool("bash", "exit 1")));
        assert!(!is_system_agent_error(&AgentError::model(
            "request refused"
        )));
        assert!(is_system_agent_error(&AgentError::Io("disk failed".into())));
        assert!(is_system_agent_error(&AgentError::Store(
            "db failed".into()
        )));
    }

    #[test]
    fn options_attach_release_app_id_and_version_without_request_data() {
        let options = options("https://public@example.com/1", "host-app").unwrap();
        assert_eq!(
            options.release.as_deref(),
            Some(format!("remi-cat@{APP_VERSION}").as_str())
        );
        let processor = options.before_send.unwrap();
        let event = processor(sentry::protocol::Event {
            request: Some(Default::default()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            event.tags.get("app.id").map(String::as_str),
            Some("host-app")
        );
        assert_eq!(
            event.tags.get("app.version").map(String::as_str),
            Some(APP_VERSION)
        );
        assert!(event.request.is_none());
    }

    #[test]
    fn runtime_sentry_environment_is_not_read() {
        assert!(!include_str!("app.rs").contains("std::env::var(\"SENTRY_DSN\")"));
    }
}
