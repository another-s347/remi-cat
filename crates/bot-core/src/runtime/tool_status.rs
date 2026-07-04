pub(crate) fn builtin_tool_catalog() -> &'static [(&'static str, &'static str)] {
    &[
        (
            "acp__remi",
            "Open, resume, or poll a named ACP client session.",
        ),
        (
            "apply_patch",
            "Apply a unified diff patch inside the workspace.",
        ),
        (
            "bash",
            "Execute a bash command in the configured workspace sandbox.",
        ),
        ("codex", "Open, resume, or poll a Codex ACP session."),
        (
            "fetch",
            "Fetch content or IM bridge files into the workspace.",
        ),
        ("fs_create", "Create a new file in the workspace."),
        ("fs_ls", "List workspace directory entries."),
        ("fs_mkdir", "Create a workspace directory."),
        ("fs_read", "Read a workspace file."),
        ("fs_remove", "Remove a workspace file or directory."),
        ("fs_write", "Write a workspace file."),
        (
            "manage_yourself",
            "Run remi-cat CLI commands for self-management.",
        ),
        ("memory__get_detail", "Read a stored memory detail."),
        ("memory__recall", "Search memory directly."),
        ("memory__upsert_named", "Save or update a named memory."),
        ("now", "Return the current time."),
        ("rg", "Search workspace files with ripgrep."),
        ("search", "Search local memory/skills or web results."),
        ("skill__get", "Read a skill document."),
        ("skill__search", "Search installed skills."),
        ("sleep", "Wait for a short duration."),
        (
            "ssh",
            "Execute a command on a remote host via host OpenSSH.",
        ),
        ("todo__add", "Add todo items."),
        ("todo__complete", "Complete a todo item."),
        ("todo__list", "List todo items."),
        ("todo__remove", "Remove a todo item."),
        ("todo__update", "Update a todo item."),
        ("web_search", "Search the web with Exa."),
    ]
}

pub(crate) fn tool_warnings(name: &str) -> Vec<String> {
    match name {
        "search" | "web_search" if !env_present("EXA_API_KEY") => {
            vec!["web search is unavailable until EXA_API_KEY is set; local memory/skill search still works".to_string()]
        }
        _ => Vec::new(),
    }
}

pub(crate) fn tool_errors(name: &str, registered: bool) -> Vec<String> {
    if registered {
        return Vec::new();
    }
    match name {
        "bash" => vec![
            "bash is not registered because shell execution is disabled; enable shell.mode=local or a sandbox mode that allows bash"
                .to_string(),
        ],
        "codex" => vec![
            "codex is not registered because ACP Codex is not configured or the Codex binary is unavailable; run remi-cat acp setup --client codex and remi-cat acp doctor"
                .to_string(),
        ],
        name if name.starts_with("acp__") => vec![
            format!(
                "{name} is not registered because the configured ACP client is unavailable; run remi-cat acp setup and remi-cat acp doctor"
            )
                .to_string(),
        ],
        _ => Vec::new(),
    }
}

pub(crate) fn tool_runtime_errors(name: &str) -> Vec<String> {
    match name {
        "ssh" if !host_command_available("ssh") => vec![
            "ssh is not usable because the host ssh executable was not found in PATH".to_string(),
        ],
        "rg" if !host_command_available("rg") => vec![
            "rg is not usable because the host rg executable was not found in PATH".to_string(),
        ],
        _ => Vec::new(),
    }
}

fn host_command_available(name: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths).any(|path| {
                let candidate = path.join(name);
                candidate.is_file()
            })
        })
        .unwrap_or(false)
}

fn env_present(name: &str) -> bool {
    std::env::var(name)
        .ok()
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false)
}
