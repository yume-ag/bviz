//! The documents a page's URL fragment links to: `open=[<kind>:]<url>` and
//! `manifest=<url>`, among the fragment's other `&`-separated pairs.

use std::fmt;

use bviz_sniff::Kind;
use url::Url;

use crate::{Fetchable, MAX_DOCUMENTS, Refused, TooManyDocuments, kind_from_token};

const OPEN: &str = "open";
const MANIFEST: &str = "manifest";

/// A document, or a manifest of documents, a fragment links to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Link {
    /// A document, of `kind` where the link names one.
    Open {
        kind: Option<Kind>,
        url: Fetchable,
    },
    Manifest(Fetchable),
}

/// A link that cannot be followed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BadLink {
    /// The link's value, decoded.
    pub text: String,
    pub why: Unfollowed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unfollowed {
    Refused(Refused),
    NoUrl,
    /// The link came after [`MAX_DOCUMENTS`] others.
    TooMany,
}

impl fmt::Display for Unfollowed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused(refused) => refused.fmt(f),
            Self::NoUrl => f.write_str("the link names no URL"),
            Self::TooMany => TooManyDocuments.fmt(f),
        }
    }
}

/// The links a fragment holds, each in the order the fragment gives it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Links {
    pub good: Vec<Link>,
    pub bad: Vec<BadLink>,
}

/// The links in `fragment`, with or without its `#`, resolved against `page`,
/// the URL of the page it is the fragment of.
pub fn links(fragment: &str, page: &Url) -> Links {
    let pairs = bviz_switches::fragment_pairs(fragment);
    let mut links = Links {
        good: Vec::with_capacity(pairs.len().min(MAX_DOCUMENTS)),
        bad: Vec::new(),
    };
    for (key, text) in pairs {
        if key != OPEN && key != MANIFEST {
            continue;
        }
        let link = if links.good.len() == MAX_DOCUMENTS {
            Err(Unfollowed::TooMany)
        } else if key == OPEN {
            open(&text, page)
        } else {
            resolve(&text, page).map(Link::Manifest)
        };
        match link {
            Ok(link) => links.good.push(link),
            Err(why) => links.bad.push(BadLink { text, why }),
        }
    }
    links
}

/// The document `text` names: `<kind>:<url>` where the text up to its first
/// `:` is a kind's token, and otherwise a URL alone.
fn open(text: &str, page: &Url) -> Result<Link, Unfollowed> {
    let (kind, reference) = match text.split_once(':') {
        Some((token, rest)) => match kind_from_token(token) {
            Some(kind) => (Some(kind), rest),
            None => (None, text),
        },
        None => (None, text),
    };
    let url = resolve(reference, page)?;
    Ok(Link::Open { kind, url })
}

/// The URL `reference` names, which an empty reference does not: it would
/// resolve to `page` itself.
fn resolve(reference: &str, page: &Url) -> Result<Fetchable, Unfollowed> {
    if reference.trim_matches(is_stripped_from_urls).is_empty() {
        return Err(Unfollowed::NoUrl);
    }
    Fetchable::resolve(reference, page).map_err(Unfollowed::Refused)
}

