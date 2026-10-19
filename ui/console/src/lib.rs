//! Replays the console output of a Bazel build, captured in a
//! `--build_event_binary_file`, in a gpui window: seek anywhere in the build and
//! play it back at real or multiplied speed.
//!
//! Bazel does not timestamp its progress output, so accurate real-time playback
//! needs the build captured with `--show_timestamps`, whose `(HH:MM:SS)` prefixes
//! become the timeline. Without them the timeline is interpolated between the
//! build's sparse timestamped events (start, finish, action and test
//! completions) and is approximate.

mod color;
mod console;
mod model;

pub use console::{ConsolePanel, register_keys};
use model::ConsoleModel;

use bviz_input::Input;
use bviz_invocation::{InvocationId, Session};
use bviz_ui_components::Pending;
use bviz_ui_console_args::{BEP_LONG, ConsoleArgs, ConsoleSource};
use bviz_ui_playback::Clock;
use bviz_ui_workspace::{Document, Member, ViewSpec, document_label};
use clap::Parser;
use gpui::{App, Entity, SharedString, Window, prelude::*};
use std::path::{Path, PathBuf};

/// A build's console output being read and reconstructed.
pub struct Load {
    stream: Pending<model::Stream>,
    title: SharedString,
}

/// The invocation that wrote the build event stream `input` holds, read from
/// the input's head alone.
pub fn invocation_id(input: &Input) -> Option<InvocationId> {
    bviz_terminal::probe_invocation_id(input)
}

impl From<Input> for Load {
    fn from(input: Input) -> Self {
        Self {
            title: document_label(Path::new(input.name())),
            stream: Pending::start(move || model::Stream::read(input)),
        }
    }
}

impl From<ConsoleSource> for Load {
    fn from(source: ConsoleSource) -> Self {
        Self {
            title: document_label(&source.path),
            stream: Pending::start(move || {
                let input = Input::open(&source.path)
                    .map_err(|e| format!("cannot read {}: {e}", source.path.display()))?;
                model::Stream::read(input)
            }),
        }
    }
}

/// The views replaying the build `load` reads, which tells `session` what its
/// stream knows of the invocation where it is opened as part of one, and plays
/// on `clock` where its document plays back.
pub fn open(
    load: impl Into<Load>,
    session: Option<Entity<Session>>,
    clock: Option<Entity<Clock>>,
    _window: &mut Window,
    cx: &mut App,
) -> Member {
    let load = load.into();
    let label = load.title;
    let model = cx.new(|cx| ConsoleModel::reading(load.stream, session, clock, cx));
    let console = ViewSpec::new(move |cx| cx.new(|cx| ConsolePanel::new(model.clone(), cx)));
    Member::new(label, console, Vec::new())
}

/// Replays the console output of a Bazel build. Each file given opens a replay
/// of its own.
#[derive(Parser)]
#[command(name = "console", version, about)]
struct Cli {
    #[command(flatten)]
    console: ConsoleArgs,
    /// The same file `--bep` names, given without the flag.
    #[arg(value_name = "FILE")]
    files: Vec<PathBuf>,
}

impl Cli {
    fn sources(self) -> impl Iterator<Item = ConsoleSource> {
        let positional = self.files.into_iter().map(|path| ConsoleSource { path });
        self.console.sources().chain(positional)
    }
}

/// Parses the build event streams and opens the console-replay window.
pub fn run() {
    bviz_ui_workspace::run_viewer(
        "bviz console",
        bviz_ui_workspace::TEXT_WINDOW_SIZE,
        |cli: Cli| cli.sources().map(Load::from),
        &format!("--{BEP_LONG} or a file argument"),
        |load, window, cx| {
            let clock = cx.new(|_| Clock::default());
            Document::from(open(load, None, Some(clock.clone()), window, cx)).playing_on(clock)
        },
        &[console::register_keys],
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use bviz_terminal::bep::build_event_stream::{BuildEvent, BuildStarted, build_event::Payload};
    use prost::Message;

    #[test]
    fn an_input_names_the_invocation_its_stream_was_written_by() {
        let dir = std::env::var_os("TEST_TMPDIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let source = |name: &str, stream: &[u8]| {
            let path = Path::new(&dir).join(name);
            std::fs::write(&path, stream).unwrap();
            Input::open(&path).expect("the stream opens")
        };
        let started = BuildEvent {
            payload: Some(Payload::Started(BuildStarted {
                uuid: "a5f7f3e6-4fda".into(),
                ..Default::default()
            })),
            ..Default::default()
        }
        .encode_length_delimited_to_vec();
        let named = source("named.bep", &started);
        assert_eq!(invocation_id(&named), InvocationId::new("a5f7f3e6-4fda"));
        let unnamed = source("unnamed.bep", &[]);
        assert_eq!(invocation_id(&unnamed), None);
    }

    /// The error reading `load` fails with.
    fn failure(load: Load) -> String {
        match load.stream.join() {
            Ok(_) => panic!("the stream reads"),
            Err(e) => e,
        }
    }

    #[test]
    fn a_stream_that_cannot_be_read_fails_naming_it() {
        let dir = std::env::var_os("TEST_TMPDIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        for unopened in [dir.join("missing.bep"), dir.clone()] {
            let error = failure(
                ConsoleSource {
                    path: unopened.clone(),
                }
                .into(),
            );
            assert!(error.contains(&unopened.display().to_string()), "{error}");
        }
        let corrupt = dir.join("corrupt.bep");
        let no_event = dir.join("empty.bep");
        std::fs::write(&corrupt, [0xff; 11]).unwrap();
        std::fs::write(&no_event, []).unwrap();
        for unread in [corrupt, no_event] {
            let input = Input::open(&unread).expect("the file opens");
            let error = failure(input.into());
            assert!(error.contains(&unread.display().to_string()), "{error}");
        }
    }
}
