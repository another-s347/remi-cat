use anyhow::Context;

mod doctor;
mod feedback;
mod feishu;
mod hooks;
mod secrets;
mod setup;
mod update;

pub(crate) use doctor::{
    command_doctor_report, print_registered_tools, run_acp_command, run_codex_command, run_doctor,
    sandbox_doctor_report,
};
#[cfg(test)]
pub(crate) use feedback::redact_known_secrets;
pub(crate) use feedback::run_feedback_command;
#[cfg(test)]
pub(crate) use feishu::{
    extract_first_url, extract_lark_cli_config_from_json, feishu_doctor_message,
    run_streaming_command, run_streaming_command_with_stdin, AuthStatusSummary, FeishuDoctorStatus,
};
pub(crate) use feishu::{run_feishu_doctor, run_feishu_init};
pub(crate) use hooks::{format_hook_statuses, run_hooks_command};
pub(crate) use secrets::{handle_runtime_secret_command, run_secret_command};
pub(crate) use setup::run_setup;
pub(crate) use update::run_update_command;
#[cfg(test)]
pub(crate) use update::{
    build_cargo_install_args, normalize_release_tag, parse_release_version, update_available,
};

use crate::app::{
    MAX_COMMAND_PREPROCESS_DEPTH, SESSION_AGENT_ID_METADATA_KEY, SESSION_DEBUG_METADATA_KEY,
    SESSION_MODEL_PROFILE_METADATA_KEY, SESSION_REASONING_EFFORT_METADATA_KEY,
};
use crate::cli::parse_secret_command;
use crate::core::Runtime;
use bot_core::{
    api_key_from_env, model_profile_key_status, AccountBalance, AccountUsage, AccountUsageStatus,
    CatBot, GoalMaxRounds, ModelProfileConfig, ReasoningEffort, SkillDocument, SkillLoadDiagnostic,
    SkillLoadDiagnosticSeverity, ToolTaskRecord,
};

pub(crate) enum RuntimeCommandResult {
    Reply(String),
    StartWorkflow {
        workflow_id: String,
        context: serde_json::Value,
        max_rounds: GoalMaxRounds,
    },
    Continue {
        text: String,
        prefix: String,
        skill_injections: Vec<SkillDocument>,
    },
}

pub(crate) enum RuntimeCommandPipelineResult {
    Reply(String),
    StartWorkflow {
        prefix: String,
        workflow_id: String,
        context: serde_json::Value,
        max_rounds: GoalMaxRounds,
    },
    Continue {
        text: String,
        prefix: String,
        skill_injections: Vec<SkillDocument>,
    },
}

pub(crate) async fn process_runtime_commands(
    runtime: &Runtime,
    session_id: &str,
    command: &str,
) -> anyhow::Result<RuntimeCommandPipelineResult> {
    let mut text = command.trim().to_string();
    let mut prefix = String::new();
    let mut skill_injections = Vec::new();
    for _ in 0..MAX_COMMAND_PREPROCESS_DEPTH {
        match handle_runtime_command(runtime, session_id, text.trim()).await? {
            Some(RuntimeCommandResult::Reply(reply)) => {
                prefix.push_str(&reply);
                return Ok(RuntimeCommandPipelineResult::Reply(prefix));
            }
            Some(RuntimeCommandResult::StartWorkflow {
                workflow_id,
                context,
                max_rounds,
            }) => {
                return Ok(RuntimeCommandPipelineResult::StartWorkflow {
                    prefix,
                    workflow_id,
                    context,
                    max_rounds,
                });
            }
            Some(RuntimeCommandResult::Continue {
                text: next_text,
                prefix: next_prefix,
                skill_injections: mut next_skill_injections,
            }) => {
                prefix.push_str(&next_prefix);
                skill_injections.append(&mut next_skill_injections);
                text = next_text;
                if text.trim().is_empty() {
                    return Ok(RuntimeCommandPipelineResult::Reply(prefix));
                }
            }
            None => {
                return Ok(RuntimeCommandPipelineResult::Continue {
                    text,
                    prefix,
                    skill_injections,
                });
            }
        }
    }
    Ok(RuntimeCommandPipelineResult::Reply(
        "command preprocessing exceeded the maximum nesting depth".to_string(),
    ))
}

pub(crate) async fn handle_runtime_command(
    runtime: &Runtime,
    session_id: &str,
    command: &str,
) -> anyhow::Result<Option<RuntimeCommandResult>> {
    if command == "/help" || command == "/commands" {
        return Ok(Some(RuntimeCommandResult::Reply(runtime_command_help())));
    }
    if command == "/tools" {
        let (session_agent_id, session_model_profile_id) = {
            let sessions = runtime.sessions.lock().await;
            (
                sessions.metadata_string(session_id, SESSION_AGENT_ID_METADATA_KEY),
                sessions.metadata_string(session_id, SESSION_MODEL_PROFILE_METADATA_KEY),
            )
        };
        let workflow_agent_id = runtime
            .bot
            .workflow_status(session_id)
            .await
            .and_then(|instance| workflow_node_agent(&instance).map(ToOwned::to_owned));
        let effective = runtime.bot.effective_agent_profile_for_workflow(
            session_agent_id.as_deref(),
            workflow_agent_id.as_deref(),
        );
        let reply = runtime
            .bot
            .tool_list_for_agent_and_model(
                Some(&effective.profile.id),
                session_model_profile_id.as_deref(),
            )
            .into_iter()
            .map(|(name, desc)| format!("- `{name}`: {desc}"))
            .collect::<Vec<_>>()
            .join("\n");
        return Ok(Some(RuntimeCommandResult::Reply(reply)));
    }
    if command == "/tasks" || command.starts_with("/tasks ") {
        let task_thread_id = {
            let sessions = runtime.sessions.lock().await;
            resolve_task_thread_id(
                session_id,
                sessions
                    .metadata_string(session_id, "sub_session_thread_id")
                    .as_deref(),
            )
        };
        let reply = handle_tasks_command(&runtime.bot, &task_thread_id, command).await?;
        return Ok(Some(RuntimeCommandResult::Reply(reply)));
    }
    if command == "/skill" || command.starts_with("/skill ") || command.starts_with("/skill:") {
        let result = handle_skill_command(runtime, session_id, command).await?;
        return Ok(Some(result));
    }
    if command.starts_with("/debug") {
        let reply = handle_debug_command(runtime, session_id, command).await?;
        return Ok(Some(RuntimeCommandResult::Reply(reply)));
    }
    if command.starts_with("/secrets") || command.starts_with("/secret") {
        let args = command
            .trim_start_matches('/')
            .split_whitespace()
            .skip(1)
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>();
        let command = parse_secret_command(&args)?;
        let reply = handle_runtime_secret_command(runtime, &command).await?;
        return Ok(Some(RuntimeCommandResult::Reply(reply)));
    }
    if command.starts_with("/goal") {
        let reply = handle_goal_command(&runtime.bot, session_id, command).await?;
        if is_goal_set_command(command) && reply.starts_with("已设置 goal。") {
            let goal = runtime
                .bot
                .goal_status(session_id)
                .await
                .ok_or_else(|| anyhow::anyhow!("goal was not persisted after set"))?;
            return Ok(Some(RuntimeCommandResult::Continue {
                text: goal.goal,
                prefix: format!("{reply}\n\n---\n\n"),
                skill_injections: Vec::new(),
            }));
        }
        return Ok(Some(RuntimeCommandResult::Reply(reply)));
    }
    if command.starts_with("/workflow") {
        return handle_workflow_command(&runtime.bot, session_id, command)
            .await
            .map(Some);
    }
    if command == "/model" || command.starts_with("/model ") {
        let reply = handle_model_command(runtime, session_id, command).await?;
        return Ok(Some(RuntimeCommandResult::Reply(reply)));
    }
    if command == "/agent" || command.starts_with("/agent ") {
        let reply = handle_agent_command(runtime, session_id, command).await?;
        return Ok(Some(RuntimeCommandResult::Reply(reply)));
    }
    if command == "/permissions"
        || command.starts_with("/permissions ")
        || command == "/permission"
        || command.starts_with("/permission ")
    {
        let reply = handle_permissions_command(runtime, session_id, command).await?;
        return Ok(Some(RuntimeCommandResult::Reply(reply)));
    }
    if command == "/hooks" || command.starts_with("/hooks ") {
        let reply = handle_hooks_command(runtime, command).await?;
        return Ok(Some(RuntimeCommandResult::Reply(reply)));
    }
    if command == "/compact" {
        let (model_profile, reasoning_effort) =
            session_model_and_reasoning(runtime, session_id).await?;
        let agent_id = runtime
            .sessions
            .lock()
            .await
            .metadata_string(session_id, SESSION_AGENT_ID_METADATA_KEY);
        let n = runtime
            .bot
            .compact_memory_with_profile(
                session_id,
                model_profile.as_deref(),
                agent_id.as_deref(),
                reasoning_effort,
            )
            .await?;
        return Ok(Some(RuntimeCommandResult::Reply(format!(
            "compacted {n} short-term message(s)."
        ))));
    }
    if command == "/clear" {
        runtime.bot.clear_memory(session_id).await?;
        return Ok(Some(RuntimeCommandResult::Reply(
            "已清空当前 session 的历史会话。Todo 等工具状态已保留。".to_string(),
        )));
    }
    if command == "/doctor" {
        return Ok(Some(RuntimeCommandResult::Reply(command_doctor_report(
            runtime,
        ))));
    }
    if command == "/usage" {
        let model_profile_id = runtime
            .sessions
            .lock()
            .await
            .metadata_string(session_id, SESSION_MODEL_PROFILE_METADATA_KEY);
        let usage = runtime
            .bot
            .account_usage_for(model_profile_id.as_deref())
            .await?;
        return Ok(Some(RuntimeCommandResult::Reply(format_account_usage(
            &usage,
        ))));
    }
    if let Some(result) = handle_direct_workflow_command(&runtime.bot, command).await? {
        return Ok(Some(result));
    }
    Ok(None)
}

