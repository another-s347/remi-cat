// =============================================================================
// Remi URI Protocol
// =============================================================================
//
// Format: remi://{location}/{path}?type={mime}&ext={ext}&device={device_id}
//
// Locations:
//   local  — app sandbox file (images/{uuid}.{ext})
//   file   — device local absolute path
//   remote — Supabase Storage URL
//   inline — data URI / base64 (reserved for future)
//
// Query parameters:
//   type   — MIME type (required), e.g. image/jpeg
//   ext    — file extension (optional), e.g. jpg
//   device — source device ID (required for local/file)

use std::fmt;

/// Location component of a Remi URI
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RemiUriLocation {
    /// App sandbox file (relative path within app documents dir)
    Local,
    /// Absolute local file path on device
    File,
    /// Remote resource (Supabase Storage public URL)
    Remote,
    /// Inline data URI (reserved for future use)
    Inline,
}

impl RemiUriLocation {
    pub fn as_str(&self) -> &str {
        match self {
            RemiUriLocation::Local => "local",
            RemiUriLocation::File => "file",
            RemiUriLocation::Remote => "remote",
            RemiUriLocation::Inline => "inline",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "local" => Some(RemiUriLocation::Local),
            "file" => Some(RemiUriLocation::File),
            "remote" => Some(RemiUriLocation::Remote),
            "inline" => Some(RemiUriLocation::Inline),
            _ => None,
        }
    }
}

impl fmt::Display for RemiUriLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A parsed Remi URI
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemiUri {
    /// Location type (local, file, remote, inline)
    pub location: RemiUriLocation,
    /// Path component (relative for local, absolute for file, URL path for remote)
    pub path: String,
    /// MIME type of the resource
    pub mime_type: String,
    /// File extension (without leading dot)
    pub extension: Option<String>,
    /// Source device ID (required for local/file resources)
    pub device_id: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum RemiUriError {
    #[error("Invalid URI scheme: expected 'remi://', got '{0}'")]
    InvalidScheme(String),
    #[error("Missing location in URI")]
    MissingLocation,
    #[error("Unknown location: '{0}'")]
    UnknownLocation(String),
    #[error("Missing path in URI")]
    MissingPath,
    #[error("Missing required 'type' query parameter")]
    MissingMimeType,
    #[error("Invalid URI format: {0}")]
    InvalidFormat(String),
}

impl RemiUri {
    /// Parse a remi:// URI string
    pub fn parse(uri: &str) -> Result<Self, RemiUriError> {
        // Strip scheme
        let rest = uri
            .strip_prefix("remi://")
            .ok_or_else(|| RemiUriError::InvalidScheme(uri.to_string()))?;

        // Split path and query
        let (path_part, query_part) = match rest.find('?') {
            Some(idx) => (&rest[..idx], Some(&rest[idx + 1..])),
            None => (rest, None),
        };

        // Parse location and path
        let (location_str, path) = match path_part.find('/') {
            Some(idx) => (&path_part[..idx], &path_part[idx + 1..]),
            None => {
                if path_part.is_empty() {
                    return Err(RemiUriError::MissingLocation);
                }
                (path_part, "")
            }
        };

        let location = RemiUriLocation::from_str(location_str)
            .ok_or_else(|| RemiUriError::UnknownLocation(location_str.to_string()))?;

        if path.is_empty() && location != RemiUriLocation::Inline {
            return Err(RemiUriError::MissingPath);
        }

        // Parse query parameters
        let mut mime_type = None;
        let mut extension = None;
        let mut device_id = None;

        if let Some(query) = query_part {
            for pair in query.split('&') {
                if let Some((key, value)) = pair.split_once('=') {
                    let value = url_decode(value);
                    match key {
                        "type" => mime_type = Some(value),
                        "ext" => extension = Some(value),
                        "device" => device_id = Some(value),
                        _ => {} // Ignore unknown params
                    }
                }
            }
        }

        let mime_type = mime_type.ok_or(RemiUriError::MissingMimeType)?;

        Ok(RemiUri {
            location,
            path: url_decode(path),
            mime_type,
            extension,
            device_id,
        })
    }

    /// Serialize to a remi:// URI string
    pub fn to_uri_string(&self) -> String {
        let mut uri = format!(
            "remi://{}/{}?type={}",
            self.location.as_str(),
            url_encode(&self.path),
            url_encode(&self.mime_type),
        );
        if let Some(ext) = &self.extension {
            uri.push_str(&format!("&ext={}", url_encode(ext)));
        }
        if let Some(device) = &self.device_id {
            uri.push_str(&format!("&device={}", url_encode(device)));
        }
        uri
    }

    /// Whether this URI points to a local resource (local or file)
    pub fn is_local(&self) -> bool {
        matches!(
            self.location,
            RemiUriLocation::Local | RemiUriLocation::File
        )
    }

    /// Whether this URI points to a remote resource
    pub fn is_remote(&self) -> bool {
        matches!(self.location, RemiUriLocation::Remote)
    }

    /// Whether this URI needs to be synced to cloud
    pub fn needs_sync(&self) -> bool {
        self.is_local()
    }

    /// Whether this URI points to an image
    pub fn is_image(&self) -> bool {
        self.mime_type.starts_with("image/")
    }

    /// Create a remote variant from this local URI
    pub fn to_remote(&self, remote_url: &str) -> Self {
        RemiUri {
            location: RemiUriLocation::Remote,
            path: remote_url.to_string(),
            mime_type: self.mime_type.clone(),
            extension: self.extension.clone(),
            device_id: None, // Remote URIs don't need device_id
        }
    }

    /// Create a URI for a file in the app sandbox (images/{filename})
    pub fn from_app_local(filename: &str, mime: &str, device_id: &str) -> Self {
        let ext = extension_from_filename(filename);
        RemiUri {
            location: RemiUriLocation::Local,
            path: format!("images/{}", filename),
            mime_type: mime.to_string(),
            extension: ext.map(|e| e.to_string()),
            device_id: Some(device_id.to_string()),
        }
    }

    /// Create a URI for a local device file (absolute path)
    pub fn from_local_file(path: &str, mime: &str, device_id: &str) -> Self {
        let ext = extension_from_path(path);
        RemiUri {
            location: RemiUriLocation::File,
            path: path.to_string(),
            mime_type: mime.to_string(),
            extension: ext.map(|e| e.to_string()),
            device_id: Some(device_id.to_string()),
        }
    }

    /// Create a URI for a remote resource (Supabase Storage public URL)
    pub fn from_remote(url: &str, mime: &str) -> Self {
        let ext = extension_from_path(url);
        RemiUri {
            location: RemiUriLocation::Remote,
            path: url.to_string(),
            mime_type: mime.to_string(),
            extension: ext.map(|e| e.to_string()),
            device_id: None,
        }
    }
}

impl fmt::Display for RemiUri {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_uri_string())
    }
}

