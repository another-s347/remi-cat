use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::Context;
use sha2::{Digest, Sha256};
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct BinaryReleaseAsset {
    archive_name: String,
    checksum_name: String,
    binary_name: &'static str,
}

fn binary_release_asset(os: &str, arch: &str) -> anyhow::Result<BinaryReleaseAsset> {
    let (target, extension, binary_name) = match (os, arch) {
        ("linux", "x86_64") => ("x86_64-unknown-linux-musl", "tar.gz", "remi-cat"),
        ("macos", "aarch64") => ("aarch64-apple-darwin", "tar.gz", "remi-cat"),
        ("windows", "x86_64") => ("x86_64-pc-windows-msvc", "zip", "remi-cat.exe"),
        _ => anyhow::bail!(
            "binary updates are not published for {os}/{arch}; use cargo-based update instead"
        ),
    };
    let archive_name = format!("remi-cat-{target}.{extension}");
    Ok(BinaryReleaseAsset {
        checksum_name: format!("{archive_name}.sha256"),
        archive_name,
        binary_name,
    })
}

fn release_download_url(repo: &str, tag: &str, file_name: &str) -> String {
    format!("https://github.com/{repo}/releases/download/{tag}/{file_name}")
}

fn verify_archive_checksum(archive: &[u8], checksum: &str) -> anyhow::Result<()> {
    let expected = checksum
        .split_whitespace()
        .next()
        .context("release checksum file is empty")?
        .to_ascii_lowercase();
    anyhow::ensure!(
        expected.len() == 64 && expected.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "release checksum is not a SHA-256 digest"
    );
    let actual = format!("{:x}", Sha256::digest(archive));
    anyhow::ensure!(
        actual == expected,
        "release checksum mismatch: expected {expected}, got {actual}"
    );
    Ok(())
}

fn extract_release_binary(archive: &[u8], asset: &BinaryReleaseAsset) -> anyhow::Result<Vec<u8>> {
    if asset.archive_name.ends_with(".tar.gz") {
        let decoder = flate2::read::GzDecoder::new(Cursor::new(archive));
        let mut archive = tar::Archive::new(decoder);
        for entry in archive
            .entries()
            .context("failed to read release archive")?
        {
            let mut entry = entry.context("failed to read release archive entry")?;
            if entry
                .path()
                .ok()
                .and_then(|path| path.file_name().map(|name| name == asset.binary_name))
                .unwrap_or(false)
            {
                let mut binary = Vec::new();
                entry
                    .read_to_end(&mut binary)
                    .context("failed to extract release binary")?;
                return Ok(binary);
            }
        }
    } else {
        let mut archive =
            zip::ZipArchive::new(Cursor::new(archive)).context("failed to read release archive")?;
        for index in 0..archive.len() {
            let mut entry = archive
                .by_index(index)
                .context("failed to read release archive entry")?;
            if Path::new(entry.name())
                .file_name()
                .is_some_and(|name| name == asset.binary_name)
            {
                let mut binary = Vec::new();
                entry
                    .read_to_end(&mut binary)
                    .context("failed to extract release binary")?;
                return Ok(binary);
            }
        }
    }
    anyhow::bail!("release archive does not contain `{}`", asset.binary_name)
}

fn local_executable_path() -> anyhow::Result<PathBuf> {
    std::env::current_exe().context("failed to resolve the current remi-cat executable")
}

fn install_binary(destination: &Path, binary: &[u8]) -> anyhow::Result<()> {
    let parent = destination
        .parent()
        .context("local executable path has no parent directory")?;
    let staged = parent.join(format!(
        ".remi-cat-update-{}{}",
        uuid::Uuid::new_v4(),
        std::env::consts::EXE_SUFFIX
    ));
    let mut file = std::fs::File::create(&staged)
        .with_context(|| format!("failed to create {}", staged.display()))?;
    file.write_all(binary)
        .with_context(|| format!("failed to write {}", staged.display()))?;
    file.sync_all()
        .with_context(|| format!("failed to sync {}", staged.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("failed to make {} executable", staged.display()))?;
        std::fs::rename(&staged, destination).with_context(|| {
            format!(
                "failed to replace local executable {}",
                destination.display()
            )
        })?;
    }

    #[cfg(windows)]
    {
        let backup = parent.join(format!(".remi-cat-update-{}.old.exe", uuid::Uuid::new_v4()));
        if destination.exists() {
            std::fs::rename(destination, &backup).with_context(|| {
                format!("failed to stage local executable {}", destination.display())
            })?;
        }
        if let Err(error) = std::fs::rename(&staged, destination) {
            if backup.exists() {
                let _ = std::fs::rename(&backup, destination);
            }
            return Err(error).with_context(|| {
                format!(
                    "failed to replace local executable {}",
                    destination.display()
                )
            });
        }
        let _ = std::fs::remove_file(backup);
    }

    Ok(())
}