fn runtime_command_help() -> String {
    [
        "**runtime commands**",
        "",
        "- `/help` - show this command list",
        "- `/tools` - list tools available to the current agent",
        "- `/tasks` - list background tool tasks for this session",
        "- `/tasks get <task_id>|cancel <task_id>` - inspect or cancel this session's background tasks",
        "- `/skill list` - list available local and builtin skills",
        "- `/skill status` - show skills already read or activated in this session",
        "- `/skill:<name> <task>` - load a skill for the same message and run the task",
        "- `/doctor` - show runtime readiness for the current session",
        "- `/model` or `/model list` - inspect or select model profiles for this session",
        "- `/agent` or `/agent list` - inspect or select the main agent for this session",
        "- `/permissions` - inspect or change tool approval mode for this session",
        "- `/hooks` - list/trust/enable/disable Remi hooks",
        "- `/goal ...` - inspect or manage the current goal workflow",
        "- `/workflow ...` - inspect or manage supervisor workflow state",
        "- `/<workflow-id> ...` - start a supervisor workflow directly",
        "- `/compact` - compact short-term memory",
        "- `/clear` - clear chat memory for this session",
    ]
    .join("\n")
}

fn resolve_task_thread_id(session_id: &str, sub_session_thread_id: Option<&str>) -> String {
    sub_session_thread_id
        .map(str::trim)
        .filter(|thread_id| !thread_id.is_empty())
        .unwrap_or(session_id)
        .to_string()
}

async fn handle_tasks_command(
    bot: &CatBot,
    session_id: &str,
    command: &str,
) -> anyhow::Result<String> {
    let parts = command.split_whitespace().collect::<Vec<_>>();
    match parts.as_slice() {
        ["/tasks"] | ["/tasks", "list"] => {
            let tasks = bot.list_background_tasks(Some(session_id)).await;
            Ok(format_tool_tasks(&tasks))
        }
        ["/tasks", "all"] => Ok("仅支持查看当前 session 的后台任务；请使用 `/tasks`。".to_string()),
        ["/tasks", "get", task_id] => {
            let task = bot
                .get_background_task(task_id)
                .await
                .filter(|task| task_belongs_to_session(task, session_id));
            Ok(task
                .map(|task| format_tool_task_detail(&task))
                .unwrap_or_else(|| format!("background task not found: {task_id}")))
        }
        ["/tasks", "cancel", task_id] => {
            let owned = bot
                .get_background_task(task_id)
                .await
                .is_some_and(|task| task_belongs_to_session(&task, session_id));
            let task = if owned {
                bot.cancel_background_task(task_id).await
            } else {
                None
            };
            Ok(task
                .map(|task| format!("{} {}", task.task_id, task.status))
                .unwrap_or_else(|| format!("background task not found: {task_id}")))
        }
        _ => Ok("用法：/tasks [list|get <task_id>|cancel <task_id>]".to_string()),
    }
}

fn task_belongs_to_session(task: &ToolTaskRecord, session_id: &str) -> bool {
    task.background && task.thread_id == session_id
}

