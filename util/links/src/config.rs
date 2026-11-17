//! A deployment's config, `bviz-config.json` beside the page. Unknown keys
//! are errors, so a misspelt allowlist fails loudly.

use std::fmt;
use std::num::NonZeroU64;

use serde::Deserialize;
use url::Url;

use crate::{DEFAULT_MAX_BYTES, Fetchable, ListTooLarge, Origin, Refused};

const DEFAULT_MAX: NonZeroU64 = NonZeroU64::new(DEFAULT_MAX_BYTES).unwrap();

const TRUSTED: &str = "trusted";
const EMBEDDERS: &str = "embedders";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    trusted: Vec<(Origin, Trust)>,
    /// Each embedder's origin, serialized as `MessageEvent.origin` gives it.
    embedders: Vec<String>,
    max_bytes: NonZeroU64,
}

/// How far bviz trusts an origin it fetches from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Trust {
    /// Whether fetches send the browser's credentials (cookies, HTTP auth).
    pub credentials: bool,
}

impl Default for Config {
    /// A config trusting no origin and embedded by none.
    fn default() -> Self {
        Self {
            trusted: Vec::new(),
            embedders: Vec::new(),
            max_bytes: DEFAULT_MAX,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RawConfig {
    #[serde(default)]
    trusted: Vec<RawTrusted>,
    #[serde(default)]
    embedders: Vec<String>,
    max_bytes: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTrusted {
    origin: String,
    #[serde(default)]
    credentials: bool,
}

impl Config {
    pub fn parse(body: &[u8]) -> Result<Self, ConfigError> {
        if crate::too_large_for_a_list(body) {
            return Err(ConfigError::TooLarge);
        }
        let raw: RawConfig = serde_json::from_slice(body).map_err(ConfigError::Malformed)?;
        let mut trusted = Vec::with_capacity(raw.trusted.len());
        for entry in raw.trusted {
            let origin = origin(&entry.origin, TRUSTED)?;
            if trusted.iter().any(|(known, _)| *known == origin) {
                return Err(ConfigError::TrustedTwice(entry.origin));
            }
            trusted.push((
                origin,
                Trust {
                    credentials: entry.credentials,
                },
            ));
        }
        let mut embedders = Vec::with_capacity(raw.embedders.len());
        for text in &raw.embedders {
            embedders.push(origin(text, EMBEDDERS)?.to_string());
        }
        let max_bytes = match raw.max_bytes {
            None => DEFAULT_MAX,
            Some(max) => NonZeroU64::new(max).ok_or(ConfigError::NoMaxBytes)?,
        };
        Ok(Self {
            trusted,
            embedders,
            max_bytes,
        })
    }

    /// How far bviz trusts `origin`, if at all.
    pub fn trust(&self, origin: &Origin) -> Option<Trust> {
        self.trusted
            .iter()
            .find(|(trusted, _)| trusted == origin)
            .map(|&(_, trust)| trust)
    }

    /// Whether the page `origin`, as `MessageEvent.origin` serializes it, may
    /// embed bviz.
    pub fn embeds(&self, origin: &str) -> bool {
        self.embedders.iter().any(|embedder| embedder == origin)
    }

    /// The most bytes bviz fetches for one document.
    pub fn max_bytes(&self) -> u64 {
        self.max_bytes.get()
    }
}

/// The origin `text` names under `key`: a fetchable URL with nothing past its
/// host and port.
fn origin(text: &str, key: &'static str) -> Result<Origin, ConfigError> {
    let bad = |why| ConfigError::BadOrigin {
        key,
        text: text.to_owned(),
        why,
    };
    let url =
        Url::parse(text).map_err(|error| bad(NotAnOrigin::Refused(Refused::Malformed(error))))?;
    if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
        return Err(bad(NotAnOrigin::HasPath));
    }
    Fetchable::new(url)
        .map(|url| url.origin())
        .map_err(|why| bad(NotAnOrigin::Refused(why)))
}

/// Why a config cannot be used.
#[derive(Debug)]
pub enum ConfigError {
    TooLarge,
    /// Not JSON, or not a config's shape, such as a key it does not know.
    Malformed(serde_json::Error),
    BadOrigin {
        key: &'static str,
        text: String,
        why: NotAnOrigin,
    },
    TrustedTwice(String),
    /// `maxBytes` is 0.
    NoMaxBytes,
}

#[derive(Debug)]
pub enum NotAnOrigin {
    Refused(Refused),
    /// A path, query or fragment past the origin.
    HasPath,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge => write!(f, "the config is {ListTooLarge}"),
            Self::Malformed(error) => write!(f, "the config is not valid: {error}"),
            Self::BadOrigin { key, text, why } => {
                write!(f, "{text:?} in {key:?} is not an origin bviz can use: ")?;
                match why {
                    NotAnOrigin::Refused(refused) => refused.fmt(f),
                    NotAnOrigin::HasPath => f.write_str("an origin has no path, query or fragment"),
                }
            }
            Self::TrustedTwice(text) => write!(f, "{text:?} is in {TRUSTED:?} twice"),
            Self::NoMaxBytes => f.write_str("\"maxBytes\" must be more than 0"),
        }
    }
}

impl std::error::Error for ConfigError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MAX_LIST_BYTES;

    fn parse(body: &str) -> Result<Config, ConfigError> {
        Config::parse(body.as_bytes())
    }

    fn origin_of(text: &str) -> Origin {
        Fetchable::new(Url::parse(text).unwrap()).unwrap().origin()
    }

