use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use uuid::Uuid;

// ============================================================================
// CRDT Document Type
// ============================================================================

/// Identifies the type of a CRDT document in the multi-document architecture.
///
/// - `Root`: Single document per user, stores the set of all Collection UUIDs
/// - `Collection`: One document per collection, stores collection metadata + thing metadata
/// - `ThingMarkdown`: One document per thing (lazily created), stores markdown content blocks
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CrdtDataType {
    Root,
    Collection,
    ThingMarkdown,
}

/// Well-known UUID for the root document (one per user)
pub const ROOT_DOC_UUID: &str = "00000000-0000-0000-0000-000000000000";

impl CrdtDataType {
    pub fn as_str(&self) -> &'static str {
        match self {
            CrdtDataType::Root => "root",
            CrdtDataType::Collection => "collection",
            CrdtDataType::ThingMarkdown => "thing_markdown",
        }
    }

    /// Sync priority: lower = sync first. Root (0) → Collection (1) → ThingMarkdown (2)
    pub fn sync_priority(&self) -> u8 {
        match self {
            CrdtDataType::Root => 0,
            CrdtDataType::Collection => 1,
            CrdtDataType::ThingMarkdown => 2,
        }
    }
}

impl fmt::Display for CrdtDataType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for CrdtDataType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "root" => Ok(CrdtDataType::Root),
            "collection" => Ok(CrdtDataType::Collection),
            "thing_markdown" => Ok(CrdtDataType::ThingMarkdown),
            _ => Err(format!("Unknown CRDT data type: {}", s)),
        }
    }
}

// ============================================================================
// Location Field (for Thing built-in fields)
// ============================================================================

/// Location field stored in CRDT Thing metadata.
/// Supports both precise coordinates and fuzzy place names.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LocationField {
    /// Precise coordinate with optional source name
    #[serde(rename = "coordinate")]
    Coordinate {
        lat: f64,
        lng: f64,
        /// Coordinate system: "wgs84" (default) or "gcj02"
        #[serde(default = "default_coord_system")]
        coord_system: String,
        /// Original place name (if obtained from geocoding)
        #[serde(skip_serializing_if = "Option::is_none")]
        source_name: Option<String>,
    },
    /// Fuzzy location by name (requires nearby search to resolve)
    #[serde(rename = "fuzzy")]
    Fuzzy {
        name: String,
        /// Google Places API type (e.g., "restaurant", "cafe")
        place_type: String,
    },
}

fn default_coord_system() -> String {
    "wgs84".to_string()
}

impl LocationField {
    /// Create a WGS-84 coordinate
    pub fn coordinate(lat: f64, lng: f64) -> Self {
        LocationField::Coordinate {
            lat,
            lng,
            coord_system: "wgs84".to_string(),
            source_name: None,
        }
    }

    /// Create a coordinate with source name
    pub fn coordinate_with_name(lat: f64, lng: f64, name: impl Into<String>) -> Self {
        LocationField::Coordinate {
            lat,
            lng,
            coord_system: "wgs84".to_string(),
            source_name: Some(name.into()),
        }
    }

    /// Create a coordinate with specified coordinate system
    pub fn coordinate_with_system(lat: f64, lng: f64, coord_system: &str) -> Self {
        LocationField::Coordinate {
            lat,
            lng,
            coord_system: coord_system.to_string(),
            source_name: None,
        }
    }

    /// Create a fuzzy location
    pub fn fuzzy(name: impl Into<String>, place_type: impl Into<String>) -> Self {
        LocationField::Fuzzy {
            name: name.into(),
            place_type: place_type.into(),
        }
    }
}

// ============================================================================
// Date Field (for Thing built-in fields)
// ============================================================================

/// Date/datetime field stored in CRDT Thing metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DateField {
    /// Timestamp in milliseconds since Unix epoch
    pub timestamp_ms: i64,
    /// Whether this includes time (true) or is date-only (false)
    pub has_time: bool,
    /// Optional timezone identifier (e.g., "Asia/Shanghai")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
}

impl DateField {
    /// Create a date-only field
    pub fn date_only(timestamp_ms: i64) -> Self {
        DateField {
            timestamp_ms,
            has_time: false,
            timezone: None,
        }
    }

