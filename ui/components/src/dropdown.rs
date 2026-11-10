//! A single-select dropdown: a trigger showing the chosen option and a menu that
//! opens from it.

use crate::popup::{self, Popup};
use crate::{Hint, KeyScope, SubmenuItem, menu_chrome};
use bviz_ui_theme::ThemeColors;
use gpui::{
    Action, App, ClickEvent, Context, ElementId, Entity, EventEmitter, FocusHandle, IntoElement,
    KeyBinding, Pixels, Render, SharedString, Subscription, Window, div, prelude::*,
};

gpui::actions!(
    bviz_dropdown,
    [
        /// Close list
        Dismiss,
    ]
);

/// The key context the dropdown carries, which [`Dismiss`] is scoped to.
pub(crate) const KEY_CONTEXT: KeyScope = KeyScope::new("Dropdown", "Dropdown");

pub(crate) fn bind_keys(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("escape", Dismiss, Some(KEY_CONTEXT.id()))]);
}

/// Emitted when the user picks a different option. Programmatic
/// [`set_selected`](Dropdown::set_selected) does NOT emit — only a user choice —
/// so a subscriber reacts to selections, not to the entity's other repaints
/// (opening, closing, hover) that a plain `observe` would also deliver.
pub enum DropdownEvent {
    Changed(usize),
}

/// One option a [`Dropdown`] offers.
struct Choice {
    label: SharedString,
    /// Whether picking it does anything. An option the data behind the control
    /// cannot answer is drawn greyed and cannot be picked, so that it says it
    /// can do nothing rather than silently doing nothing.
    available: bool,
}

/// A dropdown over a fixed list of options. The chosen index is read with
/// [`selected`](Self::selected); subscribe to [`DropdownEvent`] to react to a
/// user's selection change.
pub struct Dropdown {
    id: ElementId,
    options: Vec<Choice>,
    selected: usize,
    /// What the trigger's tooltip reads.
    hint: Hint,
    popup: Entity<Popup>,
    /// Re-renders the dropdown, which draws the menu, as it opens or closes.
    _popup_changed: Subscription,
}

impl Dropdown {
    pub fn new(
        id: impl Into<ElementId>,
        options: impl IntoIterator<Item = impl Into<SharedString>>,
        selected: usize,
        cx: &mut Context<Self>,
    ) -> Self {
        let popup = cx.new(Popup::new);
        let popup_changed = cx.observe(&popup, |_, _, cx| cx.notify());
        Self {
            id: id.into(),
            options: options
                .into_iter()
                .map(|label| Choice {
                    label: label.into(),
                    available: true,
                })
                .collect(),
            selected,
            hint: Hint::default(),
            popup,
            _popup_changed: popup_changed,
        }
    }

    /// The tooltip reading `text`.
    pub fn tooltip(mut self, text: impl Into<SharedString>) -> Self {
        self.hint = self.hint.text(text);
        self
    }

    /// The tooltip naming the keys that run `action` where `focus` stands, as
    /// [`Hint::keys_for`] names them.
    pub fn keys_for(mut self, action: &dyn Action, focus: &FocusHandle) -> Self {
        self.hint = self.hint.keys_for(action, focus);
        self
    }

    /// The tooltip naming keys as [`Hint::keys_for_focused`] names them.
    pub fn keys_for_focused(mut self, action: &dyn Action) -> Self {
        self.hint = self.hint.keys_for_focused(action);
        self
    }

    /// Has the tooltip read `text`, for a hint that follows a state held
    /// elsewhere.
    pub fn set_tooltip(&mut self, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        let text = text.into();
        if !self.hint.reads(&text) {
            self.hint = std::mem::take(&mut self.hint).text(text);
            cx.notify();
        }
    }

    /// The index of the chosen option.
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// Makes the option at `index` one that can be picked, or one that cannot.
    pub fn set_available(&mut self, index: usize, available: bool, cx: &mut Context<Self>) {
        if let Some(choice) = self.options.get_mut(index)
            && choice.available != available
        {
            choice.available = available;
            cx.notify();
        }
    }

    /// Names the option at `index` `label`, for an option whose meaning
    /// follows a choice made elsewhere.
    pub fn set_label(
        &mut self,
        index: usize,
        label: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        let label = label.into();
        if let Some(choice) = self.options.get_mut(index)
            && choice.label != label
        {
            choice.label = label;
            cx.notify();
        }
    }

    /// Set the chosen index if in range and different, reporting whether it changed.
    fn select(&mut self, index: usize) -> bool {
        let changed = index < self.options.len() && index != self.selected;
        if changed {
            self.selected = index;
        }
        changed
    }

