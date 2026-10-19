use crate::Workspace;
use gpui::{App, Global, WeakEntity, Window};
use std::path::PathBuf;
use std::rc::Rc;

/// How an app opens the files its user names to a window while it runs, by
/// the window's Open files action. Without one, a window offers none.
#[derive(Clone)]
pub struct PathOpener(Rc<Open>);

type Open = dyn Fn(Vec<PathBuf>, WeakEntity<Workspace>, &mut Window, &mut App);

impl Global for PathOpener {}

impl PathOpener {
    /// An opener calling `open` with the paths named, at least one, and the
    /// workspace of the window they were named to. `open` runs inside an
    /// update of that workspace, so it reaches the workspace only from work it
    /// defers, such as a task it spawns on `window`.
    pub fn new(
        open: impl Fn(Vec<PathBuf>, WeakEntity<Workspace>, &mut Window, &mut App) + 'static,
    ) -> Self {
        Self(Rc::new(open))
    }

    pub(crate) fn open(
        &self,
        paths: Vec<PathBuf>,
        into: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut App,
    ) {
        (self.0)(paths, into, window, cx);
    }
}
