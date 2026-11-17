//! A manifest: a JSON array naming documents to open together, each an object
//! `{ "kind"?: string, "url": string, "name"?: string, "size"?: integer }`.
//! Fields it does not know are ignored, so later manifests still open.

use std::fmt;

use bviz_sniff::Kind;
use serde::Deserialize;

use crate::{Fetchable, ListTooLarge, MAX_DOCUMENTS, Refused, TooManyDocuments, kind_from_token};

/// A document a manifest names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Document {
    kind: Option<Kind>,
    url: Fetchable,
    name: String,
    declared_size: Option<u64>,
}

impl Document {
    /// The kind the manifest names, if it names one.
    pub fn kind(&self) -> Option<Kind> {
        self.kind
    }

    pub fn url(&self) -> &Fetchable {
        &self.url
    }

    /// What to call the document: the manifest's name for it, fit to show,
    /// failing one the URL's [`Fetchable::name`].
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The size in bytes the manifest claims, unchecked.
    pub fn declared_size(&self) -> Option<u64> {
        self.declared_size
    }
}

#[derive(Deserialize)]
struct Entry {
    kind: Option<String>,
    url: String,
    name: Option<String>,
    size: Option<u64>,
}

/// A manifest entry that names no document bviz can open.
#[derive(Debug)]
pub struct BadEntry {
    /// The entry's position in the manifest, from 0.
    pub index: usize,
    pub why: Unopened,
}

#[derive(Debug)]
pub enum Unopened {
    Malformed(serde_json::Error),
    UnknownKind(String),
    Refused { url: String, why: Refused },
}

impl fmt::Display for BadEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "entry {} of the manifest ", self.index + 1)?;
        match &self.why {
            Unopened::Malformed(error) => write!(f, "is not a document: {error}"),
            Unopened::UnknownKind(kind) => write!(f, "names an unknown kind {kind:?}"),
            Unopened::Refused { url, why } => write!(f, "cannot be opened from {url}: {why}"),
        }
    }
}

impl std::error::Error for BadEntry {}

/// Why a manifest names no documents at all.
#[derive(Debug)]
pub enum ManifestError {
    TooLarge,
    NotJson(serde_json::Error),
    NotAnArray,
    TooMany(usize),
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge => write!(f, "the manifest is {ListTooLarge}"),
            Self::NotJson(error) => write!(f, "the manifest is not JSON: {error}"),
            Self::NotAnArray => f.write_str("the manifest is not a JSON array of documents"),
            Self::TooMany(count) => {
                write!(f, "the manifest has {count} entries, {TooManyDocuments}")
            }
        }
    }
}

impl std::error::Error for ManifestError {}

/// The documents the manifest `body` names, each entry resolved against
/// `base`, the URL the manifest was fetched from.
pub fn parse(
    body: &[u8],
    base: &Fetchable,
) -> Result<Vec<Result<Document, BadEntry>>, ManifestError> {
    if crate::too_large_for_a_list(body) {
        return Err(ManifestError::TooLarge);
    }
    let serde_json::Value::Array(entries) =
        serde_json::from_slice(body).map_err(ManifestError::NotJson)?
    else {
        return Err(ManifestError::NotAnArray);
    };
    if entries.len() > MAX_DOCUMENTS {
        return Err(ManifestError::TooMany(entries.len()));
    }
    Ok(entries
        .into_iter()
        .enumerate()
        .map(|(index, entry)| document(entry, base).map_err(|why| BadEntry { index, why }))
        .collect())
}

fn document(entry: serde_json::Value, base: &Fetchable) -> Result<Document, Unopened> {
    let entry = Entry::deserialize(entry).map_err(Unopened::Malformed)?;
    let kind = match entry.kind {
        Some(token) => Some(kind_from_token(&token).ok_or(Unopened::UnknownKind(token))?),
        None => None,
    };
    let url = Fetchable::resolve(&entry.url, base.url()).map_err(|why| Unopened::Refused {
        url: entry.url,
        why,
    })?;
    let name = entry
        .name
        .map(|name| crate::document_name(&name))
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| url.name());
    Ok(Document {
        kind,
        url,
        name,
        declared_size: entry.size,
    })
}

#[cfg(test)]
mod tests {
    use url::Url;

    use super::*;
    use crate::MAX_LIST_BYTES;

    fn base() -> Fetchable {
        Fetchable::new(Url::parse("https://cdn.example/runs/42/manifest.json").unwrap()).unwrap()
    }

    fn parsed(body: &str) -> Vec<Result<Document, BadEntry>> {
        parse(body.as_bytes(), &base()).unwrap()
    }

    fn good(entry: &Result<Document, BadEntry>) -> &Document {
        entry.as_ref().unwrap()
    }

    fn bad(entry: &Result<Document, BadEntry>) -> &Unopened {
        &entry.as_ref().unwrap_err().why
    }

