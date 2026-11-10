//! A view's colour key, opened from a toggle on its toolbar so it never covers
//! what it explains until asked for.

use crate::focus::focus_went_nowhere;
use crate::gesture::{Button as PointerButton, Entry, GestureRow, Gestures, Input, Place, Pointer};
use crate::{
    Button, KeyScope, MenuItem, OverflowRows, Priority, TextTooltip, ToolbarGroup, menu_chrome,
};
use bviz_ui_theme::ThemeColors;
use gpui::{
    App, AvailableSpace, Bounds, Context, Div, ElementId, Entity, EntityId, FocusHandle, Font,
    Hsla, IntoElement, KeyBinding, MouseButton, Pixels, SharedString, Subscription,
    WeakFocusHandle, Window, div, prelude::*, px, size,
};
use std::rc::Rc;

gpui::actions!(
    bviz_legend,
    [
        /// Show legend
        ShowLegend,
        /// Close legend
        CloseLegend,
    ]
);

/// The key context the open popover carries, which [`CloseLegend`] is scoped
/// to.
pub(crate) const KEY_CONTEXT: KeyScope = KeyScope::new("Legend", "Legend");

pub(crate) fn bind_keys(cx: &mut App) {
    cx.bind_keys([KeyBinding::new(
        "escape",
        CloseLegend,
        Some(KEY_CONTEXT.id()),
    )]);
}

/// The key showing the legend of a view carrying `scope`, which stands down
/// while a text field in it holds the focus.
pub fn legend_key_bindings(scope: KeyScope) -> [KeyBinding; 1] {
    let predicate = crate::text_input::unless_typing(scope);
    [KeyBinding::new("l", ShowLegend, Some(predicate.as_str()))]
}

#[derive(Clone, Copy)]
enum LegendIntent {
    Toggle,
}

const GESTURES: Gestures<LegendIntent> = Gestures::new(&[Entry::new(
    Input::click(PointerButton::Left),
    Place::On("a legend toggle"),
    "Show or hide legend",
    LegendIntent::Toggle,
)]);

/// The gestures a [`Legend`] answers to, for the view holding one to list.
pub fn legend_gestures() -> impl Iterator<Item = GestureRow> {
    GESTURES.rows()
}

/// How long a legend's toolbar group holds out against its row narrowing:
/// less than any other group.
const LEGEND_PRIORITY: Priority = 0;

/// What a [`LegendToggle`] and the legend's overflow menu row read, and the
/// icon the toggle carries.
const TOGGLE_LABEL: &str = "legend";
const TOGGLE_ICON: &str = "swatches";

/// Every swatch's box, so a swatch of one kind lines up with one of another.
const SWATCH_WIDTH: Pixels = px(16.);
const SWATCH_HEIGHT: Pixels = px(10.);

/// Stroke of a line swatch, and the dash and gap of a dashed one.
const LINE_WIDTH: Pixels = px(2.);
const DASH: Pixels = px(4.);
const DASH_GAP: Pixels = px(2.);

/// Diameter of the small dot of a [`Swatch::Sizes`]; its large dot fills the
/// swatch's height.
const SMALL_DOT: Pixels = px(4.);

/// Text size of everything the popover reads.
const TEXT_SIZE: Pixels = px(12.);

/// Room between a swatch and its label, and between entries.
const LABEL_GAP: Pixels = px(4.);
const ENTRY_GAP: Pixels = px(12.);

/// Most columns of entries the popover lays its key out in.
const POPOVER_COLUMNS: u16 = 2;

/// Room the popover leaves on either side of its key.
const POPOVER_INSET: Pixels = px(12.);

/// Widest a note in the popover runs before it wraps.
const NOTE_WIDTH: Pixels = px(320.);

/// Paints a mark into the bounds of a swatch.
pub type PaintSwatch = Rc<dyn Fn(Bounds<Pixels>, &mut Window, &mut App)>;

