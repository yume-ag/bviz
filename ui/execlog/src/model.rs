use crate::summary::Summary;
use bviz_execlog::ExecLog;
use bviz_input::Reopener;
use bviz_invocation::spawn::LoggedSpawns;
use bviz_invocation::{InvocationId, Recorded, Session};
use bviz_ui_components::{Pending, load_future_in_background, load_in_background};
use bviz_ui_playback::{Alignment, BecameJoined, Clock};
use bviz_ui_session::{Reports, Sharing};
use gpui::{App, Context, Entity, Subscription, Task};

pub(crate) type Source = bviz_ui_components::Source<Recorded<Summary>>;

/// What reading an execution log yields.
pub(crate) struct Read {
    pub(crate) summary: Recorded<Summary>,
    pub(crate) spawns: LoggedSpawns,
    /// `None` for a log read from a stream, and for one naming no invocation.
    pub(crate) reread: Option<Reread>,
}

/// How to read a log again: what opens it afresh, and the invocation it
/// named when first read, which the log opened again must name to be the
/// one the document was joined by.
pub(crate) struct Reread {
    pub(crate) reopener: Reopener,
    pub(crate) invocation: InvocationId,
}

/// How a log alone in its document, which another output of its invocation
/// may join, comes by its spawns for the session once that file joins. Its
/// spawns are many times the summary's size, and nothing reads them before
/// then, so it keeps them only where it cannot read them again.
enum Awaiting {
    Reread(Reread),
    /// The spawns of a log read from a stream, which yields its bytes once.
    Kept(LoggedSpawns),
}

/// The summary of an execution log, held apart from the panel showing it so
/// the panel can close and be shown again without the log being read afresh.
pub(crate) struct ExecLogModel {
    source: Source,
    /// Tells the log's spawns to its document's session.
    reports: Reports,
    clock: Entity<Clock>,
    /// `None` once the spawns are reported, and for a log no other file joins.
    awaiting: Option<Awaiting>,
    /// The second read of a log whose document was joined after the first,
    /// held so that it is not cancelled.
    _rereading: Option<Task<()>>,
    /// The read that fills `source`, held so that it is not cancelled.
    _load: Task<()>,
    _joined: Subscription,
}

impl ExecLogModel {
    /// The summary of the log `log` reads, its spawns reported to `session`
    /// once `clock` is joined, where `sharing` lets another file read them.
    pub(crate) fn reading(
        log: Pending<Read>,
        session: Entity<Session>,
        clock: Entity<Clock>,
        sharing: Sharing,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            source: Source::Loading,
            reports: Reports::new(session, cx),
            _joined: cx.subscribe(&clock, |model, _, _: &BecameJoined, cx| {
                model.report_awaited(cx)
            }),
            clock,
            awaiting: None,
            _rereading: None,
            _load: load_future_in_background(cx, log.finished(), move |model, result, cx| {
                model.source = match result {
                    Ok(Read {
                        summary,
                        spawns,
                        reread,
                    }) => {
                        if model.joined(cx) {
                            model.report(spawns, cx);
                        } else if sharing == Sharing::Invocation {
                            model.awaiting = Some(match reread {
                                Some(reread) => Awaiting::Reread(reread),
                                None => Awaiting::Kept(spawns),
                            });
                        }
                        Source::Loaded(summary)
                    }
                    Err(why) => Source::Failed(why),
                };
            }),
        }
    }

    fn joined(&self, cx: &App) -> bool {
        self.clock.read(cx).alignment() == Alignment::Joined
    }

    fn report(&self, spawns: LoggedSpawns, cx: &mut App) {
        self.reports.report(cx, |session, by, cx| {
            session.report_logged_spawns(by, spawns);
            cx.notify();
        });
    }

    /// Reports the spawns of a log whose document was just joined, reading
    /// the log again where it kept none.
    fn report_awaited(&mut self, cx: &mut Context<Self>) {
        match self.awaiting.take() {
            None => {}
            Some(Awaiting::Kept(spawns)) => self.report(spawns, cx),
            Some(Awaiting::Reread(reread)) => {
                self._rereading = Some(load_in_background(
                    cx,
                    move || spawns_of(&reread),
                    |model, spawns, cx| {
                        if let Some(spawns) = spawns {
                            model.report(spawns, cx);
                        }
                    },
                ));
            }
        }
    }

    pub(crate) fn source(&self) -> &Source {
        &self.source
    }

    pub(crate) fn cut_off(&self) -> bool {
        self.source.is_cut_off()
    }
}

/// The spawns of the log `reread` opens again, `None` where it no longer
/// reads or names another invocation, as a file written over since does.
fn spawns_of(reread: &Reread) -> Option<LoggedSpawns> {
    let input = reread.reopener.open().ok()?;
    if crate::invocation_id(&input).as_ref() != Some(&reread.invocation) {
        return None;
    }
    let log = ExecLog::from_input(input).ok()?;
    Some(log.into_inner().into_spawns())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bviz_input::Input;

    /// A file that no longer names the invocation its document was joined by
    /// yields no spawns to report.
    #[test]
    fn a_log_read_again_naming_another_invocation_yields_nothing() {
        let dir = std::env::var_os("TEST_TMPDIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let written_over = dir.join("written_over.execlog.zst");
        std::fs::write(&written_over, b"no execution log").unwrap();
        let input = Input::open(&written_over).expect("the file opens");
        let (_, reopener) = input.reopenable().ok().expect("a file reopens");
        let reread = Reread {
            reopener,
            invocation: InvocationId::new("a5f7f3e6").expect("a non-empty id"),
        };
        assert!(spawns_of(&reread).is_none());
    }
}