impl serde::Serialize for RemiUri {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_uri_string())
    }
}

impl<'de> serde::Deserialize<'de> for RemiUri {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        RemiUri::parse(&s).map_err(serde::de::Error::custom)
    }
}

// =============================================================================
// MIME type utilities
// =============================================================================

/// Detect MIME type from file extension
pub fn mime_from_extension(ext: &str) -> &'static str {
    match ext.trim_start_matches('.').to_lowercase().as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "heic" | "heif" => "image/heif",
        "bmp" => "image/bmp",
        "svg" => "image/svg+xml",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        "avi" => "video/x-msvideo",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "aac" => "audio/aac",
        "pdf" => "application/pdf",
        "json" => "application/json",
        "txt" => "text/plain",
        _ => "application/octet-stream",
    }
}

/// Extract file extension from a filename (without leading dot)
fn extension_from_filename(filename: &str) -> Option<&str> {
    filename.rsplit_once('.').map(|(_, ext)| ext)
}

/// Extract file extension from a path or URL (without leading dot)
fn extension_from_path(path: &str) -> Option<&str> {
    // Strip query string for URLs
    let path = path.split('?').next().unwrap_or(path);
    // Get filename part
    let filename = path.rsplit('/').next().unwrap_or(path);
    extension_from_filename(filename)
}

/// Minimal URL encoding for query parameter values
fn url_encode(s: &str) -> String {
    let mut encoded = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            ' ' => encoded.push_str("%20"),
            '&' => encoded.push_str("%26"),
            '=' => encoded.push_str("%3D"),
            '#' => encoded.push_str("%23"),
            '?' => encoded.push_str("%3F"),
            '%' => encoded.push_str("%25"),
            '+' => encoded.push_str("%2B"),
            _ => encoded.push(ch),
        }
    }
    encoded
}

