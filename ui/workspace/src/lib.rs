//! The window's root: a [`Workspace`] framing an arrangement of [`Pane`]s in the
//! application chrome, the panels ([`AnyPanel`]) each pane holds and shows one
//! of, the registry of what every window's panels show views of and the menu
//! listing it, and [`run_panels`] for an app whose window frames them.

mod arrangement;
mod carry;
mod documents;
mod documents_menu;
mod drag_icon;
mod drop;
mod gestures;
mod group;
mod layout;
mod pane;
mod panel;
mod panel_menu;
mod regions;
#[cfg(test)]
mod testing;
mod workspace;

pub(crate) use arrangement::{Arrangement, Maximize};
pub use documents::{Closed, DocumentId, Documents, Joinable, ViewSpec};
pub(crate) use documents::{OpenDocument, ViewId, ViewState, WindowsChanged};
pub use gestures::chrome_gestures;
pub(crate) use group::PanelGroup;
pub use layout::{AtEdge, DividerId, Layout, Panes, Removal, Side, Split, SplitSide};
pub(crate) use pane::Framing;
pub use pane::{Pane, PaneEvent};
pub(crate) use panel::DocumentOf;
pub use panel::{
    AnyPanel, Document, Member, Panel, PanelEvent, PanelKind, Shared, Titled, ViewKind,
    document_label, invocation_label,
};
pub(crate) use regions::Regions;
pub use regions::Role;
pub use workspace::Workspace;

use workspace::register_keys;

use clap::Parser;
use clap::error::ErrorKind;
use gpui::{App, AppContext, Pixels, Size, Window, px};

/// Opening size of a window, roomy enough for a graph or a timeline to be
/// worth looking at before it is resized.
pub const WINDOW_SIZE: Size<Pixels> = Size {
    width: px(1280.),
    height: px(820.),
};

/// Opening size of a window of a viewer that shows lines of text or rows of a
/// table, which read well in less room.
pub const TEXT_WINDOW_SIZE: Size<Pixels> = Size {
    width: px(1100.),
    height: px(720.),
};

/// Runs a standalone viewer, as [`run_panels`] does, for the documents
/// `documents` reads from its command line `C`. Where they come to none, it
/// exits with the usage error naming what is `required`.
pub fn run_viewer<C, I, T, F>(
    app_id: &str,
    window_size: Size<Pixels>,
    documents: impl FnOnce(C) -> I,
    required: &str,
    open: F,
    panel_keys: &'static [fn(&mut App)],
) where
    C: Parser,
    I: IntoIterator<Item = T>,
    T: 'static,
    F: 'static + Fn(T, &mut Window, &mut App) -> Document,
{
    let documents: Vec<T> = documents(C::parse()).into_iter().collect();
    if documents.is_empty() {
        C::command()
            .error(
                ErrorKind::MissingRequiredArgument,
                format!("{required} is required"),
            )
            .exit()
    }
    run_panels(app_id, window_size, documents, open, panel_keys);
}

/// Runs an app whose one window frames the [`Document`]s `open` builds from
/// `documents` in order, their views laid out by [`Role`], and frames no panel
/// where `documents` is empty. Each of
/// `panel_keys` registers the bindings of one panel kind the app can show, and
/// runs before the first window opens. Each runs exactly once per process:
/// `cx.bind_keys` refreshes every window, which drops what each has cached of
/// its last frame.
pub fn run_panels<T, F>(
    app_id: impl Into<String>,
    window_size: Size<Pixels>,
    documents: Vec<T>,
    open: F,
    panel_keys: &'static [fn(&mut App)],
) where
    T: 'static,
    F: 'static + Fn(T, &mut Window, &mut App) -> Document,
{
    let app_id = app_id.into();
    bviz_ui_shell::application().run(move |cx: &mut App| {
        bviz_ui_shell::setup(cx);
        register_keys(cx);
        for register in panel_keys {
            register(cx);
        }
        bviz_ui_shell::open_window(&app_id, window_size, cx, |window, cx| {
            let mut opened = Vec::with_capacity(documents.len());
            for document in documents {
                opened.push(open(document, window, cx));
            }
            cx.new(|cx| Workspace::with_documents(app_id.clone(), opened, window, cx))
        })
        .unwrap();
        cx.activate(true);
    });
}
