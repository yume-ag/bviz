//! The chrome the component library's menus are drawn from — the dropdown and
//! the multi-select opening under a trigger, and the context menu standing over
//! the window at a point — so they read alike wherever they sit.

use crate::scrollbar::SCROLLBAR_THICKNESS;
use crate::table_row::picked_hover;
use crate::{CONTROL_HEIGHT, Hint, laid_out_width};
use bviz_ui_theme::{ThemeColors, over};
use gpui::{
    AnyElement, App, Bounds, Deferred, Div, Edges, Element, ElementId, GlobalElementId, Hsla,
    InspectorElementId, LayoutId, MouseButton, Pixels, Point, Position, SharedString, Size,
    Stateful, Style, Svg, WeakEntity, Window, deferred, div, point, prelude::*, px,
};
use std::panic::Location;

/// Breathing room between a trigger and the menu it opens.
const MENU_GAP: Pixels = px(2.);

/// Side of the marks a menu's chrome draws.
pub(crate) const MENU_ICON: Pixels = px(12.);

/// Room a row leaves above and below its label.
const ROW_PADDING: Pixels = px(4.);

/// Room between what a row reads and the keys or the mark at the end of it.
pub const LABEL_GAP: Pixels = px(16.);

/// Room a row leaves at either end of what it holds, set in pixels like the rest
/// of a row's spacing so it holds whatever the root font size.
pub const ROW_INSET: Pixels = px(8.);

/// The line drawn around a sheet.
pub const SHEET_BORDER: Pixels = px(1.);

/// Room a sheet leaves above its first row and below its last, so the fill of
/// either stays clear of the sheet's rounded corners.
pub const SHEET_PADDING: Pixels = px(4.);

/// How tall a row stands under the window's inherited text style.
pub fn row_height(window: &Window) -> Pixels {
    bviz_ui_assets::text_metrics(window).line_height + ROW_PADDING * 2.
}

/// `bound` cut down to the whole rows `row` tall it holds, by which a list that
/// scrolls within it ends on a row entire rather than through the glyphs of one.
/// A bound shorter than a row holds the one row the list must still show.
pub fn whole_rows(row: Pixels, bound: Pixels) -> Pixels {
    row * (f32::from(bound) / f32::from(row)).floor().max(1.)
}

/// How tall a sheet of `rows` rows each `row` tall, under `above` that does not
/// scroll with them, stands within `bound`: as tall as all it holds, and no
/// taller than `bound`, past which its rows scroll and end on a whole row.
pub fn sheet_height(rows: usize, row: Pixels, above: Pixels, bound: Pixels) -> Pixels {
    let inset = SHEET_PADDING * 2. + SHEET_BORDER * 2. + above;
    let listed = (row * rows as f32).min(whole_rows(row, bound - inset));
    (inset + listed).min(bound)
}

/// The fill under the row the cursor or the keyboard is on, over the sheet the
/// row stands on.
fn aimed_fill(colors: &ThemeColors) -> Hsla {
    Hsla::from(over(colors.element_hover, colors.panel))
}

/// The fill under the cursor on a row already chosen.
fn chosen_hover(colors: &ThemeColors) -> Hsla {
    picked_hover(colors, colors.panel)
}

/// The button a picker's menu opens under, reading `label` within `width`
/// before a caret, and showing `hint` except while `open` holds of `picker`,
/// whose open menu the hint would cover. The caller adds the click that opens
/// the menu.
pub fn trigger<V: 'static>(
    id: ElementId,
    colors: &ThemeColors,
    hint: Hint,
    (picker, open): (WeakEntity<V>, fn(&V) -> bool),
    width: Pixels,
    label: SharedString,
) -> Stateful<Div> {
    let hover = colors.element_hover;
    let button = div()
        .id(id)
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
        // (e.g. a toolbar); that would blur the menu and, via the controls'
        // dismiss-on-blur, close it just before this click reopens it.
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation());
    hint.on_picker(button, colors.tooltip(), picker, open)
        .child(reading(width, label))
        .child(caret(colors))
}

/// The keys running what a row does, set at the end of the row.
pub fn row_keys(keys: SharedString, colors: &ThemeColors) -> Div {
    div()
        .whitespace_nowrap()
        .text_color(colors.text_muted)
        .child(keys)
}

/// The mark on a trigger saying it opens a menu.
fn caret(colors: &ThemeColors) -> Svg {
    mark("caret-down", colors)
}

fn mark(icon: &'static str, colors: &ThemeColors) -> Svg {
    bviz_ui_assets::icon(icon)
        .size(MENU_ICON)
        .text_color(colors.text)
}

/// A line of text held to `width`, so what a control reads does not change how
/// wide it is drawn, and cut short where it runs wider than that.
fn reading(width: Pixels, label: SharedString) -> Div {
    div()
        .w(width)
        .flex_none()
        .overflow_hidden()
        .whitespace_nowrap()
        .text_ellipsis()
        .child(label)
}

