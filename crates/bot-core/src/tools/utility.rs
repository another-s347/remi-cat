use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::Context;
use async_stream::stream;
use futures::Stream;
use remi_agentloop::prelude::{
    AgentError, ResumePayload, Tool, ToolContext, ToolOutput, ToolResult,
};

use super::log_preview;

pub struct ManageYourselfTool;

impl Tool for ManageYourselfTool {
    fn name(&self) -> &str {
        "manage_yourself"
    }

    fn description(&self) -> &str {
        "Run a remi-cat CLI command against the current host binary for Remi self-management. Only pass a top-level `command` string, for example: {\"command\":\"profile list\"}. Use {\"command\":\"tools --json\"} to inspect every registered tool and configuration diagnostics, including tools outside the active allowlist. Use help commands such as {\"command\":\"help\"} or {\"command\":\"profile agent --help\"} to inspect available CLI commands. Current chat runtime settings that are slash commands, including model reasoning strength, are available to the user as commands such as `/model reasoning set high` and `/model reasoning reset`. The command is parsed as shell-like arguments but is not executed through a shell."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "remi-cat arguments without the binary name, for example: profile list; use help or <command> --help to inspect available commands"
                }
            },
            "required": ["command"]
        })
    }

    fn execute(
        &self,
        arguments: serde_json::Value,
        _resume: Option<ResumePayload>,
        _ctx: &ToolContext,
    ) -> impl std::future::Future<Output = Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError>>
    {
        async move {
            let command = arguments
                .get("command")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    AgentError::tool(
                        "manage_yourself",
                        "missing 'command'; use exactly {\"command\":\"profile list\"}",
                    )
                })?
                .to_string();
            let args = parse_manage_yourself_command(&command)?;
            Ok(ToolResult::Output(stream! {
                yield ToolOutput::Delta(format!("remi-cat {}", args.join(" ")));
                tracing::info!(
                    command = %log_preview(&command, 160),
                    command_len = command.len(),
                    argc = args.len(),
                    "manage_yourself.start"
                );
                match run_manage_yourself_command(&args).await {
                    Ok(output) => yield ToolOutput::text(output),
                    Err(error) => {
                        tracing::warn!(
                            command = %log_preview(&command, 160),
                            command_len = command.len(),
                            argc = args.len(),
                            error = %error,
                            "manage_yourself.failed"
                        );
                        yield ToolOutput::text(format!("error: {error:#}"));
                    }
                }
            }))
        }
    }
}

pub(super) fn parse_manage_yourself_command(command: &str) -> Result<Vec<String>, AgentError> {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return Err(AgentError::tool(
            "manage_yourself",
            "command must not be empty",
        ));
    }
    let args = shlex::split(trimmed).ok_or_else(|| {
        AgentError::tool(
            "manage_yourself",
            "failed to parse command; check shell quoting",
        )
    })?;
    if args.is_empty() {
        return Err(AgentError::tool(
            "manage_yourself",
            "command must not be empty",
        ));
    }
    Ok(args)
}

async fn run_manage_yourself_command(args: &[String]) -> anyhow::Result<String> {
    let exe = std::env::current_exe().context("resolving current remi-cat executable")?;
    let started = Instant::now();
    tracing::debug!(
        exe = %exe.display(),
        argc = args.len(),
        "manage_yourself.process.start"
    );
    let output = tokio::task::spawn_blocking({
        let args = args.to_vec();
        move || Command::new(&exe).args(args).output()
    })
    .await
    .context("joining manage_yourself command task")?
    .context("running remi-cat command")?;
    let stdout_bytes = output.stdout.len();
    let stderr_bytes = output.stderr.len();
    let exit_code = output.status.code();
    if output.status.success() {
        tracing::info!(
            exit_code = exit_code.unwrap_or(-1),
            stdout_bytes,
            stderr_bytes,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "manage_yourself.completed"
        );
    } else {
        tracing::warn!(
            exit_code = exit_code.unwrap_or(-1),
            stdout_bytes,
            stderr_bytes,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "manage_yourself.failed"
        );
    }
    Ok(format_command_output(output))
}

