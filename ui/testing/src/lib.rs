//! Draws the panels a viewer frames in a window of gpui's headless platform,
//! which lays out and paints a frame as a display would and shows nothing, so
//! that a test fails on whatever a frame of a panel panics on. The pointer and
//! keys act through the window's own event dispatch, and every gesture is
//! followed by a frame. The headless window has no frame loop: each frame
//! [`Harness::draw`] draws first delivers what the one before scheduled for the
//! next frame, so an animation advances only as frames are drawn, and settles
//! only once it has run its course in real time.

use bviz_invocation::Session;
use bviz_ui_components::{
    Menu, OneRow, Source, TextTooltip, compact_icon_width, menu_chrome, menu_open,
};
use bviz_ui_playback::Clock;
use bviz_ui_workspace::{Document, Member, Panel, PanelEvent, Shared, Workspace};
use gpui::{
    Action, AnyWindowHandle, App, AppContext, AsyncApp, Bounds, Entity, KeyDownEvent, KeyUpEvent,
    Keystroke, Modifiers, ModifiersChangedEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, PlatformInput, Point, ScrollDelta, ScrollWheelEvent, Size, TouchPhase,
    WeakEntity, Window, WindowHandle, point, px, size,
};
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

/// How long a wait for a document to load, lay out or derive what it shows
/// may take: far beyond what the small fixtures take in an unoptimised build
/// on a loaded machine, and well inside a medium test's timeout, so that a
/// load that hangs fails here with what was awaited.
pub const LOAD_DEADLINE: Duration = Duration::from_secs(30);

/// How far apart a [`Harness::sweep`] stops: close enough to rest on each
/// kind of thing a panel draws, and so to draw its hovered look, a card or a
/// ring a hover raises, without resting on every cell.
pub const SWEEP_STEP: Size<Pixels> = size(px(128.), px(44.));

/// How long a wait sleeps between looks at what it waits for.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// How long [`Harness::toolbar_hints`] rests the pointer on a place for a tooltip to
/// show, in each round of its search: briefly first, past gpui's show delay,
/// then long enough for the slowest machine to draw the frames in between.
const HINT_DWELLS: [Duration; 2] = [Duration::from_secs(1), Duration::from_secs(8)];

/// How many rows down an opened menu [`Harness::open_menus`] hovers, which
/// bounds the frames it draws for each menu.
const MENU_ROWS: usize = 6;

/// How many moves a [`Harness::drag`] takes the pointer through.
const DRAG_STEPS: usize = 4;

/// The path of the fixture the runfiles path in environment variable `var`
/// names, as a test target's `env` sets it with `$(rlocationpath ...)`.
pub fn fixture(var: &str) -> PathBuf {
    let runfiles = runfiles::Runfiles::create().expect("the test has runfiles");
    let path = std::env::var(var).unwrap_or_else(|_| panic!("{var} names a fixture"));
    runfiles::rlocation!(runfiles, &path).unwrap_or_else(|| panic!("{path} is in the runfiles"))
}

/// The point `x` of the way across and `y` of the way down `bounds`.
pub fn at(bounds: Bounds<Pixels>, x: f32, y: f32) -> Point<Pixels> {
    bounds.origin + point(bounds.size.width * x, bounds.size.height * y)
}

/// Whether `source`, the load of `what`, has landed. Panics with why the load
/// failed where it has, so that a failed load does not wait out a deadline.
pub fn loaded<T>(what: &str, source: &Source<T>) -> bool {
    match source {
        Source::Loading(_) => false,
        Source::Loaded(_) => true,
        Source::Failed(why) => panic!("the {what} failed to load: {why}"),
    }
}

