//! Daemon command dispatcher.
//!
//! Commands starting with `/` are extracted from Feishu messages.  This
//! module decides whether a command is handled by the Daemon directly or
//! passed through to the Agent.

use crate::docker::DockerManager;
use crate::restart::RestartHandle;
use crate::rpc_server::RpcServer;
use crate::secret_store::SecretStore;
use anyhow::Result;
use im_feishu::client::build_secret_manager_card;
use im_feishu::FeishuGateway;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::info;

// Used by fetch_latest_github_version and daemon_update_url (version-pinned downloads).
use reqwest;

/// All commands that the Daemon handles without forwarding to the Agent.
const DAEMON_COMMANDS: &[&str] = &[
    "help",
    "diagnose",
    "repair",
    "start",
    "stop",
    "restart",
    "restart-all",
    "update",
    "status",
    "logs",
    "version",
    "secrets",
];

/// Whether a command name is owned by the Daemon.
pub fn is_daemon_command(name: &str) -> bool {
    DAEMON_COMMANDS.contains(&name)
}

/// Execute a Daemon-owned command.  Returns a reply string to send back to
/// the user, or an error.  An **empty string** means the command sent its own
/// reply directly (e.g. via an interactive card) and no further text reply
/// is needed.
///
/// `docker` is `None` when the daemon is running without Docker (local
/// debugging mode — `/var/run/docker.sock` not accessible).
///
/// `data_dir` is the root of persisted bot data (default `.remi-cat`);
/// used by `/repair` and `/diagnose` to inspect memory files.
#[allow(clippy::too_many_arguments)]
pub async fn execute_daemon_command(
    name: &str,
    args: &[String],
    docker: Option<&DockerManager>,
    restart_handle: &RestartHandle,
    secret_store: &Arc<RwLock<SecretStore>>,
    rpc: &RpcServer,
    gateway: &FeishuGateway,
    reply_to_id: &str,
) -> Result<String> {
    // data_dir for diagnose/repair: derive from binary location or cwd.
    let data_dir_owned = std::env::var("REMI_DATA_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from(".remi-cat"));
    let data_dir: &Path = &data_dir_owned;
    info!(command = name, "executing daemon command");

    /// Return early with a friendly message when Docker is unavailable.
    macro_rules! require_docker {
        ($docker:expr) => {
            match $docker {
                Some(d) => d,
                None => return Ok("⚠️ Docker not available — daemon running in local mode.".to_string()),
            }
        };
    }

    match name {
        "help" => Ok(help_text()),

        "diagnose" => diagnose_memory(data_dir).await,

        "repair" => repair_memory(data_dir).await,

        "status" => match docker {
            Some(d) => {
                let s = d.status().await?;
                Ok(s)
            }
            None => Ok("remi-daemon running in **local mode** (no Docker).".to_string()),
        },

        "start" => {
            let d = require_docker!(docker);
            d.start().await?;
            Ok(format!("✅ Container `{}` started.", d.container_name()))
        }

        "stop" => {
            let d = require_docker!(docker);
            d.stop().await?;
            Ok(format!("✅ Container `{}` stopped.", d.container_name()))
        }

        "restart" => {
            let d = require_docker!(docker);
            d.restart().await?;
            Ok(format!("✅ Container `{}` restarted.", d.container_name()))
        }

        "logs" => {
            let d = require_docker!(docker);
            let tail: usize = args.first().and_then(|s| s.parse().ok()).unwrap_or(50);
            let log_text = d.logs(tail).await?;
            if log_text.is_empty() {
                Ok("(no recent logs)".to_string())
            } else {
                Ok(format!("```\n{}\n```", truncate(&log_text, 3000)))
            }
        }

        "version" => {
            let current = env!("CARGO_PKG_VERSION");
            let latest_tag = fetch_latest_github_version().await;
            match latest_tag {
                Some(ref latest) if latest == current => {
                    Ok(format!("remi-daemon **v{current}**（已是最新）"))
                }
                Some(ref latest) => Ok(format!(
                    "remi-daemon **v{current}**（最新可用: **v{latest}**，可运行 `/update` 升级）"
                )),
                None => Ok(format!("remi-daemon **v{current}**")),
            }
        }

        "restart-all" => {
            // Restart Agent container first (if available), then restart Daemon.
            if let Some(d) = docker {
                d.restart().await?;
            }
            restart_handle
                .spawn_restart(None)
                .await
                .map(|_| "🔄 Daemon restarting…".to_string())
                .map_err(|e| e.context("daemon restart failed"))
        }

        "update" => {
            // Optional explicit version: /update v1.2.3
            let requested_version: Option<&str> = args.first().map(String::as_str);

            // Check whether daemon itself needs updating.
            // When Docker is present, always restart the agent container even
            // if the daemon is already on the latest version (they may differ).
            let daemon_needs_update = if requested_version.is_some() {
                true // explicit version always forces update
            } else {
                let current = env!("CARGO_PKG_VERSION");
                match fetch_latest_github_version().await {
                    Some(latest) if latest == current => false,
                    _ => true,
                }
            };

            // 1. Restart Agent container (Docker sandbox) if available.
            let agent_restarted = if let Some(d) = docker {
                d.restart().await?;
                true
            } else {
                false
            };

            // 2. Download new Daemon binary (if configured) and restart self.
            if !daemon_needs_update {
                let current = env!("CARGO_PKG_VERSION");
                if agent_restarted {
                    return Ok(format!(
                        "✅ Agent 容器已重启（拉取最新镜像）。Daemon 已是最新版本 **v{current}**，无需重启。"
                    ));
                } else {
                    return Ok(format!("✅ 已是最新版本 **v{current}**，无需更新。"));
                }
            }

            let download_url = daemon_update_url(requested_version);
            match download_url {
                None => Ok(
                    "✅ Agent 容器已重启。⚠️ `GITHUB_REPO`/`DAEMON_UPDATE_URL` 未配置，Daemon 未更新。".to_string()
                ),
                Some(url) => {
                    restart_handle.spawn_restart(Some(url)).await?;
                    Ok("🔄 Daemon 正在下载更新并重启…".to_string())
                }
            }
        }

        // ── Secret management ─────────────────────────────────────────────────
        "secrets" => {
            match args.first().map(String::as_str) {
                // /secrets set KEY VALUE
                Some("set") => {
                    let key = args.get(1).ok_or_else(|| {
                        anyhow::anyhow!("/secrets set <KEY> <VALUE>: KEY missing")
                    })?;
                    let val = args.get(2).ok_or_else(|| {
                        anyhow::anyhow!("/secrets set <KEY> <VALUE>: VALUE missing")
                    })?;
                    secret_store.write().await.set(key, val)?;
                    info!(key, "secret set via command");
                    rpc.sync_secrets_to_agent().await;
                    Ok(format!("✅ Secret `{key}` 已设置。"))
                }
                // /secrets delete KEY
                Some("delete") | Some("del") | Some("rm") => {
                    let key = args
                        .get(1)
                        .ok_or_else(|| anyhow::anyhow!("/secrets delete <KEY>: KEY missing"))?;
                    secret_store.write().await.delete(key)?;
                    info!(key, "secret deleted via command");
                    rpc.sync_secrets_to_agent().await;
                    Ok(format!("✅ Secret `{key}` 已删除。"))
                }
                // /secrets (no subcommand) — render interactive card
                _ => {
                    let guard = secret_store.read().await;
                    let keys_info: Vec<(String, bool)> = guard
                        .keys()
                        .into_iter()
                        .map(|k| (k.to_string(), guard.is_system(k)))
                        .collect();
                    drop(guard);
                    let keys_ref: Vec<(&str, bool)> =
                        keys_info.iter().map(|(k, s)| (k.as_str(), *s)).collect();
                    let card = build_secret_manager_card(&keys_ref);
                    if let Err(e) = gateway.reply_card_raw(reply_to_id, card).await {
                        return Err(e.context("failed to send secret manager card"));
                    }
                    // Return empty string — reply was sent directly as card.
                    Ok(String::new())
                }
            }
        }

        other => Err(anyhow::anyhow!("unknown daemon command: {other}")),
    }
}

// ── Global system commands ─────────────────────────────────────────────────

fn help_text() -> String {
    r#"**remi-cat 指令列表**

**系统指令（由 Daemon 处理，不经过 AI）：**
`/help` — 显示此帮助
`/diagnose` — 诊断记忆文件健康状态
`/repair` — 修复损坏的短期记忆
`/status` — 查看 Agent 容器状态
`/start` / `/stop` / `/restart` — 管理 Agent 容器
`/restart-all` — 重启 Agent 容器 + Daemon 自身
`/update` — 更新镜像并重启
`/logs [N]` — 查看 Agent 最近 N 条日志（默认 50）
`/version` — 查看 Daemon 版本
`/secrets` — 管理 Secret（可交互卡片）
`/secrets set KEY VALUE` — 直接设置 Secret
`/secrets delete KEY` — 删除 Secret

**会话指令（由 Agent 处理，不调用 LLM）：**
`/compact` — 立即将短期记忆压缩为中期记忆"#
        .to_string()
}

/// Scan memory files and report health status for each conversation thread.
async fn diagnose_memory(data_dir: &Path) -> Result<String> {
    let memory_dir = data_dir.join("memory");
    if !memory_dir.exists() {
        return Ok("📂 记忆目录不存在，尚无会话记录。".to_string());
    }

    let mut output = String::from("🔍 **记忆诊断**\n\n");
    let mut has_any = false;

    let mut read_dir = tokio::fs::read_dir(&memory_dir).await?;
    while let Some(entry) = read_dir.next_entry().await? {
        if !entry.file_type().await?.is_dir() {
            continue;
        }
        has_any = true;
        let thread_id = entry.file_name().to_string_lossy().to_string();
        output.push_str(&format!("**{}**\n", thread_id));

        // Short-term
        let short_path = entry.path().join("short_term.jsonl");
        if short_path.exists() {
            let text = tokio::fs::read_to_string(&short_path)
                .await
                .unwrap_or_default();
            let values: Vec<serde_json::Value> = text
                .lines()
                .filter(|l| !l.trim().is_empty())
                .filter_map(|l| serde_json::from_str(l).ok())
                .collect();
            let count = values.len();
            let first_role = values
                .first()
                .and_then(|v| v["role"].as_str().map(str::to_string))
                .unwrap_or_else(|| "empty".to_string());
            let health = if count == 0 || first_role == "user" || first_role == "system" {
                "✅"
            } else {
                "⚠️ 头部损坏"
            };
            output.push_str(&format!(
                "  短期记忆: {} 条（首条 role: {}）{}\n",
                count, first_role, health
            ));
        } else {
            output.push_str("  短期记忆: 无\n");
        }

        // Mid-term
        let mid_index = entry.path().join("mid_term").join("index.json");
        if mid_index.exists() {
            let text = tokio::fs::read_to_string(&mid_index)
                .await
                .unwrap_or_default();
            let count = serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|v| v["entries"].as_array().map(|a| a.len()))
                .unwrap_or(0);
            output.push_str(&format!("  中期记忆: {} 块\n", count));
        } else {
            output.push_str("  中期记忆: 无\n");
        }

        // Long-term
        let long_index = entry.path().join("long_term").join("index.json");
        if long_index.exists() {
            let text = tokio::fs::read_to_string(&long_index)
                .await
                .unwrap_or_default();
            let count = serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|v| v["entries"].as_array().map(|a| a.len()))
                .unwrap_or(0);
            output.push_str(&format!("  长期记忆: {} 块\n", count));
        } else {
            output.push_str("  长期记忆: 无\n");
        }

        output.push('\n');
    }

    if !has_any {
        output.push_str("（无会话记录）");
    }

    Ok(output.trim_end().to_string())
}

