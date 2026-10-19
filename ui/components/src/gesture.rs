//! Pointer gestures a view answers to. A view declares its gestures once, as a
//! [`Gestures`] table its pointer handlers dispatch through and help lists.

use crate::keys::{KeyStyle, glyphs, words};
use crate::keys_help::{GESTURE_MAX_CHARS, LABEL_MAX_CHARS};
use gpui::{
    ClickEvent, Modifiers, MouseButton, MouseDownEvent, NavigationDirection, PinchEvent, Pixels,
    Point, ScrollWheelEvent, px,
};

/// How far one line of wheel scroll pans a view, for the devices that report a
/// delta in lines rather than in pixels.
pub const LINE_SCROLL_PX: f32 = 24.0;

/// Factor one line of wheel scroll scales a zoomable view by, small enough that
/// a notch nudges the view rather than jumping it.
pub const ZOOM_PER_NOTCH: f32 = 1.1;

/// The modifier keys a gesture is made holding.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Held {
    shift: bool,
    alt: bool,
    secondary: bool,
}

impl Held {
    pub const NONE: Held = Held {
        shift: false,
        alt: false,
        secondary: false,
    };
    pub const SHIFT: Held = Held {
        shift: true,
        ..Held::NONE
    };
    pub const ALT: Held = Held {
        alt: true,
        ..Held::NONE
    };
    /// The platform's command modifier, as [`Modifiers::secondary`] reads it;
    /// on macOS Control makes it too, as it does on Apple's and Figma's
    /// canvases for zooming and for a modified click.
    pub const SECONDARY: Held = Held {
        secondary: true,
        ..Held::NONE
    };

    /// Holding both `self` and `other`.
    pub const fn and(self, other: Held) -> Held {
        Held {
            shift: self.shift || other.shift,
            alt: self.alt || other.alt,
            secondary: self.secondary || other.secondary,
        }
    }

    fn of(modifiers: &Modifiers) -> Held {
        Held {
            shift: modifiers.shift,
            alt: modifiers.alt,
            secondary: modifiers.secondary() || (cfg!(target_os = "macos") && modifiers.control),
        }
    }

    /// Whether every key `required` names is among those held.
    fn covers(self, required: Held) -> bool {
        (self.shift || !required.shift)
            && (self.alt || !required.alt)
            && (self.secondary || !required.secondary)
    }

    fn modifiers(self) -> Modifiers {
        let secondary = Modifiers::secondary_key();
        Modifiers {
            shift: self.shift,
            alt: self.alt,
            control: self.secondary && secondary.control,
            platform: self.secondary && secondary.platform,
            function: false,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Button {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

impl Button {
    fn of(button: MouseButton) -> Button {
        match button {
            MouseButton::Left => Button::Left,
            MouseButton::Right => Button::Right,
            MouseButton::Middle => Button::Middle,
            MouseButton::Navigate(NavigationDirection::Back) => Button::Back,
            MouseButton::Navigate(NavigationDirection::Forward) => Button::Forward,
        }
    }

    fn mouse_button(self) -> MouseButton {
        match self {
            Button::Left => MouseButton::Left,
            Button::Right => MouseButton::Right,
            Button::Middle => MouseButton::Middle,
            Button::Back => MouseButton::Navigate(NavigationDirection::Back),
            Button::Forward => MouseButton::Navigate(NavigationDirection::Forward),
        }
    }

    fn click_word(self) -> &'static str {
        match self {
            Button::Left => "click",
            Button::Right => "right-click",
            Button::Middle => "middle-click",
            Button::Back => "back-button click",
            Button::Forward => "forward-button click",
        }
    }

    fn drag_word(self) -> &'static str {
        match self {
            Button::Left => "drag",
            Button::Right => "right-drag",
            Button::Middle => "middle-drag",
            Button::Back => "back-button drag",
            Button::Forward => "forward-button drag",
        }
    }
}

/// Which presses of a run in quick succession make a click.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Clicks {
    /// Every press of the run, each acted on alike.
    Every,
    /// The first press only, so the presses after it in a run that makes a
    /// double click do nothing.
    Single,
    Double,
    Triple,
    /// The second press of a run and every press after it, each acted on
    /// alike.
    Repeated,
}

impl Clicks {
    /// Whether the `count`th press of a run makes this click.
    fn made_by(self, count: usize) -> bool {
        match self {
            Clicks::Every => true,
            Clicks::Single => count == 1,
            Clicks::Double => count == 2,
            Clicks::Triple => count == 3,
            Clicks::Repeated => count >= 2,
        }
    }