fn format_tool_tasks(tasks: &[ToolTaskRecord]) -> String {
    if tasks.is_empty() {
        return "no background tool tasks".to_string();
    }
    let now = chrono::Utc::now();
    tasks
        .iter()
        .map(|task| {
            let args = compact_task_args(&task.args, 160);
            let timing = task_timing_label(task, &now);
            format!(
                "- `{}` `{}` `{}` args: `{}` · {}",
                task.status, task.task_id, task.tool_name, args, timing
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_tool_task_detail(task: &ToolTaskRecord) -> String {
    let mut lines = vec![
        format!("task_id: {}", task.task_id),
        format!("status: {}", task.status),
        format!("thread_id: {}", task.thread_id),
        format!("tool: {}", task.tool_name),
        format!(
            "args: {}",
            serde_json::to_string_pretty(&task.args).unwrap_or_else(|_| task.args.to_string())
        ),
        format!("elapsed_ms: {}", task.elapsed_ms.unwrap_or(0)),
    ];
    if let Some(message) = task.message.as_deref() {
        lines.push(format!("message: {message}"));
    }
    if !task.recent_output.is_empty() {
        lines.push(format!("recent_output:\n{}", task.recent_output.join("\n")));
    }
    if let Some(result) = task.result_preview.as_deref() {
        lines.push(format!("result:\n{result}"));
    }
    lines.join("\n")
}

fn compact_task_args(args: &serde_json::Value, max_chars: usize) -> String {
    let value = serde_json::to_string(args).unwrap_or_else(|_| args.to_string());
    if value.chars().count() <= max_chars {
        return value;
    }
    let mut truncated = value
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    truncated.push('…');
    truncated
}

fn task_timing_label(task: &ToolTaskRecord, now: &chrono::DateTime<chrono::Utc>) -> String {
    let timestamp = if task.status == bot_core::tool_tasks::TOOL_TASK_RUNNING {
        Some(task.started_at.as_str())
    } else {
        task.completed_at.as_deref()
    };
    let elapsed = timestamp
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .map(|timestamp| {
            now.signed_duration_since(timestamp.with_timezone(&chrono::Utc))
                .num_milliseconds()
                .max(0) as u64
        });
    let Some(elapsed) = elapsed else {
        return "unknown".to_string();
    };
    if task.status == bot_core::tool_tasks::TOOL_TASK_RUNNING {
        format!("运行 {}", format_relative_duration(elapsed))
    } else {
        format!("{} 前结束", format_relative_duration(elapsed))
    }
}

fn format_relative_duration(elapsed_ms: u64) -> String {
    let seconds = elapsed_ms / 1_000;
    if seconds == 0 {
        "<1s".to_string()
    } else if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3_600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h", seconds / 3_600)
    } else {
        format!("{}d", seconds / 86_400)
    }
}

pub(crate) async fn handle_agent_command(
    runtime: &Runtime,
    session_id: &str,
    command: &str,
) -> anyhow::Result<String> {
    let rest = command.trim().strip_prefix("/agent").unwrap_or("").trim();
    if rest.is_empty() || rest == "status" {
        let stored = runtime
            .sessions
            .lock()
            .await
            .metadata_string(session_id, SESSION_AGENT_ID_METADATA_KEY);
        let stored_model = runtime
            .sessions
            .lock()
            .await
            .metadata_string(session_id, SESSION_MODEL_PROFILE_METADATA_KEY);
        return Ok(format_agent_status(
            &runtime.bot,
            stored.as_deref(),
            stored_model.as_deref(),
            runtime.bot.workflow_status(session_id).await.as_ref(),
            session_id,
        ));
    }
    if rest == "list" {
        let stored = runtime
            .sessions
            .lock()
            .await
            .metadata_string(session_id, SESSION_AGENT_ID_METADATA_KEY);
        return Ok(format_agent_list(&runtime.bot, stored.as_deref()));
    }
    if rest == "reset" {
        runtime
            .sessions
            .lock()
            .await
            .remove_metadata(session_id, SESSION_AGENT_ID_METADATA_KEY)?;
        let stored_model = runtime
            .sessions
            .lock()
            .await
            .metadata_string(session_id, SESSION_MODEL_PROFILE_METADATA_KEY);
        return Ok(format!(
            "已清除当前 session 的 agent override。\n\n{}",
            format_agent_status(
                &runtime.bot,
                None,
                stored_model.as_deref(),
                runtime.bot.workflow_status(session_id).await.as_ref(),
                session_id,
            )
        ));
    }
    if let Some(id) = rest.strip_prefix("use").map(str::trim) {
        if id.is_empty() {
            anyhow::bail!("用法：/agent use <agent_id>");
        }
        let Some(profile) = runtime.bot.get_agent_profile(id) else {
            anyhow::bail!("unknown agent `{id}`. Use `/agent list` to see available agents.");
        };
        let agent_id = profile.id.clone();
        runtime.sessions.lock().await.set_metadata_string(
            session_id,
            SESSION_AGENT_ID_METADATA_KEY,
            &agent_id,
        )?;
        let stored_model = runtime
            .sessions
            .lock()
            .await
            .metadata_string(session_id, SESSION_MODEL_PROFILE_METADATA_KEY);
        return Ok(format!(
            "已切换当前 session agent 为 `{}`。\n\n{}",
            agent_id,
            format_agent_status(
                &runtime.bot,
                Some(&agent_id),
                stored_model.as_deref(),
                runtime.bot.workflow_status(session_id).await.as_ref(),
                session_id
            )
        ));
    }
    Ok("用法：/agent status，/agent list，/agent use <agent_id>，/agent reset".to_string())
}

pub(crate) fn format_agent_status(
    bot: &CatBot,
    stored_agent_id: Option<&str>,
    stored_model_profile_id: Option<&str>,
    workflow: Option<&bot_core::WorkflowInstance>,
    session_id: &str,
) -> String {
    let workflow_agent_id = workflow.and_then(workflow_node_agent);
    let effective = bot.effective_agent_profile_for_workflow(stored_agent_id, workflow_agent_id);
    let model = bot.effective_model_profile_for_agent(stored_model_profile_id, &effective.profile);
    let source = if workflow_agent_id.is_some() && effective.invalid_session_agent.is_none() {
        "workflow"
    } else if effective.invalid_session_agent.is_none()
        && stored_agent_id
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .is_some()
    {
        "session"
    } else {
        "default"
    };
    let model_source = if stored_model_profile_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .is_some()
    {
        "session"
    } else if effective.profile.models.primary.is_some() {
        "agent"
    } else {
        "default"
    };
    let mut lines = vec![
        "**agent status**".to_string(),
        String::new(),
        format!("session_id: `{session_id}`"),
        format!("effective_agent: `{}`", effective.profile.id),
        format!("source: `{source}`"),
        format!("name: {}", effective.profile.name),
        format!("description: {}", effective.profile.description),
        format!("effective_model_profile: `{}`", model.profile.id),
        format!("model_source: `{model_source}`"),
        format!("model: `{}`", model.profile.model),
    ];
    if let Some(instance) = workflow {
        if let Some(agent) = workflow_agent_id {
            lines.push(format!(
                "workflow_override: `{}` node `{}` -> agent `{agent}`",
                instance.definition.id, instance.current_node
            ));
        }
    }
    if let Some(stored) = stored_agent_id {
        lines.push(format!("session_override: `{stored}`"));
    } else {
        lines.push("session_override: none".to_string());
    }
    if let Some(invalid) = effective.invalid_session_agent {
        lines.push(format!(
            "warning: stored session override `{invalid}` is invalid; using fallback `{}`",
            effective.profile.id
        ));
    }
    lines.join("\n")
}

pub(crate) fn format_agent_list(bot: &CatBot, stored_agent_id: Option<&str>) -> String {
    let effective = bot.effective_agent_profile(stored_agent_id);
    let default_id = &bot.default_agent_profile().id;
    let mut profiles = bot.agent_profiles();
    profiles.sort_by(|a, b| a.id.cmp(&b.id));
    let mut lines = vec!["**agents**".to_string(), String::new()];
    for profile in profiles {
        let mut markers = Vec::new();
        if profile.id == effective.profile.id {
            markers.push("current");
        }
        if &profile.id == default_id {
            markers.push("default");
        }
        let marker = if markers.is_empty() {
            String::new()
        } else {
            format!(" [{}]", markers.join(", "))
        };
        lines.push(format!(
            "- `{}`{}: {}",
            profile.id, marker, profile.description
        ));
    }
    if let Some(invalid) = effective.invalid_session_agent {
        lines.push(format!(
            "\nwarning: stored session override `{invalid}` is invalid; using fallback `{}`",
            effective.profile.id
        ));
    }
    lines.join("\n")
}

async fn handle_hooks_command(runtime: &Runtime, command: &str) -> anyhow::Result<String> {
    let parts = command.split_whitespace().collect::<Vec<_>>();
    let manager = runtime.bot.hook_manager();
    match parts.as_slice() {
        ["/hooks"] | ["/hooks", "list"] => {
            let statuses = manager.statuses().await;
            if statuses.is_empty() {
                return Ok("No hooks configured.".to_string());
            }
            Ok(format_hook_statuses(&statuses))
        }
        ["/hooks", "trust", hash] => {
            if manager.trust(hash).await? {
                Ok(format!("Trusted hook `{hash}`."))
            } else {
                Ok(format!("No hook found for `{hash}`."))
            }
        }
        ["/hooks", "enable", hash] => {
            if manager.set_enabled(hash, true).await? {
                Ok(format!("Enabled hook `{hash}`."))
            } else {
                Ok(format!("No hook found for `{hash}`."))
            }
        }
        ["/hooks", "disable", hash] => {
            if manager.set_enabled(hash, false).await? {
                Ok(format!("Disabled hook `{hash}`."))
            } else {
                Ok(format!("No hook found for `{hash}`."))
            }
        }
        _ => Ok(
            "Usage: `/hooks`, `/hooks trust <hash>`, `/hooks enable <hash>`, `/hooks disable <hash>`."
                .to_string(),
        ),
    }
}

async fn handle_permissions_command(
    runtime: &Runtime,
    session_id: &str,
    command: &str,
) -> anyhow::Result<String> {
    use bot_core::approval::ApprovalSessionPolicy;

    let rest = command
        .trim()
        .trim_start_matches('/')
        .split_once(char::is_whitespace)
        .map(|(_, rest)| rest.trim())
        .unwrap_or("");
    let manager = runtime.bot.approval_manager();

    let policy = match rest {
        "" | "status" => None,
        "low" | "ask" | "default" | "prompt" | "reset" => Some(ApprovalSessionPolicy::Low),
        "medium" | "auto" | "model-auto" | "model_auto" => Some(ApprovalSessionPolicy::Medium),
        "allow" | "allow-session" | "allow_session" | "bypass" | "trusted" => {
            Some(ApprovalSessionPolicy::Medium)
        }
        _ => {
            return Ok(format!(
                "用法：/permissions status | low | medium\n\n{}",
                format_permissions_status(session_id, manager.session_policy(session_id).await)
            ));
        }
    };

    if let Some(policy) = policy {
        manager.set_session_policy(session_id, policy).await;
    }

    Ok(format_permissions_status(
        session_id,
        manager.session_policy(session_id).await,
    ))
}

fn format_permissions_status(
    session_id: &str,
    policy: bot_core::approval::ApprovalSessionPolicy,
) -> String {
    format!(
        "**permissions**\n\nsession_id: `{session_id}`\nmode: `{}`\n{}\n\nmodes:\n- `low`: low 自动通过；medium/high 请求审批\n- `medium`: low/medium 自动通过；high 请求审批",
        policy.label(),
        policy.description()
    )
}

async fn handle_model_command(
    runtime: &Runtime,
    session_id: &str,
    command: &str,
) -> anyhow::Result<String> {
    let rest = command.trim().strip_prefix("/model").unwrap_or("").trim();
    if rest.is_empty() || rest == "status" {
        let (stored, stored_reasoning) = session_model_and_reasoning(runtime, session_id).await?;
        return Ok(format_model_status(
            &runtime.bot,
            stored.as_deref(),
            stored_reasoning,
            session_id,
        ));
    }
    if rest == "list" {
        let stored = runtime
            .sessions
            .lock()
            .await
            .metadata_string(session_id, SESSION_MODEL_PROFILE_METADATA_KEY);
        return Ok(format_model_list(&runtime.bot, stored.as_deref()));
    }
    if rest == "reasoning" || rest == "reasoning status" {
        let (stored, stored_reasoning) = session_model_and_reasoning(runtime, session_id).await?;
        return Ok(format_model_reasoning_status(
            &runtime.bot,
            stored.as_deref(),
            stored_reasoning,
            session_id,
        ));
    }
    if rest == "reasoning list" {
        return Ok(format_model_reasoning_list());
    }
    if rest == "reasoning reset" {
        let (stored, _) = {
            let mut sessions = runtime.sessions.lock().await;
            sessions.remove_metadata(session_id, SESSION_REASONING_EFFORT_METADATA_KEY)?;
            (
                sessions.metadata_string(session_id, SESSION_MODEL_PROFILE_METADATA_KEY),
                None::<ReasoningEffort>,
            )
        };
        return Ok(format!(
            "Cleared the current session reasoning override.\n\n{}",
            format_model_reasoning_status(&runtime.bot, stored.as_deref(), None, session_id)
        ));
    }
    if let Some(value) = rest
        .strip_prefix("reasoning set")
        .or_else(|| rest.strip_prefix("reasoning use"))
        .map(str::trim)
    {
        if value.is_empty() {
            anyhow::bail!(
                "Usage: /model reasoning set <auto|none|minimal|low|medium|high|xhigh|max>"
            );
        }
        let effort = ReasoningEffort::parse(value).ok_or_else(|| {
            anyhow::anyhow!(
                "unknown reasoning effort `{value}`. Use `/model reasoning list` to see available values."
            )
        })?;
        let stored_model = runtime
            .sessions
            .lock()
            .await
            .metadata_string(session_id, SESSION_MODEL_PROFILE_METADATA_KEY);
        runtime
            .bot
            .effective_model_profile_with_reasoning(stored_model.as_deref(), Some(effort))
            .with_context(|| {
                format!(
                    "reasoning effort `{}` is not supported by the current model profile",
                    effort.as_str()
                )
            })?;
        let stored_reasoning = if effort == ReasoningEffort::Auto {
            runtime
                .sessions
                .lock()
                .await
                .remove_metadata(session_id, SESSION_REASONING_EFFORT_METADATA_KEY)?;
            None
        } else {
            runtime.sessions.lock().await.set_metadata_string(
                session_id,
                SESSION_REASONING_EFFORT_METADATA_KEY,
                effort.as_str(),
            )?;
            Some(effort)
        };
        return Ok(format!(
            "Set the current session reasoning effort to `{}`.\n\n{}",
            effort.as_str(),
            format_model_reasoning_status(
                &runtime.bot,
                stored_model.as_deref(),
                stored_reasoning,
                session_id
            )
        ));
    }
    if let Some((profile_id, reasoning_effort)) = parse_direct_model_selection(&runtime.bot, rest)?
    {
        return switch_model_profile(runtime, session_id, &profile_id, reasoning_effort).await;
    }
    if rest == "reset" {
        let stored_reasoning = {
            let mut sessions = runtime.sessions.lock().await;
            sessions.remove_metadata(session_id, SESSION_MODEL_PROFILE_METADATA_KEY)?;
            sessions
                .metadata_string(session_id, SESSION_REASONING_EFFORT_METADATA_KEY)
                .as_deref()
                .and_then(ReasoningEffort::parse)
        };
        return Ok(format!(
            "Cleared the current session model override.\n\n{}",
            format_model_status(&runtime.bot, None, stored_reasoning, session_id)
        ));
    }
    if let Some(id) = rest.strip_prefix("use").map(str::trim) {
        if id.is_empty() {
            anyhow::bail!("Usage: /model use <profile_id>");
        }
        return switch_model_profile(runtime, session_id, id, None).await;
    }
    Ok("Usage: /model status, /model list, /model <profile_id> [effort], /model use <profile_id>, /model reasoning status, /model reasoning list, /model reasoning set <effort>, /model reasoning reset, /model reset".to_string())
}

fn parse_direct_model_selection(
    bot: &CatBot,
    rest: &str,
) -> anyhow::Result<Option<(String, Option<ReasoningEffort>)>> {
    let mut parts = rest.split_whitespace();
    let Some(profile_id) = parts.next() else {
        return Ok(None);
    };
    let Some(profile) = bot.get_model_profile(profile_id) else {
        return Ok(None);
    };
    let reasoning = match parts.next() {
        Some(raw) => Some(ReasoningEffort::parse(raw).ok_or_else(|| {
            anyhow::anyhow!(
                "unknown reasoning effort `{raw}`. Use `/model reasoning list` to see available values."
            )
        })?),
        None => None,
    };
    if parts.next().is_some() {
        anyhow::bail!("Usage: /model <profile_id> [auto|none|minimal|low|medium|high|xhigh|max]");
    }
    Ok(Some((profile.id.clone(), reasoning)))
}

async fn switch_model_profile(
    runtime: &Runtime,
    session_id: &str,
    id: &str,
    requested_reasoning: Option<ReasoningEffort>,
) -> anyhow::Result<String> {
    let Some(profile) = runtime.bot.get_model_profile(id) else {
        anyhow::bail!("unknown model profile `{id}`. Use `/model list` to see available profiles.");
    };
    let profile_id = profile.id.clone();
    api_key_from_env(profile)?;
    let stored_reasoning_before_switch = runtime
        .sessions
        .lock()
        .await
        .metadata_string(session_id, SESSION_REASONING_EFFORT_METADATA_KEY)
        .as_deref()
        .and_then(ReasoningEffort::parse);
    let reset_reasoning = requested_reasoning == Some(ReasoningEffort::Auto);
    let requested_reasoning = requested_reasoning.filter(|effort| *effort != ReasoningEffort::Auto);
    let candidate_reasoning = if reset_reasoning {
        None
    } else {
        requested_reasoning.or(stored_reasoning_before_switch)
    };
    let mut cleared_reasoning = None;
    let stored_reasoning = if let Some(effort) = candidate_reasoning {
        if runtime
            .bot
            .effective_model_profile_with_reasoning(Some(&profile_id), Some(effort))
            .is_ok()
        {
            Some(effort)
        } else if requested_reasoning == Some(effort) {
            anyhow::bail!(
                "reasoning effort `{}` is not supported by model profile `{profile_id}`",
                effort.as_str()
            );
        } else {
            cleared_reasoning = Some(effort);
            None
        }
    } else {
        None
    };
    {
        let mut sessions = runtime.sessions.lock().await;
        sessions.set_metadata_string(
            session_id,
            SESSION_MODEL_PROFILE_METADATA_KEY,
            &profile_id,
        )?;
        if reset_reasoning {
            sessions.remove_metadata(session_id, SESSION_REASONING_EFFORT_METADATA_KEY)?;
        } else if requested_reasoning.is_some() {
            sessions.set_metadata_string(
                session_id,
                SESSION_REASONING_EFFORT_METADATA_KEY,
                requested_reasoning.unwrap().as_str(),
            )?;
        } else if cleared_reasoning.is_some() {
            sessions.remove_metadata(session_id, SESSION_REASONING_EFFORT_METADATA_KEY)?;
        }
    }
    let notice = cleared_reasoning
        .map(|effort| {
            format!(
                "Cleared session reasoning override `{}` because `{}` does not support it.\n\n",
                effort.as_str(),
                profile_id
            )
        })
        .unwrap_or_default();
    Ok(format!(
        "Switched the current session model to `{}`.\n\n{}{}",
        profile_id,
        notice,
        format_model_status(
            &runtime.bot,
            Some(&profile_id),
            stored_reasoning,
            session_id
        )
    ))
}

pub(crate) async fn session_model_and_reasoning(
    runtime: &Runtime,
    session_id: &str,
) -> anyhow::Result<(Option<String>, Option<ReasoningEffort>)> {
    let (model_profile_id, raw_reasoning) = {
        let sessions = runtime.sessions.lock().await;
        (
            sessions.metadata_string(session_id, SESSION_MODEL_PROFILE_METADATA_KEY),
            sessions.metadata_string(session_id, SESSION_REASONING_EFFORT_METADATA_KEY),
        )
    };
    let reasoning = match raw_reasoning.as_deref() {
        Some(raw) => Some(ReasoningEffort::parse(raw).ok_or_else(|| {
            anyhow::anyhow!(
                "stored session reasoning effort `{raw}` is invalid; use `/model reasoning reset`"
            )
        })?),
        None => None,
    };
    Ok((model_profile_id, reasoning))
}

fn format_model_status(
    bot: &CatBot,
    stored_model_profile_id: Option<&str>,
    stored_reasoning_effort: Option<ReasoningEffort>,
    session_id: &str,
) -> String {
    let effective = bot
        .effective_model_profile_with_reasoning(stored_model_profile_id, stored_reasoning_effort)
        .unwrap_or_else(|_| bot.effective_model_profile(stored_model_profile_id));
    let source = if effective.invalid_session_model.is_none()
        && stored_model_profile_id
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .is_some()
    {
        "session"
    } else {
        "default"
    };
    let mut lines = vec![
        "**model status**".to_string(),
        String::new(),
        format!("session_id: `{session_id}`"),
        format!("effective_model_profile: `{}`", effective.profile.id),
        format!("source: `{source}`"),
        format!("name: {}", effective.profile.name),
        format!(
            "provider: `{}`",
            effective.profile.provider.as_deref().unwrap_or("unknown")
        ),
        format!("model: `{}`", effective.profile.model),
        format!(
            "base_url: `{}`",
            effective.profile.base_url.as_deref().unwrap_or("-")
        ),
        format!("context_tokens: {}", effective.profile.context_tokens),
        format!("max_output_tokens: {}", effective.profile.max_output_tokens),
        format!("supports_images: {}", effective.profile.supports_images),
        format!("thinking: {}", model_thinking_label(&effective.profile)),
        format!(
            "reasoning_effort: {}",
            model_reasoning_effort_label(&effective.profile)
        ),
    ];
    if let Some(stored) = stored_model_profile_id {
        lines.push(format!("session_override: `{stored}`"));
    } else {
        lines.push("session_override: none".to_string());
    }
    if let Some(effort) = stored_reasoning_effort {
        lines.push(format!("reasoning_session_override: `{}`", effort.as_str()));
    } else {
        lines.push("reasoning_session_override: none".to_string());
    }
    if let Some(invalid) = effective.invalid_session_model {
        lines.push(format!(
            "warning: stored session override `{invalid}` is invalid; using fallback `{}`",
            effective.profile.id
        ));
    }
    lines.join("\n")
}

fn format_model_list(bot: &CatBot, stored_model_profile_id: Option<&str>) -> String {
    let effective = bot.effective_model_profile(stored_model_profile_id);
    let default_id = &bot.default_model_profile().id;
    let mut profiles = bot.model_profiles();
    profiles.sort_by(|a, b| a.id.cmp(&b.id));
    let mut lines = vec!["**model profiles**".to_string(), String::new()];
    for profile in profiles {
        let mut markers = Vec::new();
        if profile.id == effective.profile.id {
            markers.push("current");
        }
        if &profile.id == default_id {
            markers.push("default");
        }
        let marker = if markers.is_empty() {
            String::new()
        } else {
            format!(" [{}]", markers.join(", "))
        };
        let key_status = model_profile_key_status(profile);
        let key_label = if key_status.configured {
            "ok".to_string()
        } else {
            format!("missing ({})", key_status.env_keys.join(" or "))
        };
        lines.push(format!(
            "- `{}`{}: {} / `{}` / context={} / images={} / reasoning={} / key={}",
            profile.id,
            marker,
            profile.provider.as_deref().unwrap_or("unknown"),
            profile.model,
            profile.context_tokens,
            profile.supports_images,
            model_reasoning_effort_label(profile),
            key_label
        ));
    }
    if let Some(invalid) = effective.invalid_session_model {
        lines.push(format!(
            "\nwarning: stored session override `{invalid}` is invalid; using fallback `{}`",
            effective.profile.id
        ));
    }
    lines.join("\n")
}

fn format_model_reasoning_status(
    bot: &CatBot,
    stored_model_profile_id: Option<&str>,
    stored_reasoning_effort: Option<ReasoningEffort>,
    session_id: &str,
) -> String {
    let effective = bot
        .effective_model_profile_with_reasoning(stored_model_profile_id, stored_reasoning_effort)
        .unwrap_or_else(|_| bot.effective_model_profile(stored_model_profile_id));
    let mut lines = vec![
        "**model reasoning**".to_string(),
        String::new(),
        format!("session_id: `{session_id}`"),
        format!("effective_model_profile: `{}`", effective.profile.id),
        format!("name: {}", effective.profile.name),
        format!("model: `{}`", effective.profile.model),
        format!("thinking: {}", model_thinking_label(&effective.profile)),
        format!(
            "reasoning_effort: {}",
            model_reasoning_effort_label(&effective.profile)
        ),
        format!(
            "session_override: {}",
            stored_reasoning_effort
                .map(|effort| format!("`{}`", effort.as_str()))
                .unwrap_or_else(|| "none".to_string())
        ),
        String::new(),
        "Use `/model reasoning set <effort>` to override this session, or `/model reasoning reset` to return to the model profile default.".to_string(),
    ];
    if let Some(invalid) = effective.invalid_session_model {
        lines.push(format!(
            "\nwarning: stored session override `{invalid}` is invalid; using fallback `{}`",
            effective.profile.id
        ));
    }
    lines.join("\n")
}

fn format_model_reasoning_list() -> String {
    let mut lines = vec!["**model reasoning efforts**".to_string(), String::new()];
    for effort in ReasoningEffort::VARIANTS {
        lines.push(format!(
            "- `{}`: {} - {}",
            effort.as_str(),
            effort.display_name(),
            effort.description()
        ));
    }
    lines.join("\n")
}

pub(crate) fn model_reasoning_effort_label(profile: &ModelProfileConfig) -> String {
    profile
        .reasoning_effort
        .map(|effort| format!("{} ({})", effort.display_name(), effort.as_str()))
        .unwrap_or_else(|| "default".to_string())
}

fn model_thinking_label(profile: &ModelProfileConfig) -> String {
    profile
        .thinking
        .map(|thinking| format!("{} ({})", thinking.display_name(), thinking.as_str()))
        .unwrap_or_else(|| "default".to_string())
}

async fn handle_skill_command(
    runtime: &Runtime,
    session_id: &str,
    command: &str,
) -> anyhow::Result<RuntimeCommandResult> {
    let trimmed = command.trim();
    if trimmed == "/skill" || trimmed == "/skill list" {
        return Ok(RuntimeCommandResult::Reply(format_skill_list(&runtime.bot)));
    }
    if trimmed == "/skill status" {
        let names = runtime.bot.read_skill_names(session_id).await;
        return Ok(RuntimeCommandResult::Reply(format_skill_status(&names)));
    }
    if trimmed.starts_with("/skill:") {
        let (skills, rest) = parse_skill_prefixes(&runtime.bot, trimmed).await?;
        if rest.trim().is_empty() {
            let names = skills
                .iter()
                .map(|skill| format!("`{}`", skill.id))
                .collect::<Vec<_>>()
                .join(", ");
            return Ok(RuntimeCommandResult::Reply(format!(
                "已加载 skill {names}。请在同一条消息中提供要执行的任务。"
            )));
        }
        let prefix = skills
            .iter()
            .map(|skill| format!("已加载 skill `{}`。\n", skill.id))
            .collect::<String>();
        return Ok(RuntimeCommandResult::Continue {
            text: rest.trim().to_string(),
            prefix,
            skill_injections: skills,
        });
    }
    Ok(RuntimeCommandResult::Reply(
        "用法：/skill list，/skill status，/skill:<name> <任务>".to_string(),
    ))
}

async fn parse_skill_prefixes(
    bot: &CatBot,
    mut text: &str,
) -> anyhow::Result<(Vec<SkillDocument>, String)> {
    let mut skills = Vec::new();
    loop {
        let Some(rest) = text.trim_start().strip_prefix("/skill:") else {
            break;
        };
        let rest = rest.trim_start();
        let split_at = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let name = rest[..split_at].trim();
        if name.is_empty() {
            anyhow::bail!("用法：/skill:<name> <任务>");
        }
        let Some(doc) = bot.get_skill(name).await? else {
            anyhow::bail!("Skill `{name}` not found. Use `/skill list` to see available skills.");
        };
        skills.push(doc);
        text = &rest[split_at..];
    }
    Ok((skills, text.to_string()))
}

fn format_skill_list(bot: &CatBot) -> String {
    let mut skills = bot.all_skill_summaries();
    skills.sort_by(|a, b| a.id.cmp(&b.id).then_with(|| a.source.cmp(&b.source)));
    let diagnostics = bot.skill_load_diagnostics();
    if skills.is_empty() {
        let mut text = "No skills are available.".to_string();
        text.push_str(&format_skill_load_diagnostics(&diagnostics));
        return text;
    }
    let mut lines = vec!["**skills**".to_string(), String::new()];
    for skill in skills {
        let depth = skill.id.matches('/').count();
        lines.push(format!(
            "{}- `/skill:{}` - {} ({})",
            "  ".repeat(depth),
            skill.id,
            skill.description,
            skill.source
        ));
    }
    let mut text = lines.join("\n");
    text.push_str(&format_skill_load_diagnostics(&diagnostics));
    text
}

fn format_skill_load_diagnostics(diagnostics: &[SkillLoadDiagnostic]) -> String {
    if diagnostics.is_empty() {
        return String::new();
    }
    let mut lines = vec![
        String::new(),
        String::new(),
        "**skill load diagnostics**".to_string(),
    ];
    let limit = 8;
    for diagnostic in diagnostics.iter().take(limit) {
        let severity = match diagnostic.severity {
            SkillLoadDiagnosticSeverity::Warning => "warning",
            SkillLoadDiagnosticSeverity::Error => "error",
        };
        lines.push(format!(
            "- `{severity}` {} - {}",
            diagnostic.path, diagnostic.message
        ));
    }
    if diagnostics.len() > limit {
        lines.push(format!(
            "- ... {} more diagnostic(s) omitted",
            diagnostics.len() - limit
        ));
    }
    lines.join("\n")
}

fn format_skill_status(names: &[String]) -> String {
    if names.is_empty() {
        return "当前 session 尚未读取 skill。".to_string();
    }
    format!(
        "**read skills**\n\n{}",
        names
            .iter()
            .map(|name| format!("- `{name}`"))
            .collect::<Vec<_>>()
            .join("\n")
    )
}

fn format_account_usage(usage: &AccountUsage) -> String {
    let mut lines = vec![
        "**model usage**".to_string(),
        String::new(),
        format!("provider: `{}`", usage.provider),
        format!("model_profile: `{}`", usage.model_profile_id),
        format!("model: `{}`", usage.model),
    ];

    match &usage.status {
        AccountUsageStatus::Unknown { reason } => {
            lines.push("status: `unknown`".to_string());
            lines.push(format!("reason: {reason}"));
        }
        AccountUsageStatus::Available { balances } => {
            lines.push("status: `available`".to_string());
            if balances.is_empty() {
                lines.push("balance: none returned".to_string());
            } else {
                for balance in balances {
                    lines.push(format_balance_line(balance));
                }
            }
        }
    }

    lines.join("\n")
}

fn format_balance_line(balance: &AccountBalance) -> String {
    let mut parts = Vec::new();
    if let Some(currency) = &balance.currency {
        parts.push(format!("currency={currency}"));
    }
    if let Some(total) = &balance.total {
        parts.push(format!("total={total}"));
    }
    if let Some(granted) = &balance.granted {
        parts.push(format!("granted={granted}"));
    }
    if let Some(topped_up) = &balance.topped_up {
        parts.push(format!("topped_up={topped_up}"));
    }
    if let Some(voucher) = &balance.voucher {
        parts.push(format!("voucher={voucher}"));
    }
    if let Some(cash) = &balance.cash {
        parts.push(format!("cash={cash}"));
    }
    if let Some(available) = balance.available {
        parts.push(format!("available={available}"));
    }

    if parts.is_empty() {
        "- balance: unknown".to_string()
    } else {
        format!("- balance: {}", parts.join(", "))
    }
}

async fn handle_debug_command(
    runtime: &Runtime,
    session_id: &str,
    command: &str,
) -> anyhow::Result<String> {
    let rest = command.trim().strip_prefix("/debug").unwrap_or("").trim();
    match rest {
        "" | "status" => {
            let enabled = runtime
                .sessions
                .lock()
                .await
                .metadata_bool(session_id, SESSION_DEBUG_METADATA_KEY);
            Ok(format!(
                "debug 信息当前为：{}",
                if enabled { "开启" } else { "关闭" }
            ))
        }
        "on" | "enable" | "enabled" | "true" | "1" => {
            runtime.sessions.lock().await.set_metadata_bool(
                session_id,
                SESSION_DEBUG_METADATA_KEY,
                true,
            )?;
            Ok("已开启本 session 的 debug 信息。之后每轮会返回最后的 Stats 调试块。".to_string())
        }
        "off" | "disable" | "disabled" | "false" | "0" => {
            runtime.sessions.lock().await.set_metadata_bool(
                session_id,
                SESSION_DEBUG_METADATA_KEY,
                false,
            )?;
            Ok("已关闭本 session 的 debug 信息。之后不再返回最后的 Stats 调试块。".to_string())
        }
        _ => Ok("用法：/debug on，/debug off，/debug status".to_string()),
    }
}

async fn handle_workflow_command(
    bot: &CatBot,
    session_id: &str,
    command: &str,
) -> anyhow::Result<RuntimeCommandResult> {
    let rest = command
        .trim()
        .strip_prefix("/workflow")
        .unwrap_or("")
        .trim();
    if rest.is_empty() || rest == "status" {
        return Ok(RuntimeCommandResult::Reply(
            match bot.workflow_status(session_id).await {
                Some(instance) => format_workflow_status(&instance),
                None => "当前 session 没有 supervisor workflow。".to_string(),
            },
        ));
    }
    if rest == "pause" || rest == "stop" {
        bot.pause_workflow(session_id).await?;
        return Ok(RuntimeCommandResult::Reply(
            "已暂停当前 session 的 supervisor workflow。".to_string(),
        ));
    }
    if rest == "clear" {
        bot.clear_workflow(session_id).await?;
        return Ok(RuntimeCommandResult::Reply(
            "已清除当前 session 的 supervisor workflow。".to_string(),
        ));
    }
    let set_args = rest
        .strip_prefix("set")
        .or_else(|| rest.strip_prefix("start"))
        .map(str::trim);
    let Some(set_args) = set_args else {
        return Ok(RuntimeCommandResult::Reply("用法：/workflow start <id> [--max-rounds N|unlimited] [--context {JSON}|plain goal text]，/workflow stop，/workflow clear，/workflow status".to_string()));
    };
    let mut split = set_args.splitn(2, char::is_whitespace);
    let workflow_id = split.next().unwrap_or("").trim();
    let options = split.next().unwrap_or("").trim();
    if workflow_id.is_empty() {
        return Ok(RuntimeCommandResult::Reply(
            "用法：/workflow start <id> ...".to_string(),
        ));
    }
    start_workflow_command(bot, workflow_id, options).await
}

async fn handle_direct_workflow_command(
    bot: &CatBot,
    command: &str,
) -> anyhow::Result<Option<RuntimeCommandResult>> {
    let Some((workflow_id, options)) = resolve_direct_workflow_command(bot, command)? else {
        return Ok(None);
    };
    start_workflow_command(bot, &workflow_id, options)
        .await
        .map(Some)
}

fn resolve_direct_workflow_command<'a>(
    bot: &CatBot,
    command: &'a str,
) -> anyhow::Result<Option<(String, &'a str)>> {
    let Some(rest) = command.trim().strip_prefix('/') else {
        return Ok(None);
    };
    if rest.trim().is_empty() {
        return Ok(None);
    }
    let mut workflows = bot.workflow_definitions()?;
    workflows.sort_by(|a, b| {
        b.name
            .len()
            .cmp(&a.name.len())
            .then_with(|| b.id.len().cmp(&a.id.len()))
            .then_with(|| a.id.cmp(&b.id))
    });
    for workflow in workflows {
        if workflow.id == "goal" {
            continue;
        }
        if let Some(options) = direct_workflow_options(rest, &workflow.id)
            .or_else(|| direct_workflow_options(rest, &workflow.name))
        {
            return Ok(Some((workflow.id, options)));
        }
    }
    Ok(None)
}

pub(crate) fn direct_workflow_options<'a>(rest: &'a str, name: &str) -> Option<&'a str> {
    let options = rest.strip_prefix(name)?;
    if options.is_empty() {
        return Some("");
    }
    options
        .chars()
        .next()
        .is_some_and(char::is_whitespace)
        .then(|| options.trim())
}

