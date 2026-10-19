use crate::summary::Summary;
use bviz_invocation::spawn::LoggedSpawns;
use bviz_invocation::{Recorded, Session};
use bviz_ui_components::{Pending, load_in_background};
use bviz_ui_session::{Reports, Sharing};
use gpui::{Context, Entity, Task};

pub(crate) type Source = bviz_ui_components::Source<Recorded<Summary>>;

/// What reading an execution log yields.
pub(crate) struct Read {
    pub(crate) summary: Recorded<Summary>,
    pub(crate) spawns: LoggedSpawns,
}

/// The summary of an execution log, held apart from the panel showing it so
/// the panel can close and be shown again without the log being read afresh.
pub(crate) struct ExecLogModel {
    source: Source,
    /// Tells the log's spawns to its document's session.
    reports: Reports,
    /// The read that fills `source`, held so that it is not cancelled.
    _load: Task<()>,
}

impl ExecLogModel {
    /// The summary of the log `log` reads, its spawns reported to `session`
    /// where `sharing` has other files read them, and dropped otherwise, as
    /// they are many times the summary's size.
    pub(crate) fn reading(
        log: Pending<Read>,
        session: Entity<Session>,
        sharing: Sharing,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            source: Source::Loading,
            reports: Reports::new(session, cx),
            _load: load_in_background(
                cx,
                move || log.join(),
                move |model, result, cx| {
                    model.source = match result {
                        Ok(Read { summary, spawns }) => {
                            if sharing == Sharing::Invocation {
                                model.reports.report(cx, |session, by, cx| {
                                    session.report_logged_spawns(by, spawns);
                                    cx.notify();
                                });
                            }
                            Source::Loaded(summary)
                        }
                        Err(why) => Source::Failed(why),
                    };
                },
            ),
        }
    }

    pub(crate) fn source(&self) -> &Source {
        &self.source
    }

    pub(crate) fn cut_off(&self) -> bool {
        self.source.is_cut_off()
    }
}
