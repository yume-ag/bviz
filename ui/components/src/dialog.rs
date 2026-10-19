//! A dialog standing over the whole window, asking the user something the
//! window waits on: a titled sheet, its own keys to accept and to cancel it,
//! and a row of buttons doing either.

use crate::menu::MENU_PRIORITY;
use crate::menu_chrome::surface;
use crate::{Button, KeyScope};
use bviz_ui_theme::ThemeColors;
use gpui::{
    App, Div, FocusHandle, KeyBinding, MouseButton, Pixels, SharedString, Window, actions,
    anchored, deferred, div, point, prelude::*, px,
};

actions!(
    bviz_dialog,
    [
        /// Accept dialog
        Accept,
        /// Cancel dialog
        Cancel,
    ]
);

/// The key context a dialog's sheet carries, so its keys reach it from any
/// field it holds.
pub const KEY_CONTEXT: KeyScope = KeyScope::new("Dialog", "Dialog");

pub(crate) fn bind_keys(cx: &mut App) {
    let dialog = Some(KEY_CONTEXT.id());
    cx.bind_keys([
        KeyBinding::new("enter", Accept, dialog),
        KeyBinding::new("escape", Cancel, dialog),
    ]);
}

/// How wide a dialog's sheet stands, room for a path of some depth in a field.
const SHEET_WIDTH: Pixels = px(560.);

/// The layer a dialog is drawn in, above any menu the window has open.
const DIALOG_PRIORITY: usize = MENU_PRIORITY + 1;

/// The sheet of a dialog headed by `title`, for the caller to fill and to give
/// its [`Accept`] and [`Cancel`] handlers.
pub fn sheet(title: impl Into<SharedString>, colors: &ThemeColors) -> Div {
    surface(div(), colors)
        .key_context(KEY_CONTEXT)
        .w(SHEET_WIDTH)
        .px_4()
        .py_3()
        .flex()
        .flex_col()
        .gap_3()
        .font_family(bviz_ui_assets::UI)
        .text_color(colors.text)
        .child(title.into())
}

/// The row of a dialog's `buttons`, set at its trailing end.
pub fn buttons(buttons: impl IntoIterator<Item = Button>) -> Div {
    div()
        .flex()
        .flex_row()
        .justify_end()
        .gap_2()
        .children(buttons)
}

/// `sheet` centred over the whole window, drawn above whatever else the window
/// draws and over a backdrop taking every press meant for what lies under it.
/// A press anywhere on the backdrop hands focus back to `answer`, where the
/// dialog's keys are read, so that a press beside a field leaves them working.
pub fn over_window(
    sheet: Div,
    answer: &FocusHandle,
    window: &Window,
    colors: &ThemeColors,
) -> impl IntoElement + use<> {
    let answer = answer.clone();
    let viewport = window.viewport_size();
    let backdrop = div()
        .id("dialog-backdrop")
        .occlude()
        .on_mouse_down(MouseButton::Left, move |_, window, cx| {
            window.focus(&answer, cx);
        })
        .w(viewport.width)
        .h(viewport.height)
        .flex()
        .items_center()
        .justify_center()
        .bg(colors.shadow)
        .child(sheet);
    let origin = point(px(0.), px(0.));
    deferred(anchored().position(origin).child(backdrop)).with_priority(DIALOG_PRIORITY)
}
