use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::profile_registry::ProfileRegistry;

const INSTANCE_SCHEMA_VERSION: u32 = 1;
const INSTANCES_DIR: &str = "profile-instances";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileInstanceRecord {
    pub schema_version: u32,
    pub profile_id: String,
    pub manifest_path: PathBuf,
    pub instance: String,
    pub pid: u32,
    pub started_at: String,
    pub log_path: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProfileInstanceStatus {
    pub profile_id: String,
    pub instance: String,
    pub pid: u32,
    pub state: &'static str,
    pub started_at: String,
    pub log_path: PathBuf,
}

pub fn start(
    registry_root: &Path,
    registry: &ProfileRegistry,
    reference: &str,
    instance: &str,
) -> anyhow::Result<ProfileInstanceStatus> {
    validate_instance_name(instance)?;
    let profile = registry.resolve(reference)?;
    let manifest_path = profile.manifest_path.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "managed instances require a manifest-backed profile; initialize and register one first"
        )
    })?;
    let record_path = record_path(registry_root, &profile.manifest.id, instance);
    if let Some(record) = read_record_if_present(&record_path)? {
        if managed_process_is_running(&record) {
            anyhow::bail!(
                "profile `{}` instance `{instance}` is already running with PID {}",
                profile.manifest.id,
                record.pid
            );
        }
        fs::remove_file(&record_path)
            .with_context(|| format!("removing stale instance record {}", record_path.display()))?;
    }

    let parent = record_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("instance record has no parent directory"))?;
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let log_path = parent.join(format!("{instance}.log"));
    let stdout = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("opening {}", log_path.display()))?;
    let stderr = stdout
        .try_clone()
        .with_context(|| format!("cloning {}", log_path.display()))?;

    let executable = std::env::current_exe().context("resolving current remi-cat executable")?;
    let mut command = Command::new(&executable);
    command
        .arg("--profile")
        .arg(&manifest_path)
        .env("REMI_PROFILE_REGISTRY_ROOT", registry_root)
        .env("REMI_PROFILE_INSTANCE", instance)
        .current_dir(
            profile
                .workspace
                .as_deref()
                .unwrap_or_else(|| manifest_path.parent().unwrap_or_else(|| Path::new("."))),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    configure_detached(&mut command);
    let mut child = command.spawn().with_context(|| {
        format!(
            "starting profile `{}` from {}",
            profile.manifest.id,
            manifest_path.display()
        )
    })?;
    std::thread::sleep(Duration::from_millis(300));
    if let Some(status) = child
        .try_wait()
        .context("checking managed profile startup")?
    {
        anyhow::bail!(
            "profile `{}` instance `{instance}` exited during startup with {status}; inspect {}",
            profile.manifest.id,
            log_path.display()
        );
    }
    let record = ProfileInstanceRecord {
        schema_version: INSTANCE_SCHEMA_VERSION,
        profile_id: profile.manifest.id,
        manifest_path,
        instance: instance.to_string(),
        pid: child.id(),
        started_at: chrono::Utc::now().to_rfc3339(),
        log_path,
    };
    write_record(&record_path, &record)?;
    Ok(status_from_record(&record, "running"))
}

pub fn stop(
    registry_root: &Path,
    registry: &ProfileRegistry,
    reference: &str,
    instance: &str,
    force: bool,
) -> anyhow::Result<ProfileInstanceStatus> {
    validate_instance_name(instance)?;
    let profile = registry.resolve(reference)?;
    let path = record_path(registry_root, &profile.manifest.id, instance);
    let record = read_record_if_present(&path)?.ok_or_else(|| {
        anyhow::anyhow!(
            "profile `{}` instance `{instance}` has no managed instance record",
            profile.manifest.id
        )
    })?;
    if managed_process_is_running(&record) {
        terminate_process(record.pid, force)?;
        wait_until_stopped(record.pid, Duration::from_secs(5))?;
    }
    fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    Ok(status_from_record(&record, "stopped"))
}

pub fn restart(
    registry_root: &Path,
    registry: &ProfileRegistry,
    reference: &str,
    instance: &str,
    force: bool,
) -> anyhow::Result<ProfileInstanceStatus> {
    let profile = registry.resolve(reference)?;
    let path = record_path(registry_root, &profile.manifest.id, instance);
    if path.exists() {
        stop(registry_root, registry, reference, instance, force)?;
    }
    start(registry_root, registry, reference, instance)
}

pub fn status(
    registry_root: &Path,
    registry: &ProfileRegistry,
    reference: Option<&str>,
    instance: Option<&str>,
    all: bool,
) -> anyhow::Result<Vec<ProfileInstanceStatus>> {
    if let Some(instance) = instance {
        validate_instance_name(instance)?;
    }
    let profile_id = if all {
        None
    } else {
        Some(
            registry
                .resolve(reference.ok_or_else(|| {
                    anyhow::anyhow!("PROFILE_REF is required unless --all is used")
                })?)?
                .manifest
                .id,
        )
    };
    let mut records = read_all_records(registry_root)?;
    records.retain(|record| {
        profile_id
            .as_deref()
            .is_none_or(|id| record.profile_id == id)
            && instance.is_none_or(|name| record.instance == name)
    });
    records.sort_by(|left, right| {
        (&left.profile_id, &left.instance).cmp(&(&right.profile_id, &right.instance))
    });
    Ok(records
        .iter()
        .map(|record| {
            status_from_record(
                record,
                if managed_process_is_running(record) {
                    "running"
                } else {
                    "stale"
                },
            )
        })
        .collect())
}

