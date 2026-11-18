use crate::CONTROL_HEIGHT;
use crate::gesture::GestureRow;
use crate::hint::Hint;
use bviz_ui_theme::{ThemeColors, label_on, level_step, luminance, mix, over};
use gpui::{
    Action, App, ClickEvent, Div, ElementId, Entity, FocusHandle, Hsla, IntoElement, KeyDownEvent,
    KeyUpEvent, Pixels, RenderOnce, Rgba, SharedString, Stateful, Window, div, prelude::*, px,
    relative, transparent_black,
};

/// A compact button's line box as a multiple of its font size, tight enough that
/// the label's own leading does not set the button's height.
const COMPACT_LINE_HEIGHT: f32 = 1.2;

/// The width and height of a button's icon.
pub(crate) const ICON_SIZE: Pixels = px(16.);

/// Room a compact button leaves either side of its label or icon.
const COMPACT_INSET: Pixels = px(8.);

/// The width of a compact button showing only an icon, the narrowest any
/// control stands.
pub fn compact_icon_width() -> Pixels {
    COMPACT_INSET * 2. + ICON_SIZE
}

/// Whether a key activates a focused button (enter or space).
fn is_activate(key: &str) -> bool {
    key == "enter" || key == "space"
}

/// What a [`Button`] runs when it is activated.
type OnClick = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;

/// A clickable button: filled with a label by default, or `.ghost().icon(..)` for
/// an icon-only control. Add `.track_focus(handle)` to make it keyboard-focusable,
/// in which case enter/space activate it and show the same pressed state as the mouse.
#[derive(IntoElement)]
pub struct Button {
    id: ElementId,
    label: Option<SharedString>,
    icon: Option<(SharedString, IconSide)>,
    ghost: bool,
    /// Whether the button reads as on, for one built as a toggle; `None` for a
    /// button that is not one.
    selected: Option<bool>,
    compact: bool,
    /// Whether the button can do nothing in the state it acts on, which it is
    /// drawn greyed for and reports no click in. A disabled toggle still shows
    /// whether it is on.
    disabled: bool,
    focus_handle: Option<FocusHandle>,
    hint: Hint,
    on_click: OnClick,
}

/// Which side of a button's label its icon stands on.
#[derive(Clone, Copy)]
enum IconSide {
    Before,
    After,
}

