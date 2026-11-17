use std::fmt;
use std::net::Ipv4Addr;

use url::{Host, Url};

/// The origin of a [`Fetchable`] URL.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Origin(url::Origin);

impl fmt::Display for Origin {
    /// The origin as browsers serialize `location.origin`, without a default
    /// port.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.ascii_serialization())
    }
}

/// A URL bviz may fetch: over https, or over http from this machine alone, and
/// naming no credentials. It carries no fragment, which a fetch never sends.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Fetchable(Url);

impl Fetchable {
    /// `url`, if bviz may fetch it.
    pub fn new(mut url: Url) -> Result<Self, Refused> {
        match url.scheme() {
            "https" => {}
            "http" if is_loopback(url.host()) => {}
            "http" => return Err(Refused::InsecureHost),
            scheme => return Err(Refused::Scheme(scheme.to_owned())),
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(Refused::Credentials);
        }
        url.set_fragment(None);
        Ok(Self(url))
    }

    /// `reference`, absolute or relative to `base`, if bviz may fetch it.
    pub fn resolve(reference: &str, base: &Url) -> Result<Self, Refused> {
        Self::new(base.join(reference).map_err(Refused::Malformed)?)
    }

    pub fn url(&self) -> &Url {
        &self.0
    }

    pub fn origin(&self) -> Origin {
        Origin(self.0.origin())
    }

    /// What to call the document fetched from here: the last segment of the
    /// path, failing one the host.
    pub fn name(&self) -> String {
        let segment = self
            .0
            .path_segments()
            .and_then(|mut segments| segments.rfind(|segment| !segment.is_empty()));
        let name = match segment {
            Some(segment) => percent_encoding::percent_decode_str(segment).decode_utf8_lossy(),
            None => self.0.host_str().unwrap_or_default().into(),
        };
        crate::document_name(&name)
    }
}

impl fmt::Display for Fetchable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.as_str())
    }
}

/// Whether `host` is this machine, named so no resolver can point it
/// elsewhere, and so the web build's content security policy admits it,
/// which cannot name an IPv6 address.
fn is_loopback(host: Option<Host<&str>>) -> bool {
    match host {
        Some(Host::Domain(domain)) => domain == "localhost",
        Some(Host::Ipv4(address)) => address == Ipv4Addr::LOCALHOST,
        Some(Host::Ipv6(_)) | None => false,
    }
}

/// Why bviz may not fetch a URL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refused {
    Malformed(url::ParseError),
    /// A scheme other than https or http.
    Scheme(String),
    /// http to a host other than this machine.
    InsecureHost,
    /// A user name or password in the URL, which the browser's fetch rejects
    /// and which can disguise the host.
    Credentials,
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(error) => write!(f, "not a valid URL: {error}"),
            Self::Scheme(scheme) => write!(f, "bviz does not fetch {scheme}: URLs"),
            Self::InsecureHost => {
                f.write_str("bviz fetches over http only from localhost; use https")
            }
            Self::Credentials => f.write_str("the URL holds a user name or password"),
        }
    }
}

impl std::error::Error for Refused {}

#[cfg(test)]
mod tests {
    use super::*;

    fn page() -> Url {
        Url::parse("https://bviz.example/app/index.html?v=1#open=x").unwrap()
    }

    fn fetchable(text: &str) -> Result<Fetchable, Refused> {
        Fetchable::new(Url::parse(text).unwrap())
    }

    #[test]
    fn https_is_fetched_from_anywhere() {
        assert!(fetchable("https://a.example/b").is_ok());
        assert!(fetchable("https://203.0.113.9:8443/b").is_ok());
    }

    #[test]
    fn http_is_fetched_from_this_machine_alone() {
        for text in [
            "http://localhost/a",
            "http://LOCALHOST:8080/a",
            "http://127.0.0.1/a",
            "http://127.1/a",
            "http://0x7f000001/a",
        ] {
            assert!(fetchable(text).is_ok(), "{text}");
        }
    }

    #[test]
    fn http_from_a_host_that_only_looks_like_this_machine_is_refused() {
        for text in [
            "http://a.example/a",
            "http://127.0.0.1.evil.example/a",
            "http://localhost.evil/a",
            "http://localhost./a",
            "http://evil.localhost/a",
            "http://[::1]:9000/a",
            "http://[::ffff:127.0.0.1]/a",
            "http://127.0.0.2/a",
            "http://0.0.0.0/a",
        ] {
            assert_eq!(fetchable(text), Err(Refused::InsecureHost), "{text}");
        }
    }

