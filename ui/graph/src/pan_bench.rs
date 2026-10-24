//! A self-driving pan benchmark, run when `BVIZ_PAN_BENCH` names a scenario,
//! for as many frames as `BVIZ_PAN_BENCH_FRAMES` asks, moving the view as
//! `BVIZ_PAN_BENCH_MOTION` says and pressing where `BVIZ_PAN_BENCH_PRESS` says.

use crate::GraphPanel;
use crate::camera::{Camera, Rect, to_screen, visible_rect};
use crate::derive;
use crate::mode::LayoutMode;
use bviz_graph_layout::Id;
use bviz_time::Instant;
use bviz_ui_components::gesture::ZOOM_PER_NOTCH;
use gpui::{
    App, Entity, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels,
    PlatformInput, Point, ScrollDelta, ScrollWheelEvent, TouchPhase, Window, point, px,
};
use std::f32::consts::TAU;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::time::Duration;

/// Names the [`Scenario`] to run.
const SCENARIO_VAR: &str = "BVIZ_PAN_BENCH";

/// How many frames of the motion to record; `0` sets the scenario up and holds
/// it, for a screenshot of the position the motion starts from.
const FRAMES_VAR: &str = "BVIZ_PAN_BENCH_FRAMES";

const DEFAULT_FRAMES: usize = 240;

/// Names the [`Motion`] recorded.
const MOTION_VAR: &str = "BVIZ_PAN_BENCH_MOTION";

/// Names the [`Press`] the pointer rests on as the motion starts.
const PRESS_VAR: &str = "BVIZ_PAN_BENCH_PRESS";

/// Frames of the motion run before recording starts, which absorb one-off
/// costs such as rasterizing glyphs the scenario had yet to draw.
const WARMUP_FRAMES: usize = 10;

/// Consecutive frames the view must hold still before a setup step counts as
/// landed.
const QUIET_FRAMES: usize = 3;

/// How far the drag path swings from the press, per axis, in window pixels.
const SWING: (f32, f32) = (320.0, 180.0);

/// Frames one lap of the motion takes. The drag path's vertical swing runs at
/// twice the horizontal rate, tracing a figure of eight.
const LAP_FRAMES: f32 = 120.0;

/// Wheel notches a zoom step of the setup turns, each aimed at where the
/// boxes on screen crowd, so a run of steps homes in on a dense part of the
/// graph rather than on whatever empty canvas the window's center shows.
const NOTCHES_PER_STEP: f32 = 5.0;

/// Zoom the zoom motion swings about, near enough to the boxes that labels
/// are drawn above their floor size across the whole swing.
const ZOOM_CENTER: f32 = 1.6;

/// Wheel notches the zoom motion swings either side of [`ZOOM_CENTER`], one
/// lap of [`LAP_FRAMES`] zooming in, back out past it, and in to it again.
const ZOOM_SWING_NOTCHES: f32 = 6.0;

/// Containers the layered scenario opens on top of those its opening view
/// holds open, which bounds how far the relayout they trigger grows the layout.
const EXPANDED_PER_STEP: usize = 16;

/// Zoom steps the map scenario takes, deep enough that a fine level fills the
/// window with boxes.
const MAP_ZOOM_STEPS: usize = 8;

#[derive(Clone, Copy, Debug)]
enum Scenario {
    Map,
    Layered,
}

impl Scenario {
    const ALL: [Self; 2] = [Self::Map, Self::Layered];

    fn named(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|scenario| scenario.label() == name)
    }

    fn label(self) -> &'static str {
        match self {
            Self::Map => "map",
            Self::Layered => "layered",
        }
    }

    /// What puts the view into the scenario, each step applied once the one
    /// before has landed.
    fn setup(self) -> &'static [Setup] {
        match self {
            Self::Map => &[Setup::Zoom; MAP_ZOOM_STEPS],
            Self::Layered => &[
                Setup::Mode(LayoutMode::Layered),
                Setup::Zoom,
                Setup::Zoom,
                Setup::ExpandNearest,
                Setup::Zoom,
                Setup::Zoom,
            ],
        }
    }
}

/// How the recorded frames move the view.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum Motion {
    /// Drags the canvas along a figure of eight.
    #[default]
    Pan,
    /// Turns the wheel in and out about the press point, which the pointer
    /// rests on unpressed.
    Zoom,
}