    /// Choose an option by index, leaving the menu and notifying observers.
    pub fn set_selected(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.select(index) {
            cx.notify();
        }
    }

    fn choose(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if !self.offers(index) {
            return;
        }
        self.popup.update(cx, |popup, cx| popup.close(window, cx));
        self.pick(index, cx);
    }

    /// Whether the option at `index` can be picked.
    fn offers(&self, index: usize) -> bool {
        (self.options.get(index)).is_some_and(|choice| choice.available)
    }

    /// Picks the option at `index` as a user's choice, where it can be picked.
    fn pick(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.offers(index) && self.select(index) {
            cx.emit(DropdownEvent::Changed(index));
        }
    }

    /// Picks the option after the chosen one, round to the first, as a user's
    /// choice, passing over those that cannot be picked.
    pub fn cycle(&mut self, cx: &mut Context<Self>) {
        if let Some(index) = next_available(&self.options, self.selected) {
            self.pick(index, cx);
        }
    }

    /// The options of `dropdown` as the rows of a submenu, the chosen one
    /// checked, each picking its option as a choice in the dropdown's own menu
    /// does, and each that cannot be picked explained by `unavailable`.
    pub fn submenu(
        dropdown: &Entity<Self>,
        unavailable: Option<SharedString>,
        cx: &App,
    ) -> Vec<SubmenuItem> {
        let this = dropdown.read(cx);
        (this.options.iter().enumerate())
            .map(|(index, option)| {
                let dropdown = dropdown.clone();
                let row = SubmenuItem::new(option.label.clone(), move |_, cx| {
                    dropdown.update(cx, |dropdown, cx| dropdown.pick(index, cx))
                })
                .checked(index == this.selected);
                match (option.available, &unavailable) {
                    (true, _) => row,
                    (false, None) => row.disabled(),
                    (false, Some(why)) => row.disabled().tooltip(why.clone()),
                }
            })
            .collect()
    }

    fn trigger(
        &self,
        colors: &ThemeColors,
        option_width: Pixels,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let label = self
            .options
            .get(self.selected)
            .map(|choice| choice.label.clone())
            .unwrap_or_default();
        let trigger = menu_chrome::trigger(
            self.id.clone(),
            colors,
            self.hint.clone(),
            (self.popup.downgrade(), Popup::is_open),
            option_width,
            label,
        );
        trigger.on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
            this.popup.update(cx, |popup, cx| popup.toggle(window, cx));
        }))
    }

    fn menu(&self, colors: &ThemeColors, cx: &Context<Self>) -> impl IntoElement {
        let rows = self.options.iter().enumerate().map(|(index, option)| {
            menu_chrome::row(
                ("dropdown-option", index).into(),
                colors,
                index == self.selected,
            )
            .when(!option.available, |row| {
                row.cursor_default().text_color(colors.text_disabled)
            })
            .on_click(
                cx.listener(move |this, _: &ClickEvent, window, cx| this.choose(index, window, cx)),
            )
            .child(div().whitespace_nowrap().child(option.label.clone()))
        });
        menu_chrome::under_trigger(menu_chrome::sheet(colors).children(rows))
    }
}

impl EventEmitter<DropdownEvent> for Dropdown {}

impl Render for Dropdown {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = bviz_ui_theme::theme(window, cx).colors;
        let option_width = menu_chrome::widest(
            window,
            self.options.iter().map(|choice| choice.label.clone()),
        );

        let open = self.popup.read(cx).is_open();
        popup::holder(&self.popup, cx)
            .child(self.trigger(&colors, option_width, cx))
            .children(open.then(|| self.menu(&colors, cx)))
    }
}

/// The place of the option after `selected` among `options` that can be
/// picked, round to the first; none where no other can.
fn next_available(options: &[Choice], selected: usize) -> Option<usize> {
    let count = options.len();
    (1..count)
        .map(|step| (selected + step) % count)
        .find(|&index| options[index].available)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(available: &[bool]) -> Vec<Choice> {
        (available.iter())
            .map(|&available| Choice {
                label: SharedString::default(),
                available,
            })
            .collect()
    }

    #[test]
    fn stepping_wraps_round_to_the_first_option() {
        assert_eq!(next_available(&options(&[true, true, true]), 0), Some(1));
        assert_eq!(next_available(&options(&[true, true, true]), 2), Some(0));
    }

    #[test]
    fn stepping_passes_over_an_option_that_cannot_be_picked() {
        assert_eq!(next_available(&options(&[true, false, true]), 0), Some(2));
        assert_eq!(next_available(&options(&[true, false, false]), 0), None);
    }
}
