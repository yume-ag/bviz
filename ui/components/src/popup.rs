//! Whether a menu dropped from a trigger stands open, for the controls that
//! open one.

use crate::dropdown::{Dismiss, KEY_CONTEXT};
use crate::focus::focus_went_nowhere;
use gpui::{App, Context, Div, Entity, FocusHandle, Subscription, Window, div, prelude::*};

/// The menu a [`Popup`] has open.
struct Opened {
    /// Where focus stood when the menu opened, which closing it hands focus
    /// back to.
    from: Option<FocusHandle>,
}

/// A menu opened from a trigger.
pub(crate) struct Popup {
    opened: Option<Opened>,
    /// Held while the menu is open, so an outside click, which takes it away,
    /// closes the menu.
    focus: FocusHandle,
    /// Installed on the first opening, where the window is in hand.
    dismiss: Option<Subscription>,
}

impl Popup {
    pub(crate) fn new(cx: &mut Context<Self>) -> Self {
        Self {
            opened: None,
            focus: cx.focus_handle(),
            dismiss: None,
        }
    }

    pub(crate) fn is_open(&self) -> bool {
        self.opened.is_some()
    }

    pub(crate) fn toggle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.opened {
            Some(_) => self.close(window, cx),
            None => self.open(window, cx),
        }
    }

    fn open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.dismiss.is_none() {
            self.dismiss = Some(
                cx.on_focus_out(&self.focus, window, |popup, _, window, cx| {
                    if focus_went_nowhere(&popup.focus, window, cx) {
                        popup.close(window, cx);
                    } else if popup.opened.take().is_some() {
                        cx.notify();
                    }
                }),
            );
        }
        self.opened = Some(Opened {
            from: window.focused(cx),
        });
        self.focus.focus(window, cx);
        cx.notify();
    }

    /// Closes the menu, handing focus back where it stood when the menu opened.
    pub(crate) fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(Opened { from }) = self.opened.take() {
            if let Some(from) = from {
                window.focus(&from, cx);
            }
            cx.notify();
        }
    }
}

/// The `relative` holder of a trigger and the menu `popup` drops from it, where
/// [`Dismiss`] closes an open menu and passes on otherwise.
pub(crate) fn holder(popup: &Entity<Popup>, cx: &App) -> Div {
    let (open, focus) = {
        let popup = popup.read(cx);
        (popup.is_open(), popup.focus.clone())
    };
    let popup = popup.clone();
    div()
        .relative()
        .key_context(KEY_CONTEXT)
        // Focusable only while open, so a press on the closed trigger leaves
        // focus to whatever around it takes it, for the menu to hand back.
        .when(open, |el| el.track_focus(&focus))
        .on_action(move |_: &Dismiss, window, cx| {
            popup.update(cx, |popup, cx| match popup.is_open() {
                true => popup.close(window, cx),
                false => cx.propagate(),
            });
        })
}