    /// The press of a run that first makes this click.
    fn first(self) -> usize {
        match self {
            Clicks::Every | Clicks::Single => 1,
            Clicks::Double | Clicks::Repeated => 2,
            Clicks::Triple => 3,
        }
    }

    fn prefix(self) -> &'static str {
        match self {
            Clicks::Every | Clicks::Single => "",
            Clicks::Double | Clicks::Repeated => "double-",
            Clicks::Triple => "triple-",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Motion {
    /// A wheel turned, or two fingers swept across a trackpad.
    Scroll,
    Pinch,
    Hover,
    /// A click acted on as its last press goes down.
    Press(Button, Clicks),
    /// A click acted on as its last press is let go without moving, which
    /// leaves the press free to begin a drag instead.
    Click(Button, Clicks),
    /// Moving the pointer with the button held, from a press of any count.
    Drag(Button),
}

/// What the hand does to make a gesture.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Input {
    motion: Motion,
    held: Held,
}

impl Input {
    pub const SCROLL: Input = Input::new(Motion::Scroll);
    pub const PINCH: Input = Input::new(Motion::Pinch);
    pub const HOVER: Input = Input::new(Motion::Hover);

    pub const fn new(motion: Motion) -> Input {
        Input {
            motion,
            held: Held::NONE,
        }
    }

    pub const fn press(button: Button) -> Input {
        Input::new(Motion::Press(button, Clicks::Every))
    }

    pub const fn click(button: Button) -> Input {
        Input::new(Motion::Click(button, Clicks::Every))
    }

    pub const fn drag(button: Button) -> Input {
        Input::new(Motion::Drag(button))
    }

    /// The same motion made holding `held`. Holding more keys than an input
    /// names still makes it, unless an entry tried earlier names them.
    pub const fn holding(self, held: Held) -> Input {
        Input { held, ..self }
    }

    fn answers(self, pointer: Pointer) -> bool {
        let moved = match (self.motion, pointer.kind) {
            (Motion::Scroll, Kind::Scroll)
            | (Motion::Pinch, Kind::Pinch)
            | (Motion::Hover, Kind::Hover) => true,
            (Motion::Press(button, clicks), Kind::Down(pressed, count))
            | (Motion::Click(button, clicks), Kind::Up(pressed, count)) => {
                button == pressed && clicks.made_by(count)
            }
            (Motion::Drag(button), Kind::Down(pressed, _)) => button == pressed,
            _ => false,
        };
        moved && pointer.held.covers(self.held)
    }

