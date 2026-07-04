use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

const MODEL_INPUT_SNAPSHOT_DIR: &str = "model-input-snapshots";

pub(crate) fn upsert_model_input_snapshot_json(
    data_dir: &Path,
    session_id: &str,
    run_id: &str,
    snapshot_json: &str,
) -> Result<()> {
    let dir = session_dir(data_dir, session_id);
    fs::create_dir_all(&dir)
        .with_context(|| format!("creating model input snapshot dir {}", dir.display()))?;
    let path = snapshot_path(data_dir, session_id, run_id);
    let tmp_path = path.with_extension("json.tmp");
    fs::write(&tmp_path, snapshot_json)
        .with_context(|| format!("writing model input snapshot {}", tmp_path.display()))?;
    match fs::rename(&tmp_path, &path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            fs::remove_file(&path).with_context(|| {
                format!("removing existing model input snapshot {}", path.display())
            })?;
            fs::rename(&tmp_path, &path)
                .with_context(|| format!("saving model input snapshot {}", path.display()))?;
        }
        Err(err) => {
            return Err(err)
                .with_context(|| format!("saving model input snapshot {}", path.display()));
        }
    }
    Ok(())
}

pub(crate) fn list_model_input_snapshot_json(
    data_dir: &Path,
    session_id: &str,
    limit: Option<usize>,
) -> Result<Vec<String>> {
    let dir = session_dir(data_dir, session_id);
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut items = Vec::new();
    for entry in fs::read_dir(&dir)
        .with_context(|| format!("reading model input snapshot dir {}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let json = fs::read_to_string(&path)
            .with_context(|| format!("reading model input snapshot {}", path.display()))?;
        let created_at_ms = created_at_ms_from_snapshot(&json)
            .or_else(|| file_modified_ms(&path))
            .unwrap_or_default();
        items.push((created_at_ms, json));
    }

    items.sort_by(|left, right| right.0.cmp(&left.0));
    let take = limit.unwrap_or(items.len());
    Ok(items.into_iter().take(take).map(|(_, json)| json).collect())
}

pub(crate) fn get_model_input_snapshot_json(
    data_dir: &Path,
    session_id: &str,
    run_id: &str,
) -> Result<Option<String>> {
    let path = snapshot_path(data_dir, session_id, run_id);
    match fs::read_to_string(&path) {
        Ok(json) => Ok(Some(json)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => {
            Err(err).with_context(|| format!("reading model input snapshot {}", path.display()))
        }
    }
}

pub(crate) fn delete_model_input_snapshots(data_dir: &Path, session_id: &str) -> Result<()> {
    let dir = session_dir(data_dir, session_id);
    match fs::remove_dir_all(&dir) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => {
            Err(err).with_context(|| format!("deleting model input snapshot dir {}", dir.display()))
        }
    }
}

fn session_dir(data_dir: &Path, session_id: &str) -> PathBuf {
    data_dir
        .join(MODEL_INPUT_SNAPSHOT_DIR)
        .join(escape_component(session_id))
}

fn snapshot_path(data_dir: &Path, session_id: &str, run_id: &str) -> PathBuf {
    session_dir(data_dir, session_id).join(format!("{}.json", escape_component(run_id)))
}

fn created_at_ms_from_snapshot(json: &str) -> Option<i64> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let created_at = value.get("created_at")?.as_str()?;
    chrono::DateTime::parse_from_rfc3339(created_at)
        .ok()
        .map(|value| value.timestamp_millis())
}

fn file_modified_ms(path: &Path) -> Option<i64> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    let duration = modified
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .ok()?;
    Some(duration.as_millis() as i64)
}

fn escape_component(value: &str) -> String {
    let mut out = String::new();
    for byte in value.as_bytes() {
        match *byte {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' => {
                out.push(*byte as char);
            }
            other => {
                out.push('%');
                out.push_str(&format!("{other:02X}"));
            }
        }
    }
    if out.is_empty() {
        "%00".to_string()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::{
        delete_model_input_snapshots, get_model_input_snapshot_json,
        list_model_input_snapshot_json, upsert_model_input_snapshot_json,
    };

    #[test]
    fn snapshots_round_trip_and_sort_newest_first() {
        let temp = tempfile::tempdir().expect("tempdir");
        upsert_model_input_snapshot_json(
            temp.path(),
            "session/1",
            "run older",
            r#"{"run_id":"run older","created_at":"2026-01-01T00:00:00Z"}"#,
        )
        .expect("write older");
        upsert_model_input_snapshot_json(
            temp.path(),
            "session/1",
            "run newer",
            r#"{"run_id":"run newer","created_at":"2026-01-02T00:00:00Z"}"#,
        )
        .expect("write newer");

        let items =
            list_model_input_snapshot_json(temp.path(), "session/1", Some(1)).expect("list");
        assert_eq!(items.len(), 1);
        assert!(items[0].contains("run newer"));
        assert!(
            get_model_input_snapshot_json(temp.path(), "session/1", "run older")
                .expect("get")
                .is_some()
        );

        delete_model_input_snapshots(temp.path(), "session/1").expect("delete");
        assert!(
            list_model_input_snapshot_json(temp.path(), "session/1", None)
                .expect("list after delete")
                .is_empty()
        );
    }

    #[test]
    fn upsert_replaces_existing_snapshot_for_same_run() {
        let temp = tempfile::tempdir().expect("tempdir");
        upsert_model_input_snapshot_json(
            temp.path(),
            "session/1",
            "run/1",
            r#"{"run_id":"run/1","value":"before","created_at":"2026-01-01T00:00:00Z"}"#,
        )
        .expect("write first");
        upsert_model_input_snapshot_json(
            temp.path(),
            "session/1",
            "run/1",
            r#"{"run_id":"run/1","value":"after","created_at":"2026-01-01T00:00:01Z"}"#,
        )
        .expect("replace existing");

        let json = get_model_input_snapshot_json(temp.path(), "session/1", "run/1")
            .expect("get")
            .expect("snapshot exists");
        assert!(json.contains(r#""value":"after""#));
    }
}