async fn start_workflow_command(
    bot: &CatBot,
    workflow_id: &str,
    options: &str,
) -> anyhow::Result<RuntimeCommandResult> {
    let (context, max_rounds) = parse_workflow_start_options(options)?;
    if !bot
        .workflow_definitions()?
        .iter()
        .any(|workflow| workflow.id == workflow_id)
    {
        anyhow::bail!("workflow `{workflow_id}` not found");
    }
    Ok(RuntimeCommandResult::StartWorkflow {
        workflow_id: workflow_id.to_string(),
        context,
        max_rounds,
    })
}

pub(crate) fn parse_workflow_start_options(
    mut options: &str,
) -> anyhow::Result<(serde_json::Value, GoalMaxRounds)> {
    let mut max_rounds = GoalMaxRounds::default();
    let mut context = serde_json::json!({});
    while !options.is_empty() {
        if let Some(rest) = options.strip_prefix("--max-rounds") {
            let rest = rest.trim_start();
            let mut parts = rest.splitn(2, char::is_whitespace);
            max_rounds = parse_goal_max_rounds(parts.next().unwrap_or(""))?;
            options = parts.next().unwrap_or("").trim();
            continue;
        }
        if let Some(rest) = options.strip_prefix("--context") {
            let raw = rest.trim_start();
            context = serde_json::from_str(raw).context("invalid workflow context JSON")?;
            options = "";
            continue;
        }
        if !options.starts_with("--") {
            context = serde_json::json!({ "goal": options.trim() });
            options = "";
            continue;
        }
        anyhow::bail!("unknown workflow option: {options}");
    }
    Ok((context, max_rounds))
}