async fn install_release_binary(repo: &str, tag: &str, dry_run: bool) -> anyhow::Result<()> {
    let asset = binary_release_asset(std::env::consts::OS, std::env::consts::ARCH)?;
    let archive_url = release_download_url(repo, tag, &asset.archive_name);
    let checksum_url = release_download_url(repo, tag, &asset.checksum_name);
    let destination = local_executable_path()?;
    if dry_run {
        println!("download: {archive_url}");
        println!("checksum: {checksum_url}");
        println!("install: {}", destination.display());
        return Ok(());
    }

    println!("Downloading {}...", asset.archive_name);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()?;
    let archive = client
        .get(&archive_url)
        .header(reqwest::header::USER_AGENT, "remi-cat")
        .send()
        .await
        .with_context(|| format!("failed to download {archive_url}"))?
        .error_for_status()
        .with_context(|| format!("failed to download {archive_url}"))?
        .bytes()
        .await
        .context("failed to read release archive")?;
    let checksum = client
        .get(&checksum_url)
        .header(reqwest::header::USER_AGENT, "remi-cat")
        .send()
        .await
        .with_context(|| format!("failed to download {checksum_url}"))?
        .error_for_status()
        .with_context(|| format!("failed to download {checksum_url}"))?
        .text()
        .await
        .context("failed to read release checksum")?;

    verify_archive_checksum(&archive, &checksum)?;
    let binary = extract_release_binary(&archive, &asset)?;
    install_binary(&destination, &binary)?;
    println!("Installed {}", destination.display());
    Ok(())
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
            binary,
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

            if binary {
                install_release_binary(&repo, &target_tag, dry_run).await?;
                if !dry_run {
                    println!("remi-cat updated to {target_tag}.");
                    println!(
                        "Restart any running remi-cat profile processes to use the new binary."
                    );
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_published_binary_assets() {
        assert_eq!(
            binary_release_asset("linux", "x86_64").unwrap(),
            BinaryReleaseAsset {
                archive_name: "remi-cat-x86_64-unknown-linux-musl.tar.gz".to_string(),
                checksum_name: "remi-cat-x86_64-unknown-linux-musl.tar.gz.sha256".to_string(),
                binary_name: "remi-cat",
            }
        );
        assert_eq!(
            binary_release_asset("windows", "x86_64")
                .unwrap()
                .archive_name,
            "remi-cat-x86_64-pc-windows-msvc.zip"
        );
        assert!(binary_release_asset("linux", "aarch64").is_err());
    }

    #[test]
    fn verifies_release_checksum() {
        let archive = b"release archive";
        let checksum = format!(
            "{:x}  remi-cat.tar.gz\n",
            Sha256::digest(archive.as_slice())
        );
        verify_archive_checksum(archive, &checksum).unwrap();
        assert!(verify_archive_checksum(archive, &format!("{} file", "0".repeat(64))).is_err());
    }

    #[test]
    fn extracts_binary_from_tar_gzip_release() {
        let mut tar_bytes = Vec::new();
        {
            let encoder =
                flate2::write::GzEncoder::new(&mut tar_bytes, flate2::Compression::default());
            let mut archive = tar::Builder::new(encoder);
            let binary = b"test executable";
            let mut header = tar::Header::new_gnu();
            header.set_size(binary.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            archive
                .append_data(&mut header, "remi-cat", binary.as_slice())
                .unwrap();
            archive.into_inner().unwrap().finish().unwrap();
        }
        let asset = binary_release_asset("linux", "x86_64").unwrap();
        assert_eq!(
            extract_release_binary(&tar_bytes, &asset).unwrap(),
            b"test executable"
        );
    }
}