/// How an entry's swatch is drawn, to match the marks it names.
#[derive(Clone, IntoElement)]
pub enum Swatch {
    Fill(Hsla),
    /// A fill edged in another colour.
    Outlined {
        fill: Hsla,
        edge: Hsla,
    },
    /// A fill wearing a heavy ring, as a halo marks a box.
    Ring {
        fill: Hsla,
        ring: Hsla,
    },
    Line(Hsla),
    DashedLine(Hsla),
    Dot(Hsla),
    /// A small and a large dot, keying a mark's size rather than its colour.
    Sizes(Hsla),
    /// Fills from the least end of a scale to the most.
    Ramp(Rc<[Hsla]>),
    /// A mark drawn by the view itself into the swatch's box.
    Painted(PaintSwatch),
}

impl Swatch {
    fn element(&self) -> Div {
        let frame = div()
            .flex()
            .flex_none()
            .items_center()
            .w(SWATCH_WIDTH)
            .h(SWATCH_HEIGHT);
        let dot = |diameter: Pixels, color: Hsla| {
            div().flex_none().size(diameter).rounded_full().bg(color)
        };
        match self {
            Swatch::Fill(fill) => frame.rounded_sm().bg(*fill),
            Swatch::Outlined { fill, edge } => {
                frame.rounded_sm().bg(*fill).border_1().border_color(*edge)
            }
            Swatch::Ring { fill, ring } => {
                frame.rounded_sm().bg(*fill).border_2().border_color(*ring)
            }
            Swatch::Line(color) => frame.child(div().w_full().h(LINE_WIDTH).bg(*color)),
            Swatch::DashedLine(color) => {
                let dashes = ((SWATCH_WIDTH + DASH_GAP) / (DASH + DASH_GAP)) as usize;
                frame
                    .gap(DASH_GAP)
                    .children((0..dashes).map(|_| div().w(DASH).h(LINE_WIDTH).bg(*color)))
            }
            Swatch::Dot(color) => frame.justify_center().child(dot(SWATCH_HEIGHT, *color)),
            Swatch::Sizes(color) => frame
                .justify_between()
                .child(dot(SMALL_DOT, *color))
                .child(dot(SWATCH_HEIGHT, *color)),
            Swatch::Ramp(fills) => frame
                .rounded_sm()
                .overflow_hidden()
                .children(fills.iter().map(|&fill| div().flex_1().h_full().bg(fill))),
            Swatch::Painted(paint) => {
                let paint = paint.clone();
                frame
                    .relative()
                    .child(crate::overlay_canvas(move |bounds, window, cx| {
                        paint(bounds, window, cx)
                    }))
            }
        }
    }
}

impl RenderOnce for Swatch {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        self.element()
    }
}

/// One mark a legend names: its swatch beside its label.
#[derive(Clone, IntoElement)]
pub struct LegendEntry {
    swatch: Swatch,
    label: SharedString,
    tooltip: Option<SharedString>,
}

impl LegendEntry {
    pub fn new(swatch: Swatch, label: impl Into<SharedString>) -> Self {
        Self {
            swatch,
            label: label.into(),
            tooltip: None,
        }
    }

    /// The entry explaining itself by `tooltip` under the pointer.
    pub fn tooltip(mut self, tooltip: impl Into<SharedString>) -> Self {
        self.tooltip = Some(tooltip.into());
        self
    }

    fn body(&self) -> Div {
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(LABEL_GAP)
            .text_size(TEXT_SIZE)
            .whitespace_nowrap()
            .child(self.swatch.element())
            .child(self.label.clone())
    }
}

/// An entry's tooltip is keyed by its label, so the entries sharing a parent
/// carry distinct labels.
impl RenderOnce for LegendEntry {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let body = self.body();
        match self.tooltip {
            Some(tooltip) => {
                let colors = bviz_ui_theme::theme(window, cx).colors.tooltip();
                body.id(ElementId::Name(self.label))
                    .tooltip(TextTooltip::builder(tooltip, colors))
                    .into_any_element()
            }
            None => body.into_any_element(),
        }
    }
}

