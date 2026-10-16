//! Shows where the spawns of a Bazel build spent their time, read from a
//! compact execution log (`--execution_log_compact_file`): the time Bazel's
//! spawn metrics report for each phase of a spawn's run, summed by runner and
//! by mnemonic, and the spawns that spent longest in one phase; and the
//! spawns that wrote or read the most bytes.

mod format;
mod model;
mod palette;
mod spawn_time;
mod summary;

pub use spawn_time::SpawnTimePanel;

use bviz_execlog::ExecLog;
use bviz_input::Input;
use bviz_invocation::{InvocationId, Session};
use bviz_ui_components::{Pending, declare_scopes, legend_key_bindings};
use bviz_ui_execlog_args::{EXEC_LOG_LONG, ExecLogArgs, ExecLogSource};
use bviz_ui_playback::Clock;
use bviz_ui_session::Sharing;
use bviz_ui_workspace::{Document, Member, Shared, ViewSpec, document_label};
use clap::Parser;
use gpui::{App, Entity, SharedString, Window, prelude::*};
use model::{ExecLogModel, Read, Reread};
use std::path::Path;
use summary::Summary;

pub fn register_keys(cx: &mut App) {
    declare_scopes(cx, [spawn_time::KEY_CONTEXT]);
    cx.bind_keys(legend_key_bindings(spawn_time::KEY_CONTEXT));
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
        summary: log.as_ref().map(Summary::of),
        spawns: log.into_inner().into_spawns(),
        reread,
    })
}

/// The view an execution log opens with, by which a document is told to hold one.
pub type LeadPanel = SpawnTimePanel;

/// The view of the spawn time the log `load` reads, which reports the log's
/// spawns to its document's `session` as `sharing` and `clock` ask.
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
    let view = ViewSpec::new(move |cx| cx.new(|cx| LeadPanel::new(model.clone(), cx)));
    Member::new(load.title, view, Vec::new())
}

/// Shows the spawn time of Bazel builds by phase. Each file given opens a view
/// of its own.
#[derive(Parser)]
#[command(name = "execlog", version, about)]
struct Cli {
    #[command(flatten)]
    exec_log: ExecLogArgs,
}

/// Reads the execution logs and opens the spawn-time window.
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
