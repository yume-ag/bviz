//! A dialog asking, of each of several items, which one of a set of choices
//! it is, or that it is to be skipped.

use crate::dialog::{self, Accept, Cancel};
use crate::list_scroll::{ListScroll, list_scrollbar};
use crate::scrollbar::{SCROLLBAR_THICKNESS, ScrollbarColors};
use crate::{Button, KeyScope, Nesting, ToggleGroup, ToggleSegment};
use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, IntoElement, KeyBinding, MouseButton,
    MouseDownEvent, Pixels, Render, SharedString, Window, actions, div, prelude::*, px,
};
use std::iter;

actions!(
    bviz_choice_dialog,
    [
        /// Aim at previous item
        AimPrevious,
        /// Aim at next item
        AimNext,
        /// Choose previous option
        PreviousChoice,
        /// Choose next option
        NextChoice,
    ]
);

/// The key context the dialog's list carries, inside the dialog's own.
pub const KEY_CONTEXT: KeyScope = KeyScope::within(
    &dialog::KEY_CONTEXT,
    Nesting::Inside,
    "ChoiceDialog",
    "Choice dialog",
);

pub(crate) fn bind_keys(cx: &mut App) {
    cx.bind_keys(bindings());
}

fn bindings() -> [KeyBinding; 4] {
    let list = Some(KEY_CONTEXT.id());
    [
        KeyBinding::new("up", AimPrevious, list),
        KeyBinding::new("down", AimNext, list),
        KeyBinding::new("left", PreviousChoice, list),
        KeyBinding::new("right", NextChoice, list),
    ]
}

/// What the option leaving an item out reads.
const SKIP: &str = "Skip";

/// How wide the sheet stands: room for the row of a handful of choices with
/// short names. An item's name stands above its row, so it need not fit
/// beside it.
const SHEET_WIDTH: Pixels = px(720.);

/// The tallest the list of items stands before it scrolls, so a long list
/// leaves the dialog's buttons on screen.
const MAX_LIST_HEIGHT: Pixels = px(360.);

/// What a [`ChoiceDialog`] answers.
pub enum ChoiceDialogEvent<C> {
    /// For each item in the order given, the choice made for it, `None` where
    /// it is skipped.
    Chosen(Vec<Option<C>>),
    Cancelled,
}

impl<C: 'static> EventEmitter<ChoiceDialogEvent<C>> for ChoiceDialog<C> {}

/// An item a [`ChoiceDialog`] asks of: its name, and a line of detail telling
/// it from items of the same name.
pub struct ChoiceItem {
    name: SharedString,
    detail: Option<SharedString>,
}

impl ChoiceItem {
    pub fn new(name: impl Into<SharedString>) -> Self {
        Self {
            name: name.into(),
            detail: None,
        }
    }