    #[test]
    fn schemes_other_than_http_and_https_are_refused() {
        for (text, scheme) in [
            ("data:text/plain,hi", "data"),
            ("blob:https://a.example/0b2c", "blob"),
            ("file:///etc/passwd", "file"),
            ("javascript:alert(1)", "javascript"),
            ("ftp://a.example/a", "ftp"),
            ("ws://localhost/a", "ws"),
            ("wss://a.example/a", "wss"),
        ] {
            assert_eq!(
                fetchable(text),
                Err(Refused::Scheme(scheme.to_owned())),
                "{text}"
            );
        }
    }

    #[test]
    fn a_url_with_credentials_is_refused() {
        assert_eq!(
            fetchable("https://user@a.example/a"),
            Err(Refused::Credentials)
        );
        assert_eq!(
            fetchable("https://:secret@a.example/a"),
            Err(Refused::Credentials)
        );
        assert_eq!(
            fetchable("http://user:secret@localhost/a"),
            Err(Refused::Credentials)
        );
    }

    #[test]
    fn the_fragment_is_dropped() {
        let url = fetchable("https://a.example/a?b=c#d").unwrap();
        assert_eq!(url.to_string(), "https://a.example/a?b=c");
    }

    #[test]
    fn a_relative_reference_resolves_against_the_base() {
        let url = Fetchable::resolve("../data/build.profile.gz", &page()).unwrap();
        assert_eq!(
            url.to_string(),
            "https://bviz.example/data/build.profile.gz"
        );
        let url = Fetchable::resolve("/root.json", &page()).unwrap();
        assert_eq!(url.to_string(), "https://bviz.example/root.json");
        let url = Fetchable::resolve("//other.example/a", &page()).unwrap();
        assert_eq!(url.to_string(), "https://other.example/a");
    }

    #[test]
    fn an_absolute_reference_ignores_the_base_but_not_the_policy() {
        let url = Fetchable::resolve("https://other.example/a", &page()).unwrap();
        assert_eq!(url.to_string(), "https://other.example/a");
        assert_eq!(
            Fetchable::resolve("http://other.example/a", &page()),
            Err(Refused::InsecureHost)
        );
        assert_eq!(
            Fetchable::resolve("javascript:alert(1)", &page()),
            Err(Refused::Scheme("javascript".to_owned()))
        );
    }

    #[test]
    fn a_reference_that_is_no_url_is_malformed() {
        for reference in ["https://exa mple/", "http://[::1/", "https://"] {
            assert!(
                matches!(
                    Fetchable::resolve(reference, &page()),
                    Err(Refused::Malformed(_))
                ),
                "{reference}"
            );
        }
    }

    #[test]
    fn an_origin_is_serialized_as_browsers_do() {
        let origin = |text| fetchable(text).unwrap().origin().to_string();
        assert_eq!(origin("https://A.example:443/x"), "https://a.example");
        assert_eq!(origin("https://a.example:8443/x"), "https://a.example:8443");
        assert_eq!(origin("http://localhost:80/x"), "http://localhost");
        assert_eq!(origin("https://[::1]:8000/x"), "https://[::1]:8000");
    }

    #[test]
    fn urls_on_one_origin_share_it() {
        assert_eq!(
            fetchable("https://a.example/x").unwrap().origin(),
            fetchable("https://a.example:443/y?z").unwrap().origin()
        );
        assert_ne!(
            fetchable("https://a.example/x").unwrap().origin(),
            fetchable("https://a.example:8443/x").unwrap().origin()
        );
    }

    #[test]
    fn a_document_is_named_by_its_last_path_segment_decoded() {
        let name = |text| fetchable(text).unwrap().name();
        assert_eq!(
            name("https://a.example/b/build%20one.profile.gz?x=y"),
            "build one.profile.gz"
        );
        assert_eq!(name("https://a.example/b/c/"), "c");
        assert_eq!(name("https://a.example/%FF.bep"), "\u{fffd}.bep");
        assert_eq!(name("https://a.example/a%0Ab%07"), "ab");
    }

    #[test]
    fn a_document_without_a_path_is_named_by_its_host() {
        assert_eq!(fetchable("https://a.example/").unwrap().name(), "a.example");
        assert_eq!(fetchable("https://[::1]:8000").unwrap().name(), "[::1]");
    }
}