/// The popover a [`LegendState`] has open.
struct Opened {
    /// Where focus stood when the popover opened, which closing it hands focus
    /// back to while it stands: a menu row opening the popover leaves focus on
    /// a menu that is let go as it closes.
    from: WeakFocusHandle,
    /// Closes the popover as focus leaves it.
    _dismiss: Subscription,
}

/// Whether a legend's popover is open. The view showing the legend holds it,
/// so that its [`ShowLegend`] handler can open the popover.
pub struct LegendState {
    /// Focus of the view the legend belongs to: what the toggle's tooltip looks
    /// [`ShowLegend`]'s keys up in, and where closing the popover hands focus
    /// when nothing held it as the popover opened.
    view_focus: FocusHandle,
    /// The view the legend belongs to, which draws the popover and so is
    /// notified whenever it opens or closes.
    view: EntityId,
    popover_focus: FocusHandle,
    drawn: OnScreen,
    open: Option<Opened>,
    /// The widths the legend last laid out was measured at, which a frame
    /// laying out the same key in the same font reuses.
    measured: Option<Rc<Measured>>,
}

impl LegendState {
    /// The legend of the view `cx` belongs to, whose focus is `view_focus`.
    pub fn new<V: 'static>(view_focus: FocusHandle, cx: &mut Context<V>) -> Entity<Self> {
        let view = cx.entity_id();
        cx.new(|cx| Self {
            view_focus,
            view,
            popover_focus: cx.focus_handle(),
            drawn: OnScreen::new(cx),
            open: None,
            measured: None,
        })
    }

    fn opened_or_closed(&self, cx: &mut Context<Self>) {
        cx.notify();
        App::notify(cx, self.view);
    }

    pub fn is_open(&self) -> bool {
        self.open.is_some()
    }

    /// Opens the popover, taking focus into it so that an outside click or
    /// Escape closes it. A legend the last frame did not draw, or that keys
    /// nothing, has no popover to open.
    pub fn show(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let keys_something = self.measured.as_ref().is_some_and(|m| !m.keys_nothing());
        if self.open.is_some() || !keys_something || !self.drawn.in_last_frame(window) {
            return;
        }
        let dismiss = cx.on_focus_out(&self.popover_focus, window, |this, _, window, cx| {
            if focus_went_nowhere(&this.popover_focus, window, cx) {
                this.close(window, cx);
            } else if this.open.take().is_some() {
                this.opened_or_closed(cx);
            }
        });
        self.open = Some(Opened {
            from: window
                .focused(cx)
                .unwrap_or_else(|| self.view_focus.clone())
                .downgrade(),
            _dismiss: dismiss,
        });
        self.popover_focus.focus(window, cx);
        self.opened_or_closed(cx);
    }

    /// Closes the popover, handing focus back where it stood when the popover
    /// opened, or to the view where that is gone.
    pub fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(Opened { from, .. }) = self.open.take() {
            let to = from.upgrade().unwrap_or_else(|| self.view_focus.clone());
            window.focus(&to, cx);
            self.opened_or_closed(cx);
        }
    }

    fn toggle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.is_open() {
            true => self.close(window, cx),
            false => self.show(window, cx),
        }
    }
}

/// A view's legend, standing on its toolbar as the [`toggle`](Self::toggle)
/// or the [`menu_row`](Self::menu_row) opening the [`popover`](Self::popover),
/// which shows every entry and the notes.
#[derive(Clone)]
pub struct Legend {
    id: ElementId,
    state: Entity<LegendState>,
    entries: Vec<LegendEntry>,
    notes: Vec<SharedString>,
}

impl Legend {
    pub fn new(id: impl Into<ElementId>, state: &Entity<LegendState>) -> Self {
        Self {
            id: id.into(),
            state: state.clone(),
            entries: Vec::new(),
            notes: Vec::new(),
        }
    }

