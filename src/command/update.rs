use std::process::Stdio;
use std::time::Duration;

use anyhow::Context;
use tokio::process::Command as TokioCommand;

use crate::cli::{GitHubRelease, UpdateCommand, UpdateStatus};

const DEFAULT_UPDATE_REPO: &str = "another-s347/remi-cat";
const DEFAULT_UPDATE_GIT_URL: &str = "https://github.com/another-s347/remi-cat.git";

fn update_repo() -> String {
    std::env::var("REMI_UPDATE_REPO")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_UPDATE_REPO.to_string())
}

fn update_git_url(repo: &str) -> String {
    std::env::var("REMI_UPDATE_GIT_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| {
            if repo == DEFAULT_UPDATE_REPO {
                DEFAULT_UPDATE_GIT_URL.to_string()
            } else {
                format!("https://github.com/{repo}.git")
            }
        })
}

pub(crate) fn parse_release_version(value: &str) -> anyhow::Result<semver::Version> {
    let version = value.trim().trim_start_matches('v');
    semver::Version::parse(version).with_context(|| format!("invalid release version `{value}`"))
}

pub(crate) fn normalize_release_tag(value: &str) -> anyhow::Result<String> {
    let version = parse_release_version(value)?;
    Ok(format!("v{version}"))
}

#[cfg(test)]
pub(crate) fn update_available(current: &str, latest: &str) -> anyhow::Result<bool> {
    Ok(parse_release_version(latest)? > parse_release_version(current)?)
}

pub(crate) fn build_cargo_install_args(git_url: &str, tag: &str) -> Vec<String> {
    vec![
        "install".to_string(),
        "--git".to_string(),
        git_url.to_string(),
        "--tag".to_string(),
        tag.to_string(),
        "remi-cat".to_string(),
        "--locked".to_string(),
        "--force".to_string(),
    ]
}

async fn fetch_latest_github_release(repo: &str) -> anyhow::Result<GitHubRelease> {
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;
    let response = client
        .get(&url)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .header(reqwest::header::USER_AGENT, "remi-cat")
        .send()
        .await
        .with_context(|| format!("failed to query {url}"))?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("GitHub release check failed with HTTP {status}: {body}");
    }

    response
        .json::<GitHubRelease>()
        .await
        .context("failed to parse GitHub release response")
}

async fn build_update_status() -> anyhow::Result<UpdateStatus> {
    let repo = update_repo();
    let git_url = update_git_url(&repo);
    let release = fetch_latest_github_release(&repo).await?;
    let latest = parse_release_version(&release.tag_name)?;
    let current = parse_release_version(env!("CARGO_PKG_VERSION"))?;
    Ok(UpdateStatus {
        current_version: current.to_string(),
        latest_version: latest.to_string(),
        latest_tag: release.tag_name,
        update_available: latest > current,
        repo,
        git_url,
    })
}

pub(crate) async fn run_update_command(command: UpdateCommand) -> anyhow::Result<()> {
    match command {
        UpdateCommand::Check { json } => {
            let status = build_update_status().await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                println!("current: {}", status.current_version);
                println!("latest: {} ({})", status.latest_version, status.latest_tag);
                println!(
                    "update_available: {}",
                    if status.update_available { "yes" } else { "no" }
                );
                if status.update_available {
                    println!("Run: remi-cat update self");
                }
            }
            Ok(())
        }
        UpdateCommand::SelfUpdate {
            version,
            force,
            dry_run,
        } => {
            let repo = update_repo();
            let git_url = update_git_url(&repo);
            let target_tag = match version {
                Some(value) => normalize_release_tag(&value)?,
                None => build_update_status().await?.latest_tag,
            };
            let target_version = parse_release_version(&target_tag)?;
            let current_version = parse_release_version(env!("CARGO_PKG_VERSION"))?;
            if target_version <= current_version && !force {
                println!(
                    "remi-cat is already at {}. Use --force to reinstall {}.",
                    current_version, target_tag
                );
                return Ok(());
            }

            let install_args = build_cargo_install_args(&git_url, &target_tag);
            if dry_run {
                println!("cargo {}", install_args.join(" "));
                return Ok(());
            }

            println!(
                "Installing remi-cat {} from {} via cargo install...",
                target_tag, git_url
            );
            let status = TokioCommand::new("cargo")
                .args(&install_args)
                .stdin(Stdio::null())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .status()
                .await
                .context("failed to run cargo install")?;
            if !status.success() {
                anyhow::bail!("cargo install failed with status {status}");
            }
            println!("remi-cat updated to {target_tag}.");
            println!("Restart any running remi-cat profile processes to use the new binary.");
            Ok(())
        }
    }
}
