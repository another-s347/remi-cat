use anyhow::Context;

use crate::app::{
    command_doctor_report, handle_runtime_secret_command, MAX_COMMAND_PREPROCESS_DEPTH,
    SESSION_DEBUG_METADATA_KEY, SESSION_MODEL_PROFILE_METADATA_KEY,
};
use crate::cli::parse_secret_command;
use crate::core::Runtime;
use bot_core::{
    AccountBalance, AccountUsage, AccountUsageStatus, CatBot, GoalMaxRounds, SkillDocument,
    SkillLoadDiagnostic, SkillLoadDiagnosticSeverity,
};

pub(crate) enum RuntimeCommandResult {
    Reply(String),
    Continue {
        text: String,
        prefix: String,
        skill_injections: Vec<SkillDocument>,
    },
}

pub(crate) enum RuntimeCommandPipelineResult {
    Reply(String),
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
        let reply = runtime
            .bot
            .tool_list()
            .into_iter()
            .map(|(name, desc)| format!("- `{name}`: {desc}"))
            .collect::<Vec<_>>()
            .join("\n");
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
        let reply = handle_workflow_command(&runtime.bot, session_id, command).await?;
        return Ok(Some(RuntimeCommandResult::Reply(reply)));
    }
    if command == "/model" || command.starts_with("/model ") {
        let reply = handle_model_command(runtime, session_id, command).await?;
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
        let n = runtime.bot.compact_memory(session_id).await?;
        return Ok(Some(RuntimeCommandResult::Reply(format!(
            "compacted {n} short-term message(s)."
        ))));
    }
    if command == "/clear" {
        runtime.bot.clear_memory(session_id).await?;
        return Ok(Some(RuntimeCommandResult::Reply(
            "已清空当前 session 的历史会话。Todo/trigger 等工具状态已保留。".to_string(),
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
    if let Some(reply) = handle_direct_workflow_command(&runtime.bot, session_id, command).await? {
        return Ok(Some(RuntimeCommandResult::Reply(reply)));
    }
    Ok(None)
}

fn runtime_command_help() -> String {
    [
        "**runtime commands**",
        "",
        "- `/help` - show this command list",
        "- `/tools` - list tools available to the current agent",
        "- `/skill list` - list available local and builtin skills",
        "- `/skill status` - show skills already read or activated in this session",
        "- `/skill:<name> <task>` - load a skill for the same message and run the task",
        "- `/doctor` - show runtime readiness for the current session",
        "- `/model` or `/model list` - inspect or select model profiles for this session",
        "- `/permissions` - inspect or change tool approval mode for this session",
        "- `/hooks` - list/trust/enable/disable Codex-compatible hooks",
        "- `/goal ...` - inspect or manage the current goal workflow",
        "- `/workflow ...` - inspect or manage supervisor workflow state",
        "- `/<workflow-id> ...` - start a supervisor workflow directly",
        "- `/compact` - compact short-term memory",
        "- `/clear` - clear chat memory for this session",
    ]
    .join("\n")
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
        "ask" | "default" | "prompt" | "reset" => Some(ApprovalSessionPolicy::Ask),
        "auto" | "medium" | "model-auto" | "model_auto" => Some(ApprovalSessionPolicy::ModelAuto),
        "allow" | "allow-session" | "allow_session" | "bypass" | "trusted" => {
            Some(ApprovalSessionPolicy::AllowAll)
        }
        _ => {
            return Ok(format!(
                "用法：/permissions status | ask | auto | allow\n\n{}",
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
        "**permissions**\n\nsession_id: `{session_id}`\nmode: `{}`\n{}\n\nmodes:\n- `ask`: low 自动通过；medium/high 请求审批\n- `auto` / `medium`: low/medium 自动通过；high 请求审批\n- `allow`: 本 session 所有 tool request 自动通过",
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
        let stored = runtime
            .sessions
            .lock()
            .await
            .metadata_string(session_id, SESSION_MODEL_PROFILE_METADATA_KEY);
        return Ok(format_model_status(
            &runtime.bot,
            stored.as_deref(),
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
    if rest == "reset" {
        runtime
            .sessions
            .lock()
            .await
            .remove_metadata(session_id, SESSION_MODEL_PROFILE_METADATA_KEY)?;
        return Ok(format!(
            "已清除当前 session 的模型 override。\n\n{}",
            format_model_status(&runtime.bot, None, session_id)
        ));
    }
    if let Some(id) = rest.strip_prefix("use").map(str::trim) {
        if id.is_empty() {
            anyhow::bail!("用法：/model use <profile_id>");
        }
        let Some(profile) = runtime.bot.get_model_profile(id) else {
            anyhow::bail!(
                "unknown model profile `{id}`. Use `/model list` to see available profiles."
            );
        };
        let profile_id = profile.id.clone();
        runtime.sessions.lock().await.set_metadata_string(
            session_id,
            SESSION_MODEL_PROFILE_METADATA_KEY,
            &profile_id,
        )?;
        return Ok(format!(
            "已切换当前 session 模型为 `{}`。\n\n{}",
            profile_id,
            format_model_status(&runtime.bot, Some(&profile_id), session_id)
        ));
    }
    Ok("用法：/model status，/model list，/model use <profile_id>，/model reset".to_string())
}

fn format_model_status(
    bot: &CatBot,
    stored_model_profile_id: Option<&str>,
    session_id: &str,
) -> String {
    let effective = bot.effective_model_profile(stored_model_profile_id);
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
    ];
    if let Some(stored) = stored_model_profile_id {
        lines.push(format!("session_override: `{stored}`"));
    } else {
        lines.push("session_override: none".to_string());
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
        lines.push(format!(
            "- `{}`{}: {} / `{}` / context={} / images={}",
            profile.id,
            marker,
            profile.provider.as_deref().unwrap_or("unknown"),
            profile.model,
            profile.context_tokens,
            profile.supports_images
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
                .map(|skill| format!("`{}`", skill.name))
                .collect::<Vec<_>>()
                .join(", ");
            return Ok(RuntimeCommandResult::Reply(format!(
                "已加载 skill {names}。请在同一条消息中提供要执行的任务。"
            )));
        }
        let prefix = skills
            .iter()
            .map(|skill| format!("已加载 skill `{}`。\n", skill.name))
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
    let mut skills = bot.skill_summaries();
    skills.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.source.cmp(&b.source)));
    let diagnostics = bot.skill_load_diagnostics();
    if skills.is_empty() {
        let mut text = "No skills are available.".to_string();
        text.push_str(&format_skill_load_diagnostics(&diagnostics));
        return text;
    }
    let mut lines = vec!["**skills**".to_string(), String::new()];
    for skill in skills {
        lines.push(format!(
            "- `/skill:{}` - {} ({})",
            skill.name, skill.description, skill.source
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
) -> anyhow::Result<String> {
    let rest = command
        .trim()
        .strip_prefix("/workflow")
        .unwrap_or("")
        .trim();
    if rest.is_empty() || rest == "status" {
        return Ok(match bot.workflow_status(session_id).await {
            Some(instance) => format_workflow_status(&instance),
            None => "当前 session 没有 supervisor workflow。".to_string(),
        });
    }
    if rest == "pause" || rest == "stop" {
        bot.pause_workflow(session_id).await?;
        return Ok("已暂停当前 session 的 supervisor workflow。".to_string());
    }
    if rest == "clear" {
        bot.clear_workflow(session_id).await?;
        return Ok("已清除当前 session 的 supervisor workflow。".to_string());
    }
    let set_args = rest
        .strip_prefix("set")
        .or_else(|| rest.strip_prefix("start"))
        .map(str::trim);
    let Some(set_args) = set_args else {
        return Ok("用法：/workflow set <id> [--max-rounds N|unlimited] [--context {JSON}]，/workflow pause，/workflow clear，/workflow status".to_string());
    };
    let mut split = set_args.splitn(2, char::is_whitespace);
    let workflow_id = split.next().unwrap_or("").trim();
    let options = split.next().unwrap_or("").trim();
    if workflow_id.is_empty() {
        return Ok("用法：/workflow set <id> ...".to_string());
    }
    start_workflow_command(bot, session_id, workflow_id, options).await
}

async fn handle_direct_workflow_command(
    bot: &CatBot,
    session_id: &str,
    command: &str,
) -> anyhow::Result<Option<String>> {
    let Some((workflow_id, options)) = resolve_direct_workflow_command(bot, command)? else {
        return Ok(None);
    };
    start_workflow_command(bot, session_id, &workflow_id, options)
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
    session_id: &str,
    workflow_id: &str,
    mut options: &str,
) -> anyhow::Result<String> {
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
        anyhow::bail!("unknown workflow option: {options}");
    }
    let instance = bot
        .start_workflow_by_id(session_id, workflow_id, context, max_rounds)
        .await?;
    Ok(format!(
        "已设置 supervisor workflow。\n\n{}",
        format_workflow_status(&instance)
    ))
}

fn format_workflow_status(instance: &bot_core::WorkflowInstance) -> String {
    let max_rounds = match &instance.max_rounds {
        bot_core::WorkflowMaxRounds::Limited(value) => value.to_string(),
        bot_core::WorkflowMaxRounds::Unlimited => "unlimited".to_string(),
    };
    let mut text = format!(
        "workflow: {}\nstatus: {:?}\nnode: {}\nmax_rounds: {}\ncontext: {}",
        instance.definition.id,
        instance.status,
        instance.current_node,
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
