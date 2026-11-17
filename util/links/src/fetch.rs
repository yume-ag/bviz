//! A fetch of one document, and what its response must show before bviz reads
//! any of its body.

use std::fmt;
use std::time::Duration;

use url::Url;

use crate::{Fetchable, Origin, Refused};

/// How long a fetch waits for its next bytes before it gives up.
pub const STALL: Duration = Duration::from_secs(60);

/// Whether a fetch sends the browser's credentials, such as cookies and HTTP
/// authentication.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Credentials {
    Omit,
    Include,
}

impl Credentials {
    /// Whether the fetch follows a redirect: one that sends credentials does
    /// not, so that it reaches the origin trusted with them alone.
    pub fn follows_redirects(self) -> bool {
        self == Self::Omit
    }
}

/// One document to fetch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub url: Fetchable,
    pub credentials: Credentials,
    /// The most bytes the document may hold.
    pub max_bytes: u64,
}

/// What a response tells of itself ahead of its body.
#[derive(Clone, Copy, Debug)]
pub struct Answered<'a> {
    /// The URL the response came from, after any redirect.
    pub url: &'a str,
    pub status: u16,
    /// The length of the body, as the response declares it.
    pub length: Option<u64>,
}

impl Request {
    /// The URL `answered` came from, where bviz may read its body: a URL it
    /// may fetch, on the origin it asked or one `accepts`, with a successful
    /// status and a declared length within the request's most.
    pub fn check(
        &self,
        answered: &Answered,
        accepts: impl FnOnce(&Origin) -> bool,
    ) -> Result<Fetchable, FetchError> {
        let url = Url::parse(answered.url)
            .map_err(Refused::Malformed)
            .and_then(Fetchable::new)
            .map_err(FetchError::Refused)?;
        let origin = url.origin();
        if origin != self.url.origin() && !accepts(&origin) {
            return Err(FetchError::Redirected(origin));
        }
        if !(200..300).contains(&answered.status) {
            return Err(FetchError::Status(answered.status));
        }
        match answered.length {
            Some(length) if length > self.max_bytes => Err(FetchError::TooLarge {
                declared: Some(length),
                max: self.max_bytes,
            }),
            _ => Ok(url),
        }
    }
}

/// Why a fetched document could not be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FetchError {
    /// The response came from a URL bviz does not fetch.
    Refused(Refused),
    /// A redirect led to an origin bviz may not fetch from.
    Redirected(Origin),
    Status(u16),
    /// The document holds more than `max` bytes: `declared` by its response,
    /// or, where that is `None`, found by reading past `max`.
    TooLarge {
        declared: Option<u64>,
        max: u64,
    },
    /// The server sent nothing for [`STALL`].
    Stalled,
    Cancelled,
    /// The browser failed the fetch, for the reason it gives.
    Failed(String),
    /// The document does not fit the memory bviz has.
    OutOfMemory,
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused(why) => write!(f, "the server sent the document from elsewhere: {why}"),
            Self::Redirected(origin) => write!(
                f,
                "the server redirected the request to {origin}, which bviz may not fetch from"
            ),
            Self::Status(status) => write!(f, "the server answered with HTTP status {status}"),
            Self::TooLarge { declared, max } => f.write_str(&too_large(*declared, *max, |bytes| {
                format!("{bytes} bytes")
            })),
            Self::Stalled => write!(f, "the server sent nothing for {} seconds", STALL.as_secs()),
            Self::Cancelled => f.write_str("the download was cancelled"),
            Self::Failed(why) => write!(
                f,
                "the browser could not fetch the document ({why}): the server may be \
                 unreachable, or may not let other sites read it"
            ),
            Self::OutOfMemory => f.write_str("too large to hold in the browser's memory"),
        }
    }
}

impl std::error::Error for FetchError {}

