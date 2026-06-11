use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeSet;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PrettyToolStatus {
    Running,
    Success,
    Error,
}

#[derive(Debug, Clone, Serialize)]
pub struct PrettyToolCall {
    pub id: String,
    pub tool_name: String,
    pub title: String,
    pub summary: String,
    pub status: PrettyToolStatus,
    pub elapsed_ms: Option<u64>,
    pub request: Value,
    pub response: Option<String>,
}

impl PrettyToolCall {
    pub fn started(id: &str, name: &str, args: &Value) -> Self {
        let (title, summary) = describe_started(name, args);
        Self {
            id: id.to_string(),
            tool_name: name.to_string(),
            title,
            summary,
            status: PrettyToolStatus::Running,
            elapsed_ms: None,
            request: args.clone(),
            response: None,
        }
    }

    pub fn completed(
        id: &str,
        name: &str,
        args: &Value,
        result: &str,
        success: bool,
        elapsed_ms: u64,
    ) -> Self {
        let (title, mut summary) = describe_completed(name, args, result, success);
        if summary.trim().is_empty() {
            summary = if success {
                "执行完成".to_string()
            } else {
                first_line(result).unwrap_or("执行失败").to_string()
            };
        }
        Self {
            id: id.to_string(),
            tool_name: name.to_string(),
            title,
            summary,
            status: if success {
                PrettyToolStatus::Success
            } else {
                PrettyToolStatus::Error
            },
            elapsed_ms: Some(elapsed_ms),
            request: args.clone(),
            response: Some(result.to_string()),
        }
    }
}

pub fn tool_success(result: &str) -> bool {
    let trimmed = result.trim();
    !(trimmed.eq_ignore_ascii_case("interrupted")
        || trimmed.starts_with("error:")
        || trimmed.starts_with("Error:")
        || trimmed.starts_with("search request failed:")
        || trimmed.starts_with("parse error:")
        || trimmed.starts_with("[exit ")
        || trimmed.contains("\n[exit ")
        || trimmed.starts_with("[terminated by signal]")
        || trimmed.contains("\n[terminated by signal]"))
}

fn describe_started(name: &str, args: &Value) -> (String, String) {
    match name {
        "fs_read" => {
            let path = string_arg(args, "path").unwrap_or("文件");
            (format!("查看 {path}"), "读取文件内容".to_string())
        }
        "fs_write" => {
            let path = string_arg(args, "path").unwrap_or("文件");
            (format!("写入 {path}"), describe_content_bytes(args))
        }
        "fs_create" => {
            let path = string_arg(args, "path").unwrap_or("文件");
            (format!("创建 {path}"), describe_content_bytes(args))
        }
        "apply_patch" => {
            let patch = string_arg(args, "patch").unwrap_or("");
            let stats = patch_stats(patch);
            (
                "应用代码补丁".to_string(),
                format_patch_stats(&stats).unwrap_or_else(|| "更新工作区文件".to_string()),
            )
        }
        "fs_ls" => {
            let path = string_arg(args, "path").unwrap_or(".");
            (format!("查看目录 {path}"), "列出目录内容".to_string())
        }
        "fs_remove" => {
            let path = string_arg(args, "path").unwrap_or("文件");
            (format!("删除 {path}"), "移除工作区路径".to_string())
        }
        "fetch" => describe_fetch(args, "获取"),
        "im_upload" => {
            let path = string_arg(args, "path").unwrap_or("文件");
            (format!("上传 {path}"), "发送文件到当前会话".to_string())
        }
        "search" | "exa_search" => {
            let query = string_arg(args, "query").unwrap_or("内容");
            (format!("搜索 {query}"), "查询外部或本地资料".to_string())
        }
        "workspace_bash" => {
            let command = string_arg(args, "command").unwrap_or("命令");
            (format!("执行命令 {command}"), "运行 shell 命令".to_string())
        }
        "sleep" => ("等待".to_string(), "暂停一段时间".to_string()),
        "now" => ("读取当前时间".to_string(), "获取系统时间".to_string()),
        "manage_yourself" => {
            let command = string_arg(args, "command").unwrap_or("命令");
            (
                format!("管理 Remi: {command}"),
                "执行 remi-cat 管理命令".to_string(),
            )
        }
        value if value.starts_with("memory__") => ("检索/更新记忆".to_string(), value.to_string()),
        value if value.starts_with("trigger__") => ("管理触发器".to_string(), value.to_string()),
        "acp__chat" => ("调用 ACP 子会话".to_string(), "等待子会话回复".to_string()),
        value if value.starts_with("agent__") => {
            let agent = value.strip_prefix("agent__").unwrap_or(value);
            (format!("调用 agent {agent}"), "等待子会话回复".to_string())
        }
        value if value.starts_with("acp__") => {
            (format!("调用 {value}"), "执行 ACP 工具".to_string())
        }
        _ => (format!("调用 {name}"), "执行工具".to_string()),
    }
}