impl Button {
    pub fn new(
        id: impl Into<ElementId>,
        on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            id: id.into(),
            label: None,
            icon: None,
            ghost: false,
            selected: None,
            compact: false,
            disabled: false,
            focus_handle: None,
            hint: Hint::default(),
            on_click: Box::new(on_click),
        }
    }

    pub fn label(mut self, label: impl Into<SharedString>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// Shows `icon` after any label, where a disclosure caret stands.
    pub fn icon(mut self, icon: impl Into<SharedString>) -> Self {
        self.icon = Some((icon.into(), IconSide::After));
        self
    }

    /// Shows `icon` before any label, for an icon naming what the button does.
    pub fn leading_icon(mut self, icon: impl Into<SharedString>) -> Self {
        self.icon = Some((icon.into(), IconSide::Before));
        self
    }

    pub fn ghost(mut self) -> Self {
        self.ghost = true;
        self
    }

    /// Makes the button a toggle standing at `selected`, filling in the accent
    /// while on.
    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = Some(selected);
        self
    }

    /// Tightens the button to its label for a dense row, dropping the square
    /// minimum an icon-only button is otherwise given.
    pub fn compact(mut self) -> Self {
        self.compact = true;
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    pub fn track_focus(mut self, handle: &FocusHandle) -> Self {
        self.focus_handle = Some(handle.clone());
        self
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

    /// The tooltip naming keys as [`Hint::keys_in`] names them.
    pub fn keys_in(mut self, action: &dyn Action, focus: Option<&FocusHandle>) -> Self {
        self.hint = self.hint.keys_in(action, focus);
        self
    }

    /// The tooltip naming keys as [`Hint::keys_for_focused`] names them.
    pub fn keys_for_focused(mut self, action: &dyn Action) -> Self {
        self.hint = self.hint.keys_for_focused(action);
        self
    }

    /// The tooltip naming the pointer gesture `row` lists.
    pub fn gesture(mut self, row: GestureRow) -> Self {
        self.hint = self.hint.gesture(row);
        self
    }
}

fn sized(el: Stateful<Div>, button: &Button) -> Stateful<Div> {
    if button.compact {
        el.px(COMPACT_INSET)
            .py_0p5()
            .line_height(relative(COMPACT_LINE_HEIGHT))
    } else if button.ghost && button.label.is_none() {
        // A square hit target, so a bare icon is no harder to hit than a label.
        el.size(CONTROL_HEIGHT)
    } else {
        el.px_4().h(CONTROL_HEIGHT)
    }
}

/// Whether the button paints as a block of accent.
fn is_filled(button: &Button) -> bool {
    !button.disabled
        && matches!(
            (button.selected, button.ghost),
            (Some(true), _) | (None, false)
        )
}

/// The ink a button's label and icon are set in, read against the fill behind them.
fn ink(button: &Button, colors: &ThemeColors) -> Hsla {
    if button.disabled {
        colors.text_disabled
    } else if is_filled(button) {
        colors.text_on_accent
    } else {
        colors.text
    }
}

/// The mark on a filled button holding keyboard focus. Its fill covers the surface
/// `border_focused` is held against, so it marks focus in the one ink the palette
/// holds against the fill itself.
fn filled_ring(colors: &ThemeColors) -> Hsla {
    colors.text_on_accent
}

/// The fills a filled button paints at rest, under the cursor, and while held.
/// Each state moves the accent as far as that state's own layer moves the surface
/// it lifts least, so a filled button changes state by as much as an unfilled one
/// does; and each moves away from the ink read against it, so the label only gains
/// contrast as the state deepens.
fn filled_fills(colors: &ThemeColors) -> [Hsla; 3] {
    let away = away_from(colors.text_on_accent);
    let moved = |layer| stepped(colors.accent, away, least_lift(layer, colors));
    [
        colors.accent,
        moved(colors.element_hover),
        moved(colors.element_active),
    ]
}

/// How far `layer` lifts the surface it is washed over, on the surface it lifts
/// least of the three a control can sit on.
fn least_lift(layer: Hsla, colors: &ThemeColors) -> f32 {
    [colors.background, colors.surface, colors.panel]
        .into_iter()
        .map(|surface| level_step(over(layer, surface), surface))
        .fold(f32::INFINITY, f32::min)
}

/// The end of the value axis — black or white — lying away from `ink`. A fill
/// blended toward it can only read more strongly against that ink.
fn away_from(ink: Hsla) -> Hsla {
    label_on(ink)
}

/// `fill` blended toward `toward` until the two part by `levels` of luma, or the
/// whole way when `toward` is nearer than that.
fn stepped(fill: Hsla, toward: Hsla, levels: f32) -> Hsla {
    let (fill, toward) = (Rgba::from(fill), Rgba::from(toward));
    let reach = 255.0 * (luminance(toward) - luminance(fill)).abs();
    Hsla::from(mix(fill, toward, (levels / reach).clamp(0.0, 1.0)))
}

/// The fill of a toggle that is on but disabled: a wash of the accent it
/// fills with when it can be switched.
fn disabled_fill(colors: &ThemeColors) -> Hsla {
    colors.selection
}

/// Marks the cursor with `hovered` and the press with `pressed`, painting the
/// press into the base while the button is `held`.
fn stateful(el: Stateful<Div>, held: bool, hovered: Hsla, pressed: Hsla) -> Stateful<Div> {
    if held {
        el.bg(pressed)
    } else {
        el.hover(move |s| s.bg(hovered))
            .active(move |s| s.bg(pressed))
    }
}

/// An unselected toggle wears the affordance of the variant it was built as; only
/// the selected state is a look every variant shares. Both of a toggle's states
/// carry the same border width, so switching it does not resize it and shift its
/// neighbours. A press by the mouse carries the cursor with it; `held` is a press
/// from the keyboard, which has no cursor for gpui's hover and active styles to
/// follow, so what those styles would have brought is painted into the base.
fn styled_variant(
    el: Stateful<Div>,
    button: &Button,
    focused: bool,
    held: bool,
    ink: Hsla,
    colors: &ThemeColors,
) -> Stateful<Div> {
    let el = sized(el, button).text_color(ink).rounded_md();
    if button.disabled {
        // No response to the cursor, which it offers nothing to.
        return match button.selected {
            Some(true) => {
                let fill = disabled_fill(colors);
                el.bg(fill).border_2().border_color(fill)
            }
            _ if button.ghost => el.border_2().border_color(transparent_black()),
            _ => el.border_2().border_color(colors.border_variant),
        };
    }
    let (hover, active) = (colors.element_hover, colors.element_active);
    match (button.selected, button.ghost) {
        (Some(false), true) => stateful(
            el.border_2().border_color(transparent_black()),
            held,
            hover,
            active,
        ),
        (Some(false), false) => {
            // The outline is this variant's whole affordance, so hovering takes
            // it to full ink strength.
            let hover_border = colors.text;
            let el = el.border_2().border_color(colors.border);
            if held {
                el.border_color(hover_border).bg(active)
            } else {
                el.hover(move |s| s.border_color(hover_border))
                    .active(move |s| s.bg(active))
            }
        }
        (None, true) => stateful(el, held, hover, active),
        (Some(true), _) | (None, false) => {
            let [rest, hovered, pressed] = filled_fills(colors);
            let border = if focused { filled_ring(colors) } else { rest };
            stateful(
                el.bg(rest).border_2().border_color(border),
                held,
                hovered,
                pressed,
            )
        }
    }
}

/// Wire enter/space to drive the persisted `pressed` state so a keyboard-held button
/// shows the same press as the mouse. A button without a focus handle is unfocusable
/// and gets no key handlers.
fn with_held_state_handlers(
    el: Stateful<Div>,
    focus_handle: Option<&FocusHandle>,
    pressed: Entity<bool>,
) -> Stateful<Div> {
    let Some(handle) = focus_handle else {
        return el;
    };
    el.track_focus(handle)
        .on_key_down({
            let pressed = pressed.clone();
            move |e: &KeyDownEvent, _, cx| {
                if is_activate(&e.keystroke.key) {
                    pressed.update(cx, |p, cx| {
                        *p = true;
                        cx.notify();
                    });
                }
            }
        })
        .on_key_up({
            let pressed = pressed.clone();
            move |e: &KeyUpEvent, _, cx| {
                if is_activate(&e.keystroke.key) {
                    pressed.update(cx, |p, cx| {
                        *p = false;
                        cx.notify();
                    });
                }
            }
        })
}

impl RenderOnce for Button {
    fn render(mut self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let colors = bviz_ui_theme::theme(window, cx).colors;
        let focused = self
            .focus_handle
            .as_ref()
            .is_some_and(|handle| handle.is_focused(window));
        // Persist the keyboard-held state across frames so enter/space show a press
        // (gpui's `.active` only tracks the mouse).
        let pressed = window.use_keyed_state(self.id.clone(), cx, |_, _| false);
        // A button disabled mid-press loses the key handlers that would end the
        // press.
        if self.disabled {
            pressed.update(cx, |pressed, _| *pressed = false);
        }
        let held = *pressed.read(cx);
        let ink = ink(&self, &colors);

        let el = div()
            .id(self.id.clone())
            .flex()
            .flex_row()
            .items_center()
            .justify_center()
            .gap_1()
            // Hold the measured size in a flex parent (e.g. a toolbar); without
            // this the row shrinks the button and wraps its label to a second line.
            .flex_none()
            .when(!self.disabled, |el| el.cursor_pointer());

        let el = std::mem::take(&mut self.hint).on(el, colors.tooltip());
        let el = styled_variant(el, &self, focused, held, ink, &colors);
        let el = match self.disabled {
            true => el,
            false => with_held_state_handlers(el, self.focus_handle.as_ref(), pressed)
                .on_click(self.on_click),
        };

        let icon = self.icon.map(|(name, side)| {
            let icon = bviz_ui_assets::icon(name.as_ref())
                .size(ICON_SIZE)
                .text_color(ink);
            (icon, side)
        });
        let (before, after) = match icon {
            Some((icon, IconSide::Before)) => (Some(icon), None),
            Some((icon, IconSide::After)) => (None, Some(icon)),
            None => (None, None),
        };
        el.children(before).children(self.label).children(after)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bviz_ui_theme::{
        MIN_SECONDARY_CONTRAST, MIN_TEXT_CONTRAST, MIN_VISIBLE_STEP, built_in_themes,
        contrast_ratio,
    };

    /// Every look a caller can build, so a fill or a mark only one of them paints
    /// is still walked.
    fn variants() -> Vec<Button> {
        let plain = || Button::new("under-test", |_, _, _| {});
        vec![
            plain(),
            plain().ghost(),
            plain().selected(true),
            plain().selected(false),
            plain().ghost().selected(true),
            plain().ghost().selected(false),
        ]
    }

    /// Every ring `button` marks focus with, each paired with the fill it is drawn
    /// on. A variant with no fill of its own rings against the surface it sits on,
    /// which the palette already holds `border_focused` against, so it is a filled
    /// button that has something to answer for here.
    fn focus_marks(button: &Button, colors: &ThemeColors) -> Vec<(Hsla, Hsla)> {
        if !is_filled(button) {
            return Vec::new();
        }
        filled_fills(colors)
            .into_iter()
            .map(|fill| (filled_ring(colors), fill))
            .collect()
    }

    /// A ring says which control the keyboard is aimed at, which it cannot do if
    /// it sinks into the button it is drawn around — in any state that button can
    /// be focused in.
    #[test]
    fn the_focus_ring_stands_against_every_fill_it_is_drawn_on() {
        for theme in built_in_themes() {
            for button in variants() {
                for (ring, fill) in focus_marks(&button, &theme.colors) {
                    let contrast = contrast_ratio(ring, fill);
                    assert!(contrast >= MIN_SECONDARY_CONTRAST, "{contrast}:1");
                }
            }
        }
    }

    /// A toggle that is on but disabled has to be told from one that is off,
    /// which draws no fill, on every surface it can sit on.
    #[test]
    fn a_disabled_toggle_that_is_on_shows_a_fill() {
        for theme in built_in_themes() {
            let colors = &theme.colors;
            for surface in [colors.background, colors.surface, colors.panel] {
                let step = level_step(over(disabled_fill(colors), surface), surface);
                assert!(step >= MIN_VISIBLE_STEP, "{step} levels");
            }
        }
    }

    /// A filled button says three different things with its fill — resting, under
    /// the cursor, held — so none of them may be taken for another; and its label
    /// is read against whichever of the three is showing.
    #[test]
    fn a_filled_button_parts_its_three_states_and_keeps_its_label() {
        for theme in built_in_themes() {
            let colors = &theme.colors;
            let filled = Button::new("under-test", |_, _, _| {}).selected(true);
            let ink = ink(&filled, colors);
            let fills = filled_fills(colors);
            for (index, fill) in fills.iter().enumerate() {
                let contrast = contrast_ratio(ink, *fill);
                assert!(contrast >= MIN_TEXT_CONTRAST, "{contrast}:1");
                for other in &fills[index + 1..] {
                    let step = level_step(*fill, *other);
                    assert!(step >= MIN_VISIBLE_STEP, "{step} levels");
                }
            }
        }
    }
}