pub(super) fn format_command_output(output: std::process::Output) -> String {
    let mut text = String::new();
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !stdout.is_empty() {
        text.push_str(stdout.trim_end());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str("[stderr] ");
        text.push_str(stderr.trim_end());
    }
    if let Some(code) = output.status.code() {
        if code != 0 {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&format!("[exit {code}]"));
        }
    } else {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str("[terminated by signal]");
    }
    text
}

/// Returns the current date and time with timezone support.
/// Includes Unix timestamp and day of week.
pub struct NowTool;

impl Tool for NowTool {
    fn name(&self) -> &str {
        "now"
    }
    fn description(&self) -> &str {
        "Return the current date and time with timezone, Unix timestamp, and weekday. \
         Use this whenever you need to know what time or date it is."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "timezone": {
                    "type": "string",
                    "description": "Optional timezone offset like '+08:00', '-05:00', '+0800', 'UTC', 'local', or 'Asia/Shanghai'. Defaults to the system local timezone."
                }
            }
        })
    }
    fn execute(
        &self,
        arguments: serde_json::Value,
        _resume: Option<ResumePayload>,
        _ctx: &ToolContext,
    ) -> impl std::future::Future<Output = Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError>>
    {
        async move {
            use chrono::{Datelike, FixedOffset, TimeZone, Timelike, Utc, Weekday};

            let timezone_arg = arguments
                .get("timezone")
                .and_then(|v| v.as_str())
                .unwrap_or("local");
            let timezone = parse_timezone_spec(timezone_arg).ok_or_else(|| {
                AgentError::tool(
                    "now",
                    format!(
                        "invalid timezone `{timezone_arg}`; use `local`, `UTC`, an offset like `+08:00`, or `Asia/Shanghai`"
                    ),
                )
            })?;

            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();
            let unix_secs = now.as_secs();

            // Use chrono for timezone-aware datetime
            let utc_dt = Utc
                .timestamp_opt(unix_secs as i64, 0)
                .single()
                .unwrap_or_else(|| Utc::now());

            let offset = FixedOffset::east_opt(timezone.offset_seconds)
                .unwrap_or_else(|| FixedOffset::east_opt(0).unwrap());
            let local_dt = utc_dt.with_timezone(&offset);

            let weekday_en = match local_dt.weekday() {
                Weekday::Mon => "Monday",
                Weekday::Tue => "Tuesday",
                Weekday::Wed => "Wednesday",
                Weekday::Thu => "Thursday",
                Weekday::Fri => "Friday",
                Weekday::Sat => "Saturday",
                Weekday::Sun => "Sunday",
            };
            let weekday_cn = match local_dt.weekday() {
                Weekday::Mon => "周一",
                Weekday::Tue => "周二",
                Weekday::Wed => "周三",
                Weekday::Thu => "周四",
                Weekday::Fri => "周五",
                Weekday::Sat => "周六",
                Weekday::Sun => "周日",
            };

            let formatted = serde_json::json!({
                "datetime": local_dt.to_rfc3339(),
                "date": format!("{:04}-{:02}-{:02}", local_dt.year(), local_dt.month(), local_dt.day()),
                "time": format!("{:02}:{:02}:{:02}", local_dt.hour(), local_dt.minute(), local_dt.second()),
                "timezone": timezone.name,
                "utc_offset": format_utc_offset(timezone.offset_seconds),
                "unix_timestamp": unix_secs,
                "unix_timestamp_ms": now.as_millis(),
                "weekday": {
                    "english": weekday_en,
                    "chinese": weekday_cn,
                    "iso_number": local_dt.weekday().number_from_monday(),
                },
            })
            .to_string();
            Ok(ToolResult::Output(async_stream::stream! {
                yield ToolOutput::text(formatted);
            }))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ParsedTimezone {
    pub(super) name: String,
    pub(super) offset_seconds: i32,
}

/// Parse a timezone string into an offset in seconds east of UTC.
/// Supports formats: "+08:00", "+0800", "-05:00", "UTC", "local", and a small
/// set of fixed-offset aliases used by Remi deployments.
pub(super) fn parse_timezone_spec(s: &str) -> Option<ParsedTimezone> {
    let s = s.trim();
    if s.is_empty() || s.eq_ignore_ascii_case("local") {
        let now = chrono::Local::now();
        let offset_seconds = now.offset().local_minus_utc();
        return Some(ParsedTimezone {
            name: "local".to_string(),
            offset_seconds,
        });
    }
    if s.eq_ignore_ascii_case("UTC") || s == "Z" || s == "z" {
        return Some(ParsedTimezone {
            name: "UTC".to_string(),
            offset_seconds: 0,
        });
    }
    if s.eq_ignore_ascii_case("Asia/Shanghai") || s.eq_ignore_ascii_case("Asia/Chongqing") {
        return Some(ParsedTimezone {
            name: "Asia/Shanghai".to_string(),
            offset_seconds: 8 * 3600,
        });
    }

    // Try to parse as "+HH:MM" or "+HHMM" or "-HH:MM" or "-HHMM"
    let (sign, rest) = if let Some(rest) = s.strip_prefix('+') {
        (1, rest)
    } else if let Some(rest) = s.strip_prefix('-') {
        (-1, rest)
    } else {
        return None;
    };

    let (hours, minutes): (i32, i32) = if let Some(pos) = rest.find(':') {
        let h = rest[..pos].parse().unwrap_or(0);
        let m = rest[pos + 1..].parse().unwrap_or(0);
        (h, m)
    } else if rest.len() == 4 {
        let h = rest[..2].parse().unwrap_or(0);
        let m = rest[2..].parse().unwrap_or(0);
        (h, m)
    } else {
        return None;
    };

    if hours > 14 || minutes > 59 {
        return None;
    }

    let offset_seconds = sign * (hours * 3600 + minutes * 60);
    Some(ParsedTimezone {
        name: format_utc_offset(offset_seconds),
        offset_seconds,
    })
}

pub(super) fn format_utc_offset(offset_seconds: i32) -> String {
    let offset_sign = if offset_seconds >= 0 { '+' } else { '-' };
    let offset_abs = offset_seconds.abs();
    let offset_h = offset_abs / 3600;
    let offset_m = (offset_abs % 3600) / 60;
    format!("{offset_sign}{offset_h:02}:{offset_m:02}")
}

/// Pause the tool loop briefly so the agent can wait before polling another tool.
pub struct SleepTool;

impl Tool for SleepTool {
    fn name(&self) -> &str {
        "sleep"
    }

    fn description(&self) -> &str {
        "Sleep for a short duration in seconds. Use this before polling a background task again. Maximum 10 seconds."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "seconds": {
                    "type": "number",
                    "description": "Sleep duration in seconds. Must be between 0 and 10."
                }
            },
            "required": ["seconds"]
        })
    }

    fn execute(
        &self,
        arguments: serde_json::Value,
        _resume: Option<ResumePayload>,
        _ctx: &ToolContext,
    ) -> impl std::future::Future<Output = Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError>>
    {
        async move {
            let seconds = arguments["seconds"]
                .as_f64()
                .ok_or_else(|| AgentError::tool("sleep", "missing or invalid 'seconds'"))?;
            if !seconds.is_finite() || !(0.0..=10.0).contains(&seconds) {
                return Err(AgentError::tool(
                    "sleep",
                    "seconds must be a finite number between 0 and 10",
                ));
            }

            tokio::time::sleep(Duration::from_secs_f64(seconds)).await;
            Ok(ToolResult::Output(async_stream::stream! {
                yield ToolOutput::text(serde_json::json!({
                    "slept_seconds": seconds,
                    "status": "completed"
                }).to_string());
            }))
        }
    }
}