impl Motion {
    const ALL: [Self; 2] = [Self::Pan, Self::Zoom];

    fn named(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|motion| motion.label() == name)
    }

    fn label(self) -> &'static str {
        match self {
            Self::Pan => "pan",
            Self::Zoom => "zoom",
        }
    }

    /// What puts the view where the motion starts, after the scenario's setup.
    fn setup(self) -> &'static [Setup] {
        match self {
            Self::Pan => &[],
            Self::Zoom => &[Setup::ZoomTo(ZOOM_CENTER)],
        }
    }
}

/// Where the pointer rests as the motion starts: resting on a box carries the
/// box's hover through the motion.
#[derive(Clone, Copy, Debug, Default)]
enum Press {
    /// The viewport's center, whatever lies under it.
    #[default]
    Center,
    /// The center of the leaf box on screen nearest the viewport's center.
    Box,
}

impl Press {
    const ALL: [Self; 2] = [Self::Center, Self::Box];

    fn named(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|press| press.label() == name)
    }

    fn label(self) -> &'static str {
        match self {
            Self::Center => "center",
            Self::Box => "box",
        }
    }

    /// The window point this press lands on in `view`.
    fn point(self, view: &GraphPanel) -> Point<Pixels> {
        let viewport = view.framing.viewport();
        match self {
            Self::Center => viewport.center(),
            Self::Box => leaf_nearest_center(view).unwrap_or_else(|| {
                bviz_switches::report(format_args!(
                    "pan_bench: no leaf box on screen to press on; pressing the {}",
                    Self::Center.label()
                ));
                viewport.center()
            }),
        }
    }
}

#[derive(Clone, Copy)]
enum Setup {
    Mode(LayoutMode),
    /// Opens the [`EXPANDED_PER_STEP`] folded containers on screen nearest
    /// the viewport's center.
    ExpandNearest,
    /// [`NOTCHES_PER_STEP`] wheel notches at [`crowded_point`].
    Zoom,
    /// The wheel notches at [`crowded_point`] that reach this zoom.
    ZoomTo(f32),
}

/// A per-frame count, tallied by the paint while a benchmark runs.
#[derive(Clone, Copy)]
pub(crate) enum Tally {
    Boxes,
    Connectors,
    /// Connectors the spatial index offered, each then tested for being drawn.
    RouteCandidates,
    PaintPaths,
    /// Labels shaped, including those then dropped for overlapping another.
    ShapedLabels,
}

impl Tally {
    const ALL: [Self; 5] = [
        Self::Boxes,
        Self::Connectors,
        Self::RouteCandidates,
        Self::PaintPaths,
        Self::ShapedLabels,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Boxes => "boxes",
            Self::Connectors => "connectors",
            Self::RouteCandidates => "route_candidates",
            Self::PaintPaths => "paint_paths",
            Self::ShapedLabels => "shaped_labels",
        }
    }
}

/// A stretch of per-frame CPU work, timed while a benchmark runs.
#[derive(Clone, Copy)]
pub(crate) enum Phase {
    /// Handling the pointer events that drive the motion.
    InputDispatch,
    PanelRender,
    /// Rebuilding the chrome a frame does not replay.
    ChromeRender,
    CanvasPrepaint,
    CanvasPaint,
}

impl Phase {
    const ALL: [Self; 5] = [
        Self::InputDispatch,
        Self::PanelRender,
        Self::ChromeRender,
        Self::CanvasPrepaint,
        Self::CanvasPaint,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::InputDispatch => "input_dispatch_ms",
            Self::PanelRender => "panel_render_ms",
            Self::ChromeRender => "chrome_render_ms",
            Self::CanvasPrepaint => "canvas_prepaint_ms",
            Self::CanvasPaint => "canvas_paint_ms",
        }
    }
}

/// Whether a panel has taken the benchmark: the counters are process-wide, so
/// only one panel may run it.
static CLAIMED: AtomicBool = AtomicBool::new(false);
static RUNNING: AtomicBool = AtomicBool::new(false);
static TALLIED: [AtomicU64; Tally::ALL.len()] = [const { AtomicU64::new(0) }; Tally::ALL.len()];
static PHASE_NANOS: [AtomicU64; Phase::ALL.len()] = [const { AtomicU64::new(0) }; Phase::ALL.len()];

