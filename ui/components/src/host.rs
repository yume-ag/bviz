//! What the application runs in.

/// What the application runs in, which decides what its windows can do and
/// which keys reach it. Code deciding by it takes it as a parameter, so tests
/// on either host cover both.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Host {
    Desktop,
    /// A page of a web browser, which holds one window and has no application
    /// to quit. The browser keeps from the page the keys that close and cycle
    /// its own windows and tabs, and may take Alt-← and Alt-→ for its history,
    /// so bviz binds chords for those actions there.
    Browser,
}

impl Host {
    /// The host this build runs in.
    pub const CURRENT: Host = if cfg!(target_family = "wasm") {
        Host::Browser
    } else {
        Host::Desktop
    };

    /// Whether the application can open a window beside the one it has.
    pub fn opens_windows(self) -> bool {
        self == Host::Desktop
    }
}
