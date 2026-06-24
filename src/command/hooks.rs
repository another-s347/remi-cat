use std::path::{Path, PathBuf};

use crate::cli::HooksCommand;
use crate::config::{detect_setup_state, SetupState};
use crate::instance_profile::InstanceProfile;
use bot_core::HookManager;

pub(crate) async fn run_hooks_command(
    profile: &InstanceProfile,
    data_dir: &Path,
    command: HooksCommand,
) -> anyhow::Result<()> {
    unsafe {
        std::env::set_var("REMI_DATA_DIR", data_dir);
    }
    let mut effective_data_dir = data_dir.to_path_buf();
    if let SetupState::Initialized { config, .. } = detect_setup_state(data_dir) {
        config.apply_env_defaults();
        effective_data_dir = PathBuf::from(
            std::env::var("REMI_DATA_DIR").unwrap_or_else(|_| config.data_dir.clone()),
        );
    }
    let workspace_root = std::env::var("REMI_SANDBOX_HOST_DIR")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| profile.data_dir.clone()));
    let manager = HookManager::new(workspace_root, effective_data_dir);
    match command {
        HooksCommand::List { json } => {
            let statuses = manager.statuses().await;
            if json {
                println!("{}", serde_json::to_string_pretty(&statuses)?);
            } else if statuses.is_empty() {
                println!("No hooks configured.");
            } else {
                println!("{}", format_hook_statuses(&statuses));
            }
        }
        HooksCommand::Trust { hash } => {
            if manager.trust(&hash).await? {
                println!("Trusted hook `{hash}`.");
            } else {
                println!("No hook found for `{hash}`.");
            }
        }
        HooksCommand::Enable { hash } => {
            if manager.set_enabled(&hash, true).await? {
                println!("Enabled hook `{hash}`.");
            } else {
                println!("No hook found for `{hash}`.");
            }
        }
        HooksCommand::Disable { hash } => {
            if manager.set_enabled(&hash, false).await? {
                println!("Disabled hook `{hash}`.");
            } else {
                println!("No hook found for `{hash}`.");
            }
        }
    }
    Ok(())
}

pub(crate) fn format_hook_statuses(statuses: &[bot_core::HookStatus]) -> String {
    let mut lines = vec!["**hooks**".to_string()];
    for status in statuses {
        let trusted = if status.trusted {
            "trusted"
        } else {
            "untrusted"
        };
        let enabled = if status.enabled {
            "enabled"
        } else {
            "disabled"
        };
        let matcher = status.matcher.as_deref().unwrap_or("*");
        let command = status.command.as_deref().unwrap_or("");
        let mut line = format!(
            "- `{}` matcher `{matcher}` {trusted}/{enabled}: {command}",
            status.event
        );
        if let Some(warning) = &status.warning {
            line.push_str(&format!(" warning: {warning}"));
        }
        line.push_str(&format!("\n  hash: `{}`", status.hash));
        line.push_str(&format!("\n  source: `{}`", status.source));
        lines.push(line);
    }
    lines.join("\n")
}
