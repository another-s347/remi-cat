use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeSet;

const PRETTY_COMMAND_MAX_CHARS: usize = 120;
const PRETTY_OUTPUT_LINE_MAX_CHARS: usize = 80;
const PRETTY_OUTPUT_PREVIEW_LINES: usize = 3;

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
            (format!("查看 {path}"), "读取文件或目录内容".to_string())
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
        "rg" => {
            let pattern = args
                .get("args")
                .and_then(|v| v.as_array())
                .and_then(|arr| arr.first())
                .and_then(|v| v.as_str())
                .unwrap_or("内容");
            (
                format!("搜索 {pattern}"),
                "使用 ripgrep 搜索工作区".to_string(),
            )
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
        "workspace_bash" | "bash" => ("执行 bash".to_string(), bash_command_preview(args)),
        "ssh" => ("执行 ssh".to_string(), ssh_command_preview(args)),
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
        "codex" => (
            "调用 Codex 子会话".to_string(),
            "等待 Codex 回复".to_string(),
        ),
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
        if is_shell_like_tool(name) {
            let detail = bash_output_preview(result).unwrap_or_else(|| "工具执行失败".to_string());
            return (
                title,
                format!("{}\n{detail}", shell_command_preview(name, args)),
            );
        }
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
        "rg" => {
            if result.trim() == "No matches." {
                "未找到匹配".to_string()
            } else {
                let count = result
                    .lines()
                    .filter(|line| !line.trim().is_empty() && !line.starts_with('['))
                    .count();
                format!("找到 {count} 行")
            }
        }
        "fetch" => fetch_summary(result).unwrap_or(started),
        "workspace_bash" | "bash" => bash_summary(args, result),
        "ssh" => ssh_summary(args, result),
        "manage_yourself" => remi_command_summary(args, result),
        "codex" => acp_chat_summary(result).unwrap_or_else(|| "Codex 子会话已完成".to_string()),
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
            format!("{verb}会话文件 {file_key}"),
            "下载或转换会话资源".to_string(),
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
    if result.starts_with(&format!("{path} is a directory;")) {
        let entries = result
            .lines()
            .skip(1)
            .filter(|line| !line.trim().is_empty())
            .count();
        return Some(format!("已查看目录 {path}，{entries} 项"));
    }
    let duplicate = result.contains("repeats a range already read");
    let mut summary = if let Some(meta) = parse_fs_read_line_metadata(result) {
        format!(
            "已读取 {path} 第 {}-{} 行，共 {} 行",
            meta.start_line, meta.end_line, meta.total_lines
        )
    } else if let Some(meta) = parse_fs_read_metadata(result) {
        if meta.remaining > 0 {
            format!(
                "已读取 {path} offset={} length={}，仍有 {} 字节剩余",
                meta.offset, meta.length, meta.remaining
            )
        } else {
            format!("已读取 {path}，{} 字节", meta.length)
        }
    } else {
        let bytes = result.len();
        let truncated = result.contains("bytes remaining");
        if truncated {
            format!("已读取 {path} 的前 {bytes} 字节，仍有剩余内容")
        } else {
            format!("已读取 {path}，{bytes} 字节")
        }
    };
    if duplicate {
        summary.push_str("；重复读取同一片段，文件未变化");
    }
    Some(summary)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FsReadMetadata {
    offset: usize,
    length: usize,
    remaining: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FsReadLineMetadata {
    start_line: usize,
    end_line: usize,
    total_lines: usize,
}

fn parse_fs_read_line_metadata(result: &str) -> Option<FsReadLineMetadata> {
    let line = result
        .lines()
        .rev()
        .find(|line| line.starts_with("[start_line=") && line.ends_with(']'))?;
    let line = line.trim_start_matches('[').trim_end_matches(']');
    let mut start_line = None;
    let mut end_line = None;
    let mut total_lines = None;
    for part in line.split_whitespace() {
        let (key, value) = part.split_once('=')?;
        match key {
            "start_line" => start_line = value.parse().ok(),
            "end_line" => end_line = value.parse().ok(),
            "total_lines" => total_lines = value.parse().ok(),
            _ => {}
        }
    }
    Some(FsReadLineMetadata {
        start_line: start_line?,
        end_line: end_line?,
        total_lines: total_lines?,
    })
}

fn parse_fs_read_metadata(result: &str) -> Option<FsReadMetadata> {
    let line = result
        .lines()
        .rev()
        .find(|line| line.starts_with("[offset=") && line.ends_with(']'))?;
    let line = line.trim_start_matches('[').trim_end_matches(']');
    let mut offset = None;
    let mut length = None;
    let mut remaining = None;
    for part in line.split_whitespace() {
        let (key, value) = part.split_once('=')?;
        match key {
            "offset" => offset = value.parse().ok(),
            "length" => length = value.parse().ok(),
            "remaining" => remaining = value.parse().ok(),
            _ => {}
        }
    }
    Some(FsReadMetadata {
        offset: offset?,
        length: length?,
        remaining: remaining?,
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

fn is_bash_tool(name: &str) -> bool {
    matches!(name, "workspace_bash" | "bash")
}

fn is_shell_like_tool(name: &str) -> bool {
    is_bash_tool(name) || name == "ssh"
}

fn shell_command_preview(name: &str, args: &Value) -> String {
    if name == "ssh" {
        ssh_command_preview(args)
    } else {
        bash_command_preview(args)
    }
}

fn bash_command_preview(args: &Value) -> String {
    if let Some(command) = string_arg(args, "command") {
        return format!("$ {}", first_sentence(command, PRETTY_COMMAND_MAX_CHARS));
    }
    match (string_arg(args, "action"), string_arg(args, "pid")) {
        (Some(action), Some(pid)) => format!("bash {action} pid={pid}"),
        (None, Some(pid)) => format!("bash poll pid={pid}"),
        _ => "bash".to_string(),
    }
}

fn ssh_command_preview(args: &Value) -> String {
    let target = ssh_target_preview(args);
    if let Some(command) = string_arg(args, "command") {
        return format!(
            "ssh {target} $ {}",
            first_sentence(command, PRETTY_COMMAND_MAX_CHARS)
        );
    }
    match (string_arg(args, "action"), string_arg(args, "pid")) {
        (Some(action), Some(pid)) => format!("ssh {action} pid={pid}"),
        (None, Some(pid)) => format!("ssh poll pid={pid}"),
        _ => format!("ssh {target}"),
    }
}

fn ssh_target_preview(args: &Value) -> String {
    let host = string_arg(args, "host").unwrap_or("host");
    let user = string_arg(args, "user")
        .map(|user| format!("{user}@"))
        .unwrap_or_default();
    let port = args
        .get("port")
        .and_then(Value::as_u64)
        .map(|port| format!(":{port}"))
        .unwrap_or_default();
    format!("{user}{host}{port}")
}

fn bash_summary(args: &Value, result: &str) -> String {
    let command = bash_command_preview(args);
    let line_count = result.lines().count();
    if line_count == 0 {
        format!("{command}\n无输出")
    } else if let Some(preview) = bash_output_preview(result) {
        format!("{command}\n输出 {line_count} 行:\n{preview}")
    } else {
        format!("{command}\n输出 {line_count} 行")
    }
}

fn ssh_summary(args: &Value, result: &str) -> String {
    let command = ssh_command_preview(args);
    let line_count = result.lines().count();
    if line_count == 0 {
        format!("{command}\n无输出")
    } else if let Some(preview) = bash_output_preview(result) {
        format!("{command}\n输出 {line_count} 行:\n{preview}")
    } else {
        format!("{command}\n输出 {line_count} 行")
    }
}

fn bash_output_preview(result: &str) -> Option<String> {
    let lines = result
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(PRETTY_OUTPUT_PREVIEW_LINES)
        .map(|line| first_sentence(line, PRETTY_OUTPUT_LINE_MAX_CHARS))
        .collect::<Vec<_>>();
    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
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
    fn formats_fs_read_with_chunk_metadata() {
        let pretty = PrettyToolCall::completed(
            "1",
            "fs_read",
            &json!({"path":"src/main.rs", "offset": 626, "length": 626}),
            "content\n[offset=626 length=626 total_bytes=2048 remaining=796]\n[796 bytes remaining — call fs_read again with offset=1252]",
            true,
            42,
        );

        assert_eq!(
            pretty.summary,
            "已读取 src/main.rs offset=626 length=626，仍有 796 字节剩余"
        );
    }

    #[test]
    fn formats_fs_read_duplicate_warning() {
        let pretty = PrettyToolCall::completed(
            "1",
            "fs_read",
            &json!({"path":"src/main.rs", "offset": 0, "length": 626}),
            "warning: this fs_read request repeats a range already read in this session, and the file appears unchanged (path=src/main.rs, offset=0, length=626). Continue with offset=626 to read the next unread chunk.\n\ncontent\n[offset=0 length=626 total_bytes=2048 remaining=1422]",
            true,
            42,
        );

        assert_eq!(
            pretty.summary,
            "已读取 src/main.rs offset=0 length=626，仍有 1422 字节剩余；重复读取同一片段，文件未变化"
        );
    }

    #[test]
    fn formats_fs_read_with_line_metadata() {
        let pretty = PrettyToolCall::completed(
            "1",
            "fs_read",
            &json!({"path":"src/main.rs", "start_line": 10, "end_line": 20}),
            "content\n[start_line=10 end_line=20 total_lines=100]",
            true,
            42,
        );

        assert_eq!(pretty.summary, "已读取 src/main.rs 第 10-20 行，共 100 行");
    }

    #[test]
    fn formats_fs_read_directory_result() {
        let pretty = PrettyToolCall::completed(
            "1",
            "fs_read",
            &json!({"path":"src"}),
            "src is a directory; use fs_ls for directory-focused listing.\nmain.rs\nlib.rs",
            true,
            42,
        );

        assert_eq!(pretty.summary, "已查看目录 src，2 项");
    }

    #[test]
    fn formats_bash_command_and_first_three_output_lines_on_separate_lines() {
        let pretty = PrettyToolCall::completed(
            "1",
            "bash",
            &json!({"command":"cargo test"}),
            "one\ntwo\nthree\nfour\n",
            true,
            42,
        );

        assert_eq!(pretty.summary, "$ cargo test\n输出 4 行:\none\ntwo\nthree");
        assert!(!pretty.summary.contains("four"));
    }

    #[test]
    fn truncates_long_bash_command_preview() {
        let command = format!("cargo test {}", "very-long-argument ".repeat(20));
        let pretty = PrettyToolCall::started("1", "workspace_bash", &json!({"command": command}));

        assert_eq!(pretty.title, "执行 bash");
        assert!(pretty
            .summary
            .starts_with("$ cargo test very-long-argument"));
        assert!(pretty.summary.ends_with('…'));
        assert!(!pretty.summary.contains('\n'));
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