    pub fn entry(mut self, entry: LegendEntry) -> Self {
        self.entries.push(entry);
        self
    }

    pub fn entries(mut self, entries: impl IntoIterator<Item = LegendEntry>) -> Self {
        self.entries.extend(entries);
        self
    }

    /// A line on how to read the key, shown only in the popover.
    pub fn note(mut self, note: impl Into<SharedString>) -> Self {
        self.notes.push(note.into());
        self
    }

    /// The legend standing as a toggle that opens the whole key under itself.
    pub fn toggle(self) -> LegendToggle {
        LegendToggle {
            legend: self,
            labelled: true,
        }
    }

    /// The legend as the overflow menu row opening the popover, naming the
    /// keys of [`ShowLegend`].
    pub fn menu_row(&self, window: &Window, cx: &App) -> MenuItem {
        let state = self.state.clone();
        let row = MenuItem::new(TOGGLE_LABEL, move |window, cx| {
            state.update(cx, |state, cx| state.show(window, cx));
        })
        .keys_for(&ShowLegend, &self.state.read(cx).view_focus, window);
        match self.keys_nothing() {
            true => row.disabled(),
            false => row,
        }
    }

    /// The popover while it is open, opened under the element it is laid out
    /// over, which [`ToolbarGroup::anchoring`] places where the legend is
    /// reached. [`LegendState::show`] opens it only in a frame after one that
    /// drew this, open or not.
    pub fn popover(self) -> LegendPopover {
        LegendPopover { legend: self }
    }

    /// The legend as the group ending a toolbar: its toggle, standing icon-only
    /// once the row has no room for its name, parted from the controls before
    /// it, and then its menu row.
    pub fn toolbar_group(self, window: &Window, cx: &App) -> ToolbarGroup {
        let overflow = OverflowRows::row(self.menu_row(window, cx), false);
        ToolbarGroup::new(LEGEND_PRIORITY, self.clone().toggle(), overflow)
            .or(self.clone().toggle().icon_only())
            .anchoring(self.popover())
            .parted()
    }

    fn keys_nothing(&self) -> bool {
        self.entries.is_empty() && self.notes.is_empty()
    }

    /// Whether the popover shows in a frame reading `seen`: a legend keying
    /// nothing has nothing to show, whatever its state.
    fn shows(&self, seen: &Seen) -> bool {
        seen.open && !self.keys_nothing()
    }

    /// What this frame reads of the legend's state.
    fn seen(&self, cx: &App) -> Seen {
        let state = self.state.read(cx);
        Seen {
            view_focus: state.view_focus.clone(),
            popover_focus: state.popover_focus.clone(),
            drawn: state.drawn.clone(),
            open: state.is_open(),
        }
    }

    /// The widths this legend lays out at: those `state` holds where they were
    /// measured for the same key in the font of this frame, or else measured
    /// afresh and kept there.
    fn widths(&self, window: &mut Window, cx: &mut App) -> Rc<Measured> {
        let font = window.text_style().font();
        let held = self.state.read(cx).measured.clone();
        if let Some(held) = held.filter(|held| held.is_of(self, &font)) {
            return held;
        }
        let measured = Rc::new(Measured::new(self, font, window, cx));
        let kept = measured.clone();
        self.state
            .update(cx, |state, _| state.measured = Some(kept));
        measured
    }

