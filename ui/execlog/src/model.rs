use crate::summary::Summary;
use bviz_invocation::spawn::LoggedSpawns;
use bviz_invocation::{Recorded, Session};
use bviz_ui_components::{Pending, load_in_background};
use gpui::{Context, Task, WeakEntity};

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
    /// The read that fills `source`, held so that it is not cancelled.
    _load: Task<()>,
}

impl ExecLogModel {
    /// The summary of the log `log` reads, its spawns reported to `session`
    /// where it is read as one of an invocation's outputs.
    pub(crate) fn reading(
        log: Pending<Read>,
        session: Option<WeakEntity<Session>>,
        cx: &mut Context<Self>,
    ) -> Self {
        let session = session.map(|session| {
            let by = session.update(cx, |session, _| session.reporter()).ok();
            (session, by)
        });
        Self {
            source: Source::Loading,
            _load: load_in_background(
                cx,
                move || log.join(),
                move |model, result, cx| {
                    model.source = match result {
                        Ok(Read { summary, spawns }) => {
                            if let Some((session, Some(by))) = session {
                                session
                                    .update(cx, |session, cx| {
                                        session.report_logged_spawns(by, spawns);
                                        cx.notify();
                                    })
                                    .ok();
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