/// Runs `test` on a window of gpui's headless platform framing the documents
/// `opened` builds, opened as bviz opens its own window, at its size, with
/// the key bindings of each of `panel_keys`. Returns once `test` has, and
/// panics with whatever `test` or a frame drawn for it panicked with.
pub fn run(
    panel_keys: &'static [fn(&mut App)],
    opened: impl FnOnce(&mut Window, &mut App) -> Vec<Document> + 'static,
    test: impl AsyncFnOnce(&mut Harness) + 'static,
) {
    let finished = Rc::new(Cell::new(false));
    let finishing = finished.clone();
    gpui_platform::headless()
        .with_assets(bviz_ui_assets::Assets)
        .run(move |cx| {
            let window = bviz_ui_workspace::open_first_window(
                "bviz test".into(),
                bviz_ui_workspace::WINDOW_SIZE,
                opened,
                None,
                Vec::new(),
                panel_keys,
                cx,
            )
            .expect("the headless platform opens a window");
            let tooltips = Rc::new(Cell::new(0));
            let counting = tooltips.clone();
            let observing =
                cx.observe_new::<TextTooltip>(move |_, _, _| counting.set(counting.get() + 1));
            let menus = Rc::new(RefCell::new(Vec::new()));
            let holding = menus.clone();
            let watching = cx.observe_new::<Menu>(move |_, _, cx| {
                holding.borrow_mut().push(cx.weak_entity());
            });
            cx.spawn(async move |cx| {
                let mut harness = Harness {
                    cx: cx.clone(),
                    window,
                    tooltips,
                    menus,
                };
                harness.draw();
                test(&mut harness).await;
                drop((observing, watching));
                finishing.set(true);
                cx.update(|cx| cx.quit());
            })
            .detach();
        });
    assert!(finished.get(), "the application quit before the test ended");
}

/// [`run`] on a window framing one file's document alone, its views those
/// `open` makes, given the document's session and clock.
pub fn run_alone(
    panel_keys: &'static [fn(&mut App)],
    open: impl FnOnce(Entity<Session>, Entity<Clock>, &mut Window, &mut App) -> Member + 'static,
    test: impl AsyncFnOnce(&mut Harness) + 'static,
) {
    let opened = |window: &mut Window, cx: &mut App| {
        let shared = Shared::new(cx);
        let (session, clock) = (shared.session().clone(), shared.clock().clone());
        let member = open(session, clock, window, cx);
        vec![Document::alone(member, None, shared)]
    };
    run(panel_keys, opened, test);
}

/// A test's hold on the window [`run`] opened.
pub struct Harness {
    cx: AsyncApp,
    window: WindowHandle<Workspace>,
    /// How many tooltips have been built since the window opened.
    tooltips: Rc<Cell<usize>>,
    /// Every [`Menu`] put up since the window opened that is still held.
    menus: Rc<RefCell<Vec<WeakEntity<Menu>>>>,
}

impl Harness {
    /// Runs `update` on the window's workspace.
    pub fn update<R>(
        &mut self,
        update: impl FnOnce(&mut Workspace, &mut Window, &mut gpui::Context<Workspace>) -> R,
    ) -> R {
        self.window
            .update(&mut self.cx, update)
            .expect("the window stays open")
    }

    /// Runs `update` on the window, whose root nothing holds meanwhile, so a
    /// frame or an event it dispatches may update the workspace.
    fn in_window<R>(&mut self, update: impl FnOnce(&mut Window, &mut App) -> R) -> R {
        let window: AnyWindowHandle = self.window.into();
        self.cx
            .update_window(window, |_, window, cx| update(window, cx))
            .expect("the window stays open")
    }

    /// Runs `read` on the application.
    pub fn read<R>(&self, read: impl FnOnce(&App) -> R) -> R {
        self.cx.update(|cx| read(cx))
    }

    /// Draws a frame of the window, having first run what the frame before
    /// scheduled for this one.
    pub fn draw(&mut self) {
        self.in_window(|window, cx| {
            window.simulate_next_frame(cx);
            let drawn = window.draw(cx);
            drawn.clear(cx);
        });
    }

