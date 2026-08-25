//! A multi-select dropdown: a trigger summarizing how many options are checked
//! and a menu of checkable rows that stays open across several toggles.

use crate::CONTROL_HEIGHT;
use crate::dropdown::{MENU_GAP, chosen_hover};
use bviz_ui_theme::ThemeColors;
use gpui::{
    ClickEvent, Context, ElementId, EventEmitter, FocusHandle, IntoElement, MouseButton, Pixels,
    Render, SharedString, Subscription, Window, deferred, div, prelude::*, px,
};
use std::collections::HashSet;

/// Emitted when the user checks or unchecks an option, carrying the option's
/// index. Programmatic [`set_selected`](MultiSelect::set_selected) does NOT
/// emit — only a user toggle does — so a subscriber reacts to the user's
/// choices, not to the entity's other repaints.
pub enum MultiSelectEvent {
    Changed(usize),
}

/// A dropdown over a fixed list of options, any number of which may be
/// checked. `label` names what the trigger's count summarizes (e.g. "kinds").
/// Checked options are read with [`selected`](Self::selected); subscribe to
/// [`MultiSelectEvent`] to react to a toggle.
pub struct MultiSelect {
    id: ElementId,
    label: SharedString,
    options: Vec<SharedString>,
    selected: HashSet<usize>,
    open: bool,
    focus_handle: FocusHandle,
    /// Closes the menu when focus leaves it (an outside click); installed lazily on
    /// the first render, where the window is in hand.
    dismiss: Option<Subscription>,
}

impl MultiSelect {
    pub fn new(
        id: impl Into<ElementId>,
        label: impl Into<SharedString>,
        options: impl IntoIterator<Item = impl Into<SharedString>>,
        selected: HashSet<usize>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            options: options.into_iter().map(Into::into).collect(),
            selected,
            open: false,
            focus_handle: cx.focus_handle(),
            dismiss: None,
        }
    }

    /// The indices currently checked.
    pub fn selected(&self) -> &HashSet<usize> {
        &self.selected
    }

    /// Replace the checked set without notifying observers or emitting.
    pub fn set_selected(&mut self, selected: HashSet<usize>, cx: &mut Context<Self>) {
        self.selected = selected;
        cx.notify();
    }

    /// Flips whether `index` is checked, leaving the menu open so the user can
    /// keep toggling, and notifies observers of the choice.
    fn toggle(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.options.len() {
            return;
        }
        if !self.selected.remove(&index) {
            self.selected.insert(index);
        }
        cx.emit(MultiSelectEvent::Changed(index));
        cx.notify();
    }

    /// The trigger's summary text at a given checked count, so the width below
    /// can be measured at every count the count could ever show.
    fn summary(&self, checked: usize) -> String {
        format!("{} ({checked}/{})", self.label, self.options.len())
    }

    /// Width of the widest count the trigger could show, so it keeps a fixed
    /// width as the checked count changes.
    fn trigger_width(&self, window: &mut Window) -> Pixels {
        let metrics = bviz_ui_assets::text_metrics(window);
        let measure_color = window.text_style().color;
        let mut measured = px(0.);
        for checked in 0..=self.options.len() {
            let width = bviz_ui_assets::shape_line(
                window,
                self.summary(checked),
                metrics.font.clone(),
                metrics.font_size,
                measure_color,
            )
            .width;
            measured = measured.max(width);
        }
        // Round up and add slack so the label sits on one line: at the exact
        // measured width, sub-pixel rounding between measuring and laying out the
        // glyph run can push it onto a second line.
        px(f32::from(measured).ceil() + 6.)
    }

    fn trigger(
        &self,
        colors: &ThemeColors,
        trigger_width: Pixels,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let hover = colors.element_hover;
        let label = self.summary(self.selected.len());
        div()
            .id(self.id.clone())
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .px_2()
            .h(CONTROL_HEIGHT)
            // Hold the trigger's measured width in a flex parent (e.g. a toolbar);
            // without this it shrinks and the label wraps to a second line.
            .flex_none()
            .rounded_md()
            .cursor_pointer()
            .text_color(colors.text)
            .hover(move |style| style.bg(hover))
            // Keep the press from reaching an ancestor that focuses on mouse-down
            // (e.g. a toolbar); that would blur the menu and, via the dismiss-on-
            // blur below, close it just before this click reopens it.
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.open = !this.open;
                // Hold focus while open so an outside click dismisses the menu.
                if this.open {
                    this.focus_handle.focus(window, cx);
                }
                cx.notify();
            }))
            // flex_none + nowrap so the flex row never shrinks the label below the
            // widest count, which would wrap the text and overflow the trigger.
            .child(
                div()
                    .w(trigger_width)
                    .flex_none()
                    .whitespace_nowrap()
                    .child(label),
            )
            .child(
                bviz_ui_assets::icon("caret-down")
                    .size(px(12.))
                    .text_color(colors.text),
            )
    }

    fn menu(&self, colors: &ThemeColors, cx: &Context<Self>) -> impl IntoElement {
        let chosen_hover = chosen_hover(colors);
        let rows = self.options.iter().enumerate().map(|(index, option)| {
            let checked = self.selected.contains(&index);
            let hover = if checked {
                chosen_hover
            } else {
                colors.element_hover
            };
            div()
                .id(("multi-select-option", index))
                .flex()
                .items_center()
                .gap_2()
                .px_2()
                .py_1()
                .cursor_pointer()
                .text_color(colors.text)
                .when(checked, |row| row.bg(colors.selection))
                .hover(move |style| style.bg(hover))
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.toggle(index, cx)))
                .child(div().size(px(12.)).flex_none().when(checked, |mark| {
                    mark.child(
                        bviz_ui_assets::icon("check")
                            .size(px(12.))
                            .text_color(colors.text),
                    )
                }))
                .child(div().whitespace_nowrap().child(option.clone()))
        });
        // Defer the menu so it paints over the content below the trigger; its
        // layout stays in place, anchored just under the trigger.
        deferred(
            div()
                .occlude()
                .absolute()
                .top(CONTROL_HEIGHT + MENU_GAP)
                .left_0()
                // No explicit width: the menu shrinks to fit its options, so a
                // row never wraps. A fixed width would have to budget for the row
                // padding and border, which is error-prone.
                .flex()
                .flex_col()
                .bg(colors.panel)
                .border_1()
                .border_color(colors.border)
                .rounded_md()
                .children(rows),
        )
    }
}

impl EventEmitter<MultiSelectEvent> for MultiSelect {}

impl Render for MultiSelect {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Close on an outside click: the menu holds focus while open (see the
        // trigger handler), so losing it means a click landed elsewhere.
        if self.dismiss.is_none() {
            self.dismiss = Some(
                cx.on_focus_out(&self.focus_handle, window, |this, _, _, cx| {
                    if this.open {
                        this.open = false;
                        cx.notify();
                    }
                }),
            );
        }

        let colors = bviz_ui_theme::theme(window, cx).colors;
        let trigger_width = self.trigger_width(window);

        div()
            .relative()
            .track_focus(&self.focus_handle)
            .child(self.trigger(&colors, trigger_width, cx))
            .children(self.open.then(|| self.menu(&colors, cx)))
    }
}
