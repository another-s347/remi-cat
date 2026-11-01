//! Read pages fit the active model's tool-output budget before overflow handling.
use bot_runtime_core::ToolContext;
use serde_json::json;

use super::{SharedRedactor, DEFAULT_FS_READ_LENGTH};

const OUTPUT_BUDGET_KEY: &str = "__fs_read_output_budget";
const PAGE_ENVELOPE_RESERVE: usize = 512;
pub(super) const INITIAL_PREVIEW_BYTES: usize = 4 * 1024;

pub(crate) fn set_output_budget(ctx: &ToolContext, bytes: usize) {
    ctx.update(|state| {
        let metadata = state.metadata.get_or_insert_with(|| json!({}));
        if !metadata.is_object() {
            *metadata = json!({"__request_metadata": metadata.take()});
        }
        metadata[OUTPUT_BUDGET_KEY] = json!(bytes);
    });
}

pub(super) fn output_budget(ctx: &ToolContext) -> usize {
    ctx.metadata()
        .and_then(|metadata| metadata.get(OUTPUT_BUDGET_KEY).and_then(|n| n.as_u64()))
        .and_then(|bytes| usize::try_from(bytes).ok())
        .unwrap_or(usize::MAX)
}

pub(super) fn byte_page(
    bytes: &[u8],
    offset: usize,
    length: usize,
    budget: usize,
    heading: &str,
    redactor: &SharedRedactor,
) -> (String, usize) {
    let start = offset.min(bytes.len());
    let allowance = budget.saturating_sub(PAGE_ENVELOPE_RESERVE).max(1);
    let mut end = start.saturating_add(length.min(allowance)).min(bytes.len());
    // A text page should end on a UTF-8 boundary when the input is text.
    let utf8 = std::str::from_utf8(bytes).ok();
    loop {
        if let Some(text) = utf8 {
            let requested_end = end;
            while end > start && !text.is_char_boundary(end) {
                end -= 1;
            }
            // Explicit one-byte reads retain byte-offset semantics and must
            // make progress even inside a multibyte character.
            if end == start {
                end = requested_end;
            }
        }
        let body = String::from_utf8_lossy(&bytes[start..end]);
        let body = redactor.read().unwrap().redact(&body);
        let remaining = bytes.len() - end;
        let mut page = format!(
            "{heading}{body}\n[offset={start} length={} total_bytes={} remaining={remaining}]",
            end - start,
            bytes.len()
        );
        if remaining > 0 {
            page.push_str(&format!(
                "\n[{remaining} bytes remaining — search for relevant text with rg, or continue fs_read with offset={end}]"
            ));
        }
        if page.len() <= budget || end == start {
            // Extremely small budgets cannot hold even the continuation envelope;
            // the agent's existing overflow guard remains authoritative there.
            return (page, end);
        }
        end = start + (end - start) / 2;
    }
}

pub(super) fn default_length(budget: usize) -> usize {
    DEFAULT_FS_READ_LENGTH.min(budget.saturating_sub(PAGE_ENVELOPE_RESERVE).max(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::SecretRedactor;
    use std::collections::HashMap;
    use std::sync::{Arc, RwLock};

    #[test]
    fn pages_fit_budget_and_reconstruct_utf8_input() {
        let text = "a中🙂z".repeat(6000);
        let redactor = Arc::new(RwLock::new(SecretRedactor::empty()));
        let mut offset = 0;
        let mut recovered = String::new();
        while offset < text.len() {
            let (page, end) = byte_page(text.as_bytes(), offset, usize::MAX, 24000, "", &redactor);
            assert!(page.len() <= 24000);
            assert!(end > offset);
            recovered.push_str(page.split("\n[offset=").next().unwrap());
            offset = end;
        }
        assert_eq!(recovered, text);
        let (_, end) = byte_page("中".as_bytes(), 0, 1, 1024, "", &redactor);
        assert_eq!(end, 1);
    }

    #[test]
    fn invalid_utf8_and_expanding_redaction_stay_within_budget() {
        let redactor = Arc::new(RwLock::new(SecretRedactor::from_entries(&HashMap::from([
            ("LONG_CREDENTIAL_LABEL".into(), "s3cr".into()),
        ]))));
        for bytes in [vec![0xff; 10_000], "s3cr".repeat(2500).into_bytes()] {
            let (page, end) = byte_page(&bytes, 0, 32768, 2048, "", &redactor);
            assert!(page.len() <= 2048);
            assert!(end > 0 && end < bytes.len());
            assert!(!page.contains("s3cr"));
        }
    }
}