    /// Draws frames until `done` holds, waiting between them, and panics
    /// naming `what` it waited for once `deadline` passes first.
    pub async fn until(&mut self, what: &str, deadline: Duration, done: impl Fn(&App) -> bool) {
        let start = bviz_time::now();
        loop {
            self.draw();
            if self.read(&done) {
                return;
            }
            assert!(
                start.elapsed() < deadline,
                "waited {deadline:?} for {what} in vain"
            );
            self.cx.background_executor().timer(POLL_INTERVAL).await;
        }
    }

    /// The panel of type `P` the window frames, and where the last frame laid
    /// out the body of the pane holding it.
    fn framed<P: Panel>(&mut self) -> (Entity<P>, Option<Bounds<Pixels>>) {
        let framed = self.update(|workspace, _, cx| workspace.framed_panels(cx));
        framed
            .into_iter()
            .find_map(|(panel, body)| Some((panel.view().downcast::<P>().ok()?, body)))
            .unwrap_or_else(|| panic!("the window frames a {}", std::any::type_name::<P>()))
    }

    /// The panel of type `P` the window frames.
    pub fn panel<P: Panel>(&mut self) -> Entity<P> {
        self.framed::<P>().0
    }

    /// Brings the panel of type `P` forward in its pane, as a gesture naming
    /// what it shows does, and draws it. Returns the panel and the room it
    /// draws in.
    pub fn show<P: Panel>(&mut self) -> (Entity<P>, Bounds<Pixels>) {
        let panel = self.panel::<P>();
        self.cx
            .update_entity(&panel, |_, cx| cx.emit(PanelEvent::Activate));
        self.draw();
        let (panel, body) = self.framed::<P>();
        let body = body.expect("a drawn frame lays out the pane showing the panel");
        (panel, body)
    }

    /// Whether the focus stands within an element of the key context `id`.
    pub fn focused_within(&mut self, id: &str) -> bool {
        self.in_window(|window, _| {
            window
                .context_stack()
                .iter()
                .any(|context| context.contains(id))
        })
    }

    /// Whether a menu stands open: one dropped from a dropdown's trigger, or a
    /// [`Menu`].
    pub fn menu_open(&mut self) -> bool {
        let menus = self.menus.clone();
        self.in_window(|window, cx| {
            let mut menus = menus.borrow_mut();
            menus.retain(|menu| menu.upgrade().is_some());
            let mut up = menus.iter().filter_map(WeakEntity::upgrade);
            menu_open(window) || up.any(|menu| menu.read(cx).is_up())
        })
    }

    fn dispatch_event(&mut self, event: PlatformInput) {
        self.in_window(|window, cx| {
            window.dispatch_event(event, cx);
        });
        self.draw();
    }

    /// Moves the pointer to `at`, no button held.
    pub fn move_to(&mut self, at: Point<Pixels>) {
        self.dispatch_event(PlatformInput::MouseMove(MouseMoveEvent {
            position: at,
            pressed_button: None,
            modifiers: Modifiers::default(),
        }));
    }

    /// Moves the pointer to `at` and clicks `button` there `times` times in a
    /// row, each press counting the clicks of the run so far, as a platform
    /// counts a double or a triple click.
    pub fn click_with(&mut self, button: MouseButton, at: Point<Pixels>, times: usize) {
        self.move_to(at);
        for click_count in 1..=times {
            self.dispatch_event(PlatformInput::MouseDown(MouseDownEvent {
                button,
                position: at,
                modifiers: Modifiers::default(),
                click_count,
                first_mouse: false,
            }));
            self.dispatch_event(PlatformInput::MouseUp(MouseUpEvent {
                button,
                position: at,
                modifiers: Modifiers::default(),
                click_count,
            }));
        }
    }

    /// Clicks the left button once at `at`.
    pub fn click(&mut self, at: Point<Pixels>) {
        self.click_with(MouseButton::Left, at, 1);
    }

    /// Clicks the left button at every `spacing` along the line from `from`
    /// to `to`, both ends among the places.
    pub fn click_along(&mut self, from: Point<Pixels>, to: Point<Pixels>, spacing: Pixels) {
        let line = to - from;
        let length = f32::from(line.x).hypot(f32::from(line.y));
        let gaps = (length / f32::from(spacing)).ceil().max(1.) as usize;
        for gap in 0..=gaps {
            self.click(from + (to - from) * (gap as f32 / gaps as f32));
        }
    }

