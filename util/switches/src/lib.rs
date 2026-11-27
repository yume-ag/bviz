//! The `BVIZ_*` switches that turn on bviz's measurements, and the reports
//! those measurements make. On the desktop a switch is an environment
//! variable and a report goes to stderr; in a browser, which has neither, a
//! switch is named in the page's URL fragment and a report goes to the
//! console.

use std::fmt;

/// The value of the switch `name`, while it is set.
#[cfg(not(target_family = "wasm"))]
pub fn get(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// The value of the switch `name`, while the fragment set it.
#[cfg(target_family = "wasm")]
pub fn get(name: &str) -> Option<String> {
    let switches = fragment::SWITCHES.get()?;
    let (_, value) = switches.iter().find(|(set, _)| set == name)?;
    Some(value.clone())
}

/// Whether the switch `name` is set, to any value.
pub fn is_set(name: &str) -> bool {
    get(name).is_some()
}

/// Reports what a switch measured.
#[cfg(not(target_family = "wasm"))]
pub fn report(line: fmt::Arguments<'_>) {
    eprintln!("{line}");
}

/// Reports what a switch measured.
#[cfg(target_family = "wasm")]
pub fn report(line: fmt::Arguments<'_>) {
    web_sys::console::log_1(&line.to_string().into());
}

/// Runs `stage`, and while the switch `switch` is set reports how long it
/// took under `label`.
pub fn timed<T>(switch: &str, label: &str, stage: impl FnOnce() -> T) -> T {
    let Some(start) = timing_start(switch) else {
        return stage();
    };
    let out = stage();
    report_timing(label, start);
    out
}

/// Now, while the switch `switch` is set: the start of a stage
/// [`report_timing`] reports.
pub fn timing_start(switch: &str) -> Option<bviz_time::Instant> {
    is_set(switch).then(bviz_time::now)
}

/// Reports the stage `label`, which ran from `start` until now, and the thread
/// it ended on.
pub fn report_timing(label: &str, start: bviz_time::Instant) {
    report(format_args!(
        "[timing] {label}: {:.3}s on {}",
        start.elapsed().as_secs_f64(),
        ThreadName(std::thread::current()),
    ));
}

/// A thread as a report names it: by its name, or failing one by its id.
struct ThreadName(std::thread::Thread);

impl fmt::Display for ThreadName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0.name() {
            Some(name) => f.write_str(name),
            None => write!(f, "{:?}", self.0.id()),
        }
    }
}

pub use fragment::fragment_pairs;
#[cfg(target_family = "wasm")]
pub use fragment::set_from_fragment;

mod fragment {
    /// The switches the page's fragment set, once read.
    #[cfg(target_family = "wasm")]
    pub(crate) static SWITCHES: std::sync::OnceLock<Vec<(String, String)>> =
        std::sync::OnceLock::new();

    /// Sets the switches `fragment` names, a URL fragment with or without its
    /// `#`; a later call changes nothing.
    #[cfg(target_family = "wasm")]
    pub fn set_from_fragment(fragment: &str) {
        let _ = SWITCHES.set(fragment_pairs(fragment));
    }

    /// The names and values a URL fragment, with or without its `#`, sets:
    /// `&`-separated, each a percent-encoded name with an optional `=value`;
    /// a name without one is set to empty.
    pub fn fragment_pairs(fragment: &str) -> Vec<(String, String)> {
        let fragment = fragment.strip_prefix('#').unwrap_or(fragment);
        let decode = |text: &str| {
            percent_encoding::percent_decode_str(text)
                .decode_utf8_lossy()
                .into_owned()
        };
        let mut pairs =
            Vec::with_capacity(fragment.bytes().filter(|&byte| byte == b'&').count() + 1);
        pairs.extend(
            fragment
                .split('&')
                .filter(|pair| !pair.is_empty())
                .map(|pair| {
                    let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
                    (decode(name), decode(value))
                }),
        );
        pairs
    }

    #[cfg(test)]
    mod tests {
        use super::fragment_pairs as parse;

        fn owned(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
            pairs
                .iter()
                .map(|&(name, value)| (name.to_owned(), value.to_owned()))
                .collect()
        }

        #[test]
        fn a_bare_name_is_set_empty() {
            assert_eq!(
                parse("#BVIZ_TRACE_TIMING"),
                owned(&[("BVIZ_TRACE_TIMING", "")])
            );
        }

        #[test]
        fn switches_are_separated_by_ampersands_and_valued_after_an_equals() {
            assert_eq!(
                parse("#BVIZ_TRACE_TIMING&BVIZ_PAN_BENCH=map&BVIZ_PAN_BENCH_FRAMES=300"),
                owned(&[
                    ("BVIZ_TRACE_TIMING", ""),
                    ("BVIZ_PAN_BENCH", "map"),
                    ("BVIZ_PAN_BENCH_FRAMES", "300"),
                ])
            );
        }

        #[test]
        fn a_value_is_percent_decoded_and_may_hold_an_equals() {
            assert_eq!(parse("A=b%20c=d"), owned(&[("A", "b c=d")]));
        }

        #[test]
        fn an_empty_fragment_sets_nothing() {
            assert_eq!(parse(""), owned(&[]));
            assert_eq!(parse("#"), owned(&[]));
            assert_eq!(parse("#&&"), owned(&[]));
        }
    }
}