fn format_workflow_status(instance: &bot_core::WorkflowInstance) -> String {
    let max_rounds = match &instance.max_rounds {
        bot_core::WorkflowMaxRounds::Limited(value) => value.to_string(),
        bot_core::WorkflowMaxRounds::Unlimited => "unlimited".to_string(),
    };
    let node_agent = workflow_node_agent(instance).unwrap_or("-");
    let mut text = format!(
        "workflow: {}\nstatus: {:?}\nnode: {}\nnode_agent: {}\nmax_rounds: {}\ncontext: {}",
        instance.definition.id,
        instance.status,
        instance.current_node,
        node_agent,
        max_rounds,
        serde_json::to_string(&instance.context).unwrap_or_else(|_| "{}".into())
    );
    if let Some(report) = &instance.last_report {
        if let Some(edge) = &report.edge {
            text.push_str(&format!(
                "\nlast_transition: {} -> {} via {}\nlast_reason: {}",
                report.from_node, report.to_node, edge, report.reason
            ));
        } else {
            text.push_str(&format!(
                "\nlast_state: {:?} at {}\nlast_reason: {}",
                report.status, report.to_node, report.reason
            ));
        }
        if let Some(message) = &report.agent_message {
            text.push_str(&format!("\nlast_agent_message: {message}"));
        }
        if let Some(message) = &report.next_node_message {
            text.push_str(&format!("\nnext_node_message: {message}"));
        }
    }
    text
}