pub(crate) fn tally(tally: Tally, n: usize) {
    if RUNNING.load(Relaxed) {
        TALLIED[tally as usize].fetch_add(n as u64, Relaxed);
    }
}

/// Times the enclosing scope as `phase` until dropped.
pub(crate) fn span(phase: Phase) -> Span {
    Span(RUNNING.load(Relaxed).then(|| (phase, bviz_time::now())))
}

pub(crate) struct Span(Option<(Phase, Instant)>);

impl Drop for Span {
    fn drop(&mut self) {
        if let Some((phase, start)) = self.0 {
            let nanos = u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX);
            PHASE_NANOS[phase as usize].fetch_add(nanos, Relaxed);
        }
    }
}

/// What one frame cost.
struct Sample {
    interval_ms: f64,
    /// Indexed by [`Phase`].
    phase_ms: [f64; Phase::ALL.len()],
    /// Indexed by [`Tally`].
    tallied: [f64; Tally::ALL.len()],
}

/// What the frame just drawn cost, emptying the per-frame accumulators.
fn take_frame(interval: Duration) -> Sample {
    Sample {
        interval_ms: interval.as_secs_f64() * 1e3,
        phase_ms: PHASE_NANOS
            .each_ref()
            .map(|nanos| nanos.swap(0, Relaxed) as f64 / 1e6),
        tallied: TALLIED
            .each_ref()
            .map(|count| count.swap(0, Relaxed) as f64),
    }
}

/// A benchmark asked for by its switches, not yet started.
pub(crate) struct Bench {
    scenario: Scenario,
    frames: usize,
    motion: Motion,
    press: Press,
}

impl Bench {
    /// The benchmark the switches ask for, handed to the first panel to
    /// ask.
    pub(crate) fn from_switches() -> Option<Self> {
        let name = bviz_switches::get(SCENARIO_VAR)?;
        if CLAIMED.swap(true, Relaxed) {
            return None;
        }
        let Some(scenario) = Scenario::named(&name) else {
            let known = Scenario::ALL.map(Scenario::label);
            bviz_switches::report(format_args!(
                "pan_bench: unknown scenario {name:?}; expected one of {}",
                known.join(", ")
            ));
            return None;
        };
        let frames = match bviz_switches::get(FRAMES_VAR) {
            None => DEFAULT_FRAMES,
            Some(frames) => frames.parse().unwrap_or_else(|_| {
                bviz_switches::report(format_args!(
                    "pan_bench: {FRAMES_VAR}={frames:?} is not a frame count; \
                     recording {DEFAULT_FRAMES}"
                ));
                DEFAULT_FRAMES
            }),
        };
        let motion = match bviz_switches::get(MOTION_VAR) {
            None => Motion::default(),
            Some(name) => Motion::named(&name).unwrap_or_else(|| {
                let known = Motion::ALL.map(Motion::label);
                let fallback = Motion::default().label();
                bviz_switches::report(format_args!(
                    "pan_bench: unknown {MOTION_VAR}={name:?}; expected one of {}; \
                     recording {fallback}",
                    known.join(", ")
                ));
                Motion::default()
            }),
        };
        let press = match bviz_switches::get(PRESS_VAR) {
            None => Press::default(),
            Some(name) => Press::named(&name).unwrap_or_else(|| {
                let known = Press::ALL.map(Press::label);
                let fallback = Press::default().label();
                bviz_switches::report(format_args!(
                    "pan_bench: unknown {PRESS_VAR}={name:?}; expected one of {}; \
                     pressing {fallback}",
                    known.join(", ")
                ));
                Press::default()
            }),
        };
        Some(Self {
            scenario,
            frames,
            motion,
            press,
        })
    }

    /// Drives `panel` from the next frame on.
    pub(crate) fn start(self, panel: Entity<GraphPanel>, window: &mut Window) {
        RUNNING.store(true, Relaxed);
        let samples = Vec::with_capacity(self.frames);
        Run {
            bench: self,
            panel,
            stage: Stage::Setup {
                step: 0,
                quiet: 0,
                camera: None,
            },
            samples,
            last_frame: None,
        }
        .schedule(window);
    }
}