/// The width of the widest of `labels` under the window's inherited text style.
pub fn widest(window: &mut Window, labels: impl Iterator<Item = SharedString>) -> Pixels {
    let metrics = bviz_ui_assets::text_metrics(window);
    let color = window.text_style().color;
    let mut measured = px(0.);
    for label in labels {
        let width = bviz_ui_assets::shape_line(
            window,
            label,
            metrics.font.clone(),
            metrics.font_size,
            color,
        )
        .width;
        measured = measured.max(width);
    }
    laid_out_width(measured)
}

/// The sheet a menu's rows sit on, for [`under_trigger`] to place.
///
/// No explicit width: the sheet shrinks to fit its rows, so a row never wraps.
pub fn sheet(colors: &ThemeColors) -> Div {
    surface(div().occlude().absolute().flex().flex_col(), colors)
}

/// `sheet` opened from the trigger its holder lays out: below the trigger, or
/// above it where the window has no room below, and held inside the window
/// where it fits neither way. The holder is the trigger's `relative` parent,
/// laid out to the trigger's bounds.
pub fn under_trigger(sheet: impl IntoElement) -> Deferred {
    deferred(UnderTrigger {
        sheet: sheet.into_any_element(),
    })
}

/// Where a list of `extent` opens from the trigger laid out at `trigger`
/// inside `room`.
fn opened_under(
    trigger: Bounds<Pixels>,
    extent: Size<Pixels>,
    room: Bounds<Pixels>,
) -> Bounds<Pixels> {
    opened(
        point(trigger.left(), trigger.bottom() + MENU_GAP),
        point(trigger.right(), trigger.top() - MENU_GAP),
        extent,
        room,
    )
}

/// Where a list of `extent` opens for a menu asked at `from`: down and to the
/// right of it, flipped back across `back` on an axis where it would otherwise
/// hang out of `room`, and held against the room's near edge where it fits
/// neither way.
pub(crate) fn opened(
    from: Point<Pixels>,
    back: Point<Pixels>,
    extent: Size<Pixels>,
    room: Bounds<Pixels>,
) -> Bounds<Pixels> {
    let along = |from: Pixels, back: Pixels, extent: Pixels, start: Pixels, end: Pixels| {
        let placed = match from + extent <= end {
            true => from,
            false => back - extent,
        };
        placed.clamp(start, (end - extent).max(start))
    };
    Bounds::new(
        point(
            along(from.x, back.x, extent.width, room.left(), room.right()),
            along(from.y, back.y, extent.height, room.top(), room.bottom()),
        ),
        extent,
    )
}

/// Lays its sheet out at its own size and places it, once the trigger's bounds
/// are known, by [`opened_under`].
struct UnderTrigger {
    sheet: AnyElement,
}

impl IntoElement for UnderTrigger {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for UnderTrigger {
    type RequestLayoutState = LayoutId;
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, LayoutId) {
        let sheet = self.sheet.request_layout(window, cx);
        let style = Style {
            position: Position::Absolute,
            inset: Edges::all(px(0.).into()),
            ..Style::default()
        };
        (window.request_layout(style, [sheet], cx), sheet)
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        trigger: Bounds<Pixels>,
        sheet: &mut LayoutId,
        window: &mut Window,
        cx: &mut App,
    ) {
        let laid = window.layout_bounds(*sheet);
        let inset = window.client_inset().unwrap_or_default();
        let room = Bounds::new(Point::default(), window.viewport_size()).inset(inset);
        let placed = opened_under(trigger, laid.size, room);
        window.with_element_offset(placed.origin - laid.origin, |window| {
            self.sheet.prepaint(window, cx);
        });
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut LayoutId,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.sheet.paint(window, cx);
    }
}

/// `element` given the look of a sheet a menu's rows sit on, wherever it
/// stands.
pub fn surface<E: Styled>(element: E, colors: &ThemeColors) -> E {
    element
        .bg(colors.panel)
        .border(SHEET_BORDER)
        .border_color(colors.border)
        .rounded_md()
        .py(SHEET_PADDING)
        .shadow_lg()
}

/// The row a menu's own actions stand on, above the values they act on and out
/// of the list those values scroll in. It is laid out like a row, so what it
/// holds lines up with what the rows hold.
pub fn header(colors: &ThemeColors) -> Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_end()
        .px(ROW_INSET)
        .py(ROW_PADDING)
        .border_b_1()
        .border_color(colors.border)
}

/// One row of a menu, `chosen` filling in the value it names as one that stands.
pub(crate) fn row(id: ElementId, colors: &ThemeColors, chosen: bool) -> Stateful<Div> {
    let hover = if chosen {
        chosen_hover(colors)
    } else {
        aimed_fill(colors)
    };
    row_frame(id, colors)
        .when(chosen, |row| row.bg(colors.selection))
        .hover(move |style| style.bg(hover))
}

/// One row of a menu whose fill follows an aim the caller keeps, moved by the
/// cursor and the keys alike, so that the two never fill a row each.
pub fn aimed_row(id: ElementId, colors: &ThemeColors, aimed: bool) -> Stateful<Div> {
    row_frame(id, colors).when(aimed, |row| row.bg(aimed_fill(colors)))
}