    /// Turns the wheel by `notches` with the pointer at `at`, a notch being
    /// the line of scroll a platform reports one as.
    pub fn scroll_wheel(&mut self, at: Point<Pixels>, notches: Point<f32>) {
        self.move_to(at);
        self.dispatch_event(PlatformInput::ScrollWheel(ScrollWheelEvent {
            position: at,
            delta: ScrollDelta::Lines(notches),
            modifiers: Modifiers::default(),
            touch_phase: TouchPhase::Moved,
        }));
    }

    /// Presses and releases `keystroke`, as gpui writes one (`"escape"`,
    /// `"secondary-enter"`), on whatever holds the focus, its modifiers held
    /// around it and let go after. The key carries no character, so it types
    /// nothing into a text field.
    pub fn press(&mut self, keystroke: &str) {
        self.press_keystroke(Keystroke::parse(keystroke).expect("a keystroke gpui parses"));
    }

    /// Presses the keys the keymap binds `action` to where the focus stands,
    /// as [`press`](Self::press) presses each, panicking where it binds none.
    pub fn press_keys_for(&mut self, action: &dyn Action) {
        let binding = self.in_window(|window, cx| {
            let focused = window.focused(cx)?;
            window.highest_precedence_binding_for_action_in(action, &focused)
        });
        let binding = binding.unwrap_or_else(|| {
            panic!("a key is bound to {} where the focus stands", action.name())
        });
        for keystroke in binding.keystrokes() {
            self.press_keystroke(keystroke.inner().clone());
        }
    }

    fn press_keystroke(&mut self, keystroke: Keystroke) {
        let modifiers = keystroke.modifiers;
        let held = modifiers != Modifiers::default();
        if held {
            self.dispatch_event(PlatformInput::ModifiersChanged(ModifiersChangedEvent {
                modifiers,
                ..ModifiersChangedEvent::default()
            }));
        }
        self.dispatch_event(PlatformInput::KeyDown(KeyDownEvent {
            keystroke: keystroke.clone(),
            is_held: false,
            prefer_character_input: false,
        }));
        self.dispatch_event(PlatformInput::KeyUp(KeyUpEvent { keystroke }));
        if held {
            self.dispatch_event(PlatformInput::ModifiersChanged(
                ModifiersChangedEvent::default(),
            ));
        }
    }

    /// Types `text` into whatever holds the focus, a key per character, each
    /// carrying its character as a platform's input method hands it on.
    pub fn type_text(&mut self, text: &str) {
        for character in text.chars() {
            let keystroke = Keystroke::parse(&character.to_string())
                .expect("a character gpui parses as a keystroke");
            self.in_window(|window, cx| {
                window.dispatch_keystroke(keystroke, cx);
            });
            self.draw();
        }
    }

    /// Dispatches `action` from whatever holds the focus.
    pub fn dispatch(&mut self, action: &dyn Action) {
        let action = action.boxed_clone();
        self.in_window(|window, cx| window.dispatch_action(action, cx));
        self.draw();
    }

    /// Moves the pointer over `region` row by row, `step` apart, drawing a
    /// frame at each stop.
    pub fn sweep(&mut self, region: Bounds<Pixels>, step: Size<Pixels>) {
        assert!(
            step.width > px(0.) && step.height > px(0.),
            "a sweep moves on at each stop, not by {step:?}"
        );
        let mut y = region.top() + step.height / 2.;
        while y < region.bottom() {
            let mut x = region.left() + step.width / 2.;
            while x < region.right() {
                self.move_to(point(x, y));
                x += step.width;
            }
            y += step.height;
        }
    }