enum Stage {
    /// Waiting for the view to hold still before applying the setup's `step`,
    /// or before the motion once every step is applied.
    Setup {
        step: usize,
        quiet: usize,
        camera: Option<Camera>,
    },
    Moving {
        press: Point<Pixels>,
        frame: usize,
    },
}

struct Run {
    bench: Bench,
    panel: Entity<GraphPanel>,
    stage: Stage,
    samples: Vec<Sample>,
    last_frame: Option<Instant>,
}

impl Run {
    fn schedule(self, window: &mut Window) {
        window.on_next_frame(move |window, cx| self.step(window, cx));
    }

    fn step(mut self, window: &mut Window, cx: &mut App) {
        let now = bviz_time::now();
        let interval = self.last_frame.map_or(Duration::ZERO, |last| now - last);
        self.last_frame = Some(now);
        let sample = take_frame(interval);
        match self.stage {
            Stage::Setup {
                step,
                quiet,
                camera,
            } => {
                let held = at_rest(self.panel.read(cx));
                let quiet = match held.is_some() && held == camera {
                    true => quiet + 1,
                    false => 0,
                };
                self.stage = Stage::Setup {
                    step,
                    quiet,
                    camera: held,
                };
                if quiet >= QUIET_FRAMES {
                    let setup = self.bench.scenario.setup().iter();
                    match setup.chain(self.bench.motion.setup()).nth(step) {
                        Some(&setup) => {
                            self.apply(setup, window, cx);
                            self.stage = Stage::Setup {
                                step: step + 1,
                                quiet: 0,
                                camera: None,
                            };
                        }
                        None => {
                            let press = self.bench.press.point(self.panel.read(cx));
                            dispatch_move(press, None, window, cx);
                            self.ready(cx);
                            if self.bench.frames == 0 {
                                RUNNING.store(false, Relaxed);
                                return;
                            }
                            if self.bench.motion == Motion::Pan {
                                window.dispatch_event(
                                    PlatformInput::MouseDown(MouseDownEvent {
                                        button: MouseButton::Left,
                                        position: press,
                                        modifiers: Modifiers::default(),
                                        click_count: 1,
                                        first_mouse: false,
                                    }),
                                    cx,
                                );
                            }
                            self.stage = Stage::Moving { press, frame: 0 };
                        }
                    }
                }
                self.panel.update(cx, |_, cx| cx.notify());
            }
            Stage::Moving { press, frame } => {
                if frame == WARMUP_FRAMES {
                    bviz_switches::report(format_args!("pan_bench recording"));
                }
                if frame > WARMUP_FRAMES {
                    self.samples.push(sample);
                }
                let last = frame == WARMUP_FRAMES + self.bench.frames;
                match self.bench.motion {
                    Motion::Pan => {
                        let at = drag_path(press, frame);
                        if last {
                            window.dispatch_event(
                                PlatformInput::MouseUp(MouseUpEvent {
                                    button: MouseButton::Left,
                                    position: at,
                                    modifiers: Modifiers::default(),
                                    click_count: 1,
                                }),
                                cx,
                            );
                        } else {
                            dispatch_move(at, Some(MouseButton::Left), window, cx);
                        }
                    }
                    Motion::Zoom if !last => {
                        let _span = span(Phase::InputDispatch);
                        dispatch_wheel(press, zoom_path_step(frame), window, cx);
                    }
                    Motion::Zoom => {}
                }
                if last {
                    self.report();
                    cx.quit();
                    return;
                }
                self.stage = Stage::Moving {
                    press,
                    frame: frame + 1,
                };
            }
        }
        self.schedule(window);
    }

    fn apply(&self, setup: Setup, window: &mut Window, cx: &mut App) {
        match setup {
            Setup::Mode(mode) => self.panel.update(cx, |view, cx| view.set_mode(mode, cx)),
            Setup::ExpandNearest => self.panel.update(cx, |view, cx| {
                let nearest = folded_nearest_center(view);
                derive::expand(view, nearest, cx);
            }),
            Setup::Zoom => self.zoom_toward_crowd(|_| NOTCHES_PER_STEP, window, cx),
            Setup::ZoomTo(zoom) => self.zoom_toward_crowd(
                |view| (zoom / view.camera().zoom).ln() / ZOOM_PER_NOTCH.ln(),
                window,
                cx,
            ),
        }
    }