    pub fn detail(mut self, detail: impl Into<SharedString>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

/// An item, and the choice standing for it.
struct Item {
    item: ChoiceItem,
    /// Index into the dialog's choices, `None` while the item is skipped.
    chosen: Option<usize>,
}

/// Asks which of its choices each of a list of items is. Every item starts
/// skipped; the arrow keys move between items and step through the choices of
/// the one aimed at, and a click on a choice makes it.
pub struct ChoiceDialog<C> {
    title: SharedString,
    explanation: SharedString,
    accept: SharedString,
    /// Each choice, and what its segment reads.
    choices: Vec<(C, SharedString)>,
    items: Vec<Item>,
    /// The item the arrow keys step through the choices of.
    aimed: usize,
    focus: FocusHandle,
    scroll: ListScroll,
}

impl<C: Clone + 'static> ChoiceDialog<C> {
    /// A dialog headed by `title` and `explanation`, asking which of `choices`
    /// each of `items` is, its accepting button reading `accept`.
    pub fn new(
        title: impl Into<SharedString>,
        explanation: impl Into<SharedString>,
        accept: impl Into<SharedString>,
        choices: Vec<(C, SharedString)>,
        items: impl IntoIterator<Item = ChoiceItem>,
        cx: &mut Context<Self>,
    ) -> Self {
        let items = items
            .into_iter()
            .map(|item| Item { item, chosen: None })
            .collect();
        Self {
            title: title.into(),
            explanation: explanation.into(),
            accept: accept.into(),
            choices,
            items,
            aimed: 0,
            focus: cx.focus_handle(),
            scroll: ListScroll::new(),
        }
    }

    fn aim(&mut self, by: isize, cx: &mut Context<Self>) {
        let last = self.items.len().saturating_sub(1);
        self.aimed = self.aimed.saturating_add_signed(by).min(last);
        self.scroll.handle().scroll_to_item(self.aimed);
        cx.notify();
    }

    /// Steps the aimed item's choice `by` places along its row.
    fn step_choice(&mut self, by: isize, cx: &mut Context<Self>) {
        let Some(item) = self.items.get_mut(self.aimed) else {
            return;
        };
        item.chosen = stepped(item.chosen, by, self.choices.len());
        cx.notify();
    }

    fn choose(&mut self, item: usize, chosen: Option<usize>, cx: &mut Context<Self>) {
        if let Some(standing) = self.items.get_mut(item) {
            standing.chosen = chosen;
            self.aimed = item;
            cx.notify();
        }
    }

    fn accept(&mut self, cx: &mut Context<Self>) {
        let chosen = (self.items.iter())
            .map(|item| item.chosen.map(|at| self.choices[at].0.clone()))
            .collect();
        cx.emit(ChoiceDialogEvent::Chosen(chosen));
    }

    fn item_row(
        &self,
        index: usize,
        item: &Item,
        aim_shown: bool,
        colors: &bviz_ui_theme::ThemeColors,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<C> {
        let this = cx.weak_entity();
        let options = iter::once(None).chain((0..self.choices.len()).map(Some));
        let choices = ToggleGroup::one_of(
            ("choice-dialog-item", index),
            item.item.name.clone(),
            options,
            item.chosen,
            |option| {
                ToggleSegment::new(option.map_or(SKIP.into(), |at| self.choices[at].1.clone()))
            },
            move |option, _, cx| {
                this.update(cx, |dialog, cx| dialog.choose(index, option, cx))
                    .ok();
            },
        );
        let aim = if aim_shown && index == self.aimed {
            colors.border_focused
        } else {
            gpui::transparent_black()
        };
        div()
            .flex()
            .flex_col()
            .items_start()
            .gap_1()
            .p_1()
            .border_1()
            .rounded_md()
            .border_color(aim)
            .child(
                div()
                    .w_full()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(item.item.name.clone()),
            )
            .children(item.item.detail.clone().map(|detail| {
                div()
                    .w_full()
                    .text_xs()
                    .text_color(colors.text_muted)
                    .child(detail)
            }))
            .child(choices)
    }
}

/// `chosen` moved `by` places along a row of the skip and then `choices`
/// options, held at either end.
fn stepped(chosen: Option<usize>, by: isize, choices: usize) -> Option<usize> {
    let at = chosen.map_or(0, |chosen| chosen + 1);
    at.saturating_add_signed(by).min(choices).checked_sub(1)
}

impl<C: 'static> Focusable for ChoiceDialog<C> {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl<C: Clone + 'static> Render for ChoiceDialog<C> {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = bviz_ui_theme::theme(window, cx).colors;
        let aim_shown = self.focus.is_focused(window);
        let mut rows = Vec::with_capacity(self.items.len());
        for (index, item) in self.items.iter().enumerate() {
            rows.push(self.item_row(index, item, aim_shown, &colors, cx));
        }
        let accept = Button::new(
            "choice-dialog-accept",
            cx.listener(|dialog, _, _, cx| dialog.accept(cx)),
        )
        .label(self.accept.clone())
        .keys_for(&Accept, &self.focus);
        let cancel = Button::new(
            "choice-dialog-cancel",
            cx.listener(|_, _, _, cx| cx.emit(ChoiceDialogEvent::Cancelled)),
        )
        .ghost()
        .label("Cancel")
        .keys_for(&Cancel, &self.focus);
        dialog::sheet(self.title.clone(), &colors)
            .w(SHEET_WIDTH)
            .on_action(cx.listener(|dialog, _: &Accept, _, cx| dialog.accept(cx)))
            .on_action(cx.listener(|_, _: &Cancel, _, cx| cx.emit(ChoiceDialogEvent::Cancelled)))
            .child(
                div()
                    .text_sm()
                    .text_color(colors.text_muted)
                    .child(self.explanation.clone()),
            )
            .child(self.list(rows, &colors, cx))
            .child(dialog::buttons([cancel, accept]))
    }
}

impl<C: Clone + 'static> ChoiceDialog<C> {
    /// The items' `rows`, scrolling under a bar once they stand taller than
    /// [`MAX_LIST_HEIGHT`].
    fn list(
        &self,
        rows: Vec<impl IntoElement>,
        colors: &bviz_ui_theme::ThemeColors,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let list = div()
            .id("choice-dialog-items")
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(|dialog, _: &AimPrevious, _, cx| dialog.aim(-1, cx)))
            .on_action(cx.listener(|dialog, _: &AimNext, _, cx| dialog.aim(1, cx)))
            .on_action(cx.listener(|dialog, _: &PreviousChoice, _, cx| dialog.step_choice(-1, cx)))
            .on_action(cx.listener(|dialog, _: &NextChoice, _, cx| dialog.step_choice(1, cx)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|dialog, event: &MouseDownEvent, _, cx| {
                    if dialog.scroll.press(event.position) {
                        cx.stop_propagation();
                        cx.notify();
                    }
                }),
            )
            .flex()
            .flex_col()
            .gap_1()
            .max_h(MAX_LIST_HEIGHT)
            .pr(px(SCROLLBAR_THICKNESS))
            .overflow_y_scroll()
            .track_scroll(self.scroll.handle())
            .children(rows);
        let bar = list_scrollbar(
            cx.entity(),
            |dialog: &Self| &dialog.scroll,
            |dialog: &mut Self| &mut dialog.scroll,
            ScrollbarColors::from_colors(colors),
        );
        div().relative().child(list).child(bar)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap_probe::{dispatched, is};

    #[test]
    fn stepping_runs_from_skipping_through_the_choices_and_stops_at_either_end() {
        assert_eq!(stepped(None, 1, 3), Some(0));
        assert_eq!(stepped(None, -1, 3), None);
        assert_eq!(stepped(Some(0), -1, 3), None);
        assert_eq!(stepped(Some(2), 1, 3), Some(2));
        assert_eq!(stepped(Some(0), 1, 3), Some(1));
    }

    /// The arrows move through the list and Enter still answers the dialog it
    /// stands in.
    #[test]
    fn the_list_answers_the_arrows_and_the_dialog_answers_enter() {
        let in_the_list = [dialog::KEY_CONTEXT.id(), KEY_CONTEXT.id()];
        let keys = || bindings().into_iter().chain(dialog::bindings()).collect();
        assert!(is(dispatched(keys(), "up", &in_the_list), AimPrevious));
        assert!(is(dispatched(keys(), "right", &in_the_list), NextChoice));
        assert!(is(dispatched(keys(), "enter", &in_the_list), Accept));
        assert!(is(dispatched(keys(), "escape", &in_the_list), Cancel));
    }
}