    /// The input as `style` spells its modifiers: `Ctrl+Scroll`, `Double-click`.
    pub fn text(self, style: KeyStyle) -> String {
        let motion = match self.motion {
            Motion::Scroll => "scroll".to_owned(),
            Motion::Pinch => "pinch".to_owned(),
            Motion::Hover => "hover".to_owned(),
            Motion::Press(button, clicks) | Motion::Click(button, clicks) => {
                format!("{}{}", clicks.prefix(), button.click_word())
            }
            Motion::Drag(button) => button.drag_word().to_owned(),
        };
        let motion = capitalized(&motion);
        let modifiers = self.held.modifiers();
        match style {
            KeyStyle::Glyphs if self.held != Held::NONE => {
                format!("{} {motion}", glyphs(&modifiers).collect::<String>())
            }
            KeyStyle::Glyphs => motion,
            KeyStyle::Words => {
                let mut parts: Vec<&str> = words(&modifiers).collect();
                parts.push(&motion);
                parts.join("+")
            }
        }
    }
}

fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Where a gesture is made. Which of a view's places a pointer is in is for
/// the view to find: the entry for a place declines a pointer elsewhere.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Place {
    /// Wherever the view takes the input. An entry here takes every pointer it
    /// answers, so a later entry answering the same pointer is never reached.
    Anywhere,
    /// Clear of everything the view draws to be pointed at.
    Empty,
    /// On the thing named by a noun phrase: "a box", "the ruler".
    On(&'static str),
}

/// An input made somewhere.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Gesture {
    pub input: Input,
    pub on: Place,
}

impl Gesture {
    /// The gesture as `style` spells its modifiers: `Ctrl+Scroll`,
    /// `Double-click empty space`, `Hover over a box`.
    pub fn text(self, style: KeyStyle) -> String {
        let input = self.input.text(style);
        let over = match self.input.motion {
            Motion::Scroll | Motion::Pinch | Motion::Hover => " over",
            Motion::Press(..) | Motion::Click(..) | Motion::Drag(_) => "",
        };
        match self.on {
            Place::Anywhere => input,
            Place::Empty => format!("{input}{over} empty space"),
            Place::On(thing) => format!("{input}{over} {thing}"),
        }
    }
}

/// A gesture and what it does, as help lists it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct GestureRow {
    pub gesture: Gesture,
    pub does: &'static str,
}

/// One row of a [`Gestures`] table, with the intent its view acts on for it.
#[derive(Clone, Copy, Debug)]
pub struct Entry<I> {
    row: GestureRow,
    intent: I,
}

impl<I> Entry<I> {
    pub const fn new(input: Input, on: Place, does: &'static str, intent: I) -> Self {
        Entry {
            row: GestureRow {
                gesture: Gesture { input, on },
                does,
            },
            intent,
        }
    }

    pub const fn gesture(&self) -> Gesture {
        self.row.gesture
    }

    pub const fn row(&self) -> GestureRow {
        self.row
    }
}

/// A view's gestures, in the order a pointer tries them.
pub struct Gestures<I: 'static> {
    entries: &'static [Entry<I>],
}

impl<I: Copy> Gestures<I> {
    pub const fn new(entries: &'static [Entry<I>]) -> Self {
        Gestures { entries }
    }

    pub fn rows(&self) -> impl Iterator<Item = GestureRow> + 'static {
        let entries = self.entries;
        entries.iter().map(|entry| entry.row)
    }

    /// Offers `act` the intent of each entry answering `pointer`, in table
    /// order, until it takes one by returning `true`; reports whether one was
    /// taken. `act` declines an intent whose place the pointer is not in.
    pub fn dispatch(&self, pointer: Pointer, mut act: impl FnMut(I) -> bool) -> bool {
        self.entries
            .iter()
            .filter(|entry| entry.row.gesture.input.answers(pointer))
            .any(|entry| act(entry.intent))
    }

    /// The rows no pointer can reach: those whose own input classifies to a
    /// pointer they do not answer, an earlier entry [`Place::Anywhere`] takes,
    /// or an earlier row repeats.
    pub fn unreachable(&self) -> Vec<GestureRow> {
        self.entries
            .iter()
            .enumerate()
            .filter(|(index, entry)| {
                let pointer = Pointer::made_by(entry.row.gesture.input);
                let earlier = &self.entries[..*index];
                !entry.row.gesture.input.answers(pointer)
                    || earlier.iter().any(|before| {
                        before.row.gesture == entry.row.gesture
                            || (before.row.gesture.on == Place::Anywhere
                                && before.row.gesture.input.answers(pointer))
                    })
            })
            .map(|(_, entry)| entry.row)
            .collect()
    }

    /// The rows the keys help could not hold on one line: those whose
    /// description runs past [`LABEL_MAX_CHARS`], or whose gesture, in either
    /// style, runs past [`GESTURE_MAX_CHARS`].
    pub fn overlong(&self) -> Vec<GestureRow> {
        let too_long = |text: &str, max: usize| text.chars().count() > max;
        self.rows()
            .filter(|row| {
                too_long(row.does, LABEL_MAX_CHARS)
                    || [KeyStyle::Glyphs, KeyStyle::Words]
                        .into_iter()
                        .any(|style| too_long(&row.gesture.text(style), GESTURE_MAX_CHARS))
            })
            .collect()
    }
}