    /// Turns the wheel by `notches` of the view at [`crowded_point`].
    fn zoom_toward_crowd(
        &self,
        notches: impl FnOnce(&GraphPanel) -> f32,
        window: &mut Window,
        cx: &mut App,
    ) {
        let view = self.panel.read(cx);
        let Some(at) = crowded_point(view) else {
            bviz_switches::report(format_args!(
                "pan_bench: no box on screen to zoom toward; skipping a zoom step"
            ));
            return;
        };
        let notches = notches(view);
        dispatch_wheel(at, notches, window, cx);
    }

    /// Reports the view the motion starts from, the pointer resting at the
    /// press point.
    fn ready(&self, cx: &App) {
        let view = self.panel.read(cx);
        let camera = view.camera();
        let viewport = view.framing.viewport();
        let drawn = view.drawn();
        let (boxes, connectors) = drawn
            .as_ref()
            .and_then(|drawn| drawn.view(camera.zoom))
            .map_or((0, 0), |g| (g.boxes.len(), g.connectors.len()));
        let level = drawn
            .and_then(|drawn| drawn.level(camera.zoom))
            .map_or_else(
                || "-".to_owned(),
                |l| format!("{}/{}", l.index + 1, l.count),
            );
        bviz_switches::report(format_args!(
            "pan_bench ready scenario={} motion={} viewport={}x{} zoom={:.4} pan={:.1},{:.1} level={level} \
             view_boxes={boxes} view_connectors={connectors} press={} hovered={} neighborhood={}",
            self.bench.scenario.label(),
            self.bench.motion.label(),
            f32::from(viewport.size.width),
            f32::from(viewport.size.height),
            camera.zoom,
            camera.pan.0,
            camera.pan.1,
            self.bench.press.label(),
            view.hovered.is_some(),
            view.neighborhood_enabled,
        ));
    }

    fn report(&self) {
        self.report_metric("frame_interval_ms", |s| s.interval_ms);
        for phase in Phase::ALL {
            self.report_metric(phase.name(), |s| s.phase_ms[phase as usize]);
        }
        for tally in Tally::ALL {
            self.report_metric(tally.name(), |s| s.tallied[tally as usize]);
        }
    }

    fn report_metric(&self, name: &str, metric: impl Fn(&Sample) -> f64) {
        let scenario = self.bench.scenario.label();
        let motion = self.bench.motion.label();
        let press = self.bench.press.label();
        let frames = self.samples.len();
        let mut values: Vec<f64> = self.samples.iter().map(metric).collect();
        values.sort_by(f64::total_cmp);
        let rank = |q: f64| ((frames as f64 * q).ceil() as usize).saturating_sub(1);
        let show = |v: Option<&f64>| v.map_or_else(|| "nan".to_owned(), |v| format!("{v:.3}"));
        bviz_switches::report(format_args!(
            "pan_bench scenario={scenario} motion={motion} press={press} metric={name} frames={frames} \
             median={} p90={} max={}",
            show(values.get(rank(0.5))),
            show(values.get(rank(0.9))),
            show(values.last()),
        ));
    }
}

/// The camera of a view drawn and settled, with nothing deriving or easing
/// that would move what it shows; `None` while anything is.
fn at_rest(view: &GraphPanel) -> Option<Camera> {
    let camera = view.camera();
    let settled = view.framing.fitted()
        && !view.derived.deriving()
        && view.morph_frame().is_none()
        && view
            .drawn()
            .is_some_and(|drawn| drawn.underived(camera.zoom).is_none());
    settled.then_some(camera)
}

/// The window point at the per-axis median of the centers of the boxes on
/// screen; `None` where none is.
fn crowded_point(view: &GraphPanel) -> Option<Point<Pixels>> {
    let camera = view.camera();
    let viewport = view.framing.viewport();
    let visible = visible_rect(viewport, camera);
    let drawn = view.drawn()?;
    let g = drawn.view(camera.zoom)?;
    let (mut xs, mut ys): (Vec<f64>, Vec<f64>) = g
        .boxes
        .iter()
        .map(Rect::of)
        .filter(|r| visible.overlaps(r))
        .map(|r| (r.x + r.w / 2., r.y + r.h / 2.))
        .unzip();
    if xs.is_empty() {
        return None;
    }
    let mid = xs.len() / 2;
    let x = *xs.select_nth_unstable_by(mid, f64::total_cmp).1;
    let y = *ys.select_nth_unstable_by(mid, f64::total_cmp).1;
    Some(to_screen(x, y, viewport, camera))
}