/// Remove orphaned Tool/Assistant messages from the head of every
/// short_term.jsonl.  Returns a summary of what was fixed.
async fn repair_memory(data_dir: &Path) -> Result<String> {
    let memory_dir = data_dir.join("memory");
    if !memory_dir.exists() {
        return Ok("📂 记忆目录不存在，无需修复。".to_string());
    }

    let mut total = 0usize;
    let mut repaired = 0usize;

    let mut read_dir = tokio::fs::read_dir(&memory_dir).await?;
    while let Some(entry) = read_dir.next_entry().await? {
        if !entry.file_type().await?.is_dir() {
            continue;
        }
        total += 1;
        let short_path = entry.path().join("short_term.jsonl");
        if !short_path.exists() {
            continue;
        }

        let text = tokio::fs::read_to_string(&short_path).await?;
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();

        // Find the first User or System message
        let start = lines
            .iter()
            .position(|l| {
                serde_json::from_str::<serde_json::Value>(l)
                    .ok()
                    .and_then(|v| v["role"].as_str().map(str::to_string))
                    .map(|r| r == "user" || r == "system")
                    .unwrap_or(false)
            })
            .unwrap_or(lines.len());

        if start > 0 {
            let cleaned: String = lines[start..].iter().map(|l| format!("{l}\n")).collect();
            tokio::fs::write(&short_path, cleaned).await?;
            repaired += 1;
        }
    }

    if repaired == 0 {
        Ok(format!(
            "✅ 检查了 {total} 个会话，记忆状态正常，无需修复。"
        ))
    } else {
        Ok(format!(
            "🔧 修复完成：{repaired}/{total} 个会话的短期记忆头部损坏已清除。"
        ))
    }
}

