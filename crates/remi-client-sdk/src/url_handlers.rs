use async_trait::async_trait;
use base64::Engine as _;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anytomd::{ConversionOptions, ConversionResult, convert_bytes, convert_file};
use exif::{In, Reader as ExifReader, Tag, Value as ExifValue};
use serde_json::{Map as JsonMap, Value as JsonValue, json};
use unpdf::{parse_bytes, render, render::RenderOptions};
use url::Url;

use crate::chat_types::{RichHandlerResult, ToolImagePart};
use crate::external_tool_handler::ExternalToolHandler;
use crate::external_tools::ExternalToolExecutor;
use crate::remi_uri::{RemiUri, RemiUriLocation};

const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;
const MAX_REMOTE_TEXT_BYTES: usize = 10 * 1024 * 1024;

pub struct FetchHandler;

enum FetchTarget {
    HttpUrl(String),
    LocalPath(PathBuf),
    RemiUri(String),
}

struct FetchResponse {
    json: JsonValue,
    rich: Option<RichHandlerResult>,
}

#[async_trait]
impl ExternalToolHandler for FetchHandler {
    async fn handle(&self, _interrupt_id: &str, payload: &JsonValue) -> Result<JsonValue, String> {
        let uri = extract_uri(payload)?;
        Ok(fetch_response(uri).await?.json)
    }

    async fn handle_rich(
        &self,
        _interrupt_id: &str,
        payload: &JsonValue,
    ) -> Result<RichHandlerResult, String> {
        let uri = extract_uri(payload)?;
        let response = fetch_response(uri).await?;
        Ok(response
            .rich
            .unwrap_or(RichHandlerResult::Json(response.json)))
    }
}

pub fn register_url_external_tools(executor: &mut ExternalToolExecutor) {
    executor.register("fetch_request", FetchHandler);
    executor.register("resolve_uri", FetchHandler);
}

fn extract_uri(payload: &JsonValue) -> Result<&str, String> {
    payload
        .get("uri")
        .and_then(|value| value.as_str())
        .or_else(|| {
            payload
                .get("arguments")
                .and_then(|value| value.get("uri"))
                .and_then(|value| value.as_str())
        })
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "fetch requires a non-empty uri".to_string())
}

fn build_http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (compatible; RemiBot/1.0)")
        .redirect(reqwest::redirect::Policy::limited(5))
        .timeout(Duration::from_secs(10))
        .danger_accept_invalid_certs(false)
        .build()
        .map_err(|e| e.to_string())
}

async fn fetch_response(source: &str) -> Result<FetchResponse, String> {
    let started_at = Instant::now();
    let target = classify_target(source)?;
    tracing::info!(
        source = %source,
        target_kind = %fetch_target_kind(&target),
        "[FetchHandler] Starting fetch"
    );

    let result = match &target {
        FetchTarget::HttpUrl(url) => fetch_http_target(source, url).await,
        FetchTarget::LocalPath(path) => fetch_local_path(source, path).await,
        FetchTarget::RemiUri(uri) => fetch_remi_uri(source, uri).await,
    };

    match &result {
        Ok(response) => tracing::info!(
            source = %source,
            target_kind = %fetch_target_kind(&target),
            fetch_kind = %response_fetch_kind(response),
            content_type = %response_content_type(response),
            rich = response.rich.is_some(),
            elapsed_ms = started_at.elapsed().as_millis(),
            "[FetchHandler] Fetch completed"
        ),
        Err(error) => tracing::warn!(
            source = %source,
            target_kind = %fetch_target_kind(&target),
            elapsed_ms = started_at.elapsed().as_millis(),
            error = %error,
            "[FetchHandler] Fetch failed"
        ),
    }

    result
}

fn classify_target(source: &str) -> Result<FetchTarget, String> {
    let trimmed = source.trim();
    if trimmed.starts_with("remi://") {
        return Ok(FetchTarget::RemiUri(trimmed.to_string()));
    }

    if looks_like_windows_path(trimmed) {
        return Ok(FetchTarget::LocalPath(PathBuf::from(trimmed)));
    }

    if let Ok(url) = Url::parse(trimmed) {
        return match url.scheme() {
            "http" | "https" => Ok(FetchTarget::HttpUrl(trimmed.to_string())),
            "file" => url
                .to_file_path()
                .map(FetchTarget::LocalPath)
                .map_err(|_| format!("Unsupported file:// path: {trimmed}")),
            other => Err(format!(
                "Unsupported URI scheme '{other}' for fetch: {trimmed}"
            )),
        };
    }

    Ok(FetchTarget::LocalPath(PathBuf::from(trimmed)))
}

