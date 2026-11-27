//! The documents a page link, a manifest or a deployment's config names for
//! bviz to fetch, and the policy deciding which URLs it may fetch at all.
//!
//! Every input here is untrusted: no parser panics, and each returns what it
//! could not use.

pub mod config;
mod fetchable;
pub mod fragment;
mod kind;
pub mod manifest;

pub use fetchable::{Fetchable, Origin, Refused};
pub use kind::{kind_from_token, kind_token};
use std::fmt;
use unicode_properties::{GeneralCategory, UnicodeGeneralCategory};

/// The most documents one link fragment, manifest, or embedding message may
/// name, which bounds the fetches, viewers and dialog rows one untrusted
/// source can start.
pub const MAX_DOCUMENTS: usize = 64;

/// The most bytes bviz fetches for one document unless the deployment's
/// config says otherwise.
pub const DEFAULT_MAX_BYTES: u64 = 1 << 30;

/// The most bytes a manifest or config body may hold.
pub const MAX_LIST_BYTES: u64 = 1 << 20;

/// The most characters a document's name keeps.
pub const MAX_NAME_CHARS: usize = 200;

/// What bviz says of documents past [`MAX_DOCUMENTS`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TooManyDocuments;

impl fmt::Display for TooManyDocuments {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "over the limit of {MAX_DOCUMENTS} documents")
    }
}

/// What bviz says of a list body over [`MAX_LIST_BYTES`].
struct ListTooLarge;

impl fmt::Display for ListTooLarge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "larger than the limit of {MAX_LIST_BYTES} bytes")
    }
}

/// Whether `body` is over [`MAX_LIST_BYTES`].
fn too_large_for_a_list(body: &[u8]) -> bool {
    u64::try_from(body.len()).map_or(true, |len| len > MAX_LIST_BYTES)
}

/// `text`, which an untrusted source gave, fit to show: trimmed, at most
/// `max_chars` long, and without the characters that show nothing or change
/// how the text around them reads, such as controls, direction overrides and
/// isolates, zero-width characters, and line and paragraph separators.
pub fn display_text(text: &str, max_chars: usize) -> String {
    let trimmed = text.trim_matches(|c: char| c.is_whitespace() || !shows(c));
    let mut shown = String::with_capacity(trimmed.len().min(max_chars * char::MAX_LEN_UTF8));
    shown.extend(trimmed.chars().filter(|&c| shows(c)).take(max_chars));
    shown.truncate(shown.trim_end().len());
    shown
}

/// Whether `c` is a character [`display_text`] keeps.
fn shows(c: char) -> bool {
    !matches!(
        c.general_category(),
        GeneralCategory::Control
            | GeneralCategory::Format
            | GeneralCategory::LineSeparator
            | GeneralCategory::ParagraphSeparator
    )
}

/// `text` fit to name a document.
fn document_name(text: &str) -> String {
    display_text(text, MAX_NAME_CHARS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_loses_its_control_characters_and_surrounding_space() {
        assert_eq!(
            document_name(" \u{7}build\n.profile\u{1b} \t"),
            "build.profile"
        );
    }

    #[test]
    fn a_name_loses_its_invisible_and_direction_characters() {
        let text =
            "\u{200b}build\u{202e}gz.\u{2066}exe\u{2069}\u{feff}\u{2028}x\u{2029}\u{ad}y\u{200d}";
        assert_eq!(document_name(text), "buildgz.exexy");
    }

    #[test]
    fn displayed_text_keeps_what_shows() {
        let text = "café 日本 ✓ a\u{301} 🙂";
        assert_eq!(display_text(text, MAX_NAME_CHARS), text);
        assert_eq!(display_text("abc", 2), "ab");
    }

    #[test]
    fn a_long_name_is_cut_at_a_character_boundary() {
        let name = document_name(&"é".repeat(MAX_NAME_CHARS + 10));
        assert_eq!(name.chars().count(), MAX_NAME_CHARS);
        assert!(name.chars().all(|c| c == 'é'));
    }

    #[test]
    fn a_name_cut_before_a_space_ends_without_it() {
        let text = format!("{} b", "a".repeat(MAX_NAME_CHARS - 1));
        assert_eq!(document_name(&text), "a".repeat(MAX_NAME_CHARS - 1));
    }

    #[test]
    fn a_list_body_may_reach_the_list_limit_but_not_pass_it() {
        let limit = usize::try_from(MAX_LIST_BYTES).unwrap();
        assert!(!too_large_for_a_list(&vec![b' '; limit]));
        assert!(too_large_for_a_list(&vec![b' '; limit + 1]));
    }
}
