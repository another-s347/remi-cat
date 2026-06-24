use std::path::{Component, Path, PathBuf};

use anyhow::{anyhow, Context, Result};

pub(super) async fn absolute_existing_dir(path: &Path) -> Result<PathBuf> {
    tokio::fs::create_dir_all(path)
        .await
        .with_context(|| format!("creating sandbox dir {}", path.display()))?;
    tokio::fs::canonicalize(path)
        .await
        .with_context(|| format!("canonicalizing sandbox dir {}", path.display()))
}

fn clean_workspace_path(path: &str, workspace_root_label: &str) -> Result<PathBuf> {
    let input = Path::new(path);
    if input.is_absolute() {
        let label = Path::new(workspace_root_label);
        let stripped = input.strip_prefix(label).with_context(|| {
            format!("absolute sandbox path must start with {}", label.display())
        })?;
        return clean_relative_components(stripped);
    }
    clean_relative_components(input)
}

fn clean_relative_components(path: &Path) -> Result<PathBuf> {
    let mut cleaned = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => cleaned.push(part),
            Component::CurDir => {}
            Component::ParentDir => return Err(anyhow!("path may not contain '..'")),
            Component::RootDir | Component::Prefix(_) => {
                return Err(anyhow!("path must be relative to the sandbox root"))
            }
        }
    }
    if cleaned.as_os_str().is_empty() {
        Ok(PathBuf::from("."))
    } else {
        Ok(cleaned)
    }
}

pub(super) async fn resolve_local_path(root: &Path, path: &str) -> Result<PathBuf> {
    tokio::fs::create_dir_all(root)
        .await
        .with_context(|| format!("creating local root {}", root.display()))?;
    let input = Path::new(path);
    if input.is_absolute() {
        Ok(input.to_path_buf())
    } else {
        Ok(root.join(input))
    }
}

pub(super) async fn resolve_local_existing_path(root: &Path, path: &str) -> Result<PathBuf> {
    let full = resolve_local_path(root, path).await?;
    tokio::fs::canonicalize(&full)
        .await
        .with_context(|| format!("canonicalizing local path {}", path))
}

async fn canonical_root(root: &Path) -> Result<PathBuf> {
    tokio::fs::create_dir_all(root)
        .await
        .with_context(|| format!("creating sandbox root {}", root.display()))?;
    tokio::fs::canonicalize(root)
        .await
        .with_context(|| format!("canonicalizing sandbox root {}", root.display()))
}

pub(super) fn current_host_user_spec() -> Option<String> {
    let uid = std::process::Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())?;
    let gid = std::process::Command::new("id")
        .arg("-g")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())?;
    Some(format!("{uid}:{gid}"))
}

pub(super) async fn resolve_workspace_existing_path(
    root: &Path,
    workspace_root_label: &str,
    path: &str,
) -> Result<PathBuf> {
    let root = canonical_root(root).await?;
    let relative = clean_workspace_path(path, workspace_root_label)?;
    resolve_existing_relative_path(&root, &relative, path).await
}

async fn resolve_existing_relative_path(
    root: &Path,
    relative: &Path,
    original_path: &str,
) -> Result<PathBuf> {
    let full = root.join(relative);
    let canonical = tokio::fs::canonicalize(&full)
        .await
        .with_context(|| format!("canonicalizing sandbox path {}", original_path))?;
    if !canonical.starts_with(&root) {
        return Err(anyhow!("path escapes sandbox root"));
    }
    Ok(canonical)
}

pub(super) async fn resolve_workspace_writable_file_path(
    root: &Path,
    workspace_root_label: &str,
    path: &str,
) -> Result<PathBuf> {
    let root = canonical_root(root).await?;
    let relative = clean_workspace_path(path, workspace_root_label)?;
    resolve_writable_file_relative_path(&root, &relative, path).await
}

async fn resolve_writable_file_relative_path(
    root: &Path,
    relative: &Path,
    original_path: &str,
) -> Result<PathBuf> {
    if relative == Path::new(".") {
        return Err(anyhow!("path must refer to a file"));
    }
    let full = root.join(relative);
    if tokio::fs::symlink_metadata(&full).await.is_ok() {
        let canonical = tokio::fs::canonicalize(&full)
            .await
            .with_context(|| format!("canonicalizing sandbox path {}", original_path))?;
        if !canonical.starts_with(&root) {
            return Err(anyhow!("path escapes sandbox root"));
        }
        return Ok(canonical);
    }
    let parent = full
        .parent()
        .ok_or_else(|| anyhow!("path has no parent directory"))?;
    let parent = tokio::fs::canonicalize(parent)
        .await
        .with_context(|| format!("canonicalizing parent for sandbox path {}", original_path))?;
    if !parent.starts_with(&root) {
        return Err(anyhow!("path escapes sandbox root"));
    }
    Ok(full)
}

pub(super) async fn resolve_workspace_writable_dir_path(
    root: &Path,
    workspace_root_label: &str,
    path: &str,
    recursive: bool,
) -> Result<PathBuf> {
    let root = canonical_root(root).await?;
    let relative = clean_workspace_path(path, workspace_root_label)?;
    let full = root.join(&relative);
    if tokio::fs::symlink_metadata(&full).await.is_ok() {
        let canonical = tokio::fs::canonicalize(&full)
            .await
            .with_context(|| format!("canonicalizing sandbox path {}", path))?;
        if !canonical.starts_with(&root) {
            return Err(anyhow!("path escapes sandbox root"));
        }
        return Ok(canonical);
    }

    let ancestor = if recursive {
        nearest_existing_ancestor(&full)
    } else {
        full.parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| anyhow!("path has no parent directory"))?
    };
    let ancestor = tokio::fs::canonicalize(&ancestor)
        .await
        .with_context(|| format!("canonicalizing parent for sandbox path {}", path))?;
    if !ancestor.starts_with(&root) {
        return Err(anyhow!("path escapes sandbox root"));
    }
    Ok(full)
}

fn nearest_existing_ancestor(path: &Path) -> PathBuf {
    let mut current = path.to_path_buf();
    while !current.exists() {
        let Some(parent) = current.parent() else {
            break;
        };
        current = parent.to_path_buf();
    }
    current
}