fn describe_completed(name: &str, args: &Value, result: &str, success: bool) -> (String, String) {
    let (title, started) = describe_started(name, args);
    if !success {
        return (
            title,
            first_line(result).unwrap_or("工具执行失败").to_string(),
        );
    }

    let summary = match name {
        "fs_read" => read_summary(args, result).unwrap_or(started),
        "fs_write" | "fs_create" => first_line(result).unwrap_or(&started).to_string(),
        "apply_patch" => apply_patch_summary(args, result).unwrap_or(started),
        "fs_ls" => {
            let count = result
                .lines()
                .filter(|line| !line.trim().is_empty())
                .count();
            format!("列出 {count} 项")
        }
        "fetch" => fetch_summary(result).unwrap_or(started),
        "workspace_bash" => bash_summary(result),
        "manage_yourself" => remi_command_summary(args, result),
        "acp__chat" => acp_chat_summary(result).unwrap_or_else(|| "子会话已完成".to_string()),
        value if value.starts_with("agent__") => "子会话已完成".to_string(),
        _ => first_line(result).unwrap_or(&started).to_string(),
    };
    (title, summary)
}

fn string_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)?
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn describe_content_bytes(args: &Value) -> String {
    let bytes = string_arg(args, "content").map(str::len).unwrap_or(0);
    if bytes == 0 {
        "写入内容".to_string()
    } else {
        format!("写入 {bytes} 字节")
    }
}

fn describe_fetch(args: &Value, verb: &str) -> (String, String) {
    if let Some(url) = string_arg(args, "url") {
        return (format!("{verb} {url}"), "下载或转换远程内容".to_string());
    }
    if let Some(file_key) = string_arg(args, "file_key") {
        return (
            format!("{verb} 飞书文件 {file_key}"),
            "下载会话文件".to_string(),
        );
    }
    if let Some(task_id) = string_arg(args, "task_id") {
        return (
            format!("查询 fetch 任务 {task_id}"),
            "轮询下载进度".to_string(),
        );
    }
    (
        format!("{verb}内容"),
        "下载当前消息中的附件或链接".to_string(),
    )
}

fn remi_command_summary(args: &Value, result: &str) -> String {
    if string_arg(args, "command").is_some_and(|command| command.contains("agent list")) {
        let agents = result
            .lines()
            .filter_map(|line| {
                let mut columns = line.split('\t').map(str::trim);
                let id = columns.next()?.trim();
                let name = columns.next().unwrap_or("").trim();
                if id.is_empty() {
                    None
                } else if name.is_empty() {
                    Some(id.to_string())
                } else {
                    Some(format!("{id}: {name}"))
                }
            })
            .take(3)
            .collect::<Vec<_>>();
        if !agents.is_empty() {
            return format!("列出 agent: {}", agents.join(", "));
        }
    }
    first_line(result)
        .map(|line| first_sentence(line, 80))
        .unwrap_or_else(|| "Remi 管理命令已完成".to_string())
}

fn acp_chat_summary(result: &str) -> Option<String> {
    let json: Value = serde_json::from_str(result).ok()?;
    for key in ["final_summary", "summary", "reply"] {
        if let Some(value) = json
            .get(key)
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return Some(first_sentence(value, 80));
        }
    }
    None
}

fn first_sentence(text: &str, max_chars: usize) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() <= max_chars {
        return line;
    }
    let mut truncated = line.chars().take(max_chars).collect::<String>();
    truncated.push('…');
    truncated
}

