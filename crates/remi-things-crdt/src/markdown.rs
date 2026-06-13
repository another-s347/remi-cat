use crate::view::{ContentView, ThingView};
use crate::ThingDatatype;

#[derive(Debug, Clone, PartialEq)]
pub enum MarkdownOnlyDecoded {
    /// A normal markdown thing: concatenated markdown text.
    Markdown { text: String },

    /// A non-markdown datatype encoded as a single markdown embed block.
    Embed {
        kind: ThingDatatype,
        payload: serde_json::Value,
    },
}

/// Decode the current "markdown-only" storage convention.
///
/// - If the thing is a markdown thing, returns concatenated block text.
/// - If the thing is an embedded non-markdown datatype, returns (kind, payload).
///
/// Returns `None` if the thing has no content.
pub fn decode_markdown_only_thing(view: &ThingView) -> Option<MarkdownOnlyDecoded> {
    let content = view.content.as_ref()?;
    decode_markdown_only_content(content)
}

pub fn decode_markdown_only_content(content: &ContentView) -> Option<MarkdownOnlyDecoded> {
    if content.kind != "markdown" {
        return None;
    }

    let blocks = content.blocks.as_ref()?;
    if blocks.is_empty() {
        return Some(MarkdownOnlyDecoded::Markdown {
            text: String::new(),
        });
    }

    // Embed convention: first block has attrs = { embed_kind, payload }.
    if let Some(attrs) = blocks[0].attrs.as_ref() {
        if let Some(embed_kind) = attrs.get("embed_kind").and_then(|v| v.as_str()) {
            let payload = attrs
                .get("payload")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            return Some(MarkdownOnlyDecoded::Embed {
                kind: ThingDatatype::from_str(embed_kind),
                payload,
            });
        }
    }

    // Normal markdown: concatenate block texts.
    let mut parts: Vec<&str> = Vec::new();
    for b in blocks {
        if let Some(t) = b.text.as_deref() {
            parts.push(t);
        }
    }

    Some(MarkdownOnlyDecoded::Markdown {
        text: parts.join("\n\n"),
    })
}
