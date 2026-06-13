use anyhow::{Context, Result};
use automerge::transaction::Transactable;
use automerge::{ActorId, AutoCommit, ObjId, ObjType, ReadDoc, ScalarValue, Value};

fn collect_object_keys_by_type(
    doc: &AutoCommit,
    obj: &ObjId,
    key: &str,
    expected_type: ObjType,
) -> Result<Vec<ObjId>> {
    let mut out = Vec::new();

    if let Ok(all) = doc.get_all(obj, key) {
        for (value, id) in all {
            if matches!(value, Value::Object(obj_type) if obj_type == expected_type) {
                out.push(id);
            }
        }
    }

    if out.is_empty() {
        if let Some((Value::Object(obj_type), id)) = doc.get(obj, key)? {
            if obj_type == expected_type {
                out.push(id);
            }
        }
    }

    Ok(out)
}

pub fn collect_map_keys(doc: &AutoCommit, obj: &ObjId, key: &str) -> Result<Vec<ObjId>> {
    collect_object_keys_by_type(doc, obj, key, ObjType::Map)
}

pub fn collect_list_keys(doc: &AutoCommit, obj: &ObjId, key: &str) -> Result<Vec<ObjId>> {
    collect_object_keys_by_type(doc, obj, key, ObjType::List)
}

pub fn actor_id(actor: &str) -> ActorId {
    let bytes = if actor.is_empty() {
        b"remi-actor".to_vec()
    } else {
        actor.as_bytes().to_vec()
    };
    ActorId::from(bytes)
}

pub fn set_doc_actor(doc: &mut AutoCommit, actor: &str) {
    doc.set_actor(actor_id(actor));
}

pub fn ensure_map_key(doc: &mut AutoCommit, obj: &ObjId, key: &str) -> Result<ObjId> {
    if let Some((Value::Object(ObjType::Map), id)) = doc.get(obj, key)? {
        return Ok(id);
    }
    if let Some(id) = collect_map_keys(doc, obj, key)?.into_iter().next() {
        return Ok(id);
    }
    Ok(doc
        .put_object(obj, key, ObjType::Map)
        .with_context(|| format!("Failed to create map '{key}'"))?)
}

pub fn ensure_list_key(doc: &mut AutoCommit, obj: &ObjId, key: &str) -> Result<ObjId> {
    if let Some((Value::Object(ObjType::List), id)) = doc.get(obj, key)? {
        return Ok(id);
    }
    if let Some(id) = collect_list_keys(doc, obj, key)?.into_iter().next() {
        return Ok(id);
    }
    Ok(doc
        .put_object(obj, key, ObjType::List)
        .with_context(|| format!("Failed to create list '{key}'"))?)
}

pub fn ensure_child_map(doc: &mut AutoCommit, parent_map: &ObjId, key: &str) -> Result<ObjId> {
    if let Some((Value::Object(ObjType::Map), id)) = doc.get(parent_map, key)? {
        return Ok(id);
    }
    if let Some(id) = collect_map_keys(doc, parent_map, key)?.into_iter().next() {
        return Ok(id);
    }
    Ok(doc
        .put_object(parent_map, key, ObjType::Map)
        .with_context(|| format!("Failed to create child map '{key}'"))?)
}

pub fn put_string(doc: &mut AutoCommit, obj: &ObjId, key: &str, value: &str) -> Result<()> {
    doc.put(obj, key, value)
        .with_context(|| format!("Failed to put string '{key}'"))?;
    Ok(())
}

pub fn put_u64(doc: &mut AutoCommit, obj: &ObjId, key: &str, value: u64) -> Result<()> {
    doc.put(obj, key, i64::try_from(value).unwrap_or(i64::MAX))
        .with_context(|| format!("Failed to put u64 '{key}'"))?;
    Ok(())
}

pub fn get_string(doc: &AutoCommit, obj: &ObjId, key: &str) -> Result<Option<String>> {
    match doc.get(obj, key)? {
        Some((Value::Scalar(sv), _)) => match sv.as_ref() {
            ScalarValue::Str(s) => Ok(Some(s.to_string())),
            ScalarValue::Bytes(b) => Ok(Some(String::from_utf8_lossy(b).to_string())),
            other => anyhow::bail!("Expected string for '{key}', got {other:?}"),
        },
        None => Ok(None),
        Some((other, _)) => anyhow::bail!("Expected scalar string for '{key}', got {other:?}"),
    }
}

pub fn get_u64(doc: &AutoCommit, obj: &ObjId, key: &str) -> Result<Option<u64>> {
    match doc.get(obj, key)? {
        Some((Value::Scalar(sv), _)) => match sv.as_ref() {
            ScalarValue::Int(i) => Ok(Some((*i).max(0) as u64)),
            ScalarValue::Uint(u) => Ok(Some(*u as u64)),
            other => anyhow::bail!("Expected int for '{key}', got {other:?}"),
        },
        None => Ok(None),
        Some((other, _)) => anyhow::bail!("Expected scalar int for '{key}', got {other:?}"),
    }
}

pub fn get_json_string(
    doc: &AutoCommit,
    obj: &ObjId,
    key: &str,
) -> Result<Option<serde_json::Value>> {
    let Some(s) = get_string(doc, obj, key)? else {
        return Ok(None);
    };
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Ok(Some(serde_json::Value::Object(Default::default())));
    }
    Ok(serde_json::from_str(trimmed).ok())
}

pub fn collect_root_maps(doc: &AutoCommit, key: &str) -> Result<Vec<ObjId>> {
    let mut out: Vec<ObjId> = Vec::new();
    if let Ok(all) = doc.get_all(automerge::ROOT, key) {
        for (val, obj_id) in all {
            if matches!(val, Value::Object(ObjType::Map)) {
                out.push(obj_id);
            }
        }
    }
    if out.is_empty() {
        if let Some((Value::Object(ObjType::Map), obj_id)) = doc.get(automerge::ROOT, key)? {
            out.push(obj_id);
        }
    }
    Ok(out)
}