#[derive(Default)]
struct PatchStats {
    files: BTreeSet<String>,
    added_lines: usize,
    removed_lines: usize,
}

fn patch_stats(patch: &str) -> PatchStats {
    let mut stats = PatchStats::default();
    for line in patch.lines() {
        if let Some(path) = line
            .strip_prefix("*** Update File: ")
            .or_else(|| line.strip_prefix("*** Add File: "))
            .or_else(|| line.strip_prefix("*** Delete File: "))
        {
            stats.files.insert(path.trim().to_string());
            continue;
        }
        if line.starts_with('+') && !line.starts_with("+++") {
            stats.added_lines += 1;
        } else if line.starts_with('-') && !line.starts_with("---") {
            stats.removed_lines += 1;
        }
    }
    stats
}

fn format_patch_stats(stats: &PatchStats) -> Option<String> {
    if stats.files.is_empty() && stats.added_lines == 0 && stats.removed_lines == 0 {
        return None;
    }
    Some(format!(
        "更新 {} 个文件，+{} / -{} 行",
        stats.files.len().max(1),
        stats.added_lines,
        stats.removed_lines
    ))
}

fn apply_patch_summary(args: &Value, result: &str) -> Option<String> {
    let patch = string_arg(args, "patch").unwrap_or("");
    let stats = patch_stats(patch);
    let mut summary = format_patch_stats(&stats)?;
    if let Some(line) = first_line(result) {
        if !line.trim().is_empty() {
            summary.push_str(&format!("；{line}"));
        }
    }
    Some(summary)
}

fn read_summary(args: &Value, result: &str) -> Option<String> {
    let path = string_arg(args, "path")?;
    let bytes = result.len();
    let truncated = result.contains("bytes remaining");
    Some(if truncated {
        format!("已读取 {path} 的前 {bytes} 字节，仍有剩余内容")
    } else {
        format!("已读取 {path}，{bytes} 字节")
    })
}

fn fetch_summary(result: &str) -> Option<String> {
    let value: Value = serde_json::from_str(result).ok()?;
    let status = value
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("完成");
    let path = value.get("path").and_then(Value::as_str);
    let size = value.get("size_bytes").and_then(Value::as_u64);
    match (path, size) {
        (Some(path), Some(size)) => Some(format!("{status}，保存到 {path}，{size} 字节")),
        (Some(path), None) => Some(format!("{status}，保存到 {path}")),
        _ => Some(status.to_string()),
    }
}

fn bash_summary(result: &str) -> String {
    let line_count = result.lines().count();
    if line_count == 0 {
        "命令执行完成，无输出".to_string()
    } else {
        format!("命令执行完成，输出 {line_count} 行")
    }
}

fn first_line(text: &str) -> Option<&str> {
    text.lines().find(|line| !line.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn formats_fs_read() {
        let pretty = PrettyToolCall::completed(
            "1",
            "fs_read",
            &json!({"path":"src/main.rs"}),
            "hello",
            true,
            42,
        );
        assert_eq!(pretty.title, "查看 src/main.rs");
        assert!(pretty.summary.contains("已读取 src/main.rs"));
    }

    #[test]
    fn formats_apply_patch_stats() {
        let patch = "*** Update File: src/a.rs\n+new\n-old\n*** Add File: src/b.rs\n+line\n";
        let pretty = PrettyToolCall::started("1", "apply_patch", &json!({"patch": patch}));
        assert!(pretty.summary.contains("2 个文件"));
        assert!(pretty.summary.contains("+2 / -1"));
    }

    #[test]
    fn formats_manage_yourself() {
        let pretty = PrettyToolCall::completed(
            "1",
            "manage_yourself",
            &json!({"command": "profile list"}),
            "NAME\tSETUP\tRUNNING",
            true,
            511,
        );

        assert_eq!(pretty.title, "管理 Remi: profile list");
        assert_eq!(pretty.summary, "NAME SETUP RUNNING");
    }

    #[test]
    fn detects_error_result() {
        assert!(!tool_success("error: failed"));
        assert!(tool_success("ok"));
    }
}