    /// Every entry and note, opened under the element it is a child of, the
    /// key laid out in `columns`.
    fn sheet(
        &self,
        seen: &Seen,
        columns: u16,
        measured: &Measured,
        colors: &ThemeColors,
    ) -> impl IntoElement + use<> {
        let state = self.state.clone();
        let key = div()
            .id("popover-key")
            .grid()
            .grid_cols(columns)
            .gap_x(ENTRY_GAP)
            .gap_y_1()
            .children(self.entries.iter().cloned());
        let notes = self
            .notes
            .iter()
            .zip(&measured.note_widths)
            .map(|(text, &width)| note(text).w(width).text_color(colors.text_muted));
        let sheet = menu_chrome::sheet(colors)
            .key_context(KEY_CONTEXT)
            .track_focus(&seen.popover_focus)
            .on_action(move |_: &CloseLegend, window, cx| {
                state.update(cx, |state, cx| state.close(window, cx));
            })
            .px(POPOVER_INSET)
            .gap_2()
            .text_size(TEXT_SIZE)
            .text_color(colors.text)
            .child(key)
            .children(notes);
        menu_chrome::under_trigger(sheet)
    }
}

/// A [`Legend`] standing as a toggle, made by [`Legend::toggle`].
#[derive(IntoElement)]
pub struct LegendToggle {
    legend: Legend,
    labelled: bool,
}

impl LegendToggle {
    /// The toggle bearing its icon alone, its name left to the tooltip.
    pub fn icon_only(mut self) -> Self {
        self.labelled = false;
        self
    }
}

impl RenderOnce for LegendToggle {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let legend = self.legend;
        let seen = legend.seen(cx);
        let open = legend.shows(&seen);
        let state = legend.state.clone();
        let toggle = Button::new(legend.id.clone(), move |event, window, cx| {
            GESTURES.dispatch(Pointer::click(event), |intent| match intent {
                LegendIntent::Toggle => {
                    state.update(cx, |state, cx| state.toggle(window, cx));
                    true
                }
            });
        })
        .when(self.labelled, |toggle| toggle.label(TOGGLE_LABEL))
        .leading_icon(TOGGLE_ICON)
        .selected(open)
        .disabled(legend.keys_nothing())
        // A tooltip would cover the popover the toggle holds open.
        .when(!open, |toggle| {
            toggle.keys_for(&ShowLegend, &seen.view_focus)
        });
        div()
            .flex_none()
            // Keeps the press from an ancestor that focuses on it, which would
            // close the popover just before the click toggles it open again.
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(toggle)
    }
}

/// A [`Legend`]'s popover, made by [`Legend::popover`].
#[derive(IntoElement)]
pub struct LegendPopover {
    legend: Legend,
}

impl RenderOnce for LegendPopover {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let legend = self.legend;
        let measured = legend.widths(window, cx);
        let seen = legend.seen(cx);
        let sheet = legend.shows(&seen).then(|| {
            let colors = bviz_ui_theme::theme(window, cx).colors;
            let inset = window.client_inset().unwrap_or_default();
            let room = window.viewport_size().width - inset * 2.;
            let columns = popover_columns(&measured.entry_widths, room);
            legend.sheet(&seen, columns, &measured, &colors)
        });
        div().size_full().children(sheet).child(seen.drawn.mark())
    }
}

/// Marks the frames a legend is drawn in, for [`LegendState::show`] to tell
/// the legend is on screen by. Layout runs neither for a frame leaving the
/// legend out nor for one replaying it from a cache, so only what the frame
/// carries tells the two apart. gpui tells whether a frame carries a focus
/// handle only by whether one handle contains another, so the mark carries
/// two.
#[derive(Clone)]
struct OnScreen {
    outer: FocusHandle,
    inner: FocusHandle,
}

impl OnScreen {
    fn new(cx: &mut App) -> Self {
        Self {
            outer: cx.focus_handle(),
            inner: cx.focus_handle(),
        }
    }

    /// The mark a frame carries, of no size so that no click focuses it.
    fn mark(&self) -> Div {
        div()
            .absolute()
            .track_focus(&self.outer)
            .child(div().track_focus(&self.inner))
    }

    fn in_last_frame(&self, window: &Window) -> bool {
        self.outer.contains(&self.inner, window)
    }
}

/// What a frame of a [`Legend`] reads of its [`LegendState`].
struct Seen {
    view_focus: FocusHandle,
    popover_focus: FocusHandle,
    drawn: OnScreen,
    open: bool,
}