    #[test]
    fn the_default_trusts_nothing_and_embeds_nowhere() {
        let config = Config::default();
        assert_eq!(config.trust(&origin_of("https://a.example/")), None);
        assert!(!config.embeds("https://a.example"));
        assert_eq!(config.max_bytes(), DEFAULT_MAX_BYTES);
    }

    #[test]
    fn an_empty_config_is_the_default() {
        assert_eq!(parse("{}").unwrap(), Config::default());
    }

    #[test]
    fn trusted_origins_carry_whether_credentials_are_sent() {
        let config = parse(
            r#"{
                "trusted": [
                    {"origin": "https://ci.example", "credentials": true},
                    {"origin": "http://localhost:8080/"}
                ],
                "maxBytes": 1000
            }"#,
        )
        .unwrap();
        assert_eq!(
            config.trust(&origin_of("https://ci.example/runs/1.gz")),
            Some(Trust { credentials: true })
        );
        assert_eq!(
            config.trust(&origin_of("http://localhost:8080/a")),
            Some(Trust { credentials: false })
        );
        assert_eq!(config.trust(&origin_of("http://localhost:8081/a")), None);
        assert_eq!(config.trust(&origin_of("https://ci.example:8443/a")), None);
        assert_eq!(config.max_bytes(), 1000);
    }

    #[test]
    fn embedders_match_the_serialized_origin_exactly() {
        let config =
            parse(r#"{"embedders": ["https://Portal.example:443/", "https://[::1]:3000"]}"#)
                .unwrap();
        assert!(config.embeds("https://portal.example"));
        assert!(config.embeds("https://[::1]:3000"));
        assert!(!config.embeds("https://portal.example:443"));
        assert!(!config.embeds("https://portal.example/"));
        assert!(!config.embeds("http://portal.example"));
        assert!(!config.embeds("null"));
        assert!(!config.embeds(""));
    }

    #[test]
    fn an_unknown_key_at_any_level_is_an_error() {
        for body in [
            r#"{"trustd": []}"#,
            r#"{"trusted": [{"origin": "https://a.example", "credential": true}]}"#,
            r#"{"max_bytes": 1}"#,
        ] {
            assert!(
                matches!(parse(body), Err(ConfigError::Malformed(_))),
                "{body}"
            );
        }
    }

    #[test]
    fn an_unknown_key_is_named_in_the_error() {
        let error = parse(r#"{"trustd": []}"#).unwrap_err().to_string();
        assert!(error.contains("trustd"), "{error}");
    }

    #[test]
    fn an_origin_with_anything_past_its_port_is_an_error() {
        for text in [
            "https://a.example/path",
            "https://a.example/?q",
            "https://a.example/#f",
        ] {
            let body = format!(r#"{{"trusted": [{{"origin": "{text}"}}]}}"#);
            assert!(
                matches!(
                    parse(&body),
                    Err(ConfigError::BadOrigin {
                        key: TRUSTED,
                        why: NotAnOrigin::HasPath,
                        ..
                    })
                ),
                "{text}"
            );
        }
    }

    #[test]
    fn an_origin_must_pass_the_fetch_policy() {
        let error = |text: &str| {
            let body = format!(r#"{{"embedders": ["{text}"]}}"#);
            match parse(&body) {
                Err(ConfigError::BadOrigin {
                    key: EMBEDDERS,
                    why: NotAnOrigin::Refused(refused),
                    ..
                }) => refused,
                other => panic!("{text}: {other:?}"),
            }
        };
        assert_eq!(error("https://user:pw@a.example"), Refused::Credentials);
        assert_eq!(error("http://a.example"), Refused::InsecureHost);
        assert_eq!(error("ftp://a.example"), Refused::Scheme("ftp".to_owned()));
        assert_eq!(
            error("null"),
            Refused::Malformed(url::ParseError::RelativeUrlWithoutBase)
        );
        assert_eq!(
            error("a.example"),
            Refused::Malformed(url::ParseError::RelativeUrlWithoutBase)
        );
    }

    #[test]
    fn a_bad_origin_is_named_in_the_error() {
        let error = parse(r#"{"trusted": [{"origin": "https://a.example/x"}]}"#)
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            r#""https://a.example/x" in "trusted" is not an origin bviz can use: an origin has no path, query or fragment"#
        );
    }

    #[test]
    fn an_origin_trusted_twice_is_an_error() {
        let body = r#"{"trusted": [{"origin": "https://a.example"}, {"origin": "https://a.example:443/", "credentials": true}]}"#;
        assert!(
            matches!(parse(body), Err(ConfigError::TrustedTwice(text)) if text == "https://a.example:443/")
        );
    }

    #[test]
    fn max_bytes_must_be_positive() {
        assert!(matches!(
            parse(r#"{"maxBytes": 0}"#),
            Err(ConfigError::NoMaxBytes)
        ));
        assert!(matches!(
            parse(r#"{"maxBytes": -1}"#),
            Err(ConfigError::Malformed(_))
        ));
    }

    #[test]
    fn a_config_that_is_not_an_object_is_an_error() {
        for body in ["", "[]", "null", "\"trusted\"", "{"] {
            assert!(
                matches!(parse(body), Err(ConfigError::Malformed(_))),
                "{body:?}"
            );
        }
    }

    #[test]
    fn a_config_over_the_list_limit_is_not_read() {
        let mut body = b"{}".to_vec();
        body.resize(usize::try_from(MAX_LIST_BYTES).unwrap() + 1, b' ');
        assert!(matches!(Config::parse(&body), Err(ConfigError::TooLarge)));
    }
}
