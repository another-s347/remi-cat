use std::path::Path;
use std::process::Command;
use std::time::UNIX_EPOCH;

use anyhow::{anyhow, Context, Result};
use serde::Serialize;

const DEFAULT_LIMIT: usize = 20;
const MAX_LIMIT: usize = 50;
const MAX_RG_LINES: usize = 20_000;

const RG_EXCLUDE_GLOBS: &[&str] = &[
    "!.git/**",
    "!target/**",
    "!node_modules/**",
    "!dist/**",
    "!build/**",
    "!.cache/**",
    "!.codex-tmp/**",
    "!.next/**",
    "!.turbo/**",
    "!coverage/**",
];

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct WorkspaceFileMatch {
    pub relative_path: String,
    pub mention_path: String,
    pub display_path: String,
    pub kind: WorkspaceEntryKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceEntryKind {
    File,
}

#[derive(Debug)]
struct Candidate {
    item: WorkspaceFileMatch,
    score: i64,
}

pub fn search_workspace_files(
    host_root: &Path,
    workspace_root_label: &str,
    query: &str,
    limit: usize,
) -> Result<Vec<WorkspaceFileMatch>> {
    let host_root = std::fs::canonicalize(host_root)
        .with_context(|| format!("canonicalizing workspace root {}", host_root.display()))?;
    let query = normalize_query(query);
    let terms = query
        .split_whitespace()
        .filter(|term| !term.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    let output = run_rg_files(&host_root, workspace_root_label)?;
    let limit = limit.clamp(1, MAX_LIMIT);
    let mut matches = Vec::new();
    for relative_path in output.lines().take(MAX_RG_LINES).filter_map(clean_rg_path) {
        let searchable = normalize_query(&relative_path);
        if !terms.iter().all(|term| searchable.contains(term)) {
            continue;
        }
        let metadata = std::fs::metadata(host_root.join(&relative_path)).ok();
        matches.push(Candidate {
            score: score_candidate(&searchable, &terms),
            item: WorkspaceFileMatch {
                mention_path: relative_path.clone(),
                display_path: display_path(workspace_root_label, &relative_path),
                relative_path,
                kind: WorkspaceEntryKind::File,
                size: metadata.as_ref().map(|metadata| metadata.len()),
                modified: metadata
                    .and_then(|metadata| metadata.modified().ok())
                    .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                    .map(|duration| duration.as_secs().to_string()),
            },
        });
    }
    matches.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| left.item.relative_path.cmp(&right.item.relative_path))
    });
    matches.truncate(limit);
    Ok(matches
        .into_iter()
        .map(|candidate| candidate.item)
        .collect())
}

fn run_rg_files(host_root: &Path, workspace_root_label: &str) -> Result<String> {
    let mut args = vec!["--files", "--hidden"];
    for glob in RG_EXCLUDE_GLOBS {
        args.push("--glob");
        args.push(glob);
    }

    let output = if sandbox_kind_is_docker() {
        let container_name = std::env::var("REMI_SANDBOX_CONTAINER_NAME")
            .unwrap_or_else(|_| "remi-cat-sandbox".to_string());
        Command::new("docker")
            .args(["exec", "-w", workspace_root_label, &container_name, "rg"])
            .args(&args)
            .output()
            .with_context(|| {
                format!("running docker rg --files in sandbox container `{container_name}`")
            })?
    } else {
        Command::new("rg")
            .args(&args)
            .current_dir(host_root)
            .output()
            .context("running rg --files in workspace")?
    };

    if !output.status.success() {
        return Err(anyhow!(
            "rg --files failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8(output.stdout).context("rg --files output was not UTF-8")
}

fn sandbox_kind_is_docker() -> bool {
    std::env::var("REMI_SANDBOX_KIND")
        .ok()
        .map(|value| value.trim().eq_ignore_ascii_case("docker"))
        .unwrap_or(false)
}

fn clean_rg_path(line: &str) -> Option<String> {
    let path = line.trim();
    if path.is_empty() || path.starts_with('/') || path.contains('\0') {
        return None;
    }
    if path
        .split('/')
        .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return None;
    }
    Some(path.to_string())
}

fn normalize_query(value: &str) -> String {
    value
        .trim()
        .trim_start_matches('@')
        .trim_start_matches('/')
        .to_lowercase()
}

fn score_candidate(path: &str, terms: &[String]) -> i64 {
    if terms.is_empty() {
        return 10 - path.len() as i64;
    }
    let mut score = 10;
    let name = path.rsplit('/').next().unwrap_or(path);
    for term in terms {
        if path == term {
            score += 1000;
        } else if name.starts_with(term) {
            score += 700;
        } else if path.starts_with(term) {
            score += 500;
        } else if path.contains(term) {
            score += 250;
        }
    }
    score - path.len() as i64
}

fn display_path(workspace_root_label: &str, relative_path: &str) -> String {
    let label = workspace_root_label.trim_end_matches('/');
    if label.is_empty() {
        relative_path.to_string()
    } else {
        format!("{label}/{relative_path}")
    }
}

pub fn default_file_search_limit(limit: Option<usize>) -> usize {
    limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_path_uses_sandbox_label() {
        assert_eq!(
            display_path("/workspace", "src/main.rs"),
            "/workspace/src/main.rs"
        );
        assert_eq!(
            display_path("/tmp/remi", "src/main.rs"),
            "/tmp/remi/src/main.rs"
        );
    }

    #[test]
    fn match_keeps_relative_mention_path() {
        let relative_path = "Soul.md".to_string();
        let item = WorkspaceFileMatch {
            mention_path: relative_path.clone(),
            display_path: display_path("/workspace", &relative_path),
            relative_path,
            kind: WorkspaceEntryKind::File,
            size: None,
            modified: None,
        };

        assert_eq!(item.mention_path, "Soul.md");
        assert_eq!(item.display_path, "/workspace/Soul.md");
    }

    #[test]
    fn rejects_unsafe_rg_paths() {
        assert_eq!(clean_rg_path("src/main.rs").as_deref(), Some("src/main.rs"));
        assert!(clean_rg_path("/etc/passwd").is_none());
        assert!(clean_rg_path("../secret").is_none());
        assert!(clean_rg_path("src/../secret").is_none());
    }
}