    /// Create a datetime field
    pub fn datetime(timestamp_ms: i64) -> Self {
        DateField {
            timestamp_ms,
            has_time: true,
            timezone: None,
        }
    }

    /// Create a datetime field with timezone
    pub fn datetime_with_tz(timestamp_ms: i64, timezone: impl Into<String>) -> Self {
        DateField {
            timestamp_ms,
            has_time: true,
            timezone: Some(timezone.into()),
        }
    }
}

// ============================================================================
// Image Field (for Thing built-in fields)
// ============================================================================

/// Image field stored in CRDT Thing metadata.
/// Stores a Remi URI pointing to the image resource, along with optional
/// caption and image metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageField {
    /// Remi URI pointing to the image resource
    pub uri: String,
    /// Optional caption / description
    #[serde(skip_serializing_if = "Option::is_none")]
    pub caption: Option<String>,
    /// Image width in pixels
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    /// Image height in pixels
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// File size in bytes
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    /// Source device ID (for local-only images)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
}

impl ImageField {
    /// Create an image field with just a URI
    pub fn new(uri: impl Into<String>) -> Self {
        ImageField {
            uri: uri.into(),
            caption: None,
            width: None,
            height: None,
            size_bytes: None,
            device_id: None,
        }
    }

    /// Create an image field with a URI and caption
    pub fn with_caption(uri: impl Into<String>, caption: impl Into<String>) -> Self {
        ImageField {
            uri: uri.into(),
            caption: Some(caption.into()),
            width: None,
            height: None,
            size_bytes: None,
            device_id: None,
        }
    }

    /// Create an image field with full metadata
    pub fn with_metadata(
        uri: impl Into<String>,
        caption: Option<String>,
        width: u32,
        height: u32,
        size_bytes: u64,
        device_id: Option<String>,
    ) -> Self {
        ImageField {
            uri: uri.into(),
            caption,
            width: Some(width),
            height: Some(height),
            size_bytes: Some(size_bytes),
            device_id,
        }
    }
}

// ============================================================================
// URL Field (for Thing built-in fields)
// ============================================================================

/// URL field stored in CRDT Thing metadata.
/// Stores the original URL and resolved metadata (title, description, preview image, etc.).
/// The `resolved` flag indicates whether metadata has been fetched from the server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UrlField {
    /// Original URL (https://...)
    pub url: String,
    /// Page title (from og:title or <title>)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Page description (from og:description or <meta name="description">)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Preview image URL (from og:image)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_url: Option<String>,
    /// Favicon URL
    #[serde(skip_serializing_if = "Option::is_none")]
    pub favicon_url: Option<String>,
    /// Site name (from og:site_name)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub site_name: Option<String>,
    /// Whether metadata has been resolved from the server
    #[serde(default)]
    pub resolved: bool,
}

impl UrlField {
    /// Create an unresolved URL field (metadata pending)
    pub fn new(url: impl Into<String>) -> Self {
        UrlField {
            url: url.into(),
            title: None,
            description: None,
            image_url: None,
            favicon_url: None,
            site_name: None,
            resolved: false,
        }
    }

    /// Create a resolved URL field with metadata
    pub fn with_metadata(
        url: impl Into<String>,
        title: Option<String>,
        description: Option<String>,
        image_url: Option<String>,
        favicon_url: Option<String>,
        site_name: Option<String>,
    ) -> Self {
        UrlField {
            url: url.into(),
            title,
            description,
            image_url,
            favicon_url,
            site_name,
            resolved: true,
        }
    }

    /// Extract the domain from the URL for display (e.g., "github.com")
    pub fn domain(&self) -> Option<String> {
        // Simple extraction without url crate dependency
        let s = self
            .url
            .strip_prefix("https://")
            .or_else(|| self.url.strip_prefix("http://"))?;
        Some(
            s.split('/')
                .next()?
                .split('?')
                .next()?
                .split('#')
                .next()?
                .to_string(),
        )
    }
}

// ============================================================================
// Content Entry (multi-value fields with ordering)
// ============================================================================

/// Generate a new UUID v4
pub fn generate_uuid() -> String {
    Uuid::new_v4().to_string()
}

