use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use tokio::sync::Mutex;

use crate::cli::{FeedbackCommand, GitHubIssueCreateRequest, GitHubIssueCreateResponse};
use crate::command::{sandbox_doctor_report, sdk_doctor_report};
use crate::config::{detect_setup_state, SetupState};
use crate::instance_profile::InstanceProfile;
use crate::secret_store::{redaction_entries, SecretStore};

const DEFAULT_FEEDBACK_REPO: &str = "another-s347/remi-cat";
const GITHUB_API_VERSION: &str = "2026-03-10";

pub(crate) async fn run_feedback_command(
    store: Arc<Mutex<SecretStore>>,
    profile: &InstanceProfile,
    data_dir: &Path,
    command: FeedbackCommand,
) -> anyhow::Result<()> {
    let repo = feedback_repo(command.repo.as_deref())?;
    let secret_entries = store.lock().await.entries()?;
    let redactions = redaction_entries(&secret_entries);
    let body = build_feedback_issue_body(
        &command.body,
        profile,
        data_dir,
        command.include_logs,
        &redactions,
    );
    let payload = GitHubIssueCreateRequest {
        title: command.title,
        body,
        labels: command.labels,
    };

    if command.dry_run {
        println!("{}", serde_json::to_string_pretty(&payload)?);
        println!("repo: {repo}");
        return Ok(());
    }

    let Some(token) = github_issue_token(&secret_entries) else {
        let url = github_new_issue_url(&repo, &payload);
        println!("GitHub token not found; open this URL to create the issue:");
        println!("{url}");
        println!(
            "To submit directly, set GITHUB_TOKEN, GH_TOKEN, or REMI_GITHUB_TOKEN with Issues: write permission."
        );
        return Ok(());
    };

    let response = create_github_issue(&repo, &token, &payload).await?;
    println!(
        "Created GitHub issue #{}: {}",
        response.number, response.html_url
    );
    Ok(())
}

pub(crate) fn feedback_repo(override_repo: Option<&str>) -> anyhow::Result<String> {
    let repo = override_repo
        .map(ToOwned::to_owned)
        .or_else(|| std::env::var("REMI_FEEDBACK_REPO").ok())
        .unwrap_or_else(|| DEFAULT_FEEDBACK_REPO.to_string());
    let repo = repo.trim().trim_start_matches('/').trim_end_matches('/');
    let parts = repo.split('/').collect::<Vec<_>>();
    if parts.len() != 2 || parts.iter().any(|part| part.trim().is_empty()) {
        anyhow::bail!("invalid GitHub repo `{repo}`; expected owner/repo");
    }
    Ok(repo.to_string())
}

fn github_issue_token(secrets: &std::collections::BTreeMap<String, String>) -> Option<String> {
    ["GITHUB_TOKEN", "GH_TOKEN", "REMI_GITHUB_TOKEN"]
        .iter()
        .find_map(|key| {
            std::env::var(key)
                .ok()
                .or_else(|| secrets.get(*key).cloned())
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        })
}

fn build_feedback_issue_body(
    user_body: &str,
    profile: &InstanceProfile,
    data_dir: &Path,
    include_logs: bool,
    redactions: &HashMap<String, String>,
) -> String {
    let setup_state = detect_setup_state(data_dir);
    let setup_label = match &setup_state {
        SetupState::Initialized { .. } => "initialized",
        SetupState::Invalid { .. } => "invalid",
        SetupState::LegacyEnvCompatible { .. } => "legacy-env-compatible",
        SetupState::Uninitialized { .. } => "uninitialized",
    };
    let mut body = format!(
        "{}\n\n---\n\n### Diagnostics\n\n```text\nremi-cat_version: {}\nos: {}\narch: {}\nprofile: {}\nsetup_state: {}\n{}\n{}\n```\n",
        user_body.trim(),
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
        profile.label(),
        setup_label,
        sdk_doctor_report(data_dir),
        sandbox_doctor_report(data_dir, &setup_state),
    );
    if include_logs {
        let logs = collect_feedback_logs(profile, redactions);
        if logs.trim().is_empty() {
            body.push_str("\n### Logs\n\nNo log files found.\n");
        } else {
            body.push_str("\n### Logs\n\n```text\n");
            body.push_str(&logs.replace("```", "'''"));
            body.push_str("\n```\n");
        }
    } else {
        body.push_str(
            "\nLogs omitted. Re-run with `--include-logs` to attach recent local logs.\n",
        );
    }
    body
}

fn collect_feedback_logs(
    profile: &InstanceProfile,
    redactions: &HashMap<String, String>,
) -> String {
    let mut sections = Vec::new();
    for path in [profile.log_file(), profile.log_dir().join("tui.log")] {
        if let Ok(text) = std::fs::read_to_string(&path) {
            let redacted = redact_known_secrets(&text, redactions);
            sections.push(format!(
                "== {} ==\n{}",
                path.display(),
                tail_lines(&redacted, 200)
            ));
        }
    }
    sections.join("\n\n")
}

pub(crate) fn redact_known_secrets(text: &str, redactions: &HashMap<String, String>) -> String {
    redactions.values().fold(text.to_string(), |acc, secret| {
        if secret.is_empty() {
            acc
        } else {
            acc.replace(secret, "***REDACTED***")
        }
    })
}

fn tail_lines(text: &str, max_lines: usize) -> String {
    let lines = text.lines().collect::<Vec<_>>();
    let start = lines.len().saturating_sub(max_lines);
    lines[start..].join("\n")
}

async fn create_github_issue(
    repo: &str,
    token: &str,
    payload: &GitHubIssueCreateRequest,
) -> anyhow::Result<GitHubIssueCreateResponse> {
    let url = format!("https://api.github.com/repos/{repo}/issues");
    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?
        .post(&url)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
        .header(reqwest::header::USER_AGENT, "remi-cat")
        .bearer_auth(token)
        .json(payload)
        .send()
        .await
        .with_context(|| format!("failed to create GitHub issue at {url}"))?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("GitHub issue creation failed with HTTP {status}: {body}");
    }

    response
        .json::<GitHubIssueCreateResponse>()
        .await
        .context("failed to parse GitHub issue response")
}

pub(crate) fn github_new_issue_url(repo: &str, payload: &GitHubIssueCreateRequest) -> String {
    let mut url = format!(
        "https://github.com/{repo}/issues/new?title={}&body={}",
        percent_encode_query(&payload.title),
        percent_encode_query(&payload.body)
    );
    if !payload.labels.is_empty() {
        url.push_str("&labels=");
        url.push_str(&percent_encode_query(&payload.labels.join(",")));
    }
    url
}

pub(crate) fn percent_encode_query(value: &str) -> String {
    let mut out = String::new();
    for byte in value.as_bytes() {
        match *byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(*byte as char)
            }
            b' ' => out.push('+'),
            byte => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}
