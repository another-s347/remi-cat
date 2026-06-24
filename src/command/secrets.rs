use std::sync::Arc;

use tokio::sync::Mutex;

use crate::cli::SecretCommand;
use crate::core::Runtime;
use crate::secret_store::{redaction_entries, SecretStore};

pub(crate) async fn run_secret_command(
    store: Arc<Mutex<SecretStore>>,
    command: &SecretCommand,
    redact_values: bool,
) -> anyhow::Result<String> {
    let store = store.lock().await;
    match command {
        SecretCommand::List => {
            let keys = store.keys()?;
            if keys.is_empty() {
                Ok(format!("secret store `{}` is empty", store.backend_label()))
            } else {
                Ok(format!(
                    "secret store `{}` keys:\n{}",
                    store.backend_label(),
                    keys.into_iter()
                        .map(|key| format!("- `{key}`"))
                        .collect::<Vec<_>>()
                        .join("\n")
                ))
            }
        }
        SecretCommand::Get(key) => match store.get(key)? {
            Some(value) if redact_values => Ok(format!("`{key}` is set: {}", mask_secret(&value))),
            Some(value) => Ok(format!("{key}={value}")),
            None => Ok(format!("`{key}` is not set")),
        },
        SecretCommand::Set { key, value } => {
            store.set(key, value)?;
            unsafe {
                std::env::set_var(key, value);
            }
            Ok(format!(
                "secret `{key}` saved to `{}`",
                store.backend_label()
            ))
        }
        SecretCommand::Delete(key) => {
            store.delete(key)?;
            unsafe {
                std::env::remove_var(key);
            }
            Ok(format!(
                "secret `{key}` deleted from `{}`",
                store.backend_label()
            ))
        }
    }
}

pub(crate) async fn handle_runtime_secret_command(
    runtime: &Runtime,
    command: &SecretCommand,
) -> anyhow::Result<String> {
    let reply = run_secret_command(Arc::clone(&runtime.secret_store), command, true).await?;
    let entries = runtime.secret_store.lock().await.entries()?;
    runtime
        .bot
        .update_secret_redactor(&redaction_entries(&entries));
    Ok(reply)
}

fn mask_secret(value: &str) -> String {
    let chars = value.chars().collect::<Vec<_>>();
    if chars.len() <= 8 {
        return "********".to_string();
    }
    format!(
        "{}…{}",
        chars.iter().take(4).collect::<String>(),
        chars
            .iter()
            .rev()
            .take(4)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<String>()
    )
}