/// Type of content stored in a ContentEntry
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentEntryKind {
    /// Location content (coordinate or fuzzy)
    Location,
    /// Markdown document (stored in separate ThingMarkdown document)
    Markdown,
    /// JSON object content with separate data/schema documents
    JsonObject,
    /// Date/datetime content
    Date,
    /// Image content (URI + caption + metadata)
    Image,
    /// URL / web link content (URL + resolved metadata)
    Url,
    /// Custom content type
    Custom(String),
}

impl ContentEntryKind {
    pub fn as_str(&self) -> &str {
        match self {
            ContentEntryKind::Location => "location",
            ContentEntryKind::Markdown => "markdown",
            ContentEntryKind::JsonObject => "json_object",
            ContentEntryKind::Date => "date",
            ContentEntryKind::Image => "image",
            ContentEntryKind::Url => "url",
            ContentEntryKind::Custom(s) => s.as_str(),
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "location" => ContentEntryKind::Location,
            "markdown" => ContentEntryKind::Markdown,
            "json_object" => ContentEntryKind::JsonObject,
            "date" => ContentEntryKind::Date,
            "image" => ContentEntryKind::Image,
            "url" => ContentEntryKind::Url,
            other => ContentEntryKind::Custom(other.to_string()),
        }
    }
}

/// JSON object entry stored as separate content documents.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonObjectField {
    /// UUID of the opaque content document that stores the object data.
    pub data_doc_uuid: String,
    /// Optional UUID of the opaque content document that stores the JSON Schema.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema_doc_uuid: Option<String>,
}

impl JsonObjectField {
    pub fn new(data_doc_uuid: impl Into<String>) -> Self {
        Self {
            data_doc_uuid: data_doc_uuid.into(),
            schema_doc_uuid: None,
        }
    }

    pub fn with_schema_doc_uuid(mut self, schema_doc_uuid: impl Into<String>) -> Self {
        self.schema_doc_uuid = Some(schema_doc_uuid.into());
        self
    }
}

/// Payload for a content entry
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentEntryPayload {
    /// Location content
    Location(LocationField),
    /// Markdown document reference
    Markdown {
        /// UUID of the ThingMarkdown document
        doc_uuid: String,
    },
    /// JSON object content with separate data/schema documents
    JsonObject(JsonObjectField),
    /// Date content
    Date(DateField),
    /// Image content (URI + caption + metadata)
    Image(ImageField),
    /// URL / web link content (URL + resolved metadata)
    Url(UrlField),
    /// Custom content with an explicit external type name and arbitrary JSON payload.
    Custom {
        content_type: String,
        data: serde_json::Value,
    },
}

impl ContentEntryPayload {
    pub fn kind(&self) -> ContentEntryKind {
        match self {
            ContentEntryPayload::Location(_) => ContentEntryKind::Location,
            ContentEntryPayload::Markdown { .. } => ContentEntryKind::Markdown,
            ContentEntryPayload::JsonObject(_) => ContentEntryKind::JsonObject,
            ContentEntryPayload::Date(_) => ContentEntryKind::Date,
            ContentEntryPayload::Image(_) => ContentEntryKind::Image,
            ContentEntryPayload::Url(_) => ContentEntryKind::Url,
            ContentEntryPayload::Custom { content_type, .. } => {
                ContentEntryKind::Custom(content_type.clone())
            }
        }
    }
}

/// A content entry within a Thing.
/// Multiple entries can be added with different types, each with optional title and stable ordering.
/// Each entry has a unique UUID for reference.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContentEntry {
    /// Unique identifier for this entry (UUID v4)
    pub id: String,
    /// Optional title/label for this entry
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Order value for sorting (fractional index for insertion between entries)
    pub order: f64,
    /// The actual content
    pub payload: ContentEntryPayload,
}

impl ContentEntry {
    /// Create a new content entry with auto-generated UUID
    pub fn new(payload: ContentEntryPayload, order: f64) -> Self {
        Self {
            id: generate_uuid(),
            title: None,
            order,
            payload,
        }
    }

    /// Create a new content entry with specified ID
    pub fn with_id(id: impl Into<String>, payload: ContentEntryPayload, order: f64) -> Self {
        Self {
            id: id.into(),
            title: None,
            order,
            payload,
        }
    }