async fn fetch_http_target(source: &str, url: &str) -> Result<FetchResponse, String> {
    let client = build_http_client()?;
    let started_at = Instant::now();
    tracing::info!(url = %url, "[FetchHandler] Sending HTTP fetch request");
    let response = client
        .get(url)
        .header("Accept", "text/html,application/xhtml+xml,*/*;q=0.8")
        .send()
        .await
        .map_err(|e| e.to_string())?;

    let status = response.status();
    let content_length = response.content_length();

    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();

    tracing::info!(
        url = %url,
        status = %status,
        content_type = %content_type,
        content_length = content_length,
        elapsed_ms = started_at.elapsed().as_millis(),
        "[FetchHandler] Received HTTP fetch response headers"
    );

    if content_type.starts_with("image/") {
        let bytes = response.bytes().await.map_err(|e| e.to_string())?;
        return Ok(build_image_response(
            source,
            url,
            bytes.to_vec(),
            &content_type,
        ));
    }

    let bytes = response.bytes().await.map_err(|e| e.to_string())?;
    if bytes.len() > MAX_REMOTE_TEXT_BYTES {
        return Err(format!(
            "Remote resource too large for Markdown conversion: {} bytes (max {})",
            bytes.len(),
            MAX_REMOTE_TEXT_BYTES
        ));
    }

    let extension = infer_extension_from_url(url)
        .or_else(|| infer_extension_from_content_type(&content_type).map(ToString::to_string))
        .unwrap_or_else(|| "txt".to_string());
    let html = if looks_like_html(&content_type, &extension) {
        Some(String::from_utf8_lossy(&bytes).into_owned())
    } else {
        None
    };
    let metadata = html
        .as_deref()
        .map(metadata_from_html)
        .unwrap_or_else(|| JsonValue::Object(JsonMap::new()));
    let converted = convert_remote_bytes(&bytes, &extension).await;

    Ok(build_document_response(
        source,
        url,
        "web",
        Some(&content_type),
        converted,
        metadata,
    ))
}

async fn fetch_local_path(source: &str, path: &Path) -> Result<FetchResponse, String> {
    if !path.exists() {
        return Err(format!("Local path not found: {}", path.display()));
    }

    let bytes = tokio::fs::read(path)
        .await
        .map_err(|e| format!("Cannot read {}: {e}", path.display()))?;
    let resolved_uri = path.display().to_string();
    let content_type = infer_content_type_from_path(path);

    if is_image_content_type(content_type.as_deref()) {
        return Ok(build_image_response(
            source,
            &resolved_uri,
            bytes,
            content_type
                .as_deref()
                .unwrap_or("application/octet-stream"),
        ));
    }

    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("txt")
        .trim_start_matches('.')
        .to_string();
    let metadata = if matches!(extension.as_str(), "html" | "htm") {
        metadata_from_html(&String::from_utf8_lossy(&bytes))
    } else {
        JsonValue::Object(JsonMap::new())
    };
    let converted = convert_local_file(path.to_path_buf(), &extension).await;

    Ok(build_document_response(
        source,
        &resolved_uri,
        "local_file",
        content_type.as_deref(),
        converted,
        metadata,
    ))
}

async fn fetch_remi_uri(source: &str, uri: &str) -> Result<FetchResponse, String> {
    let parsed = RemiUri::parse(uri).map_err(|e| e.to_string())?;
    match parsed.location {
        RemiUriLocation::Remote => fetch_http_target(source, &parsed.path).await,
        RemiUriLocation::File => {
            let path = if parsed.path.len() > 2 && parsed.path.chars().nth(1) == Some(':') {
                PathBuf::from(parsed.path.clone())
            } else {
                PathBuf::from(format!("/{}", parsed.path))
            };
            let mut response = fetch_local_path(source, &path).await?;
            if let Some(object) = response.json.as_object_mut() {
                object.insert("fetch_kind".to_string(), json!("remi_file"));
                object.insert("resolved_uri".to_string(), json!(uri));
                object.insert("url".to_string(), json!(uri));
                object
                    .entry("content_type".to_string())
                    .or_insert_with(|| json!(parsed.mime_type));
            }
            Ok(response)
        }
        RemiUriLocation::Local => Err(format!(
            "remi://local URIs require app data dir context and cannot be resolved by the SDK fetch handler: {uri}"
        )),
        RemiUriLocation::Inline => Ok(FetchResponse {
            json: json!({
                "source": source,
                "resolved_uri": uri,
                "url": uri,
                "fetch_kind": "inline",
                "type": "inline"
            }),
            rich: None,
        }),
    }
}

