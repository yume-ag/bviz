//! Serves the web build from this machine, with the headers it needs: the
//! wasm media type, cross-origin isolation, and a content security policy.

use clap::Parser;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

/// The port served on when none is asked for.
const DEFAULT_PORT: u16 = 8411;

/// The page a path naming a directory serves.
const INDEX: &str = "index.html";

#[derive(Parser)]
#[command(about = "Serve the bviz web build on this machine")]
struct Args {
    /// The site directory, as a runfiles path or an absolute one.
    site: PathBuf,
    /// The port to listen on; 0 takes any free one.
    #[arg(long, default_value_t = DEFAULT_PORT)]
    port: u16,
    /// Leave out the cross-origin isolation headers, as a static host without
    /// them does.
    #[arg(long)]
    no_isolation: bool,
}

fn main() -> ExitCode {
    let args = Args::parse();
    let Some(site) = locate(&args.site) else {
        eprintln!("error: no site at {}", args.site.display());
        return ExitCode::FAILURE;
    };
    let server = match Server::http(("127.0.0.1", args.port)) {
        Ok(server) => server,
        Err(error) => {
            eprintln!("error: cannot listen on port {}: {error}", args.port);
            return ExitCode::FAILURE;
        }
    };
    match server.server_addr().to_ip() {
        Some(address) => println!("Serving bviz at http://{address}/"),
        None => println!("Serving bviz"),
    }
    let isolated = !args.no_isolation;
    for request in server.incoming_requests() {
        respond(&site, isolated, request);
    }
    ExitCode::SUCCESS
}

/// The directory `site` names, found through the runfiles where it is relative.
fn locate(site: &Path) -> Option<PathBuf> {
    let found = if site.is_absolute() {
        site.to_path_buf()
    } else {
        let runfiles = runfiles::Runfiles::create().ok()?;
        runfiles::rlocation!(runfiles, site)?
    };
    found.is_dir().then_some(found)
}

fn respond(site: &Path, isolated: bool, request: Request) {
    let sent = match served(site, &request) {
        Ok((file, path)) => {
            let response =
                Response::from_file(file).with_header(header("Content-Type", content_type(&path)));
            request.respond(with_headers(response, isolated))
        }
        Err(status) => request.respond(with_headers(Response::empty(status), isolated)),
    };
    if let Err(error) = sent {
        eprintln!("warning: {error}");
    }
}

/// The file `request` asks for under `site`, with its path, or the status
/// that refuses it.
fn served(site: &Path, request: &Request) -> Result<(File, PathBuf), StatusCode> {
    if !matches!(request.method(), Method::Get | Method::Head) {
        return Err(StatusCode(405));
    }
    let not_found = StatusCode(404);
    let path = resolve(site, request.url()).ok_or(not_found)?;
    let file = File::open(&path).map_err(|_| not_found)?;
    if !file.metadata().is_ok_and(|meta| meta.is_file()) {
        return Err(not_found);
    }
    Ok((file, path))
}

fn with_headers<R: Read>(response: Response<R>, isolated: bool) -> Response<R> {
    headers(isolated)
        .into_iter()
        .fold(response, |response, (name, value)| {
            response.with_header(header(name, value))
        })
}

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name, value).expect("a header of printable ASCII")
}

/// The file under `site` that the request target `url` names, or `None` where
/// it names one outside it or is not UTF-8 once decoded.
fn resolve(site: &Path, url: &str) -> Option<PathBuf> {
    let path = url.split(['?', '#']).next()?.strip_prefix('/')?;
    let decoded = percent_encoding::percent_decode_str(path)
        .decode_utf8()
        .ok()?;
    let mut resolved = site.to_path_buf();
    for segment in decoded.split('/').filter(|segment| !segment.is_empty()) {
        if matches!(segment, "." | "..") || segment.contains(['\\', '\0']) {
            return None;
        }
        resolved.push(segment);
    }
    if decoded.is_empty() || decoded.ends_with('/') {
        resolved.push(INDEX);
    }
    Some(resolved)
}