    /// Create a new location content entry
    pub fn location(location: LocationField, order: f64) -> Self {
        Self::new(ContentEntryPayload::Location(location), order)
    }

    /// Create a new location content entry with specified ID
    pub fn location_with_id(id: impl Into<String>, location: LocationField, order: f64) -> Self {
        Self::with_id(id, ContentEntryPayload::Location(location), order)
    }

    /// Create a new location content entry with title
    pub fn location_with_title(
        title: impl Into<String>,
        location: LocationField,
        order: f64,
    ) -> Self {
        Self {
            id: generate_uuid(),
            title: Some(title.into()),
            order,
            payload: ContentEntryPayload::Location(location),
        }
    }

    /// Create a new markdown content entry
    pub fn markdown(doc_uuid: impl Into<String>, order: f64) -> Self {
        Self::new(
            ContentEntryPayload::Markdown {
                doc_uuid: doc_uuid.into(),
            },
            order,
        )
    }

    /// Create a new markdown content entry with specified ID
    pub fn markdown_with_id(
        id: impl Into<String>,
        doc_uuid: impl Into<String>,
        order: f64,
    ) -> Self {
        Self::with_id(
            id,
            ContentEntryPayload::Markdown {
                doc_uuid: doc_uuid.into(),
            },
            order,
        )
    }

    /// Create a new markdown content entry with title
    pub fn markdown_with_title(
        title: impl Into<String>,
        doc_uuid: impl Into<String>,
        order: f64,
    ) -> Self {
        Self {
            id: generate_uuid(),
            title: Some(title.into()),
            order,
            payload: ContentEntryPayload::Markdown {
                doc_uuid: doc_uuid.into(),
            },
        }
    }

    /// Create a new JSON object content entry.
    pub fn json_object(data_doc_uuid: impl Into<String>, order: f64) -> Self {
        Self::new(
            ContentEntryPayload::JsonObject(JsonObjectField::new(data_doc_uuid)),
            order,
        )
    }

    /// Create a new JSON object content entry with specified ID.
    pub fn json_object_with_id(
        id: impl Into<String>,
        data_doc_uuid: impl Into<String>,
        order: f64,
    ) -> Self {
        Self::with_id(
            id,
            ContentEntryPayload::JsonObject(JsonObjectField::new(data_doc_uuid)),
            order,
        )
    }

    /// Create a new JSON object content entry with title.
    pub fn json_object_with_title(
        title: impl Into<String>,
        data_doc_uuid: impl Into<String>,
        order: f64,
    ) -> Self {
        Self {
            id: generate_uuid(),
            title: Some(title.into()),
            order,
            payload: ContentEntryPayload::JsonObject(JsonObjectField::new(data_doc_uuid)),
        }
    }

    /// Create a new date content entry
    pub fn date(date: DateField, order: f64) -> Self {
        Self::new(ContentEntryPayload::Date(date), order)
    }

    /// Create a new date content entry with specified ID
    pub fn date_with_id(id: impl Into<String>, date: DateField, order: f64) -> Self {
        Self::with_id(id, ContentEntryPayload::Date(date), order)
    }

    /// Create a new date content entry with title
    pub fn date_with_title(title: impl Into<String>, date: DateField, order: f64) -> Self {
        Self {
            id: generate_uuid(),
            title: Some(title.into()),
            order,
            payload: ContentEntryPayload::Date(date),
        }
    }

    /// Create a new image content entry
    pub fn image(image: ImageField, order: f64) -> Self {
        Self::new(ContentEntryPayload::Image(image), order)
    }

    /// Create a new image content entry with specified ID
    pub fn image_with_id(id: impl Into<String>, image: ImageField, order: f64) -> Self {
        Self::with_id(id, ContentEntryPayload::Image(image), order)
    }

    /// Create a new image content entry with title
    pub fn image_with_title(title: impl Into<String>, image: ImageField, order: f64) -> Self {
        Self {
            id: generate_uuid(),
            title: Some(title.into()),
            order,
            payload: ContentEntryPayload::Image(image),
        }
    }

    /// Create a simple image entry from URI string
    pub fn image_from_uri(uri: impl Into<String>, order: f64) -> Self {
        Self::new(ContentEntryPayload::Image(ImageField::new(uri)), order)
    }