fn validate_instance_name(instance: &str) -> anyhow::Result<()> {
    if instance.is_empty()
        || !instance
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
    {
        anyhow::bail!("invalid profile instance name `{instance}`")
    }
    Ok(())
}

fn record_path(registry_root: &Path, profile_id: &str, instance: &str) -> PathBuf {
    registry_root
        .join(INSTANCES_DIR)
        .join(profile_id)
        .join(format!("{instance}.json"))
}

fn read_record_if_present(path: &Path) -> anyhow::Result<Option<ProfileInstanceRecord>> {
    match fs::read(path) {
        Ok(raw) => Ok(Some(
            serde_json::from_slice(&raw).with_context(|| format!("parsing {}", path.display()))?,
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

fn read_all_records(registry_root: &Path) -> anyhow::Result<Vec<ProfileInstanceRecord>> {
    let root = registry_root.join(INSTANCES_DIR);
    let Ok(profiles) = fs::read_dir(&root) else {
        return Ok(Vec::new());
    };
    let mut records = Vec::new();
    for profile in profiles {
        let profile = profile?;
        if !profile.file_type()?.is_dir() {
            continue;
        }
        for entry in fs::read_dir(profile.path())? {
            let entry = entry?;
            if entry.path().extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            if let Some(record) = read_record_if_present(&entry.path())? {
                records.push(record);
            }
        }
    }
    Ok(records)
}

fn write_record(path: &Path, record: &ProfileInstanceRecord) -> anyhow::Result<()> {
    let temporary = path.with_extension(format!("json.{}.tmp", std::process::id()));
    fs::write(&temporary, serde_json::to_vec_pretty(record)?)
        .with_context(|| format!("writing {}", temporary.display()))?;
    crate::atomic_file::replace(&temporary, path)
        .with_context(|| format!("replacing {}", path.display()))
}

fn status_from_record(
    record: &ProfileInstanceRecord,
    state: &'static str,
) -> ProfileInstanceStatus {
    ProfileInstanceStatus {
        profile_id: record.profile_id.clone(),
        instance: record.instance.clone(),
        pid: record.pid,
        state,
        started_at: record.started_at.clone(),
        log_path: record.log_path.clone(),
    }
}

fn wait_until_stopped(pid: u32, timeout: Duration) -> anyhow::Result<()> {
    let started = Instant::now();
    while process_is_running(pid) {
        if started.elapsed() >= timeout {
            anyhow::bail!(
                "process {pid} did not stop within {} seconds",
                timeout.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

fn managed_process_is_running(record: &ProfileInstanceRecord) -> bool {
    process_is_running(record.pid)
        && managed_process_command_line(record.pid)
            .is_some_and(|command_line| command_line_matches_record(&command_line, record))
}

fn command_line_matches_record(command_line: &str, record: &ProfileInstanceRecord) -> bool {
    command_line.contains("--profile")
        && command_line.contains(&record.manifest_path.to_string_lossy().to_string())
}

#[cfg(unix)]
fn managed_process_command_line(pid: u32) -> Option<String> {
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(windows)]
fn managed_process_command_line(pid: u32) -> Option<String> {
    let script = format!("(Get-CimInstance Win32_Process -Filter 'ProcessId = {pid}').CommandLine");
    let output = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(unix)]
fn process_is_running(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(windows)]
fn process_is_running(pid: u32) -> bool {
    Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .output()
        .is_ok_and(|output| {
            output.status.success()
                && String::from_utf8_lossy(&output.stdout).contains(&format!("\"{pid}\""))
        })
}

#[cfg(unix)]
fn terminate_process(pid: u32, force: bool) -> anyhow::Result<()> {
    let signal = if force { "-KILL" } else { "-TERM" };
    let status = Command::new("kill")
        .args([signal, &pid.to_string()])
        .status()
        .context("running kill")?;
    if !status.success() {
        anyhow::bail!("failed to send {signal} to process {pid}")
    }
    Ok(())
}

#[cfg(windows)]
fn terminate_process(pid: u32, force: bool) -> anyhow::Result<()> {
    let mut command = Command::new("taskkill");
    command.args(["/PID", &pid.to_string(), "/T"]);
    if force {
        command.arg("/F");
    }
    let output = command.output().context("running taskkill")?;
    if !output.status.success() {
        anyhow::bail!(
            "failed to stop process {pid}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
    Ok(())
}

#[cfg(unix)]
fn configure_detached(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(windows)]
fn configure_detached(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    command.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
}

#[cfg(test)]
mod tests {
    use super::{command_line_matches_record, validate_instance_name, ProfileInstanceRecord};
    use std::path::PathBuf;

    #[test]
    fn validates_managed_instance_names() {
        for valid in ["default", "feishu-work", "blue_2", "a.b"] {
            validate_instance_name(valid).unwrap();
        }
        for invalid in ["", "../x", "a b", "测试"] {
            assert!(validate_instance_name(invalid).is_err());
        }
    }

    #[test]
    fn managed_process_identity_requires_the_recorded_manifest() {
        let record = ProfileInstanceRecord {
            schema_version: 1,
            profile_id: "travel".to_string(),
            manifest_path: PathBuf::from("/profiles/travel/profile.yaml"),
            instance: "default".to_string(),
            pid: 42,
            started_at: "now".to_string(),
            log_path: PathBuf::from("travel.log"),
        };
        assert!(command_line_matches_record(
            "remi-cat --profile /profiles/travel/profile.yaml",
            &record
        ));
        assert!(!command_line_matches_record("unrelated-server", &record));
        assert!(!command_line_matches_record(
            "remi-cat --profile /profiles/other/profile.yaml",
            &record
        ));
    }
}
