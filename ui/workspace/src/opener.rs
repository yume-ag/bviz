use crate::Workspace;
use crate::opening::{Failure, Opening, Unopened};
use bviz_input::Named;
use gpui::{App, AsyncApp, Global, WeakEntity, Window};
use std::rc::Rc;

/// How an app opens the files its user names to a window while it runs, by
/// the window's Open files action or by dropping them on the window. Without
/// one, a window offers neither.
#[derive(Clone)]
pub struct PathOpener(Rc<Open>);

type Open = dyn Fn(Vec<Named>, PendingOpen, &mut Window, &mut App);

impl Global for PathOpener {}

impl PathOpener {
    /// An opener calling `open` with the files named, at least one, and the
    /// opening of them the window shows, which `open` settles through
    /// [`PendingOpen::settle`] once their views stand. `open` runs inside an
    /// update of that workspace, so it reaches the workspace only from work it
    /// defers, such as a task it spawns on `window`.
    pub fn new(open: impl Fn(Vec<Named>, PendingOpen, &mut Window, &mut App) + 'static) -> Self {
        Self(Rc::new(open))
    }

    pub(crate) fn open(
        &self,
        files: Vec<Named>,
        pending: PendingOpen,
        window: &mut Window,
        cx: &mut App,
    ) {
        (self.0)(files, pending, window, cx);
    }
}

/// Files a window shows as being opened until this is settled. Dropped
/// unsettled, as where the work opening them is lost, it settles every file
/// still on its way as not opened.
#[must_use = "a window shows the files as being opened until this is settled"]
pub struct PendingOpen {
    workspace: WeakEntity<Workspace>,
    /// The note the window shows the opening as.
    pub(crate) note: u64,
    /// Reaches the workspace from wherever this is settled or dropped, until
    /// it is settled.
    app: Option<AsyncApp>,
}

impl PendingOpen {
    pub(crate) fn new(workspace: WeakEntity<Workspace>, note: u64, cx: &App) -> Self {
        Self {
            workspace,
            note,
            app: Some(cx.to_async()),
        }
    }

    /// The workspace of the window the files were named to.
    pub fn workspace(&self) -> &WeakEntity<Workspace> {
        &self.workspace
    }

    /// Settles the opening, its files' views standing in the window but for
    /// `failures`, which the window goes on showing until the user dismisses
    /// them.
    pub fn settle(mut self, failures: Vec<Failure>) {
        self.close(move |opening| opening.settle(failures));
    }

    /// Replaces the window's note of the opening with what `settle` leaves of
    /// it, from a task of its own, as this may be settled or dropped inside
    /// an update of the workspace.
    fn close(&mut self, settle: impl FnOnce(Opening) -> Option<Unopened> + 'static) {
        let Some(app) = self.app.take() else {
            return;
        };
        let (workspace, note) = (self.workspace.clone(), self.note);
        app.spawn(async move |cx| {
            (workspace.update(cx, |workspace, cx| {
                workspace.settle_opening(note, settle, cx)
            }))
            .ok();
        })
        .detach();
    }
}

impl Drop for PendingOpen {
    fn drop(&mut self) {
        self.close(Opening::lose);
    }
}