    /// Create a new URL content entry (unresolved)
    pub fn url(url: impl Into<String>, order: f64) -> Self {
        Self::new(ContentEntryPayload::Url(UrlField::new(url)), order)
    }

    /// Create a new URL content entry with specified ID
    pub fn url_with_id(id: impl Into<String>, url: impl Into<String>, order: f64) -> Self {
        Self::with_id(id, ContentEntryPayload::Url(UrlField::new(url)), order)
    }

    /// Create a URL content entry with title
    pub fn url_with_title(title: impl Into<String>, url: impl Into<String>, order: f64) -> Self {
        Self {
            id: generate_uuid(),
            title: Some(title.into()),
            order,
            payload: ContentEntryPayload::Url(UrlField::new(url)),
        }
    }

    /// Create a resolved URL content entry with full metadata
    pub fn url_resolved(url_field: UrlField, order: f64) -> Self {
        Self::new(ContentEntryPayload::Url(url_field), order)
    }

    /// Get the kind of this content entry
    pub fn kind(&self) -> ContentEntryKind {
        self.payload.kind()
    }

    /// Set the title
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }
}

/// Update operation for a single content entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentEntryUpdate {
    /// Entry ID to update
    pub id: String,
    /// Update title (Some(Some(x)) = set, Some(None) = clear, None = no change)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<Option<String>>,
    /// Update order (Some(x) = set, None = no change)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order: Option<f64>,
    /// Update payload (Some(x) = set, None = no change)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<ContentEntryPayload>,
}

// ============================================================================
// Thing Built-in Fields (V3 multi-value)
// ============================================================================

/// Built-in fields for a Thing, stored in the Collection document.
/// V3 supports multiple content entries with ordering.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ThingBuiltInFields {
    /// Content entries (location, markdown, date, etc.) - sorted by order
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub content_entries: Vec<ContentEntry>,

    /// Extra key-value fields for future extensibility
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra: Option<serde_json::Value>,
}

impl ThingBuiltInFields {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a content entry
    pub fn with_entry(mut self, entry: ContentEntry) -> Self {
        self.content_entries.push(entry);
        self.sort_entries();
        self
    }

