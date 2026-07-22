//! Grounded excerpts, shared by every surface that returns them.
//!
//! Lives in one place on purpose: the HTTP API, the IPC surface, and the MCP
//! agent tools all cite spans out of the same extracted text, and an excerpt
//! that disagreed between surfaces would make a citation unverifiable.
//!
//! Offsets are byte indices into the CAES-extracted text — never into the file
//! on disk, which may have changed since extraction.

use rsd_caes::{CaesKey, ABI_VERSION};
use rsd_catalog::Catalog;

/// Fetch the extracted text an object was indexed from, if it is still cached.
pub(crate) fn text_for(
    catalog: &Catalog,
    caes: Option<&rsd_caes::Store>,
    oid: u64,
) -> Option<String> {
    let caes = caes?;
    let record = catalog.get_object(oid).ok()??;
    let (content_hash, hints_hash) = (record.content_hash?, record.caes_hints_hash?);
    let entry = caes
        .get(&CaesKey {
            content_hash,
            extractor_id: rsd_extract::EXTRACTOR_ID.into(),
            extractor_version: rsd_extract::EXTRACTOR_VERSION,
            hints_hash,
            abi_version: ABI_VERSION,
        })
        .ok()??;
    Some(entry.text)
}

/// Byte range of the excerpt window around the first query-term occurrence.
///
/// The search runs over the original string (case-insensitively for ASCII)
/// rather than mapping offsets from a lowercased copy — lowercasing is not
/// length-preserving, so those offsets can land mid-character or invert.
pub(crate) fn span(text: &str, query: &str) -> (usize, usize) {
    let needle = query.split_whitespace().next().unwrap_or(query);
    let pos = text
        .find(needle)
        .or_else(|| {
            text.char_indices().find_map(|(start, _)| {
                let end = start.checked_add(needle.len())?;
                (end <= text.len()
                    && text.is_char_boundary(end)
                    && text[start..end].eq_ignore_ascii_case(needle))
                .then_some(start)
            })
        })
        .unwrap_or(0);
    let mut start = pos.saturating_sub(60);
    let mut end = pos.saturating_add(140).min(text.len());
    while !text.is_char_boundary(start) {
        start -= 1;
    }
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (start, end)
}

/// The excerpt itself, with runs of whitespace collapsed for display.
pub(crate) fn window(text: &str, query: &str) -> String {
    let (start, end) = span(text, query);
    text[start..end]
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn span_stays_on_character_boundaries_for_non_ascii() {
        // 'İ' lowercases to two chars, which is what broke offset mapping when
        // the search ran over a lowercased copy.
        let text = "İİİ invoice İİİ";
        let (start, end) = span(text, "invoice");
        assert!(text.is_char_boundary(start) && text.is_char_boundary(end));
        assert!(start <= end);
        assert!(window(text, "invoice").contains("invoice"));
    }

    #[test]
    fn span_is_case_insensitive_and_falls_back_to_the_head() {
        let text = "Quarterly INVOICE for Acme";
        assert!(window(text, "invoice").contains("INVOICE"));
        // A term that does not occur still yields a valid leading window.
        let (start, end) = span(text, "absent");
        assert_eq!(start, 0);
        assert!(end <= text.len());
    }
}
