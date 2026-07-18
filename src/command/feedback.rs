use std::collections::HashMap;
use std::io::{Read as _, Seek as _};
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

use crate::cli::FeedbackCommand;
use crate::command::sandbox_doctor_report;
use crate::config::{detect_setup_state, SetupState};
use crate::instance_profile::InstanceProfile;
use crate::secret_store::{redaction_entries, SecretStore};

const MAX_TITLE_CHARS: usize = 200;
const MAX_BODY_CHARS: usize = 16_000;
const MAX_LABELS: usize = 10;
const MAX_LABEL_CHARS: usize = 64;
const MAX_LOG_BYTES_PER_FILE: u64 = 64 * 1024;
const FEEDBACK_PER_HOUR: usize = 3;
const FEEDBACK_PER_DAY: usize = 10;
const HOUR_SECS: u64 = 60 * 60;
const DAY_SECS: u64 = 24 * HOUR_SECS;

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct FeedbackRateState {
    submitted_at: Vec<u64>,
}

pub(crate) async fn run_feedback_command(
    store: Arc<Mutex<SecretStore>>,
    profile: &InstanceProfile,
    data_dir: &Path,
    command: FeedbackCommand,
) -> anyhow::Result<()> {
    validate_feedback(&command)?;
    let secret_entries = store.lock().await.entries()?;
    let redactions = redaction_entries(&secret_entries);
    let body = truncate_chars_owned(
        build_feedback_issue_body(
            &command.body,
            profile,
            data_dir,
            command.include_logs,
            &redactions,
        ),
        MAX_BODY_CHARS,
    );
    if command.dry_run {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "title": command.title,
                "body": body,
                "labels": command.labels,
            }))?
        );
        return Ok(());
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| anyhow::anyhow!("system clock is before the Unix epoch"))?
        .as_secs();
    reserve_feedback_slot(data_dir, now)?;
    let event_id = crate::telemetry::capture_feedback(
        &command.title,
        &body,
        &command.labels,
        profile.label(),
    )?;
    println!("Submitted feedback to Sentry: {event_id}");
    Ok(())
}

fn validate_feedback(command: &FeedbackCommand) -> anyhow::Result<()> {
    let title_len = command.title.chars().count();
    let body_len = command.body.chars().count();
    anyhow::ensure!(
        title_len <= MAX_TITLE_CHARS,
        "feedback title is too long ({title_len}/{MAX_TITLE_CHARS} characters)"
    );
    anyhow::ensure!(
        body_len <= MAX_BODY_CHARS,
        "feedback body is too long ({body_len}/{MAX_BODY_CHARS} characters)"
    );
    anyhow::ensure!(
        command.labels.len() <= MAX_LABELS,
        "too many feedback labels ({}/{MAX_LABELS})",
        command.labels.len()
    );
    for label in &command.labels {
        let len = label.chars().count();
        anyhow::ensure!(
            len <= MAX_LABEL_CHARS,
            "feedback label is too long ({len}/{MAX_LABEL_CHARS} characters)"
        );
    }
    Ok(())
}

fn reserve_feedback_slot(data_dir: &Path, now: u64) -> anyhow::Result<()> {
    let state_dir = data_dir.join("telemetry");
    let state_path = state_dir.join("feedback-rate.json");
    let mut state = match std::fs::read(&state_path) {
        Ok(bytes) => serde_json::from_slice::<FeedbackRateState>(&bytes).unwrap_or_default(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => FeedbackRateState::default(),
        Err(error) => return Err(error.into()),
    };
    state
        .submitted_at
        .retain(|timestamp| now.saturating_sub(*timestamp) < DAY_SECS);
    let hourly = state
        .submitted_at
        .iter()
        .filter(|timestamp| now.saturating_sub(**timestamp) < HOUR_SECS)
        .count();
    anyhow::ensure!(
        hourly < FEEDBACK_PER_HOUR && state.submitted_at.len() < FEEDBACK_PER_DAY,
        "feedback rate limit reached; try again later"
    );
    state.submitted_at.push(now);
    std::fs::create_dir_all(&state_dir)?;
    let temporary = state_dir.join(format!("feedback-rate-{}.tmp", std::process::id()));
    std::fs::write(&temporary, serde_json::to_vec(&state)?)?;
    std::fs::rename(temporary, state_path)?;
    Ok(())
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
        "{}\n\n---\n\n### Diagnostics\n\n```text\nremi-cat_version: {}\nos: {}\narch: {}\nprofile: {}\nsetup_state: {}\n{}\n```\n",
        user_body.trim(),
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
        profile.label(),
        setup_label,
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
        if let Ok(text) = read_log_tail(&path) {
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

fn read_log_tail(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    if len > MAX_LOG_BYTES_PER_FILE {
        file.seek(std::io::SeekFrom::Start(len - MAX_LOG_BYTES_PER_FILE))?;
    }
    let mut bytes = Vec::with_capacity(len.min(MAX_LOG_BYTES_PER_FILE) as usize);
    file.read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn truncate_chars_owned(value: String, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value;
    }
    value.chars().take(max_chars).collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feedback_rate_limit_has_hourly_and_daily_caps() {
        let directory = tempfile::tempdir().unwrap();
        for second in 0..FEEDBACK_PER_HOUR {
            reserve_feedback_slot(directory.path(), second as u64).unwrap();
        }
        assert!(reserve_feedback_slot(directory.path(), 10).is_err());

        for hour in 1..=(FEEDBACK_PER_DAY - FEEDBACK_PER_HOUR) {
            reserve_feedback_slot(directory.path(), hour as u64 * HOUR_SECS).unwrap();
        }
        assert!(reserve_feedback_slot(directory.path(), 23 * HOUR_SECS).is_err());
    }

    #[test]
    fn feedback_payload_is_bounded() {
        let command = FeedbackCommand {
            title: "x".repeat(MAX_TITLE_CHARS + 1),
            body: "body".into(),
            labels: Vec::new(),
            include_logs: false,
            dry_run: false,
        };
        assert!(validate_feedback(&command).is_err());

        let oversized = format!("prefix{}", "x".repeat(MAX_BODY_CHARS * 2));
        assert_eq!(
            truncate_chars_owned(oversized, MAX_BODY_CHARS)
                .chars()
                .count(),
            MAX_BODY_CHARS
        );
    }
}