/// The widths a legend's key lays out at in one font. An entry's width
/// follows from its label alone, as every swatch is one size.
pub struct Measured {
    font: Font,
    labels: Vec<SharedString>,
    note_texts: Vec<SharedString>,
    entry_widths: Vec<Pixels>,
    /// Each note's unwrapped width, up to `NOTE_WIDTH`, which it is held to:
    /// the sheet takes its width from what it holds, so a note sizing itself
    /// would wrap as narrow as the sheet's holder.
    note_widths: Vec<Pixels>,
}

impl Measured {
    fn new(legend: &Legend, font: Font, window: &mut Window, cx: &mut App) -> Self {
        let entry_widths = legend
            .entries
            .iter()
            .map(|entry| measured(entry.body(), window, cx))
            .collect();
        let note_widths = legend
            .notes
            .iter()
            .map(|text| measured(note(text), window, cx).min(NOTE_WIDTH))
            .collect();
        Self {
            font,
            labels: legend
                .entries
                .iter()
                .map(|entry| entry.label.clone())
                .collect(),
            note_texts: legend.notes.clone(),
            entry_widths,
            note_widths,
        }
    }

    /// Whether these are the widths of `legend`'s key in `font`.
    fn is_of(&self, legend: &Legend, font: &Font) -> bool {
        self.font == *font
            && self.note_texts == legend.notes
            && self
                .labels
                .iter()
                .eq(legend.entries.iter().map(|entry| &entry.label))
    }

    fn keys_nothing(&self) -> bool {
        self.labels.is_empty() && self.note_texts.is_empty()
    }
}

/// The most columns, up to `POPOVER_COLUMNS` and one for each entry, that
/// entries `widths` wide lay out in within `room` across.
fn popover_columns(widths: &[Pixels], room: Pixels) -> u16 {
    let widest = widths.iter().copied().fold(px(0.), Pixels::max);
    let most = POPOVER_COLUMNS.min(widths.len().max(1) as u16);
    let edges = (POPOVER_INSET + menu_chrome::SHEET_BORDER) * 2.;
    (1..=most)
        .rev()
        .find(|&columns| {
            let columns = f32::from(columns);
            widest * columns + ENTRY_GAP * (columns - 1.) + edges <= room
        })
        .unwrap_or(1)
}

/// A note of the popover.
fn note(text: &SharedString) -> Div {
    div().text_size(TEXT_SIZE).child(text.clone())
}

/// How wide `element` lays out given all the room it asks for.
fn measured(element: impl IntoElement, window: &mut Window, cx: &mut App) -> Pixels {
    let content = size(AvailableSpace::MaxContent, AvailableSpace::MaxContent);
    element
        .into_any_element()
        .layout_as_root(content, window, cx)
        .width
}

#[cfg(test)]
mod tests {
    use super::*;

    fn widths(each: &[f32]) -> Vec<Pixels> {
        each.iter().copied().map(px).collect()
    }

    #[test]
    fn a_popover_takes_one_column_where_two_overflow_its_room() {
        let widths = widths(&[300., 100., 100.]);
        let edges = (POPOVER_INSET + menu_chrome::SHEET_BORDER) * 2.;
        let two = px(600.) + ENTRY_GAP + edges;
        assert_eq!(popover_columns(&widths, two), 2);
        assert_eq!(popover_columns(&widths, two - px(1.)), 1);
        assert_eq!(popover_columns(&widths, px(10.)), 1);
    }

    #[test]
    fn a_popover_takes_no_more_columns_than_entries() {
        assert_eq!(popover_columns(&widths(&[50.]), px(1000.)), 1);
        assert_eq!(popover_columns(&[], px(1000.)), 1);
    }

    #[test]
    fn the_toggle_click_is_reachable_and_fits_the_keys_help() {
        assert_eq!(GESTURES.unreachable(), []);
        assert_eq!(GESTURES.overlong(), []);
    }
}