/// Whether the URL parser strips `c` from either end of a URL: a
/// [C0 control or space](https://infra.spec.whatwg.org/#c0-control-or-space).
fn is_stripped_from_urls(c: char) -> bool {
    c <= ' '
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page() -> Url {
        Url::parse("https://bviz.example/app/").unwrap()
    }

    fn url(text: &str) -> Fetchable {
        Fetchable::new(Url::parse(text).unwrap()).unwrap()
    }

    fn open(kind: Option<Kind>, text: &str) -> Link {
        Link::Open {
            kind,
            url: url(text),
        }
    }

    #[test]
    fn an_open_link_without_a_kind_is_left_to_sniffing() {
        let links = links("#open=https://x.example/a", &page());
        assert_eq!(links.good, [open(None, "https://x.example/a")]);
        assert!(links.bad.is_empty());
    }

    #[test]
    fn an_open_link_may_name_its_kind_before_the_url() {
        let links = links("open=profile:https://x.example/a", &page());
        assert_eq!(
            links.good,
            [open(Some(Kind::Profile), "https://x.example/a")]
        );
    }

    #[test]
    fn a_relative_link_resolves_against_the_page() {
        let links = links("#open=bep:relative/path.json&open=../b.binpb", &page());
        assert_eq!(
            links.good,
            [
                open(
                    Some(Kind::Bep),
                    "https://bviz.example/app/relative/path.json"
                ),
                open(None, "https://bviz.example/b.binpb"),
            ]
        );
    }

    #[test]
    fn links_keep_their_order_and_other_pairs_are_ignored() {
        let links = links(
            "#BVIZ_TRACE_TIMING&open=a.gz&manifest=m.json&BVIZ_PAN_BENCH=map&open=cquery:c.pb",
            &page(),
        );
        assert_eq!(
            links.good,
            [
                open(None, "https://bviz.example/app/a.gz"),
                Link::Manifest(url("https://bviz.example/app/m.json")),
                open(Some(Kind::Cquery), "https://bviz.example/app/c.pb"),
            ]
        );
        assert!(links.bad.is_empty());
    }

    #[test]
    fn an_encoded_ampersand_or_hash_stays_inside_the_url() {
        let links = links(
            "open=https%3A%2F%2Fx.example%2Fa%3Fb%3D1%26c%3D2&open=https://x.example/d%23e",
            &page(),
        );
        assert_eq!(
            links.good,
            [
                open(None, "https://x.example/a?b=1&c=2"),
                open(None, "https://x.example/d"),
            ]
        );
    }

    #[test]
    fn a_refused_link_is_reported_with_its_text_beside_the_good_ones() {
        let links = links(
            "open=http://x.example/a&open=b.gz&manifest=javascript:alert(1)",
            &page(),
        );
        assert_eq!(links.good, [open(None, "https://bviz.example/app/b.gz")]);
        assert_eq!(
            links.bad,
            [
                BadLink {
                    text: "http://x.example/a".to_owned(),
                    why: Unfollowed::Refused(Refused::InsecureHost),
                },
                BadLink {
                    text: "javascript:alert(1)".to_owned(),
                    why: Unfollowed::Refused(Refused::Scheme("javascript".to_owned())),
                },
            ]
        );
    }

    #[test]
    fn a_kind_prefix_does_not_hide_the_scheme_it_wraps() {
        let links = links("open=profile:data:text/plain,x", &page());
        assert_eq!(
            links.bad,
            [BadLink {
                text: "profile:data:text/plain,x".to_owned(),
                why: Unfollowed::Refused(Refused::Scheme("data".to_owned())),
            }]
        );
    }

    #[test]
    fn links_past_the_document_limit_are_not_followed() {
        let fragment = (0..MAX_DOCUMENTS + 2)
            .map(|n| {
                if n % 2 == 0 {
                    format!("open={n}.gz")
                } else {
                    format!("manifest={n}.json")
                }
            })
            .collect::<Vec<_>>()
            .join("&");
        let links = links(&fragment, &page());
        assert_eq!(links.good.len(), MAX_DOCUMENTS);
        let past = MAX_DOCUMENTS;
        assert_eq!(
            links.bad,
            [
                BadLink {
                    text: format!("{past}.gz"),
                    why: Unfollowed::TooMany
                },
                BadLink {
                    text: format!("{}.json", past + 1),
                    why: Unfollowed::TooMany
                },
            ]
        );
    }

    #[test]
    fn a_link_without_a_url_is_not_followed() {
        let links = links("open=&open=profile:&manifest=&open=%20%09", &page());
        assert!(links.good.is_empty());
        let texts: Vec<_> = links.bad.iter().map(|bad| bad.text.as_str()).collect();
        assert_eq!(texts, ["", "profile:", "", " \t"]);
        assert!(links.bad.iter().all(|bad| bad.why == Unfollowed::NoUrl));
    }

    #[test]
    fn a_fragment_without_links_has_none() {
        for fragment in ["", "#", "#&&", "#BVIZ_WORKERS=2", "#=open"] {
            assert_eq!(links(fragment, &page()), Links::default(), "{fragment}");
        }
    }
}
