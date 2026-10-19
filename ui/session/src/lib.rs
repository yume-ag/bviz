//! A reader's part in the [`Session`] of the invocation it reads an output of.

use bviz_invocation::{ReporterId, Session};
use gpui::{App, Context, Entity, Subscription};

/// Whether a file opens as one of an invocation's outputs, whose views read
/// what it tells its session.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Sharing {
    /// A file in a document of its own, whose session no other file reads.
    Alone,
    Invocation,
}

/// What a model reports to its invocation's session, all of it withdrawn when
/// the model is released.
pub struct Reports {
    session: Entity<Session>,
    by: ReporterId,
    _withdrawal: Subscription,
}

impl Reports {
    /// A reporter to `session` for as long as the model `cx` builds is held.
    pub fn new<M: 'static>(session: Entity<Session>, cx: &mut Context<M>) -> Self {
        let by = session.update(cx, |session, _| session.reporter());
        let withdrawn = session.clone();
        let withdrawal = cx.on_release(move |_, cx| {
            withdrawn.update(cx, |session, cx| {
                if session.withdraw(by) {
                    cx.notify();
                }
            });
        });
        Self {
            session,
            by,
            _withdrawal: withdrawal,
        }
    }

    pub fn session(&self) -> &Entity<Session> {
        &self.session
    }

    /// Updates the session with `report`, which reports by this reporter.
    pub fn report<R>(
        &self,
        cx: &mut App,
        report: impl FnOnce(&mut Session, ReporterId, &mut Context<Session>) -> R,
    ) -> R {
        let by = self.by;
        self.session
            .update(cx, |session, cx| report(session, by, cx))
    }
}