/// How far `event` scrolls a view, a line counted as [`LINE_SCROLL_PX`].
pub fn scroll_pixels(event: &ScrollWheelEvent) -> Point<Pixels> {
    event.delta.pixel_delta(px(LINE_SCROLL_PX))
}

/// The factor `event` zooms a view by: [`ZOOM_PER_NOTCH`] for each line its
/// vertical scroll makes, a scroll in pixels counted in lines as it pans.
pub fn wheel_zoom_factor(event: &ScrollWheelEvent) -> f32 {
    ZOOM_PER_NOTCH.powf(f32::from(scroll_pixels(event).y) / LINE_SCROLL_PX)
}

/// The factor a pinch scales a view by: the exponential of its delta, which is
/// close to `1 + delta` for the small steps a pinch reports and stays positive
/// however large a step is.
pub fn pinch_factor(event: &PinchEvent) -> f32 {
    event.delta.exp()
}

/// A pointer event as a [`Gestures`] table matches it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Pointer {
    kind: Kind,
    held: Held,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Scroll,
    Pinch,
    Hover,
    /// A button going down, the `usize`th press of its run.
    Down(Button, usize),
    /// A button let go without moving, the `usize`th press of its run.
    Up(Button, usize),
}

impl Pointer {
    pub const HOVER: Pointer = Pointer {
        kind: Kind::Hover,
        held: Held::NONE,
    };

    pub fn scroll(event: &ScrollWheelEvent) -> Pointer {
        Pointer {
            kind: Kind::Scroll,
            held: Held::of(&event.modifiers),
        }
    }

    pub fn pinch(event: &PinchEvent) -> Pointer {
        Pointer {
            kind: Kind::Pinch,
            held: Held::of(&event.modifiers),
        }
    }

    pub fn press(event: &MouseDownEvent) -> Pointer {
        Pointer {
            kind: Kind::Down(Button::of(event.button), event.click_count),
            held: Held::of(&event.modifiers),
        }
    }

    /// The click `event` completes once its button is let go without moving.
    pub fn completing(event: &MouseDownEvent) -> Pointer {
        Pointer {
            kind: Kind::Up(Button::of(event.button), event.click_count),
            held: Held::of(&event.modifiers),
        }
    }

    /// A click as an element's click listener receives it, one made from the
    /// keyboard reading as a single click of the left button, a tap as the left
    /// button, and a long press, which gpui delivers alongside right clicks, as
    /// the right.
    pub fn click(event: &ClickEvent) -> Pointer {
        let button = match event {
            ClickEvent::Mouse(click) => Button::of(click.up.button),
            ClickEvent::Keyboard(_) => Button::Left,
            ClickEvent::Touch(touch) if touch.long_press => Button::Right,
            ClickEvent::Touch(_) => Button::Left,
        };
        Pointer {
            kind: Kind::Up(button, event.click_count()),
            held: Held::of(&event.modifiers()),
        }
    }