fn build_image_response(
    source: &str,
    resolved_uri: &str,
    bytes: Vec<u8>,
    content_type: &str,
) -> FetchResponse {
    let exif = extract_image_exif(&bytes);
    let media_type = content_type
        .split(';')
        .next()
        .unwrap_or(content_type)
        .trim()
        .to_string();

    if bytes.len() > MAX_IMAGE_BYTES {
        return FetchResponse {
            json: json!({
                "source": source,
                "resolved_uri": resolved_uri,
                "url": resolved_uri,
                "fetch_kind": "image",
                "type": "image",
                "content_type": media_type,
                "image_exif": exif,
                "warnings": [format!("resource_too_large:image_bytes>{MAX_IMAGE_BYTES}")]
            }),
            rich: None,
        };
    }

    FetchResponse {
        json: json!({
            "source": source,
            "resolved_uri": resolved_uri,
            "url": resolved_uri,
            "fetch_kind": "image",
            "type": "image",
            "content_type": media_type,
            "image_exif": exif,
        }),
        rich: Some(RichHandlerResult::Image(ToolImagePart {
            media_type,
            data: bytes,
            exif,
        })),
    }
}

fn extract_image_exif(bytes: &[u8]) -> Option<JsonValue> {
    let mut cursor = Cursor::new(bytes);
    let exif = ExifReader::new().read_from_container(&mut cursor).ok()?;
    let fields = exif
        .fields()
        .map(|field| {
            let mut object = JsonMap::new();
            object.insert("ifd".to_string(), json!(format!("{:?}", field.ifd_num)));
            object.insert("tag".to_string(), json!(format!("{:?}", field.tag)));
            object.insert(
                "display_value".to_string(),
                json!(field.display_value().with_unit(&exif).to_string()),
            );
            object.insert("value".to_string(), exif_value_to_json(&field.value));
            JsonValue::Object(object)
        })
        .collect::<Vec<_>>();

    if fields.is_empty() {
        return None;
    }

    let mut object = JsonMap::new();
    object.insert("fields".to_string(), JsonValue::Array(fields));
    if let Some(orientation) = exif
        .get_field(Tag::Orientation, In::PRIMARY)
        .map(|field| exif_value_to_json(&field.value))
    {
        object.insert("orientation".to_string(), orientation);
    }
    Some(JsonValue::Object(object))
}

fn exif_value_to_json(value: &ExifValue) -> JsonValue {
    match value {
        ExifValue::Byte(values) => numeric_slice_json(values),
        ExifValue::Ascii(values) => {
            let strings = values
                .iter()
                .map(|value| {
                    String::from_utf8_lossy(value)
                        .trim_end_matches('\0')
                        .to_string()
                })
                .collect::<Vec<_>>();
            string_slice_json(strings)
        }
        ExifValue::Short(values) => numeric_slice_json(values),
        ExifValue::Long(values) => numeric_slice_json(values),
        ExifValue::Rational(values) => rational_slice_json(values),
        ExifValue::SByte(values) => numeric_slice_json(values),
        ExifValue::Undefined(values, _) => {
            json!(base64::engine::general_purpose::STANDARD.encode(values))
        }
        ExifValue::SShort(values) => numeric_slice_json(values),
        ExifValue::SLong(values) => numeric_slice_json(values),
        ExifValue::SRational(values) => srational_slice_json(values),
        ExifValue::Float(values) => float_slice_json(values),
        ExifValue::Double(values) => float_slice_json(values),
        other => json!(format!("{other:?}")),
    }
}

fn numeric_slice_json<T>(values: &[T]) -> JsonValue
where
    T: Copy + serde::Serialize,
{
    if values.len() == 1 {
        json!(values[0])
    } else {
        json!(values)
    }
}

