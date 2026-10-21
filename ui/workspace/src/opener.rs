use crate::Workspace;
use bviz_input::Named;
use gpui::{App, Global, WeakEntity, Window};
use std::rc::Rc;

/// How an app opens the files its user names to a window while it runs, by
/// the window's Open files action or by dropping them on the window. Without
/// one, a window offers neither.
#[derive(Clone)]
pub struct PathOpener(Rc<Open>);

type Open = dyn Fn(Vec<Named>, WeakEntity<Workspace>, &mut Window, &mut App);

impl Global for PathOpener {}

impl PathOpener {
    /// An opener calling `open` with the files named, at least one, and the
    /// workspace of the window they were named to. `open` runs inside an
    /// update of that workspace, so it reaches the workspace only from work it
    /// defers, such as a task it spawns on `window`.
    pub fn new(
        open: impl Fn(Vec<Named>, WeakEntity<Workspace>, &mut Window, &mut App) + 'static,
    ) -> Self {
        Self(Rc::new(open))
    }

    pub(crate) fn open(
        &self,
        files: Vec<Named>,
        into: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut App,
    ) {
        (self.0)(files, into, window, cx);
    }
}
