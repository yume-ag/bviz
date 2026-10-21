//! Shows where the spawns of a Bazel build spent their time and what they
//! wrote and read, from a compact execution log
//! (`--execution_log_compact_file`), in two panels: the time Bazel's spawn
//! metrics report for each phase of a spawn's run, and the bytes of the files
//! spawns wrote and read, each summed by runner and by mnemonic, beside the
//! spawns that spent longest in one phase or moved the most bytes.

mod bytes;
mod format;
mod model;
mod palette;
mod panel;
mod phase_bar;
mod summary;
mod table;
mod time;

pub use bytes::BytesPanel;
pub use time::TimePanel;

use bviz_execlog::ExecLog;
use bviz_input::Input;
use bviz_invocation::{InvocationId, Session};
use bviz_ui_components::{Pending, declare_scopes, legend_key_bindings, text_input};
use bviz_ui_execlog_args::{EXEC_LOG_LONG, ExecLogArgs, ExecLogSource};
use bviz_ui_playback::Clock;
use bviz_ui_session::Sharing;
use bviz_ui_workspace::{Document, Member, Shared, ViewSpec, document_label};
use clap::Parser;
use gpui::{App, Entity, KeyBinding, SharedString, Window, prelude::*};
use model::{ExecLogModel, Read, Reread};
use std::path::Path;
use summary::Summary;

pub fn register_keys(cx: &mut App) {
    declare_scopes(cx, [time::KEY_CONTEXT, bytes::KEY_CONTEXT]);
    cx.bind_keys([time::KEY_CONTEXT, bytes::KEY_CONTEXT].map(|scope| {
        let panel = text_input::unless_typing(scope);
        KeyBinding::new("v", panel::CycleList, Some(panel.as_str()))
    }));
    cx.bind_keys(legend_key_bindings(time::KEY_CONTEXT));
}

/// An execution log being read.
pub struct Load {
    log: Pending<Read>,
    title: SharedString,
}

/// The invocation that wrote the execution log `input` holds, read from the
/// input's head alone.
pub fn invocation_id(input: &Input) -> Option<InvocationId> {
    bviz_execlog::probe_invocation_id(input)
}

impl From<Input> for Load {
    fn from(input: Input) -> Self {
        Self {
            title: document_label(Path::new(input.name())),
            log: Pending::start(move || read_log(input)),
        }
    }
}

impl From<ExecLogSource> for Load {
    fn from(source: ExecLogSource) -> Self {
        Self {
            title: document_label(&source.path),
            log: Pending::start(move || {
                let input = Input::open(&source.path)
                    .map_err(|e| format!("cannot read {}: {e}", source.path.display()))?;
                read_log(input)
            }),
        }
    }
}

fn read_log(input: Input) -> Result<Read, String> {
    let name = input.name().to_owned();
    let invocation = invocation_id(&input);
    let (input, reopener) = match input.reopenable() {
        Ok((input, reopener)) => (input, Some(reopener)),
        Err(stream) => (stream, None),
    };
    let log = ExecLog::from_input(input).map_err(|e| format!("cannot read {name}: {e}"))?;
    let reread = reopener
        .zip(invocation)
        .map(|(reopener, invocation)| Reread {
            reopener,
            invocation,
        });
    Ok(Read {
        summary: log.as_ref().map(|log| Summary::of(log.log())),
        log,
        reread,
    })
}

/// The view an execution log opens with, by which a document is told to hold one.
pub type LeadPanel = TimePanel;

/// The views of the spawn time and the spawn bytes the log `load` reads,
/// which report the log's spawns to its document's `session` as `sharing` and
/// `clock` ask.
pub fn open(
    load: impl Into<Load>,
    session: Entity<Session>,
    clock: Entity<Clock>,
    sharing: Sharing,
    _window: &mut Window,
    cx: &mut App,
) -> Member {
    let load = load.into();
    let model = cx.new(|cx| ExecLogModel::reading(load.log, session, clock, sharing, cx));
    let time = ViewSpec::new({
        let model = model.clone();
        move |cx| cx.new(|cx| LeadPanel::new(model.clone(), cx))
    });
    let bytes = ViewSpec::new(move |cx| cx.new(|cx| BytesPanel::new(model.clone(), cx)));
    Member::new(load.title, time, vec![bytes])
}

/// Shows the spawn time of Bazel builds by phase and the bytes their spawns
/// wrote and read. Each file given opens views of its own.
#[derive(Parser)]
#[command(name = "execlog", version, about)]
struct Cli {
    #[command(flatten)]
    exec_log: ExecLogArgs,
}

/// Reads the execution logs and opens the window showing them.
pub fn run() {
    bviz_ui_workspace::run_viewer(
        "bviz execlog",
        bviz_ui_workspace::TEXT_WINDOW_SIZE,
        |cli: Cli| cli.exec_log.sources().map(Load::from),
        &format!("--{EXEC_LOG_LONG}"),
        |load, window, cx| {
            let shared = Shared::new(cx);
            let (session, clock) = (shared.session().clone(), shared.clock().clone());
            let member = open(load, session, clock, Sharing::Alone, window, cx);
            Document::alone(member, None, shared)
        },
        &[register_keys],
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The error reading `load` fails with.
    fn failure(load: Load) -> String {
        match load.log.join() {
            Ok(_) => panic!("the log reads"),
            Err(e) => e,
        }
    }

    #[test]
    fn a_log_that_cannot_be_read_fails_naming_it() {
        let dir = std::env::var_os("TEST_TMPDIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        for unopened in [dir.join("missing.execlog.zst"), dir.clone()] {
            let error = failure(
                ExecLogSource {
                    path: unopened.clone(),
                }
                .into(),
            );
            assert!(error.contains(&unopened.display().to_string()), "{error}");
        }
        let corrupt = dir.join("corrupt.execlog.zst");
        std::fs::write(&corrupt, b"no execution log").unwrap();
        let input = Input::open(&corrupt).expect("the file opens");
        let error = failure(input.into());
        assert!(error.contains(&corrupt.display().to_string()), "{error}");
    }
}