fn float_slice_json<T>(values: &[T]) -> JsonValue
where
    T: Copy + serde::Serialize,
{
    if values.len() == 1 {
        json!(values[0])
    } else {
        json!(values)
    }
}

fn string_slice_json(values: Vec<String>) -> JsonValue {
    if values.len() == 1 {
        json!(values.into_iter().next())
    } else {
        json!(values)
    }
}

fn rational_slice_json(values: &[exif::Rational]) -> JsonValue {
    if values.len() == 1 {
        rational_json(values[0])
    } else {
        JsonValue::Array(values.iter().copied().map(rational_json).collect())
    }
}

fn srational_slice_json(values: &[exif::SRational]) -> JsonValue {
    if values.len() == 1 {
        srational_json(values[0])
    } else {
        JsonValue::Array(values.iter().copied().map(srational_json).collect())
    }
}

fn rational_json(value: exif::Rational) -> JsonValue {
    json!({
        "num": value.num,
        "denom": value.denom,
        "value": if value.denom == 0 { None::<f64> } else { Some(value.num as f64 / value.denom as f64) },
    })
}

fn srational_json(value: exif::SRational) -> JsonValue {
    json!({
        "num": value.num,
        "denom": value.denom,
        "value": if value.denom == 0 { None::<f64> } else { Some(value.num as f64 / value.denom as f64) },
    })
}

