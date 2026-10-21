//! The window's root: a [`Workspace`] framing an arrangement of [`Pane`]s in the
//! application chrome, the panels ([`AnyPanel`]) each pane holds and shows one
//! of, the registry of what every window's panels show views of and the menu
//! listing it, and [`run_panels`] for an app whose window frames them.

mod app_actions;
mod arrangement;
mod carry;
mod documents;
mod documents_menu;
mod drag_icon;
mod drop;
mod gestures;
mod group;
mod layout;
mod opener;
mod pane;
mod panel;
mod panel_menu;
#[cfg(not(target_family = "wasm"))]
mod path_prompt;
mod regions;
#[cfg(test)]
mod testing;
mod workspace;

pub use app_actions::AppAction;
pub(crate) use app_actions::AppActions;
pub(crate) use arrangement::{Arrangement, Maximize};
pub use documents::{Closed, DocumentId, Documents, Joinable, ViewSpec};
pub(crate) use documents::{OpenDocument, ViewId, ViewState, WindowsChanged};
pub use gestures::chrome_gestures;
pub(crate) use group::PanelGroup;
pub use layout::{AtEdge, DividerId, Layout, Panes, Removal, Side, Split, SplitSide};
pub use opener::PathOpener;
pub(crate) use pane::Framing;
pub use pane::{Pane, PaneEvent};
pub(crate) use panel::DocumentOf;
pub use panel::{
    AnyPanel, Document, Member, Panel, PanelEvent, PanelKind, Shared, Titled, ViewKind,
    document_label, invocation_label, named_label,
};
pub(crate) use regions::Regions;
pub use regions::Role;
pub use workspace::Workspace;

use workspace::register_keys;

use clap::Parser;
use clap::error::ErrorKind;
use gpui::{App, AppContext, Pixels, Size, WeakEntity, Window, px};

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

/// Runs a standalone viewer whose one window frames the [`Document`]s `open`
/// builds from the documents `documents` reads from its command line `C`, in
/// order, their views laid out by [`Role`]. Where they come to none, it exits
/// with the usage error naming what is `required`.
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
    let opened = move |window: &mut Window, cx: &mut App| {
        let mut opened = Vec::with_capacity(documents.len());
        for document in documents {
            opened.push(open(document, window, cx));
        }
        opened
    };
    run(
        app_id.into(),
        window_size,
        opened,
        |_, _, _| {},
        None,
        Vec::new(),
        panel_keys,
    );
}

/// Runs an app whose one window opens framing no panel and is handed, once
/// open, to `arriving`, which opens into it the documents the app was started
/// with as they become known. `arriving` runs inside an update of the window's
/// workspace, so it reaches the workspace only from work it defers, such as a
/// task it spawns on the window. Files the user names to a window while the
/// app runs go to `opener`, where the app opens any, and each window's
/// Documents menu lists `actions` after opening files. Each of `panel_keys`
/// registers the bindings of one panel kind the app can show, and runs before
/// the first window opens. Each runs exactly once per process: `cx.bind_keys`
/// refreshes every window, which drops what each has cached of its last frame.
pub fn run_panels(
    app_id: impl Into<String>,
    window_size: Size<Pixels>,
    arriving: impl FnOnce(WeakEntity<Workspace>, &mut Window, &mut App) + 'static,
    opener: Option<PathOpener>,
    actions: Vec<AppAction>,
    panel_keys: &'static [fn(&mut App)],
) {
    run(
        app_id.into(),
        window_size,
        |_, _| Vec::new(),
        arriving,
        opener,
        actions,
        panel_keys,
    );
}

/// Runs an app whose one window opens framing the documents `opened` builds,
/// as [`run_panels`] runs one, and frames no panel where they are none.
fn run(
    app_id: String,
    window_size: Size<Pixels>,
    opened: impl FnOnce(&mut Window, &mut App) -> Vec<Document> + 'static,
    arriving: impl FnOnce(WeakEntity<Workspace>, &mut Window, &mut App) + 'static,
    opener: Option<PathOpener>,
    actions: Vec<AppAction>,
    panel_keys: &'static [fn(&mut App)],
) {
    bviz_ui_shell::application().run(move |cx: &mut App| {
        bviz_ui_shell::setup(cx);
        register_keys(cx, opener.is_some());
        if let Some(opener) = opener {
            cx.set_global(opener);
        }
        cx.set_global(AppActions(actions));
        for register in panel_keys {
            register(cx);
        }
        let opening = bviz_ui_shell::open_window(&app_id, window_size, cx, |window, cx| {
            let opened = opened(window, cx);
            cx.new(|cx| Workspace::with_documents(app_id.clone(), opened, window, cx))
        });
        #[cfg(not(target_family = "wasm"))]
        let window = opening.unwrap();
        #[cfg(target_family = "wasm")]
        let Ok(window) = opening.inspect_err(|error| bviz_ui_web::failed(&format!("{error:#}")))
        else {
            return;
        };
        window
            .update(cx, |_, window, cx| arriving(cx.weak_entity(), window, cx))
            .unwrap();
        cx.activate(true);
    });
}