/// Basic URL decoding for query parameter values
fn url_decode(s: &str) -> String {
    let mut decoded = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(ch) = chars.next() {
        if ch == '%' {
            let hex: String = chars.by_ref().take(2).collect();
            if hex.len() == 2 {
                if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                    decoded.push(byte as char);
                    continue;
                }
            }
            decoded.push('%');
            decoded.push_str(&hex);
        } else if ch == '+' {
            decoded.push(' ');
        } else {
            decoded.push(ch);
        }
    }
    decoded
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_local_uri() {
        let uri = RemiUri::parse(
            "remi://local/images/abc123.jpg?type=image/jpeg&ext=jpg&device=pixel8-xxxx",
        )
        .unwrap();
        assert_eq!(uri.location, RemiUriLocation::Local);
        assert_eq!(uri.path, "images/abc123.jpg");
        assert_eq!(uri.mime_type, "image/jpeg");
        assert_eq!(uri.extension.as_deref(), Some("jpg"));
        assert_eq!(uri.device_id.as_deref(), Some("pixel8-xxxx"));
        assert!(uri.is_local());
        assert!(!uri.is_remote());
        assert!(uri.needs_sync());
        assert!(uri.is_image());
    }

    #[test]
    fn test_parse_remote_uri() {
        let uri =
            RemiUri::parse("remi://remote/media/user123/1707500000.jpg?type=image/jpeg").unwrap();
        assert_eq!(uri.location, RemiUriLocation::Remote);
        assert_eq!(uri.path, "media/user123/1707500000.jpg");
        assert_eq!(uri.mime_type, "image/jpeg");
        assert!(uri.is_remote());
        assert!(!uri.is_local());
        assert!(!uri.needs_sync());
    }

    #[test]
    fn test_parse_file_uri() {
        let uri = RemiUri::parse(
            "remi://file/storage/emulated/0/DCIM/photo.jpg?type=image/jpeg&device=pixel8",
        )
        .unwrap();
        assert_eq!(uri.location, RemiUriLocation::File);
        assert_eq!(uri.path, "storage/emulated/0/DCIM/photo.jpg");
        assert!(uri.is_local());
        assert!(uri.needs_sync());
    }

    #[test]
    fn test_roundtrip() {
        let original = RemiUri {
            location: RemiUriLocation::Local,
            path: "images/test.png".to_string(),
            mime_type: "image/png".to_string(),
            extension: Some("png".to_string()),
            device_id: Some("device-123".to_string()),
        };
        let uri_string = original.to_uri_string();
        let parsed = RemiUri::parse(&uri_string).unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn test_to_remote() {
        let local = RemiUri::from_app_local("test.jpg", "image/jpeg", "device-1");
        let remote = local.to_remote("https://storage.example.com/media/user/test.jpg");
        assert!(remote.is_remote());
        assert!(!remote.is_local());
        assert_eq!(
            remote.path,
            "https://storage.example.com/media/user/test.jpg"
        );
        assert_eq!(remote.mime_type, "image/jpeg");
        assert!(remote.device_id.is_none());
    }

    #[test]
    fn test_factory_methods() {
        let local = RemiUri::from_app_local("abc.jpg", "image/jpeg", "dev1");
        assert_eq!(local.location, RemiUriLocation::Local);
        assert_eq!(local.path, "images/abc.jpg");
        assert_eq!(local.extension.as_deref(), Some("jpg"));
        assert_eq!(local.device_id.as_deref(), Some("dev1"));

        let file = RemiUri::from_local_file("/sdcard/DCIM/photo.png", "image/png", "dev2");
        assert_eq!(file.location, RemiUriLocation::File);
        assert_eq!(file.path, "/sdcard/DCIM/photo.png");
        assert_eq!(file.extension.as_deref(), Some("png"));

        let remote = RemiUri::from_remote("https://example.com/media/img.webp", "image/webp");
        assert_eq!(remote.location, RemiUriLocation::Remote);
        assert!(remote.device_id.is_none());
        assert_eq!(remote.extension.as_deref(), Some("webp"));
    }

    #[test]
    fn test_mime_from_extension() {
        assert_eq!(mime_from_extension("jpg"), "image/jpeg");
        assert_eq!(mime_from_extension(".JPEG"), "image/jpeg");
        assert_eq!(mime_from_extension("png"), "image/png");
        assert_eq!(mime_from_extension("webp"), "image/webp");
        assert_eq!(mime_from_extension("gif"), "image/gif");
        assert_eq!(mime_from_extension("mp4"), "video/mp4");
        assert_eq!(mime_from_extension("unknown"), "application/octet-stream");
    }

    #[test]
    fn test_serde_roundtrip() {
        let uri = RemiUri::from_app_local("photo.jpg", "image/jpeg", "device-1");
        let json = serde_json::to_string(&uri).unwrap();
        let parsed: RemiUri = serde_json::from_str(&json).unwrap();
        assert_eq!(uri, parsed);
    }

    #[test]
    fn test_invalid_uris() {
        assert!(RemiUri::parse("http://example.com").is_err());
        assert!(RemiUri::parse("remi://unknown/path?type=image/jpeg").is_err());
        assert!(RemiUri::parse("remi://local/path").is_err()); // missing type
        assert!(RemiUri::parse("remi://").is_err());
    }

    #[test]
    fn test_url_encoding_special_chars() {
        let uri = RemiUri {
            location: RemiUriLocation::Local,
            path: "images/my photo.jpg".to_string(),
            mime_type: "image/jpeg".to_string(),
            extension: Some("jpg".to_string()),
            device_id: Some("device 1".to_string()),
        };
        let encoded = uri.to_uri_string();
        assert!(encoded.contains("my%20photo.jpg"));
        assert!(encoded.contains("device%201"));
        let parsed = RemiUri::parse(&encoded).unwrap();
        assert_eq!(parsed.path, "images/my photo.jpg");
        assert_eq!(parsed.device_id.as_deref(), Some("device 1"));
    }
}