async fn convert_local_file(path: PathBuf, extension: &str) -> Result<ConversionResult, String> {
    let extension = extension.to_string();
    tokio::task::spawn_blocking(move || {
        if extension.eq_ignore_ascii_case("pdf") {
            let markdown = unpdf::to_markdown(&path).map_err(|e| e.to_string())?;
            return Ok(simple_conversion_result(markdown));
        }

        convert_file(&path, &ConversionOptions::default()).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

async fn convert_remote_bytes(bytes: &[u8], extension: &str) -> Result<ConversionResult, String> {
    let data = bytes.to_vec();
    let extension = extension.to_string();
    tokio::task::spawn_blocking(move || {
        if extension.eq_ignore_ascii_case("pdf") {
            let document = parse_bytes(&data).map_err(|e| e.to_string())?;
            let markdown = render::to_markdown(&document, &RenderOptions::default())
                .map_err(|e| e.to_string())?;
            return Ok(simple_conversion_result(markdown));
        }

        convert_bytes(&data, &extension, &ConversionOptions::default()).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

fn simple_conversion_result(markdown: String) -> ConversionResult {
    ConversionResult {
        markdown: markdown.clone(),
        plain_text: markdown,
        title: None,
        images: Vec::new(),
        warnings: Vec::new(),
    }
}

fn build_document_response(
    source: &str,
    resolved_uri: &str,
    fetch_kind: &str,
    content_type: Option<&str>,
    converted: Result<ConversionResult, String>,
    metadata: JsonValue,
) -> FetchResponse {
    match converted {
        Ok(converted) => {
            let warnings = converted
                .warnings
                .iter()
                .map(|warning| format!("{warning:?}"))
                .collect::<Vec<_>>();

            let mut object = JsonMap::new();
            object.insert("source".to_string(), json!(source));
            object.insert("resolved_uri".to_string(), json!(resolved_uri));
            object.insert("url".to_string(), json!(resolved_uri));
            object.insert("fetch_kind".to_string(), json!(fetch_kind));
            object.insert("content_markdown".to_string(), json!(converted.markdown));
            object.insert("plain_text".to_string(), json!(converted.plain_text));
            if let Some(content_type) = content_type.filter(|value| !value.is_empty()) {
                object.insert("content_type".to_string(), json!(content_type));
            }
            if let Some(title) = converted.title.filter(|value| !value.trim().is_empty()) {
                object.insert("title".to_string(), json!(title));
            }
            if !warnings.is_empty() {
                object.insert("warnings".to_string(), json!(warnings));
            }
            merge_metadata(&mut object, metadata);
            FetchResponse {
                json: JsonValue::Object(object),
                rich: None,
            }
        }
        Err(error) => {
            let mut object = JsonMap::new();
            object.insert("source".to_string(), json!(source));
            object.insert("resolved_uri".to_string(), json!(resolved_uri));
            object.insert("url".to_string(), json!(resolved_uri));
            object.insert("fetch_kind".to_string(), json!(fetch_kind));
            object.insert("error".to_string(), json!(error));
            if let Some(content_type) = content_type.filter(|value| !value.is_empty()) {
                object.insert("content_type".to_string(), json!(content_type));
            }
            merge_metadata(&mut object, metadata);
            FetchResponse {
                json: JsonValue::Object(object),
                rich: None,
            }
        }
    }
}

fn merge_metadata(target: &mut JsonMap<String, JsonValue>, metadata: JsonValue) {
    if let JsonValue::Object(map) = metadata {
        for (key, value) in map {
            if !value.is_null() {
                target.entry(key).or_insert(value);
            }
        }
    }
}

fn metadata_from_html(html: &str) -> JsonValue {
    let meta = extract_meta(html);
    let mut result = JsonMap::new();

    let title = meta
        .get("og:title")
        .or_else(|| meta.get("twitter:title"))
        .or_else(|| meta.get("html_title"))
        .cloned();
    let description = meta
        .get("og:description")
        .or_else(|| meta.get("twitter:description"))
        .or_else(|| meta.get("description"))
        .cloned();

    if let Some(title) = title.filter(|value| !value.is_empty()) {
        result.insert("title".to_string(), json!(title));
    }
    if let Some(description) = description.filter(|value| !value.is_empty()) {
        result.insert("description".to_string(), json!(description));
    }
    if let Some(value) = meta.get("og:site_name") {
        result.insert("site_name".to_string(), json!(value));
    }
    if let Some(value) = meta.get("og:image").or_else(|| meta.get("twitter:image")) {
        result.insert("image".to_string(), json!(value));
    }
    if let Some(value) = meta.get("og:type") {
        result.insert("type".to_string(), json!(value));
    }

    JsonValue::Object(result)
}

fn infer_extension_from_url(url: &str) -> Option<String> {
    Url::parse(url)
        .ok()?
        .path_segments()?
        .next_back()?
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
}

fn infer_content_type_from_path(path: &Path) -> Option<String> {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())?
        .to_ascii_lowercase();
    Some(match extension.as_str() {
        "md" | "markdown" => "text/markdown".to_string(),
        "txt" | "log" | "rst" | "toml" | "yaml" | "yml" | "ini" => "text/plain".to_string(),
        "html" | "htm" => "text/html".to_string(),
        "json" => "application/json".to_string(),
        "xml" => "application/xml".to_string(),
        "csv" => "text/csv".to_string(),
        "pdf" => "application/pdf".to_string(),
        "docx" => {
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document".to_string()
        }
        "pptx" => {
            "application/vnd.openxmlformats-officedocument.presentationml.presentation".to_string()
        }
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet".to_string(),
        "png" => "image/png".to_string(),
        "jpg" | "jpeg" => "image/jpeg".to_string(),
        "gif" => "image/gif".to_string(),
        "webp" => "image/webp".to_string(),
        "bmp" => "image/bmp".to_string(),
        "svg" => "image/svg+xml".to_string(),
        other => format!("application/octet-stream; ext={other}"),
    })
}

fn infer_extension_from_content_type(content_type: &str) -> Option<&'static str> {
    let normalized = content_type
        .split(';')
        .next()
        .unwrap_or(content_type)
        .trim()
        .to_ascii_lowercase();
    match normalized.as_str() {
        "text/html" | "application/xhtml+xml" => Some("html"),
        "text/plain" | "text/markdown" => Some("txt"),
        "application/json" | "text/json" => Some("json"),
        "application/xml" | "text/xml" => Some("xml"),
        "text/csv" | "application/csv" => Some("csv"),
        "application/pdf" => Some("pdf"),
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => Some("docx"),
        "application/vnd.openxmlformats-officedocument.presentationml.presentation" => Some("pptx"),
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => Some("xlsx"),
        _ => None,
    }
}

fn looks_like_html(content_type: &str, extension: &str) -> bool {
    content_type.contains("html") || matches!(extension, "html" | "htm")
}

fn looks_like_windows_path(source: &str) -> bool {
    let bytes = source.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/')
}

fn is_image_content_type(content_type: Option<&str>) -> bool {
    content_type.is_some_and(|value| value.starts_with("image/"))
}

fn find_ascii_case_insensitive(haystack: &str, needle: &str) -> Option<usize> {
    let needle_bytes = needle.as_bytes();
    if needle_bytes.is_empty() {
        return Some(0);
    }

    haystack
        .as_bytes()
        .windows(needle_bytes.len())
        .position(|window| window.eq_ignore_ascii_case(needle_bytes))
}

fn extract_meta(html: &str) -> std::collections::HashMap<String, String> {
    use std::collections::HashMap;

    let mut result = HashMap::new();

    if let Some(start) = find_ascii_case_insensitive(html, "<title") {
        if let Some(end_tag) = html[start..].find('>') {
            let after_open = start + end_tag + 1;
            if let Some(close) = find_ascii_case_insensitive(&html[after_open..], "</title>") {
                let title_text = &html[after_open..after_open + close];
                result.insert("html_title".into(), html_decode(title_text.trim()));
            }
        }
    }

    let mut pos = 0;
    while let Some(offset) = find_ascii_case_insensitive(&html[pos..], "<meta") {
        let start = pos + offset;
        let end = match html[start..].find('>') {
            Some(end) => start + end + 1,
            None => break,
        };
        let tag = &html[start..end];
        let (prop, content) = parse_meta_tag(tag);
        let relevant = matches!(
            prop.as_str(),
            "og:title"
                | "og:description"
                | "og:image"
                | "og:site_name"
                | "og:url"
                | "og:type"
                | "twitter:title"
                | "twitter:description"
                | "twitter:image"
                | "description"
        );
        if relevant && !content.is_empty() {
            result.entry(prop).or_insert(content);
        }
        pos = end;
    }

    result
}

fn parse_meta_tag(tag: &str) -> (String, String) {
    let mut prop = String::new();
    let mut content = String::new();
    let mut remaining = tag;

    while let Some(eq_pos) = remaining.find('=') {
        let key = remaining[..eq_pos]
            .trim()
            .split_whitespace()
            .last()
            .unwrap_or("")
            .to_lowercase();
        remaining = &remaining[eq_pos + 1..];
        let (value, rest) = parse_attr_value(remaining);
        remaining = rest;

        let key_lc = key.trim_start_matches('/');
        if matches!(key_lc, "property" | "name") {
            prop = value.to_lowercase();
        } else if key_lc == "content" {
            content = html_decode(&value);
        }
    }

    (prop, content)
}

fn parse_attr_value(source: &str) -> (String, &str) {
    let source = source.trim_start();
    if let Some(inner) = source.strip_prefix('"') {
        if let Some(end) = inner.find('"') {
            return (inner[..end].into(), &inner[end + 1..]);
        }
    } else if let Some(inner) = source.strip_prefix('\'') {
        if let Some(end) = inner.find('\'') {
            return (inner[..end].into(), &inner[end + 1..]);
        }
    } else {
        let end = source
            .find(|c: char| c.is_whitespace() || c == '>')
            .unwrap_or(source.len());
        return (source[..end].into(), &source[end..]);
    }

    (String::new(), source)
}

fn fetch_target_kind(target: &FetchTarget) -> &'static str {
    match target {
        FetchTarget::HttpUrl(_) => "http",
        FetchTarget::LocalPath(_) => "local_path",
        FetchTarget::RemiUri(_) => "remi_uri",
    }
}

fn response_fetch_kind(response: &FetchResponse) -> &str {
    response
        .json
        .get("fetch_kind")
        .and_then(JsonValue::as_str)
        .unwrap_or("")
}

fn response_content_type(response: &FetchResponse) -> &str {
    response
        .json
        .get("content_type")
        .and_then(JsonValue::as_str)
        .unwrap_or("")
}

fn html_decode(source: &str) -> String {
    source
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    fn spawn_test_server(content_type: &str, body: Vec<u8>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let addr = listener.local_addr().expect("local addr");
        let content_type = content_type.to_string();

        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept client");
            let mut buffer = [0_u8; 2048];
            let _ = stream.read(&mut buffer);
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(header.as_bytes()).expect("write header");
            stream.write_all(&body).expect("write body");
            stream.flush().expect("flush response");
        });

        format!("http://{addr}/test")
    }

    fn build_test_pdf_bytes(text: &str) -> Vec<u8> {
        let content_stream = format!("BT\n/F1 18 Tf\n72 120 Td\n({text}) Tj\nET\n");
        let objects = vec![
            "1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n".to_string(),
            "2 0 obj\n<< /Type /Pages /Count 1 /Kids [3 0 R] >>\nendobj\n".to_string(),
            "3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 200] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>\nendobj\n".to_string(),
            format!(
                "4 0 obj\n<< /Length {} >>\nstream\n{}endstream\nendobj\n",
                content_stream.len(),
                content_stream
            ),
            "5 0 obj\n<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>\nendobj\n"
                .to_string(),
        ];

        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::with_capacity(objects.len() + 1);
        offsets.push(0_usize);
        for object in &objects {
            offsets.push(pdf.len());
            pdf.extend_from_slice(object.as_bytes());
        }

        let xref_offset = pdf.len();
        pdf.extend_from_slice(format!("xref\n0 {}\n", offsets.len()).as_bytes());
        pdf.extend_from_slice(b"0000000000 65535 f \n");
        for offset in offsets.iter().skip(1) {
            pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n",
                offsets.len(),
                xref_offset
            )
            .as_bytes(),
        );
        pdf
    }

    fn build_test_jpeg_with_orientation_exif(orientation: u16) -> Vec<u8> {
        let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xE1, 0x00, 0x22];
        bytes.extend_from_slice(b"Exif\0\0");
        bytes.extend_from_slice(&[
            0x49,
            0x49,
            0x2A,
            0x00,
            0x08,
            0x00,
            0x00,
            0x00,
            0x01,
            0x00,
            0x12,
            0x01,
            0x03,
            0x00,
            0x01,
            0x00,
            0x00,
            0x00,
            (orientation & 0x00FF) as u8,
            (orientation >> 8) as u8,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
        ]);
        bytes.extend_from_slice(&[0xFF, 0xD9]);
        bytes
    }

    fn assert_orientation_exif(exif: &JsonValue, expected: u16) {
        assert_eq!(
            exif.get("orientation").and_then(JsonValue::as_u64),
            Some(expected as u64)
        );
        let fields = exif
            .get("fields")
            .and_then(JsonValue::as_array)
            .expect("exif fields");
        assert!(!fields.is_empty(), "expected at least one exif field");
    }

    #[test]
    fn extract_uri_supports_direct_shape() {
        let payload = json!({ "uri": "https://example.com" });
        let url = extract_uri(&payload).expect("direct uri");
        assert_eq!(url, "https://example.com");
    }

    #[test]
    fn extract_uri_supports_nested_arguments_shape() {
        let payload = json!({
            "type": "external_tool_call",
            "tool_name": "fetch",
            "arguments": { "uri": "https://example.com/page" }
        });
        let url = extract_uri(&payload).expect("nested uri");
        assert_eq!(url, "https://example.com/page");
    }

    #[tokio::test]
    async fn fetch_handler_requires_uri() {
        let handler = FetchHandler;
        let error = handler
            .handle("call-1", &json!({ "type": "fetch_request" }))
            .await
            .expect_err("missing uri must fail");
        assert!(error.contains("non-empty uri"));
    }

    #[test]
    fn classify_target_treats_windows_path_as_local_file() {
        let target = classify_target(r"C:\temp\note.md").expect("target");
        assert!(matches!(target, FetchTarget::LocalPath(_)));
    }

    #[tokio::test]
    async fn fetch_local_markdown_file_returns_markdown_content() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("note.md");
        std::fs::write(&path, "# Local Note\n\nBody").expect("write note");

        let response = fetch_local_path(path.to_string_lossy().as_ref(), &path)
            .await
            .expect("fetch local path");

        let markdown = response
            .json
            .get("content_markdown")
            .and_then(JsonValue::as_str)
            .expect("content_markdown");
        assert!(markdown.contains("Local Note"));
        assert_eq!(
            response.json.get("fetch_kind").and_then(JsonValue::as_str),
            Some("local_file")
        );
    }

    #[tokio::test]
    async fn fetch_local_jpeg_returns_exif_metadata() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("photo.jpg");
        std::fs::write(&path, build_test_jpeg_with_orientation_exif(6)).expect("write jpeg");

        let response = fetch_local_path(path.to_string_lossy().as_ref(), &path)
            .await
            .expect("fetch local image");

        let exif = response.json.get("image_exif").expect("image_exif");
        assert_orientation_exif(exif, 6);
        match response.rich {
            Some(RichHandlerResult::Image(part)) => {
                assert_eq!(part.media_type, "image/jpeg");
                assert_orientation_exif(part.exif.as_ref().expect("rich exif"), 6);
            }
            other => panic!("expected image rich result, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fetch_remi_file_markdown_returns_markdown_content() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("remi-note.md");
        std::fs::write(&path, "# Remi File\n\nBody").expect("write note");
        let uri = RemiUri::from_local_file(&path.to_string_lossy(), "text/markdown", "device-1")
            .to_uri_string();

        let response = fetch_remi_uri(&uri, &uri).await.expect("fetch remi file");

        assert_eq!(
            response.json.get("fetch_kind").and_then(JsonValue::as_str),
            Some("remi_file")
        );
        assert_eq!(
            response
                .json
                .get("resolved_uri")
                .and_then(JsonValue::as_str),
            Some(uri.as_str())
        );
        let markdown = response
            .json
            .get("content_markdown")
            .and_then(JsonValue::as_str)
            .expect("content_markdown");
        assert!(markdown.contains("Remi File"));
    }

    #[tokio::test]
    async fn fetch_remi_remote_html_returns_markdown_content() {
        let body = b"<html><head><title>Remote Title</title></head><body><h1>Remote Body</h1></body></html>"
            .to_vec();
        let url = spawn_test_server("text/html; charset=utf-8", body);
        let uri = RemiUri::from_remote(&url, "text/html").to_uri_string();

        let response = fetch_remi_uri(&uri, &uri).await.expect("fetch remi remote");

        assert_eq!(
            response.json.get("fetch_kind").and_then(JsonValue::as_str),
            Some("web")
        );
        assert_eq!(
            response.json.get("title").and_then(JsonValue::as_str),
            Some("Remote Title")
        );
        let markdown = response
            .json
            .get("content_markdown")
            .and_then(JsonValue::as_str)
            .expect("content_markdown");
        assert!(markdown.contains("Remote Body"));
    }

    #[tokio::test]
    async fn fetch_http_pdf_returns_markdown_content() {
        let pdf = build_test_pdf_bytes("Remote PDF");
        let url = spawn_test_server("application/pdf", pdf);

        let response = fetch_http_target(&url, &url)
            .await
            .expect("fetch remote pdf");

        assert_eq!(
            response.json.get("fetch_kind").and_then(JsonValue::as_str),
            Some("web")
        );
        assert_eq!(
            response
                .json
                .get("content_type")
                .and_then(JsonValue::as_str),
            Some("application/pdf")
        );
        let markdown = response
            .json
            .get("content_markdown")
            .and_then(JsonValue::as_str)
            .expect("content_markdown");
        assert!(markdown.contains("Remote PDF"));
    }

    #[tokio::test]
    async fn fetch_remote_jpeg_returns_exif_metadata() {
        let url = spawn_test_server("image/jpeg", build_test_jpeg_with_orientation_exif(3));

        let response = fetch_http_target(&url, &url)
            .await
            .expect("fetch remote image");

        let exif = response.json.get("image_exif").expect("image_exif");
        assert_orientation_exif(exif, 3);
        match response.rich {
            Some(RichHandlerResult::Image(part)) => {
                assert_eq!(part.media_type, "image/jpeg");
                assert_orientation_exif(part.exif.as_ref().expect("rich exif"), 3);
            }
            other => panic!("expected image rich result, got {other:?}"),
        }
    }

    #[test]
    fn extract_meta_handles_multibyte_text_before_title_tag() {
        let html = "prefix<title>&#31034;&#20363;&#26631;&#39064;</title>";
        let meta = extract_meta(html);

        assert_eq!(
            meta.get("html_title").map(String::as_str),
            Some("&#31034;&#20363;&#26631;&#39064;")
        );
    }

    #[test]
    fn extract_meta_handles_multibyte_text_before_meta_tag() {
        let html = "prefix<meta property=\"og:title\" content=\"world-title\">";
        let meta = extract_meta(html);

        assert_eq!(
            meta.get("og:title").map(String::as_str),
            Some("world-title")
        );
    }
}