/// The folded containers on screen nearest the viewport's center, at most
/// [`EXPANDED_PER_STEP`] of them, nearest first.
fn folded_nearest_center(view: &GraphPanel) -> Vec<Id> {
    let camera = view.camera();
    let visible = visible_rect(view.framing.viewport(), camera);
    let (cx, cy) = (visible.x + visible.w / 2., visible.y + visible.h / 2.);
    let expandable = view.expandable();
    let Some(drawn) = view.drawn() else {
        return Vec::new();
    };
    let Some(g) = drawn.view(camera.zoom) else {
        return Vec::new();
    };
    let mut folded: Vec<(f64, Id)> = g
        .boxes
        .iter()
        .filter(|b| expandable.holds(b) && !derive::is_expanded(view, b.id))
        .map(|b| (b, Rect::of(b)))
        .filter(|(_, r)| visible.overlaps(r))
        .map(|(b, r)| ((r.x + r.w / 2. - cx).hypot(r.y + r.h / 2. - cy), b.id))
        .collect();
    folded.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.0.cmp(&b.1.0)));
    folded
        .into_iter()
        .take(EXPANDED_PER_STEP)
        .map(|(_, id)| id)
        .collect()
}

/// The window point at the center of the leaf box nearest the viewport's
/// center among those whose center is on screen; `None` where none is.
fn leaf_nearest_center(view: &GraphPanel) -> Option<Point<Pixels>> {
    let camera = view.camera();
    let viewport = view.framing.viewport();
    let visible = visible_rect(viewport, camera);
    let (cx, cy) = (visible.x + visible.w / 2., visible.y + visible.h / 2.);
    let from_center = |(x, y): (f64, f64)| (x - cx).hypot(y - cy);
    let drawn = view.drawn()?;
    let g = drawn.view(camera.zoom)?;
    let (x, y, _) = g
        .boxes
        .iter()
        .filter(|b| !b.compound)
        .map(|b| {
            let r = Rect::of(b);
            (r.x + r.w / 2., r.y + r.h / 2., b.id)
        })
        .filter(|&(x, y, _)| visible.covers(x as f32, y as f32))
        .min_by(|&(ax, ay, a), &(bx, by, b)| {
            from_center((ax, ay))
                .total_cmp(&from_center((bx, by)))
                .then(a.0.cmp(&b.0))
        })?;
    Some(to_screen(x, y, viewport, camera))
}

/// Where the drag stands `frame` frames after pressing at `press`.
fn drag_path(press: Point<Pixels>, frame: usize) -> Point<Pixels> {
    let turn = TAU * frame as f32 / LAP_FRAMES;
    point(
        press.x + px(SWING.0 * turn.sin()),
        press.y + px(SWING.1 * (2. * turn).sin()),
    )
}

/// The wheel notches the zoom motion turns between its `frame` and the next,
/// tracing the zoom along a sine in wheel notches about where it starts.
fn zoom_path_step(frame: usize) -> f32 {
    let swing = |frame: usize| ZOOM_SWING_NOTCHES * (TAU * frame as f32 / LAP_FRAMES).sin();
    swing(frame + 1) - swing(frame)
}

/// Turns the wheel by `notches` holding the secondary modifier, which makes
/// the scroll a zoom rather than a pan.
fn dispatch_wheel(position: Point<Pixels>, notches: f32, window: &mut Window, cx: &mut App) {
    window.dispatch_event(
        PlatformInput::ScrollWheel(ScrollWheelEvent {
            position,
            delta: ScrollDelta::Lines(point(0., notches)),
            modifiers: Modifiers::secondary_key(),
            touch_phase: TouchPhase::Moved,
        }),
        cx,
    );
}

fn dispatch_move(
    position: Point<Pixels>,
    pressed_button: Option<MouseButton>,
    window: &mut Window,
    cx: &mut App,
) {
    let _span = span(Phase::InputDispatch);
    window.dispatch_event(
        PlatformInput::MouseMove(MouseMoveEvent {
            position,
            pressed_button,
            modifiers: Modifiers::default(),
        }),
        cx,
    );
}