    /// Presses the left button at `from`, drags the pointer to `to` in
    /// [`DRAG_STEPS`] moves and lets go there.
    pub fn drag(&mut self, from: Point<Pixels>, to: Point<Pixels>) {
        self.move_to(from);
        self.dispatch_event(PlatformInput::MouseDown(MouseDownEvent {
            button: MouseButton::Left,
            position: from,
            modifiers: Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        }));
        for step in 1..=DRAG_STEPS {
            let along = step as f32 / DRAG_STEPS as f32;
            self.dispatch_event(PlatformInput::MouseMove(MouseMoveEvent {
                position: from + (to - from) * along,
                pressed_button: Some(MouseButton::Left),
                modifiers: Modifiers::default(),
            }));
        }
        self.dispatch_event(PlatformInput::MouseUp(MouseUpEvent {
            button: MouseButton::Left,
            position: to,
            modifiers: Modifiers::default(),
            click_count: 1,
        }));
    }

    /// Clicks along the middle of `strip` from left to right, `step` apart;
    /// where a click opens a menu, hovers down it and presses escape to put it
    /// away. Returns how many menus opened, the clicks that open the one
    /// already opened by the click before counting once.
    pub fn open_menus(&mut self, strip: Bounds<Pixels>, step: Pixels) -> usize {
        let y = strip.center().y;
        let row = self.in_window(|window, _| menu_chrome::row_height(window));
        let mut opened = 0;
        let mut opened_before = false;
        let mut x = strip.left() + step / 2.;
        while x < strip.right() {
            self.click(point(x, y));
            let open = self.menu_open();
            if open {
                opened += usize::from(!opened_before);
                for below in 1..=MENU_ROWS {
                    self.move_to(point(x, y + row * below as f32));
                }
                self.press("escape");
            }
            opened_before = open;
            x += step;
        }
        opened
    }

    /// The strip of controls a [`OneRow`] lays out across the top of `region`.
    fn toolbar(region: Bounds<Pixels>) -> Bounds<Pixels> {
        Bounds::new(region.origin, size(region.size.width, OneRow::height()))
    }

    /// [`open_menus`](Self::open_menus) along the strip of controls a
    /// [`OneRow`] lays out across the top of `region`, the narrowest control
    /// apart.
    pub fn toolbar_menus(&mut self, region: Bounds<Pixels>) -> usize {
        self.open_menus(Self::toolbar(region), compact_icon_width())
    }

    /// Rests the pointer on each control along the strip of controls a
    /// [`OneRow`] lays out across the top of `region` until a tooltip shows, panicking
    /// where none does, then moves it on across the rest of the strip. Each
    /// tooltip gpui shows at once on the way, the pointer coming to the next
    /// control soon after the last one hid, is drawn too, but no more than
    /// the first is waited for.
    pub async fn toolbar_hints(&mut self, region: Bounds<Pixels>) {
        let strip = Self::toolbar(region);
        let y = strip.center().y;
        let step = compact_icon_width();
        let places: Vec<Pixels> = (0..)
            .map(|place| strip.left() + OneRow::inset() + step * (place as f32 + 0.5))
            .take_while(|&x| x < strip.right())
            .collect();
        let before = self.tooltips.get();
        let mut found = None;
        'search: for dwell in HINT_DWELLS {
            for &x in &places {
                self.move_to(point(x, y));
                if self.raised_tooltip(before, dwell).await {
                    found = Some(x);
                    break 'search;
                }
            }
        }
        let mut x = found.unwrap_or_else(|| {
            let tried = places.len();
            panic!("no tooltip shows at any of {tried} places along {strip:?}")
        });
        while x < strip.right() {
            self.move_to(point(x, y));
            x += step;
        }
    }

    /// Whether a tooltip has been built since `before` of them were, waiting
    /// up to `dwell` for one the pointer rests on to show.
    async fn raised_tooltip(&mut self, before: usize, dwell: Duration) -> bool {
        let start = bviz_time::now();
        while start.elapsed() < dwell {
            self.draw();
            if self.tooltips.get() > before {
                self.draw();
                return true;
            }
            self.cx.background_executor().timer(POLL_INTERVAL).await;
        }
        false
    }
}
