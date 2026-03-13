//! Blob store — extracts large binary payloads (base64 data URLs) from textual
//! memory files and replaces them with lightweight filesystem references.
//!
//! Placeholders have the form `[[blob:<uuid>.<ext>]]` and are valid inside
//! JSON strings, so they survive JSONL round-trips without escaping.
//!
//! Blobs are stored at `<thread_dir>/blobs/<uuid>.<ext>`.

use base64::Engine as _;
use std::path::Path;
use uuid::Uuid;

const DATA_PREFIX: &str = "data:";
const B64_MARKER: &str = ";base64,";
const BLOB_PREFIX: &str = "[[blob:";
const BLOB_SUFFIX: &str = "]]";

fn mime_to_ext(mime: &str) -> &str {
    match mime {
        "image/jpeg" | "image/jpg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "bin",
    }
}

fn ext_to_mime(ext: &str) -> &str {
    match ext {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => "application/octet-stream",
    }
}

/// Scan `text` for `data:<mime>;base64,<data>` patterns, write each payload
/// as raw bytes to `<blobs_dir>/<uuid>.<ext>`, and return the text with
/// inline `[[blob:<uuid>.<ext>]]` placeholders in their place.
pub async fn extract_blobs(text: &str, blobs_dir: &Path) -> std::io::Result<String> {
    let mut result = String::with_capacity(text.len());
    let mut pos = 0;

    while pos < text.len() {
        match text[pos..].find(DATA_PREFIX) {
            None => {
                result.push_str(&text[pos..]);
                break;
            }
            Some(rel) => {
                let start = pos + rel;
                result.push_str(&text[pos..start]);

                let after_data = start + DATA_PREFIX.len();

                // Mime types are short; search at most 100 chars ahead for ";base64,"
                let search_bound = (after_data + 100).min(text.len());
                let maybe_b64 = text[after_data..search_bound].find(B64_MARKER);

                if let Some(b64_rel) = maybe_b64 {
                    let mime = &text[after_data..after_data + b64_rel];
                    let mime_ok =
                        mime.contains('/') && mime.len() < 100 && !mime.bytes().any(|b| b <= b' ');

                    if mime_ok {
                        let data_start = after_data + b64_rel + B64_MARKER.len();
                        let data_end = text[data_start..]
                            .find(|c: char| {
                                !matches!(c, 'A'..='Z' | 'a'..='z' | '0'..='9' | '+' | '/' | '=')
                            })
                            .map(|i| data_start + i)
                            .unwrap_or(text.len());

                        let b64_data = &text[data_start..data_end];
                        if !b64_data.is_empty() {
                            if let Ok(bytes) =
                                base64::engine::general_purpose::STANDARD.decode(b64_data)
                            {
                                let ext = mime_to_ext(mime);
                                let filename = format!("{}.{ext}", Uuid::new_v4());
                                tokio::fs::create_dir_all(blobs_dir).await?;
                                tokio::fs::write(blobs_dir.join(&filename), &bytes).await?;
                                result.push_str(BLOB_PREFIX);
                                result.push_str(&filename);
                                result.push_str(BLOB_SUFFIX);
                                pos = data_end;
                                continue;
                            }
                        }
                    }
                }

                // Not a valid data URL — emit "data:" literally and advance.
                result.push_str(DATA_PREFIX);
                pos = after_data;
            }
        }
    }

    Ok(result)
}

/// Scan `text` for `[[blob:<uuid>.<ext>]]` placeholders, read each file from
/// `blobs_dir`, and restore them as inline `data:<mime>;base64,<data>` URLs.
pub async fn restore_blobs(text: &str, blobs_dir: &Path) -> String {
    let mut result = String::with_capacity(text.len());
    let mut pos = 0;

    while pos < text.len() {
        match text[pos..].find(BLOB_PREFIX) {
            None => {
                result.push_str(&text[pos..]);
                break;
            }
            Some(rel) => {
                let start = pos + rel;
                result.push_str(&text[pos..start]);

                let name_start = start + BLOB_PREFIX.len();
                if let Some(end_rel) = text[name_start..].find(BLOB_SUFFIX) {
                    let filename = &text[name_start..name_start + end_rel];
                    let abs_end = name_start + end_rel + BLOB_SUFFIX.len();

                    let ext = filename.rsplit('.').next().unwrap_or("bin");
                    let mime = ext_to_mime(ext);

                    match tokio::fs::read(blobs_dir.join(filename)).await {
                        Ok(bytes) => {
                            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                            result.push_str(DATA_PREFIX);
                            result.push_str(mime);
                            result.push_str(B64_MARKER);
                            result.push_str(&b64);
                        }
                        Err(_) => {
                            // Blob file missing — preserve placeholder as-is.
                            result.push_str(&text[start..abs_end]);
                        }
                    }
                    pos = abs_end;
                } else {
                    // No closing ]] — emit prefix and continue.
                    result.push_str(BLOB_PREFIX);
                    pos = name_start;
                }
            }
        }
    }

    result
}