fn workflow_node_agent(instance: &bot_core::WorkflowInstance) -> Option<&str> {
    if instance.status != bot_core::WorkflowStatus::Active {
        return None;
    }
    instance.definition.node_agent(&instance.current_node)
}

async fn handle_goal_command(
    bot: &CatBot,
    session_id: &str,
    command: &str,
) -> anyhow::Result<String> {
    let rest = command.trim().strip_prefix("/goal").unwrap_or("").trim();
    if rest.is_empty() || rest == "status" {
        return Ok(match bot.goal_status(session_id).await {
            Some(goal) => format_goal_status(&goal),
            None => "当前 session 没有设置 goal。".to_string(),
        });
    }
    if rest == "clear" {
        bot.clear_goal(session_id).await?;
        return Ok("已清除当前 session 的 goal。".to_string());
    }
    if rest == "pause" {
        bot.pause_workflow(session_id).await?;
        return Ok("已暂停当前 session 的 goal。".to_string());
    }
    let Some(mut goal_text) = rest.strip_prefix("set").map(str::trim) else {
        return Ok(
            "用法：/goal set [--max-rounds N|unlimited] <目标>，/goal pause，/goal clear，/goal status".to_string(),
        );
    };
    let mut max_rounds = GoalMaxRounds::default();
    if let Some(after_flag) = goal_text.strip_prefix("--max-rounds") {
        let after_flag = after_flag.trim_start();
        let mut parts = after_flag.splitn(2, char::is_whitespace);
        let raw_limit = parts.next().unwrap_or("").trim();
        let remaining = parts.next().unwrap_or("").trim();
        if raw_limit.is_empty() || remaining.is_empty() {
            return Ok("用法：/goal set --max-rounds <N|unlimited> <目标>".to_string());
        }
        max_rounds = parse_goal_max_rounds(raw_limit)?;
        goal_text = remaining;
    }
    if goal_text.trim().is_empty() {
        return Ok("用法：/goal set [--max-rounds N|unlimited] <目标>".to_string());
    }
    let goal = bot.set_goal(session_id, goal_text, max_rounds).await?;
    Ok(format!("已设置 goal。\n\n{}", format_goal_status(&goal)))
}