/// `row` keeping the strip a list's scroll bar is drawn over inside its own
/// trailing padding, so its fill spans the sheet and the bar covers none of
/// what it holds.
pub fn clear_of_scrollbar<E: Styled>(row: E) -> E {
    row.pr(ROW_INSET + px(SCROLLBAR_THICKNESS))
}

fn row_frame(id: ElementId, colors: &ThemeColors) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .px(ROW_INSET)
        .py(ROW_PADDING)
        .cursor_pointer()
        .text_color(colors.text)
}

/// The check mark a row carries while the value it names is chosen, and the
/// space it leaves where it does not, so the labels of a menu's rows line up
/// whichever of them are checked.
pub(crate) fn check(colors: &ThemeColors, checked: bool) -> Div {
    slot(checked.then_some("check"), colors)
}

/// The mark at the trailing end of a row that opens a submenu, and the space it
/// leaves where the row opens none, so the labels of a menu's rows line up
/// whichever of them open one.
pub(crate) fn submenu_mark(colors: &ThemeColors, opens: bool) -> Div {
    slot(opens.then_some("caret-right"), colors)
}

/// The room a row keeps for a mark, holding `icon` where it has one.
fn slot(icon: Option<&'static str>, colors: &ThemeColors) -> Div {
    div()
        .size(MENU_ICON)
        .flex_none()
        .children(icon.map(|icon| mark(icon, colors)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bviz_ui_theme::{
        MIN_TEXT_CONTRAST, MIN_VISIBLE_STEP, built_in_themes, contrast_ratio, level_step,
    };
    use gpui::size;

    /// A sheet stands inside its bound however little room that leaves, even
    /// where the bound is shorter than what it holds above its rows.
    #[test]
    fn a_sheet_stands_no_taller_than_its_bound() {
        let row = px(24.);
        for bound in [px(400.), px(40.), px(10.)] {
            for above in [px(0.), row] {
                assert!(sheet_height(100, row, above, bound) <= bound, "{bound:?}");
            }
        }
        assert_eq!(
            sheet_height(2, row, row, px(400.)),
            SHEET_PADDING * 2. + SHEET_BORDER * 2. + row * 3.
        );
    }

    /// A menu opens below its trigger where the window has room for it, and
    /// above it where it does not, so its last row is never past the edge.
    #[test]
    fn a_menu_opens_above_a_trigger_with_no_room_below() {
        let room = Bounds::new(Point::default(), size(px(800.), px(600.)));
        let extent = size(px(120.), px(300.));
        let trigger = |top: f32| Bounds::new(point(px(100.), px(top)), size(px(80.), px(28.)));

        let high = trigger(100.);
        assert_eq!(
            opened_under(high, extent, room).top(),
            high.bottom() + MENU_GAP
        );
        let low = trigger(500.);
        let opened = opened_under(low, extent, room);
        assert_eq!(opened.bottom(), low.top() - MENU_GAP);
        assert_eq!(opened.left(), low.left());
    }

    /// A row names what choosing it does, so its label has to be read on the
    /// fill the row carries while the cursor or the keyboard is on it.
    #[test]
    fn the_aimed_row_keeps_its_label() {
        for theme in built_in_themes() {
            let colors = &theme.colors;
            let contrast = contrast_ratio(colors.text, aimed_fill(colors));
            assert!(
                contrast >= MIN_TEXT_CONTRAST,
                "{}: {contrast}:1",
                theme.name
            );
        }
    }

    /// Which row the cursor or the keyboard is on is said by the fill under it
    /// alone, so that fill has to stand off the sheet's own surface.
    #[test]
    fn the_aimed_row_stands_off_the_rows_around_it() {
        for theme in built_in_themes() {
            let colors = &theme.colors;
            let step = level_step(aimed_fill(colors), colors.panel);
            assert!(step >= MIN_VISIBLE_STEP, "{}: {step} levels", theme.name);
        }
    }

    /// A row names the value it stands for, so the label has to be read on the
    /// fill the row carries — including the deepest one, where the cursor rests
    /// on a row already chosen.
    #[test]
    fn a_chosen_row_under_the_cursor_keeps_its_label() {
        for theme in built_in_themes() {
            let colors = &theme.colors;
            let contrast = contrast_ratio(colors.text, chosen_hover(colors));
            assert!(
                contrast >= MIN_TEXT_CONTRAST,
                "{}: {contrast}:1",
                theme.name
            );
        }
    }

    /// Bringing the cursor onto a row already chosen has to show, or the row the
    /// cursor rests on is told only by the choice already made.
    #[test]
    fn the_cursor_shows_on_a_row_already_chosen() {
        for theme in built_in_themes() {
            let colors = &theme.colors;
            let chosen = over(colors.selection, colors.panel);
            let step = level_step(chosen_hover(colors), chosen);
            assert!(step >= MIN_VISIBLE_STEP, "{}: {step} levels", theme.name);
        }
    }
}