    /// Sort entries by order
    pub fn sort_entries(&mut self) {
        self.content_entries.sort_by(|a, b| {
            a.order
                .partial_cmp(&b.order)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }

    /// Get entries by kind
    pub fn entries_by_kind(&self, kind: &ContentEntryKind) -> Vec<&ContentEntry> {
        self.content_entries
            .iter()
            .filter(|e| &e.kind() == kind)
            .collect()
    }

    /// Get the first location entry (for backward compatibility)
    pub fn first_location(&self) -> Option<&LocationField> {
        self.content_entries.iter().find_map(|e| {
            if let ContentEntryPayload::Location(loc) = &e.payload {
                Some(loc)
            } else {
                None
            }
        })
    }

    /// Get the first date entry (for backward compatibility)
    pub fn first_date(&self) -> Option<&DateField> {
        self.content_entries.iter().find_map(|e| {
            if let ContentEntryPayload::Date(date) = &e.payload {
                Some(date)
            } else {
                None
            }
        })
    }

    /// Get the first markdown doc UUID (for backward compatibility)
    pub fn first_markdown_doc_uuid(&self) -> Option<&str> {
        self.content_entries.iter().find_map(|e| {
            if let ContentEntryPayload::Markdown { doc_uuid } = &e.payload {
                Some(doc_uuid.as_str())
            } else {
                None
            }
        })
    }

    /// Get the first image entry
    pub fn first_image(&self) -> Option<&ImageField> {
        self.content_entries.iter().find_map(|e| {
            if let ContentEntryPayload::Image(img) = &e.payload {
                Some(img)
            } else {
                None
            }
        })
    }

    /// Get all image entries
    pub fn images(&self) -> Vec<&ImageField> {
        self.content_entries
            .iter()
            .filter_map(|e| {
                if let ContentEntryPayload::Image(img) = &e.payload {
                    Some(img)
                } else {
                    None
                }
            })
            .collect()
    }

    /// Get the first URL entry
    pub fn first_url(&self) -> Option<&UrlField> {
        self.content_entries.iter().find_map(|e| {
            if let ContentEntryPayload::Url(url) = &e.payload {
                Some(url)
            } else {
                None
            }
        })
    }

    /// Get all URL entries
    pub fn urls(&self) -> Vec<&UrlField> {
        self.content_entries
            .iter()
            .filter_map(|e| {
                if let ContentEntryPayload::Url(url) = &e.payload {
                    Some(url)
                } else {
                    None
                }
            })
            .collect()
    }

    /// Get all unresolved URL entries (for async resolution)
    pub fn unresolved_urls(&self) -> Vec<(&str, &UrlField)> {
        self.content_entries
            .iter()
            .filter_map(|e| {
                if let ContentEntryPayload::Url(url) = &e.payload {
                    if !url.resolved {
                        Some((e.id.as_str(), url))
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect()
    }

    /// Calculate the next order value (for appending)
    pub fn next_order(&self) -> f64 {
        self.content_entries
            .last()
            .map(|e| e.order + 1.0)
            .unwrap_or(0.0)
    }

    /// Calculate order between two entries (for insertion)
    pub fn order_between(before: Option<f64>, after: Option<f64>) -> f64 {
        match (before, after) {
            (Some(b), Some(a)) => (b + a) / 2.0,
            (Some(b), None) => b + 1.0,
            (None, Some(a)) => a - 1.0,
            (None, None) => 0.0,
        }
    }
}

/// Update payload for ThingBuiltInFields
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ThingBuiltInFieldsUpdate {
    /// Add new content entries
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub add_entries: Vec<ContentEntry>,

    /// Update existing content entries
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub update_entries: Vec<ContentEntryUpdate>,

    /// Delete content entries by ID
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub delete_entry_ids: Vec<String>,

    /// Extra fields update (replaces entire extra map if Some)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra: Option<Option<serde_json::Value>>,
}

// ============================================================================
// Thing Datatype (existing, unchanged)
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ThingDatatype {
    Markdown,
    Text,
    Location,
    Image,
    Todo,
    Custom(String),
}

impl ThingDatatype {
    pub fn as_str(&self) -> &str {
        match self {
            ThingDatatype::Markdown => "markdown",
            ThingDatatype::Text => "text",
            ThingDatatype::Location => "location",
            ThingDatatype::Image => "image",
            ThingDatatype::Todo => "todo",
            ThingDatatype::Custom(s) => s.as_str(),
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s.trim() {
            "" => ThingDatatype::Markdown,
            "markdown" => ThingDatatype::Markdown,
            "text" => ThingDatatype::Text,
            "location" => ThingDatatype::Location,
            "image" => ThingDatatype::Image,
            "todo" => ThingDatatype::Todo,
            other => ThingDatatype::Custom(other.to_string()),
        }
    }

    pub fn is_markdownish(&self) -> bool {
        matches!(self, ThingDatatype::Markdown | ThingDatatype::Text)
    }
}

impl fmt::Display for ThingDatatype {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for ThingDatatype {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ThingDatatype {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(ThingDatatype::from_str(&s))
    }
}

pub fn deserialize_datatype_opt<'de, D>(deserializer: D) -> Result<Option<ThingDatatype>, D::Error>
where
    D: Deserializer<'de>,
{
    let opt = Option::<String>::deserialize(deserializer)?;
    Ok(opt.map(|s| ThingDatatype::from_str(&s)))
}

pub fn serialize_datatype_opt<S>(
    value: &Option<ThingDatatype>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    match value {
        Some(v) => serializer.serialize_some(v.as_str()),
        None => serializer.serialize_none(),
    }
}

pub fn deserialize_datatype_default_markdown<'de, D>(
    deserializer: D,
) -> Result<ThingDatatype, D::Error>
where
    D: Deserializer<'de>,
{
    let opt = Option::<String>::deserialize(deserializer)?;
    Ok(opt
        .as_deref()
        .map(ThingDatatype::from_str)
        .unwrap_or(ThingDatatype::Markdown))
}

pub fn is_default_markdown(dt: &ThingDatatype) -> bool {
    matches!(dt, ThingDatatype::Markdown)
}

pub fn is_none_or_default_markdown(dt: &Option<ThingDatatype>) -> bool {
    dt.as_ref().map(is_default_markdown).unwrap_or(true)
}