/// What bviz says of a document over the fetch limit of `max` bytes, of
/// `declared` bytes where its response declares that, each size written as
/// `size` writes it.
pub fn too_large<S: fmt::Display>(
    declared: Option<u64>,
    max: u64,
    size: impl Fn(u64) -> S,
) -> String {
    match declared {
        Some(declared) => format!(
            "the document is {}, over the fetch limit of {}",
            size(declared),
            size(max)
        ),
        None => format!(
            "the document is larger than the fetch limit of {}",
            size(max)
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REQUESTED: &str = "https://ci.example/runs/1/build.profile.gz";
    const MAX: u64 = 1000;

    fn request() -> Request {
        Request {
            url: Fetchable::new(Url::parse(REQUESTED).unwrap()).unwrap(),
            credentials: Credentials::Omit,
            max_bytes: MAX,
        }
    }

    fn answered(url: &str) -> Answered<'_> {
        Answered {
            url,
            status: 200,
            length: Some(MAX),
        }
    }

    fn accepts_nothing(_: &Origin) -> bool {
        false
    }

    #[test]
    fn a_response_from_the_url_asked_is_read() {
        let url = request().check(&answered(REQUESTED), accepts_nothing);
        assert_eq!(url.unwrap().to_string(), REQUESTED);
    }

    #[test]
    fn a_redirect_within_the_origin_asked_is_followed() {
        let moved = "https://ci.example/elsewhere.gz";
        let url = request().check(&answered(moved), accepts_nothing);
        assert_eq!(url.unwrap().to_string(), moved);
    }

    #[test]
    fn a_redirect_to_another_origin_is_followed_only_where_it_is_accepted() {
        let moved = "https://cdn.example/blob/1";
        assert_eq!(
            request().check(&answered(moved), accepts_nothing),
            Err(FetchError::Redirected(
                Fetchable::new(Url::parse(moved).unwrap()).unwrap().origin()
            ))
        );
        let accepted = request().check(&answered(moved), |origin| {
            origin.to_string() == "https://cdn.example"
        });
        assert_eq!(accepted.unwrap().to_string(), moved);
    }

    #[test]
    fn a_redirect_to_a_url_bviz_does_not_fetch_is_refused_whatever_is_accepted() {
        for (moved, why) in [
            ("http://ci.example/a", Refused::InsecureHost),
            ("https://user@ci.example/a", Refused::Credentials),
        ] {
            assert_eq!(
                request().check(&answered(moved), |_| true),
                Err(FetchError::Refused(why)),
                "{moved}"
            );
        }
        assert!(matches!(
            request().check(&answered(""), |_| true),
            Err(FetchError::Refused(Refused::Malformed(_)))
        ));
    }

    #[test]
    fn only_a_successful_status_is_read() {
        for status in [204, 206, 299] {
            let ok = Answered {
                status,
                ..answered(REQUESTED)
            };
            assert!(request().check(&ok, accepts_nothing).is_ok(), "{status}");
        }
        for status in [199, 301, 404, 500] {
            let failed = Answered {
                status,
                ..answered(REQUESTED)
            };
            assert_eq!(
                request().check(&failed, accepts_nothing),
                Err(FetchError::Status(status))
            );
        }
    }

    #[test]
    fn a_declared_length_over_the_most_is_too_large() {
        let over = Answered {
            length: Some(MAX + 1),
            ..answered(REQUESTED)
        };
        assert_eq!(
            request().check(&over, accepts_nothing),
            Err(FetchError::TooLarge {
                declared: Some(MAX + 1),
                max: MAX
            })
        );
        let undeclared = Answered {
            length: None,
            ..answered(REQUESTED)
        };
        assert!(request().check(&undeclared, accepts_nothing).is_ok());
    }

    #[test]
    fn the_origin_is_checked_before_the_status() {
        let failed = Answered {
            status: 500,
            ..answered("https://cdn.example/a")
        };
        assert!(matches!(
            request().check(&failed, accepts_nothing),
            Err(FetchError::Redirected(_))
        ));
    }

    #[test]
    fn only_a_fetch_without_credentials_follows_redirects() {
        assert!(Credentials::Omit.follows_redirects());
        assert!(!Credentials::Include.follows_redirects());
    }

    #[test]
    fn a_too_large_document_is_told_by_its_sizes() {
        let told = FetchError::TooLarge {
            declared: Some(2000),
            max: MAX,
        }
        .to_string();
        assert!(told.contains("2000") && told.contains("1000"), "{told}");
    }
}