    #[test]
    fn an_entry_names_its_document_relative_to_the_manifest() {
        let entries = parsed(
            r#"[{"kind": "profile", "url": "build.profile.gz", "name": "Build", "size": 1234}]"#,
        );
        let document = good(&entries[0]);
        assert_eq!(document.kind(), Some(Kind::Profile));
        assert_eq!(
            document.url().to_string(),
            "https://cdn.example/runs/42/build.profile.gz"
        );
        assert_eq!(document.name(), "Build");
        assert_eq!(document.declared_size(), Some(1234));
    }

    #[test]
    fn an_entry_needs_only_its_url() {
        let entries = parsed(r#"[{"url": "https://other.example/x/exec.log.zst"}]"#);
        let document = good(&entries[0]);
        assert_eq!(document.kind(), None);
        assert_eq!(document.name(), "exec.log.zst");
        assert_eq!(document.declared_size(), None);
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let entries = parsed(r#"[{"url": "a.bep", "sha256": "00", "kind": "bep", "later": {}}]"#);
        assert_eq!(good(&entries[0]).kind(), Some(Kind::Bep));
    }

    #[test]
    fn a_name_is_sanitized_and_falls_back_to_the_url() {
        let entries = parsed(
            r#"[
                {"url": "a.gz", "name": " \u001b[31mred\u0007 "},
                {"url": "b.gz", "name": " \n\t "},
                {"url": "c/", "name": null}
            ]"#,
        );
        assert_eq!(good(&entries[0]).name(), "[31mred");
        assert_eq!(good(&entries[1]).name(), "b.gz");
        assert_eq!(good(&entries[2]).name(), "c");
    }

    #[test]
    fn bad_entries_are_reported_beside_the_good_ones() {
        let entries = parsed(
            r#"[
                {"url": "a.gz"},
                {"kind": "profile"},
                {"url": "b.gz", "kind": "trace"},
                {"url": "http://evil.example/c"},
                "d.gz",
                {"url": "e.gz", "size": -1},
                {"url": 7},
                {"url": "f.gz", "kind": "aquery"}
            ]"#,
        );
        assert_eq!(entries.len(), 8);
        assert_eq!(good(&entries[0]).name(), "a.gz");
        assert!(matches!(bad(&entries[1]), Unopened::Malformed(_)));
        assert!(matches!(bad(&entries[2]), Unopened::UnknownKind(kind) if kind == "trace"));
        assert!(matches!(
            bad(&entries[3]),
            Unopened::Refused { url, why: Refused::InsecureHost } if url == "http://evil.example/c"
        ));
        assert!(matches!(bad(&entries[4]), Unopened::Malformed(_)));
        assert!(matches!(bad(&entries[5]), Unopened::Malformed(_)));
        assert!(matches!(bad(&entries[6]), Unopened::Malformed(_)));
        assert_eq!(good(&entries[7]).kind(), Some(Kind::Aquery));
        assert_eq!(entries[3].as_ref().unwrap_err().index, 3);
    }

    #[test]
    fn a_bad_entry_says_which_it_is_and_why() {
        let entries = parsed(r#"[{"url": "a.gz"}, {"url": "b", "kind": "trace"}]"#);
        assert_eq!(
            entries[1].as_ref().unwrap_err().to_string(),
            r#"entry 2 of the manifest names an unknown kind "trace""#
        );
    }

    #[test]
    fn an_empty_manifest_names_nothing() {
        assert!(parsed("[]").is_empty());
    }

    #[test]
    fn a_manifest_that_is_not_an_array_of_entries_names_nothing() {
        let error = |body: &[u8]| parse(body, &base()).unwrap_err();
        assert!(matches!(
            error(br#"{"url": "a"}"#),
            ManifestError::NotAnArray
        ));
        assert!(matches!(error(b"null"), ManifestError::NotAnArray));
        assert!(matches!(error(b"[{"), ManifestError::NotJson(_)));
        assert!(matches!(error(b"\xff"), ManifestError::NotJson(_)));
        assert!(matches!(error(b""), ManifestError::NotJson(_)));
    }

    #[test]
    fn a_deeply_nested_manifest_fails_without_overflowing() {
        let body = "[".repeat(100_000);
        assert!(matches!(
            parse(body.as_bytes(), &base()),
            Err(ManifestError::NotJson(_))
        ));
    }

    #[test]
    fn a_manifest_over_the_document_limit_names_nothing() {
        let entries = vec![r#"{"url": "a"}"#; MAX_DOCUMENTS + 1].join(",");
        let body = format!("[{entries}]");
        assert!(matches!(
            parse(body.as_bytes(), &base()),
            Err(ManifestError::TooMany(count)) if count == MAX_DOCUMENTS + 1
        ));
        let entries = vec![r#"{"url": "a"}"#; MAX_DOCUMENTS].join(",");
        assert_eq!(parsed(&format!("[{entries}]")).len(), MAX_DOCUMENTS);
    }

    #[test]
    fn a_manifest_over_the_list_limit_is_not_read() {
        let mut body = b"[]".to_vec();
        body.resize(usize::try_from(MAX_LIST_BYTES).unwrap() + 1, b' ');
        assert!(matches!(
            parse(&body, &base()),
            Err(ManifestError::TooLarge)
        ));
    }
}