pub(crate) fn is_goal_set_command(command: &str) -> bool {
    command
        .trim()
        .strip_prefix("/goal")
        .map(str::trim)
        .and_then(|rest| rest.strip_prefix("set"))
        .is_some_and(|rest| rest.is_empty() || rest.chars().next().is_some_and(char::is_whitespace))
}

pub(crate) fn parse_goal_max_rounds(raw: &str) -> anyhow::Result<GoalMaxRounds> {
    if raw.eq_ignore_ascii_case("unlimited") || raw == "无限" {
        return Ok(GoalMaxRounds::Unlimited);
    }
    let value: u32 = raw
        .parse()
        .map_err(|_| anyhow::anyhow!("--max-rounds must be a positive integer or unlimited"))?;
    if value == 0 {
        anyhow::bail!("--max-rounds must be greater than 0");
    }
    Ok(GoalMaxRounds::Limited(value))
}

fn format_goal_status(goal: &bot_core::GoalState) -> String {
    let status = match &goal.status {
        bot_core::GoalStatus::Active => "active",
        bot_core::GoalStatus::Paused => "paused",
        bot_core::GoalStatus::Completed => "completed",
    };
    let max_rounds = match &goal.max_rounds {
        GoalMaxRounds::Limited(value) => value.to_string(),
        GoalMaxRounds::Unlimited => "unlimited".to_string(),
    };
    let last = goal
        .last_evaluation
        .as_ref()
        .map(|decision| {
            let status = match &decision.status {
                bot_core::goal::SupervisorDecisionStatus::Completed => "completed",
                bot_core::goal::SupervisorDecisionStatus::Continue => "continue",
            };
            format!("\nlast_supervisor: {status} - {}", decision.reason)
        })
        .unwrap_or_default();
    format!(
        "goal: {}\nstatus: {status}\nmax_rounds: {max_rounds}{last}",
        goal.goal
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_task() -> ToolTaskRecord {
        ToolTaskRecord {
            task_id: "task-1".to_string(),
            thread_id: "session-1".to_string(),
            run_id: "run-1".to_string(),
            app_id: None,
            tool_call_id: "call-1".to_string(),
            tool_name: "bash".to_string(),
            args: serde_json::json!({"command": "sleep 60"}),
            status: bot_core::tool_tasks::TOOL_TASK_RUNNING.to_string(),
            background: true,
            started_at: "2026-07-09T00:00:00Z".to_string(),
            completed_at: None,
            elapsed_ms: None,
            success: None,
            result_preview: Some("result body".to_string()),
            recent_output: vec!["tail line".to_string()],
            message: None,
            notify_on_finish: false,
            notification_delivered: false,
        }
    }

    #[test]
    fn formats_background_task_list_and_detail() {
        let mut task = test_task();
        task.started_at = "2026-07-09T00:00:00Z".to_string();

        let list = format_tool_tasks(std::slice::from_ref(&task));
        assert!(list.contains("task-1"));
        assert!(list.contains("running"));
        assert!(list.contains("bash"));
        assert!(list.contains("args: `{"));
        assert!(list.contains("运行"));

        let detail = format_tool_task_detail(&task);
        assert!(detail.contains("task_id: task-1"));
        assert!(detail.contains("recent_output:\ntail line"));
        assert!(detail.contains("result:\nresult body"));
        assert!(detail.contains("args:"));
    }

    #[test]
    fn task_list_truncates_args_and_formats_relative_time() {
        let mut task = test_task();
        task.args = serde_json::json!({"command": "界".repeat(200)});
        task.started_at = "2026-07-09T00:00:00Z".to_string();
        let now = chrono::DateTime::parse_from_rfc3339("2026-07-09T00:02:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);

        assert_eq!(compact_task_args(&task.args, 20).chars().count(), 20);
        assert!(compact_task_args(&task.args, 20).ends_with('…'));
        assert_eq!(task_timing_label(&task, &now), "运行 2m");

        task.status = bot_core::tool_tasks::TOOL_TASK_FAILED.to_string();
        task.completed_at = Some("2026-07-09T00:01:30Z".to_string());
        assert_eq!(task_timing_label(&task, &now), "30s 前结束");
    }

    #[test]
    fn background_task_access_is_scoped_to_session() {
        let mut task = test_task();
        assert!(task_belongs_to_session(&task, "session-1"));
        assert!(!task_belongs_to_session(&task, "session-2"));
        task.background = false;
        assert!(!task_belongs_to_session(&task, "session-1"));
    }

    #[test]
    fn task_thread_id_uses_sub_session_thread_when_present() {
        assert_eq!(
            resolve_task_thread_id("ui-session", Some("subagent:actual-thread")),
            "subagent:actual-thread"
        );
        assert_eq!(
            resolve_task_thread_id("normal-session", None),
            "normal-session"
        );
        assert_eq!(
            resolve_task_thread_id("normal-session", Some("   ")),
            "normal-session"
        );
    }
}