/// Construct the download URL for the daemon binary.
/// `version` — explicit tag like `"v1.2.3"`; `None` means latest release.
fn daemon_update_url(version: Option<&str>) -> Option<String> {
    if let Ok(url) = std::env::var("DAEMON_UPDATE_URL") {
        if !url.is_empty() {
            return Some(url);
        }
    }
    if let Ok(repo) = std::env::var("GITHUB_REPO") {
        if !repo.is_empty() {
            let url = match version {
                Some(tag) => format!(
                    "https://github.com/{repo}/releases/download/{tag}/remi-daemon-linux-x86_64"
                ),
                None => format!(
                    "https://github.com/{repo}/releases/latest/download/remi-daemon-linux-x86_64"
                ),
            };
            return Some(url);
        }
    }
    None
}

/// Query the GitHub Releases API and return the latest tag name (without
/// the leading `v`), e.g. `"0.2.0"`. Returns `None` if `GITHUB_REPO` is
/// not set or the request fails.
async fn fetch_latest_github_version() -> Option<String> {
    let repo = std::env::var("GITHUB_REPO")
        .ok()
        .filter(|s| !s.is_empty())?;
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let client = reqwest::Client::new();
    let resp = client
        .get(&url)
        .header(reqwest::header::USER_AGENT, "remi-daemon")
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .timeout(std::time::Duration::from_secs(8))
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?;
    let json: serde_json::Value = resp.json().await.ok()?;
    let tag = json["tag_name"].as_str()?;
    Some(tag.trim_start_matches('v').to_string())
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let mut end = max_chars;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}… (truncated)", &s[..end])
    }
}