    /// The pointer the platform's event for `input` classifies to.
    fn made_by(input: Input) -> Pointer {
        let modifiers = input.held.modifiers();
        let down = |button: Button, clicks: Clicks| MouseDownEvent {
            button: button.mouse_button(),
            modifiers,
            click_count: clicks.first(),
            ..Default::default()
        };
        match input.motion {
            Motion::Scroll => Pointer::scroll(&ScrollWheelEvent {
                modifiers,
                ..Default::default()
            }),
            Motion::Pinch => Pointer::pinch(&PinchEvent {
                modifiers,
                ..Default::default()
            }),
            Motion::Hover => Pointer::HOVER,
            Motion::Press(button, clicks) => Pointer::press(&down(button, clicks)),
            Motion::Click(button, clicks) => Pointer::completing(&down(button, clicks)),
            Motion::Drag(button) => Pointer::press(&down(button, Clicks::Single)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn down(button: MouseButton, click_count: usize, modifiers: Modifiers) -> MouseDownEvent {
        MouseDownEvent {
            button,
            click_count,
            modifiers,
            ..Default::default()
        }
    }

    fn secondary() -> Modifiers {
        Held::SECONDARY.modifiers()
    }

    #[test]
    fn a_double_click_is_not_also_a_single_click() {
        let double = Pointer::press(&down(MouseButton::Left, 2, Modifiers::none()));
        let press = |clicks| Input::new(Motion::Press(Button::Left, clicks));
        assert!(press(Clicks::Double).answers(double));
        assert!(press(Clicks::Every).answers(double));
        assert!(!press(Clicks::Single).answers(double));
        assert!(!press(Clicks::Triple).answers(double));
    }

    #[test]
    fn a_repeated_click_is_made_by_every_press_after_the_first() {
        let press = |count| Pointer::press(&down(MouseButton::Left, count, Modifiers::none()));
        let repeated = Input::new(Motion::Press(Button::Left, Clicks::Repeated));
        assert!(!repeated.answers(press(1)));
        assert!(repeated.answers(press(2)));
        assert!(repeated.answers(press(3)));
    }

    #[test]
    fn a_drag_begins_from_a_press_of_any_count_but_no_release() {
        let drag = Input::drag(Button::Left);
        for count in 1..=4 {
            let event = down(MouseButton::Left, count, Modifiers::none());
            assert!(drag.answers(Pointer::press(&event)));
            assert!(!drag.answers(Pointer::completing(&event)));
        }
    }

    #[test]
    fn a_click_answers_its_release_and_not_its_press() {
        let event = down(MouseButton::Left, 1, Modifiers::none());
        assert!(Input::click(Button::Left).answers(Pointer::completing(&event)));
        assert!(!Input::click(Button::Left).answers(Pointer::press(&event)));
    }

    #[test]
    fn an_input_is_made_holding_more_keys_than_it_names_but_not_fewer() {
        let held = Modifiers {
            shift: true,
            ..secondary()
        };
        let scroll = |modifiers| {
            Pointer::scroll(&ScrollWheelEvent {
                modifiers,
                ..Default::default()
            })
        };
        assert!(Input::SCROLL.answers(scroll(held)));
        assert!(Input::SCROLL.holding(Held::SECONDARY).answers(scroll(held)));
        assert!(!Input::SCROLL.holding(Held::ALT).answers(scroll(held)));
    }

    #[test]
    fn buttons_are_told_apart() {
        let back = Pointer::press(&down(
            MouseButton::Navigate(NavigationDirection::Back),
            1,
            Modifiers::none(),
        ));
        assert!(Input::press(Button::Back).answers(back));
        assert!(!Input::press(Button::Forward).answers(back));
        assert!(!Input::press(Button::Left).answers(back));
    }

    #[test]
    fn a_gesture_reads_as_its_modifiers_motion_and_place() {
        let gesture = |input, on| Gesture { input, on };
        let fit = gesture(
            Input::new(Motion::Press(Button::Left, Clicks::Double)),
            Place::Empty,
        );
        assert_eq!(fit.text(KeyStyle::Words), "Double-click empty space");
        let hover = gesture(Input::HOVER, Place::On("a box"));
        assert_eq!(hover.text(KeyStyle::Glyphs), "Hover over a box");
        let menu = gesture(Input::press(Button::Right), Place::On("a cell"));
        assert_eq!(menu.text(KeyStyle::Words), "Right-click a cell");
        let pan = gesture(Input::drag(Button::Left), Place::Anywhere);
        assert_eq!(pan.text(KeyStyle::Words), "Drag");
        let range = gesture(
            Input::SCROLL.holding(Held::SHIFT.and(Held::ALT)),
            Place::Anywhere,
        );
        assert_eq!(range.text(KeyStyle::Words), "Alt+Shift+Scroll");
        assert_eq!(range.text(KeyStyle::Glyphs), "⌥⇧ Scroll");
    }

    #[test]
    fn a_pinch_scales_by_a_positive_factor_that_undoes_its_opposite() {
        let pinch = |delta| {
            pinch_factor(&PinchEvent {
                delta,
                ..Default::default()
            })
        };
        assert!(pinch(0.1) > 1.0);
        assert!(pinch(-0.1) < 1.0);
        assert!(pinch(-5.0) > 0.0);
        assert!((pinch(0.3) * pinch(-0.3) - 1.0).abs() < 1e-6);
    }

    #[derive(Clone, Copy)]
    struct Act;

    #[test]
    fn rows_are_reached_past_conditional_entries_but_not_past_anywhere() {
        const PAN: Entry<Act> = Entry::new(Input::drag(Button::Left), Place::Anywhere, "", Act);
        const GRAB: Entry<Act> =
            Entry::new(Input::drag(Button::Left), Place::On("the ruler"), "", Act);
        const OPEN: Entry<Act> =
            Entry::new(Input::click(Button::Left), Place::On("a box"), "", Act);
        assert_eq!(Gestures::new(&[GRAB, PAN, OPEN]).unreachable(), []);
        assert_eq!(Gestures::new(&[PAN, GRAB]).unreachable(), [GRAB.row]);
        assert_eq!(Gestures::new(&[OPEN, OPEN]).unreachable(), [OPEN.row]);
    }

    #[test]
    fn a_narrower_input_after_a_broader_one_anywhere_is_unreachable() {
        const SCROLL: Entry<Act> = Entry::new(Input::SCROLL, Place::Anywhere, "", Act);
        const ZOOM: Entry<Act> = Entry::new(
            Input::SCROLL.holding(Held::SECONDARY),
            Place::Anywhere,
            "",
            Act,
        );
        assert_eq!(Gestures::new(&[ZOOM, SCROLL]).unreachable(), []);
        assert_eq!(Gestures::new(&[SCROLL, ZOOM]).unreachable(), [ZOOM.row]);
    }

    #[test]
    fn a_row_past_either_limit_is_overlong() {
        const TERSE: Entry<Act> = Entry::new(Input::HOVER, Place::On("a box"), "Describe it", Act);
        const WORDY: Entry<Act> = Entry::new(
            Input::click(Button::Left),
            Place::On("a box"),
            "Open the box and every box it holds, then fit them",
            Act,
        );
        const FAR: Entry<Act> = Entry::new(
            Input::drag(Button::Left).holding(Held::SHIFT.and(Held::SECONDARY)),
            Place::On("the furthest of the boxes"),
            "Move it",
            Act,
        );
        assert_eq!(
            Gestures::new(&[TERSE, WORDY, FAR]).overlong(),
            [WORDY.row, FAR.row]
        );
    }

    #[test]
    fn dispatch_offers_intents_in_order_until_one_is_taken() {
        #[derive(Clone, Copy, PartialEq, Debug)]
        enum Intent {
            Grab,
            Pan,
        }
        const TABLE: Gestures<Intent> = Gestures::new(&[
            Entry::new(
                Input::drag(Button::Left),
                Place::On("the ruler"),
                "",
                Intent::Grab,
            ),
            Entry::new(Input::drag(Button::Left), Place::Anywhere, "", Intent::Pan),
        ]);
        let press = Pointer::press(&down(MouseButton::Left, 1, Modifiers::none()));
        let mut offered = Vec::new();
        let taken = TABLE.dispatch(press, |intent| {
            offered.push(intent);
            intent == Intent::Pan
        });
        assert!(taken);
        assert_eq!(offered, [Intent::Grab, Intent::Pan]);
        let scroll = Pointer::scroll(&ScrollWheelEvent::default());
        assert!(!TABLE.dispatch(scroll, |_| true));
    }
}