/// The media type of the file at `path`, by its extension.
fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("wasm") => "application/wasm",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("html") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// The headers of every response. Cross-origin isolation is what lets the
/// page share memory between threads.
fn headers(isolated: bool) -> Vec<(&'static str, &'static str)> {
    let isolation = [
        ("Cross-Origin-Opener-Policy", "same-origin"),
        ("Cross-Origin-Embedder-Policy", "require-corp"),
    ];
    let always = [
        ("Cross-Origin-Resource-Policy", "same-origin"),
        ("X-Content-Type-Options", "nosniff"),
        ("Referrer-Policy", "no-referrer"),
        ("Cache-Control", "no-cache"),
        (
            "Content-Security-Policy",
            "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'",
        ),
    ];
    let mut headers = Vec::with_capacity(isolation.len() + always.len());
    if isolated {
        headers.extend(isolation);
    }
    headers.extend(always);
    headers
}

#[cfg(test)]
mod tests {
    use super::*;

    fn site() -> PathBuf {
        PathBuf::from("/site")
    }

    #[test]
    fn a_directory_serves_its_index() {
        assert_eq!(resolve(&site(), "/"), Some(site().join(INDEX)));
        assert_eq!(resolve(&site(), "/?a=b"), Some(site().join(INDEX)));
        assert_eq!(
            resolve(&site(), "/docs/"),
            Some(site().join("docs").join(INDEX))
        );
    }

    #[test]
    fn a_file_resolves_under_the_site() {
        assert_eq!(
            resolve(&site(), "/bviz_bg.wasm?v=1#x"),
            Some(site().join("bviz_bg.wasm"))
        );
        assert_eq!(
            resolve(&site(), "/a%20b/c.js"),
            Some(site().join("a b").join("c.js"))
        );
    }

    #[test]
    fn a_malformed_escape_is_taken_literally() {
        assert_eq!(resolve(&site(), "/%zz"), Some(site().join("%zz")));
        assert_eq!(resolve(&site(), "/a%2"), Some(site().join("a%2")));
    }

    #[test]
    fn no_target_escapes_the_site() {
        for url in [
            "/../secret",
            "/a/../../secret",
            "/%2e%2e/secret",
            "/a/%2E%2E/%2e%2E/secret",
            "/..%2fsecret",
            "/a\\..\\secret",
            "/a%5c..%5csecret",
            "/a%00",
            "/./index.html",
            "relative",
            "/%ff",
        ] {
            assert_eq!(resolve(&site(), url), None, "{url}");
        }
    }

    #[test]
    fn media_types_follow_the_extension() {
        for (name, expected) in [
            ("bviz_bg.wasm", "application/wasm"),
            ("bviz.js", "text/javascript; charset=utf-8"),
            ("index.html", "text/html; charset=utf-8"),
            ("style.css", "text/css; charset=utf-8"),
            ("icon.svg", "image/svg+xml"),
            ("NOTICE.txt", "text/plain; charset=utf-8"),
            ("data.bin", "application/octet-stream"),
            ("README", "application/octet-stream"),
        ] {
            assert_eq!(content_type(Path::new(name)), expected, "{name}");
        }
    }

    #[test]
    fn isolation_is_what_the_flag_drops() {
        let isolating = ["Cross-Origin-Opener-Policy", "Cross-Origin-Embedder-Policy"];
        let names = |isolated| -> Vec<_> {
            headers(isolated)
                .into_iter()
                .map(|(name, _)| name)
                .collect()
        };
        let isolated = names(true);
        let open = names(false);
        assert!(isolating.iter().all(|name| isolated.contains(name)));
        assert!(isolating.iter().all(|name| !open.contains(name)));
        assert_eq!(isolated.len(), open.len() + isolating.len());
    }
}
