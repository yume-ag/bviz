//! Loads a Bazel `aquery`, `cquery`, or `query` `--output=streamed_proto` file
//! through `bviz_graph_layout`, lays the graph out, and opens a window
//! rendering the result.

use bviz_graph_layout::{Id, Kind};
use bviz_ui_components::{
    DragEnd, Dropdown, DropdownEvent, MultiSelect, MultiSelectEvent, ScrollbarColors, TextInput,
};
use bviz_ui_shell::Chrome;
use bviz_ui_theme::{DataColors, Theme, ThemeColors};
use clap::Parser;
use gpui::{
    AnyElement, Bounds, Context, Entity, FocusHandle, Focusable, IntoElement, KeyDownEvent,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, Render, Rgba,
    ScrollDelta, ScrollWheelEvent, Subscription, Window, canvas, div, prelude::*, px, size,
};
use std::path::PathBuf;
use std::time::{Duration, Instant};

mod build;
mod camera;
mod drag;
mod emphasis;
mod framing;
mod legend;
mod mode;
mod model;
mod morph;
mod neighborhood;
mod paint;
mod scroll;
mod search;
mod toolbar;

use build::{Input, InputKind};
use camera::{Camera, Rect, enclosed_boxes, from_screen, recenter_rect, zoom_step};
use drag::{Drag, end_drag, start_pan};
use emphasis::Emphasis;
use framing::{Framing, build_prepaint};
use legend::legend;
use mode::LayoutMode;
use model::{Drawn, Expandable, Model};
use morph::{Frame, Morph};
use paint::{Scene, TitleTarget, canvas_body};
use scroll::{scrollbar_hit, start_scroll_drag};
use search::Search;

/// Padding around the graph, in layout pixels.
const MARGIN: f64 = 16.0;

/// Largest zoom-in factor. The smallest is computed per graph so the whole graph
/// always fits the viewport.
const MAX_ZOOM: f32 = 4.0;

/// Factor one wheel notch scales the zoom by, small enough that a notch nudges
/// the view rather than jumping it.
const ZOOM_PER_NOTCH: f32 = 1.1;

/// Scroll delta one wheel notch amounts to, for the devices that report a delta
/// in pixels rather than in lines.
const PIXELS_PER_NOTCH: f32 = 120.0;

/// Fraction of the viewport the fitted graph spans, leaving a margin so the whole
/// graph sits within the inner part of the window rather than edge to edge.
const FIT_MARGIN: f32 = 2.0 / 3.0;

/// Breathing room held between the reframed content and the viewport edge when a
/// relayout settles the view, so a kept box never sits flush against the border.
const RECENTER_PAD: f32 = 24.0;

/// How long a toggle takes to come to rest — the camera easing onto the reframed
/// content, and each box onto its place in the new drawing, over the one stretch
/// of time. Short enough to stay responsive, long enough that the eye can follow
/// the shift instead of the view jumping there in a single frame.
const SETTLE_DURATION: Duration = Duration::from_millis(220);

/// Below these per-axis deltas a settle is a no-op and is not animated, so a
/// toggle that leaves the view already framed does not twitch.
const SETTLE_MIN_ZOOM_STEP: f32 = 0.002;
const SETTLE_MIN_PAN_STEP: f32 = 0.5;

/// Theme-resolved colors the canvas paints with, owned so the `'static` paint
/// closure can capture them.
#[derive(Clone, Copy)]
struct Palette {
    border: Rgba,
    /// Text on the controls framing the canvas.
    label: Rgba,
    /// Fill of a container box, and the sheet the legend floats on.
    container: Rgba,
    /// Fill of the outermost package box, which stands one step further off the
    /// canvas than `container` so nesting reads outermost first.
    package: Rgba,
    background: Rgba,
    /// Accent every emphasis is drawn in: the border picking out the
    /// just-toggled box and the wash over a marked box's fill.
    highlight: Rgba,
    /// The slots the node kinds and the edge provenances are colored from.
    data: DataColors,
    scroll: ScrollbarColors,
    /// The theme's own roles, for handing to a shared component that resolves
    /// its colors from them; the canvas paints from this palette's roles rather
    /// than from these.
    colors: ThemeColors,
}

impl Palette {
    fn from_theme(theme: &Theme) -> Palette {
        let colors = &theme.colors;
        Palette {
            border: colors.border.into(),
            label: colors.text.into(),
            container: colors.panel.into(),
            package: colors.surface.into(),
            background: colors.background.into(),
            highlight: colors.highlight.into(),
            data: theme.data,
            scroll: ScrollbarColors::from_colors(colors),
            colors: *colors,
        }
    }
}

/// Loads, lays out, and renders a Bazel build graph.
struct GraphView {
    focus_handle: FocusHandle,
    model: Model,
    framing: Framing,
    drag: Drag,
    emphasis: Emphasis,
    morph: Morph,
    mode: LayoutMode,
    mode_picker: Entity<Dropdown>,
    /// Carries the picker's selections into [`GraphView::set_mode`].
    _mode_choices: Subscription,
    kind_picker: Entity<MultiSelect>,
    /// Carries the picker's toggles into [`model::set_kind_visible`].
    _kind_choices: Subscription,
    search: Search,
    /// Box under the cursor.
    hovered: Option<Id>,
    /// Whether hovering a box marks its neighborhood; flipped by the `h` key or
    /// the toolbar toggle so the marking can be turned off when it is more noise
    /// than help.
    neighborhood_enabled: bool,
    /// Whether the layered layout draws the transitive reduction of the edges
    /// (the default) rather than every edge; flipped by the `r` key or the
    /// toolbar toggle, which relays out.
    reduced: bool,
    /// The titles the canvas drew, rewritten by every paint and resolved against
    /// a pointer by [`box_at`].
    titles: Vec<TitleTarget>,
}

impl GraphView {
    fn camera(&self) -> Camera {
        self.framing.camera()
    }

    /// The layout the selected mode draws, or `None` while it is being computed.
    fn drawn(&self) -> Option<Drawn> {
        self.model.drawn(self.mode)
    }

    /// The boxes the selected mode's layout lets the user open or close.
    fn expandable(&self) -> Expandable {
        self.model.expandable(self.mode)
    }

    /// How far the drawing being eased onto its successor has travelled at this
    /// instant; `None` at rest, where every box sits at its own layout rect. The
    /// paint and every gesture that names a box place through this one frame, so
    /// what the cursor lands on is what is drawn under it.
    fn morph_frame(&self) -> Option<Frame> {
        self.morph.at(Instant::now())
    }

    fn new(inputs: Vec<Input>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        focus_handle.focus(window, cx);
        let mode = LayoutMode::default();
        let mode_picker = cx.new(|cx| {
            Dropdown::new(
                "layout-mode",
                LayoutMode::ALL.map(LayoutMode::label),
                mode.index(),
                cx,
            )
        });
        // The picker notifies on every repaint it makes, and only a selection
        // should move the view off its layout.
        let choices = cx.subscribe(&mode_picker, |view, _, event, cx| {
            let DropdownEvent::Changed(index) = event;
            if let Some(mode) = LayoutMode::at(*index) {
                view.set_mode(mode, cx);
            }
        });
        let kind_picker = cx.new(|cx| {
            MultiSelect::new(
                "kind-filter",
                "kinds",
                Kind::ALL.map(Kind::label),
                (0..Kind::ALL.len()).collect(),
                cx,
            )
        });
        let kind_choices = cx.subscribe(&kind_picker, |view, picker, event, cx| {
            let MultiSelectEvent::Changed(index) = *event;
            let Some(&kind) = Kind::ALL.get(index) else {
                return;
            };
            let checked = picker.read(cx).selected().contains(&index);
            model::set_kind_visible(view, kind, checked, cx);
        });
        let search = Search::new(cx);
        Self {
            focus_handle,
            model: Model::loading(inputs, cx),
            framing: Framing::new(),
            drag: Drag::None,
            emphasis: Emphasis::none(),
            morph: Morph::none(),
            mode,
            mode_picker,
            _mode_choices: choices,
            kind_picker,
            _kind_choices: kind_choices,
            search,
            hovered: None,
            neighborhood_enabled: true,
            reduced: true,
            titles: Vec::new(),
        }
    }

    /// Draws the selected mode's layout, or a spinner while it is still being
    /// computed — the toolbar stays above either, so the picker remains reachable.
    fn viewport(&mut self, palette: Palette, cx: &mut Context<Self>) -> AnyElement {
        let Some(drawn) = self.drawn() else {
            return bviz_ui_components::centered_spinner("Laying out…", &palette.colors)
                .into_any_element();
        };
        let g = drawn.view(self.camera().zoom);
        let edge_legend = legend(g, &palette);
        let content = drawn.content();
        let recenter_target = recenter_rect(g, &self.emphasis);
        let scene = Scene {
            expandable: self.expandable(),
            palette,
            neighborhood: self.hovered.filter(|_| self.neighborhood_enabled),
            highlight: self.emphasis.highlight(),
            drawn,
        };
        let entity = cx.entity();
        let prepaint = build_prepaint(entity.clone(), content, recenter_target);
        div()
            .id("graph-viewport")
            .relative()
            .size_full()
            .overflow_hidden()
            .child(
                canvas(prepaint, canvas_body(scene, entity))
                    .absolute()
                    .left_0()
                    .top_0()
                    .size_full(),
            )
            .children(edge_legend)
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_hover(cx.listener(Self::on_hover))
            .on_scroll_wheel(cx.listener(Self::on_scroll_wheel))
            // Track the shared focus handle here too (the chrome already tracks
            // it on an ancestor) so this deepest focused element receives key
            // events. Events still bubble up to the chrome's own key bindings.
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::on_key_down))
            .into_any_element()
    }

    /// Switches which layout is drawn, starting the newly selected one's if it
    /// has not been computed yet. The two span different extents, so the view
    /// refits.
    ///
    /// The hover, the emphasis and the morph all describe the layout being left:
    /// an emphasis carried across would let a relayout still in flight settle the
    /// incoming view onto a box picked out in the other one, and highlight it,
    /// and a morph would ease boxes of the layout left behind over the one
    /// arriving.
    fn set_mode(&mut self, mode: LayoutMode, cx: &mut Context<Self>) {
        if self.mode == mode {
            return;
        }
        self.mode = mode;
        self.hovered = None;
        self.emphasis.clear();
        self.morph.clear();
        self.framing.refit();
        if self.drawn().is_none() && !self.model.relaying() {
            model::relayout(self, cx);
        }
        cx.notify();
    }

    fn on_mouse_down(&mut self, e: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        // A press on a scroll-bar gutter drives the thumb instead of panning the
        // content under it.
        if let Some(axis) = scrollbar_hit(self, e.position) {
            start_scroll_drag(self, axis, e.position, cx);
            return;
        }
        start_pan(self, e.position, cx);
    }

    /// Ends the drag on the release the element receives directly, which the
    /// window-global handlers can miss: they are installed only once a frame
    /// paints with a drag in progress, so a press-release quick enough to finish
    /// before that paint would leave the drag running.
    fn on_mouse_up(&mut self, e: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        end_drag(self, e.position, DragEnd::Released, cx);
    }

    fn on_mouse_move(&mut self, e: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        // A pan moves the view under a stationary pointer, so the box it is over
        // is the one already resolved.
        if matches!(self.drag, Drag::Pan(_)) {
            return;
        }
        let Some(drawn) = self.drawn() else {
            return;
        };
        let camera = self.camera();
        let vp = self.framing.viewport();
        let morph = self.morph_frame();
        let hit = box_at(
            &self.titles,
            drawn.view(camera.zoom),
            morph.as_ref(),
            vp,
            camera,
            e.position,
        )
        .map(|b| b.id);
        if hit != self.hovered {
            self.hovered = hit;
            cx.notify();
        }
    }

    /// Drops the hover when the pointer leaves the canvas, so a node's
    /// neighborhood does not stay marked while the cursor is over the toolbar or
    /// the window chrome — the move handler is bounds-gated and never fires there.
    /// gpui reports the canvas unhovered while a button is held, so the drag guard
    /// keeps a pan from dropping the marked set it started with.
    fn on_hover(&mut self, over: &bool, _: &mut Window, cx: &mut Context<Self>) {
        if !over && matches!(self.drag, Drag::None) && self.hovered.take().is_some() {
            cx.notify();
        }
    }

    fn on_scroll_wheel(&mut self, e: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(content) = self.drawn().map(|d| d.content()) else {
            return;
        };
        let notches = match e.delta {
            ScrollDelta::Lines(p) => p.y,
            ScrollDelta::Pixels(p) => f32::from(p.y) / PIXELS_PER_NOTCH,
        };
        let viewport = self.framing.viewport();
        self.framing.move_to(zoom_step(
            e.position,
            self.camera(),
            notches,
            viewport,
            content,
        ));
        cx.notify();
    }

    fn toggle_neighborhood(&mut self, cx: &mut Context<Self>) {
        self.neighborhood_enabled = !self.neighborhood_enabled;
        cx.notify();
    }

    fn toggle_reduce(&mut self, cx: &mut Context<Self>) {
        if !self.mode.honors_reduce() {
            return;
        }
        self.reduced = !self.reduced;
        model::relayout(self, cx);
        cx.notify();
    }

    fn on_key_down(&mut self, e: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if e.keystroke.key == "h" {
            self.toggle_neighborhood(cx);
        } else if e.keystroke.key == "r" {
            self.toggle_reduce(cx);
        }
    }
}

impl Render for GraphView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::from_theme(&bviz_ui_theme::theme(window, cx));
        let body = match self.model.source().placeholder(&palette.colors) {
            Some(placeholder) => placeholder.into_any_element(),
            None => {
                let search_focused = self.search.input.focus_handle(cx).is_focused(window);
                let query = self.search.input.read(cx).content();
                let (search_matches, search_over) = match self.model.raw() {
                    Some(raw) if search_focused => {
                        let (found, over) = search::matches(raw, &query);
                        (
                            found
                                .into_iter()
                                .map(|n| search::Match {
                                    id: n.id,
                                    label: n.label.clone(),
                                    visible: self.model.is_visible(n.id),
                                })
                                .collect(),
                            over,
                        )
                    }
                    _ => (Vec::new(), 0),
                };
                let controls = Controls {
                    mode: self.mode,
                    picker: self.mode_picker.clone(),
                    kind_picker: self.kind_picker.clone(),
                    search_input: self.search.input.clone(),
                    search_focused,
                    search_matches,
                    search_over,
                    neighborhood_enabled: self.neighborhood_enabled,
                    reduced: self.reduced,
                    relaying: self.model.relaying(),
                };
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .child(toolbar::toolbar(controls, &palette, cx))
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .overflow_hidden()
                            .child(self.viewport(palette, cx)),
                    )
                    .into_any_element()
            }
        };
        Chrome::new(&self.focus_handle)
            .title("bazel_viz graph")
            .status(self.model.status(self.mode, self.camera().zoom))
            .child(body)
    }
}

/// What the toolbar's controls show.
struct Controls {
    mode: LayoutMode,
    picker: Entity<Dropdown>,
    kind_picker: Entity<MultiSelect>,
    search_input: Entity<TextInput>,
    /// Whether `search_input` holds focus, so its dropdown of `search_matches`
    /// is open.
    search_focused: bool,
    search_matches: Vec<search::Match>,
    /// Matches beyond `search_matches`'s cap, for a trailing count.
    search_over: usize,
    neighborhood_enabled: bool,
    reduced: bool,
    relaying: bool,
}

/// The box a pointer at `pos` (window px) resolves to, which every gesture that
/// names a box — the hover that marks a neighborhood, the click that opens or
/// closes one — goes through, so the box under the cursor is the same box for
/// all of them.
///
/// A title the frame painted wins over the box the cursor falls in, because a
/// title the zoom has floated out of its container's band is drawn over the
/// boxes inside it: there the innermost box under the cursor is one of those,
/// and the opaque title covering it is both what the user sees and what the
/// user is aiming at. A title is taken only where the box it names is drawn over
/// the cursor in `g` under `camera` and `morph`. Since a title is always painted
/// within its own box, that leaves a set recorded from a frame the view has moved
/// on from — a relayout, a mode switch, a camera step — resolving to the box
/// under the cursor instead.
fn box_at<'a>(
    titles: &[TitleTarget],
    g: &'a bviz_graph_layout::LaidOut,
    morph: Option<&Frame>,
    viewport: Bounds<Pixels>,
    camera: Camera,
    pos: Point<Pixels>,
) -> Option<&'a bviz_graph_layout::Box> {
    let (gx, gy) = from_screen(pos, viewport.origin, camera);
    let titled = paint::title_at(titles, pos)
        .and_then(|id| g.boxes.iter().find(|b| b.id == id))
        .filter(|b| placed_rect(b, morph).covers(gx, gy));
    titled.or_else(|| hit_box(&g.boxes, morph, gx, gy))
}

/// Opens or closes the box under `pos` (window px) and settles the view to keep
/// it in frame. A box the drawn layout holds no handle on, or a `pos` off the
/// canvas, is left alone.
fn click_box_at(view: &mut GraphView, pos: Point<Pixels>, cx: &mut Context<GraphView>) {
    let vp = view.framing.viewport();
    if !vp.contains(&pos) {
        return;
    }
    let camera = view.camera();
    let Some(drawn) = view.drawn() else {
        return;
    };
    let g = drawn.view(camera.zoom);
    let morph = view.morph_frame();
    let hit = box_at(&view.titles, g, morph.as_ref(), vp, camera, pos)
        .filter(|b| view.expandable().holds(b));
    let Some(id) = hit.map(|b| b.id) else {
        return;
    };
    let enclosed = enclosed_boxes(&g.boxes, vp.size, camera);
    // The relayout places the boxes afresh, so the box resolved here need not be
    // the one the cursor ends up over.
    view.hovered = None;
    model::toggle_expansion(view, id, cx);
    view.emphasis.toggled(id, enclosed);
}

/// Where `b` is drawn: its own layout rect at rest, and the rect `morph` has
/// eased it to while one drawing settles onto another.
fn placed_rect(b: &bviz_graph_layout::Box, morph: Option<&Frame>) -> Rect {
    morph.map_or(Rect::of(b), |frame| frame.place(b).0)
}

/// The smallest-area box (in layout space) drawn over `(gx, gy)`, so an inner
/// member or header wins over its enclosing container; `None` if outside all.
/// The boxes a morph is fading out are absent from `boxes`, so a gesture passes
/// through them to whatever the new drawing puts there.
fn hit_box<'a>(
    boxes: &'a [bviz_graph_layout::Box],
    morph: Option<&Frame>,
    gx: f32,
    gy: f32,
) -> Option<&'a bviz_graph_layout::Box> {
    boxes
        .iter()
        .map(|b| (b, placed_rect(b, morph)))
        .filter(|(_, rect)| rect.covers(gx, gy))
        .min_by(|(_, a), (_, b)| (a.w * a.h).total_cmp(&(b.w * b.h)))
        .map(|(b, _)| b)
}

/// Renders a Bazel build graph from one or more `--output=streamed_proto`
/// files. Each flag selects a file and the query that produced it; passing
/// several merges them into one joint graph, with shared entities collapsed.
#[derive(Parser)]
#[command(group(
    clap::ArgGroup::new("input").required(true).multiple(true).args(["aquery", "cquery", "query"])
))]
struct Cli {
    /// `bazel aquery --output=streamed_proto` file.
    #[arg(long, value_name = "FILE")]
    aquery: Option<PathBuf>,
    /// `bazel cquery --output=streamed_proto` file (run cquery with
    /// `--transitions=lite` for configuration-resolved edges).
    #[arg(long, value_name = "FILE")]
    cquery: Option<PathBuf>,
    /// `bazel query --output=streamed_proto` file.
    #[arg(long, value_name = "FILE")]
    query: Option<PathBuf>,
}

impl Cli {
    /// Every provided input flag, in a stable order: aquery, cquery, query.
    fn inputs(self) -> Vec<Input> {
        let mut inputs = Vec::new();
        if let Some(path) = self.aquery {
            inputs.push(Input {
                kind: InputKind::Aquery,
                path,
            });
        }
        if let Some(path) = self.cquery {
            inputs.push(Input {
                kind: InputKind::Cquery,
                path,
            });
        }
        if let Some(path) = self.query {
            inputs.push(Input {
                kind: InputKind::Query,
                path,
            });
        }
        inputs
    }
}

/// Parses the query inputs and opens the graph-viewer window.
pub fn run() {
    let inputs = Cli::parse().inputs();
    bviz_ui_shell::run(
        "bazel_viz graph",
        size(px(1280.), px(820.)),
        move |window, cx| cx.new(|cx| GraphView::new(inputs, window, cx)),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::camera::{
        Rect, clamp_pan, ease_out_cubic, fit_view, fit_zoom, settle_view, to_screen,
    };
    use crate::paint::lane_polyline;
    use bviz_graph_layout::{Box, Kind};
    use gpui::{Bounds, Size, point};

    fn lbox(id: u32, x: f64, y: f64, w: f64, h: f64) -> Box {
        Box {
            id: bviz_graph_layout::Id(id),
            label: String::new(),
            kind: Kind::Source,
            x,
            y,
            width: w,
            height: h,
            compound: false,
            group: false,
        }
    }

    #[test]
    fn from_screen_inverts_to_screen() {
        let bounds = Bounds::new(point(px(10.), px(20.)), size(px(800.), px(600.)));
        let camera = Camera {
            zoom: 1.7,
            pan: (33.0, -12.0),
        };
        for &(x, y) in &[(0.0_f64, 0.0_f64), (42.5, 7.25), (123.0, 456.0)] {
            let screen = to_screen(x, y, bounds, camera);
            let (gx, gy) = from_screen(screen, bounds.origin, camera);
            assert!((gx as f64 - x).abs() < 1e-3, "x: {gx} vs {x}");
            assert!((gy as f64 - y).abs() < 1e-3, "y: {gy} vs {y}");
        }
    }

    #[test]
    fn an_emphasis_holds_a_target_in_whichever_layout_is_drawn() {
        // A box the emphasis names is named by graph id, which every layout
        // shares, so one left over from the layout that produced it still
        // resolves against the one switched to — and would settle the fresh view
        // onto it. Clearing it, as a mode switch does, leaves nothing to settle.
        let mut emphasis = Emphasis::none();
        emphasis.toggled(bviz_graph_layout::Id(7), vec![bviz_graph_layout::Id(1)]);
        let g = bviz_graph_layout::LaidOut {
            boxes: vec![lbox(1, 10.0, 20.0, 30.0, 40.0)],
            ..Default::default()
        };
        assert!(recenter_rect(&g, &emphasis).is_some());
        emphasis.clear();
        assert!(recenter_rect(&g, &emphasis).is_none());
    }

    #[test]
    fn hit_box_picks_innermost_and_none_outside() {
        // An outer container (id 0) with a small inner box (id 1) nested in it.
        let boxes = vec![
            lbox(0, 0.0, 0.0, 100.0, 100.0),
            lbox(1, 20.0, 20.0, 30.0, 30.0),
        ];
        // Inside both rects: the smaller (inner) wins.
        assert_eq!(
            hit_box(&boxes, None, 30.0, 30.0).unwrap().id,
            bviz_graph_layout::Id(1)
        );
        // Inside only the container.
        assert_eq!(
            hit_box(&boxes, None, 5.0, 5.0).unwrap().id,
            bviz_graph_layout::Id(0)
        );
        // Outside everything.
        assert!(hit_box(&boxes, None, 200.0, 200.0).is_none());
        assert!(hit_box(&boxes, None, -1.0, 50.0).is_none());
    }

    #[test]
    fn lane_polyline_offsets_every_vertex_in_parallel() {
        // An L route: right to (100,0), then down to (100,100).
        let lane = lane_polyline(&[(0.0, 0.0), (100.0, 0.0), (100.0, 100.0)], 4.0);
        // Endpoints are pushed perpendicular too (not anchored), so the first and
        // last segments stay parallel to the route and meet the node offset.
        assert!(
            (lane[0].0).abs() < 0.01 && (lane[0].1 - 4.0).abs() < 0.5,
            "start {:?}",
            lane[0]
        );
        assert!(
            (lane[2].0 - 96.0).abs() < 0.5 && (lane[2].1 - 100.0).abs() < 0.01,
            "end {:?}",
            lane[2]
        );
        // First segment horizontal like the route's (no fanning in at the node).
        assert!(
            (lane[0].1 - lane[1].1).abs() < 0.01,
            "first seg not parallel: {lane:?}"
        );
        // Last segment vertical like the route's.
        assert!(
            (lane[1].0 - lane[2].0).abs() < 0.01,
            "last seg not parallel: {lane:?}"
        );
    }

    #[test]
    fn lane_polyline_zero_offset_is_identity() {
        let route = vec![(0.0, 0.0), (10.0, 0.0), (10.0, 10.0)];
        assert_eq!(lane_polyline(&route, 0.0), route);
    }

    #[test]
    fn lane_polyline_opposite_signs_nest_on_opposite_sides() {
        let route = vec![(0.0, 0.0), (100.0, 0.0), (100.0, 100.0)];
        let plus = lane_polyline(&route, 4.0)[1];
        let minus = lane_polyline(&route, -4.0)[1];
        // The two lanes never cross: their corners fall on opposite sides.
        assert_ne!(plus.1.signum(), minus.1.signum(), "{plus:?} {minus:?}");
    }

    #[test]
    fn lane_polyline_parallel_shifts_a_straight_run() {
        // A straight two-point edge shifts whole, both ends offset perpendicular.
        let lane = lane_polyline(&[(0.0, 0.0), (100.0, 0.0)], 4.0);
        assert_eq!(lane.len(), 2);
        assert!(
            (lane[0].0).abs() < 0.01 && (lane[0].1 - 4.0).abs() < 0.5,
            "{lane:?}"
        );
        assert!(
            (lane[1].0 - 100.0).abs() < 0.01 && (lane[1].1 - 4.0).abs() < 0.5,
            "{lane:?}"
        );
    }

    // The whole graph, large enough that fit_zoom's floor stays well below 1.0 and
    // never bites in these cases.
    const CONTENT: (f32, f32) = (4000.0, 3000.0);

    fn vp(w: f32, h: f32) -> Size<Pixels> {
        size(px(w), px(h))
    }

    /// Asserts `target`'s screen rect under `(zoom, pan)` lies fully within the
    /// viewport, inset by the breathing-room pad (with a small tolerance).
    fn assert_enclosed(target: Rect, vw: f32, vh: f32, zoom: f32, pan: (f32, f32)) {
        let (bx, by, bw, bh) = (
            target.x as f32,
            target.y as f32,
            target.w as f32,
            target.h as f32,
        );
        let x0 = (bx + MARGIN as f32) * zoom + pan.0;
        let y0 = (by + MARGIN as f32) * zoom + pan.1;
        let x1 = (bx + bw + MARGIN as f32) * zoom + pan.0;
        let y1 = (by + bh + MARGIN as f32) * zoom + pan.1;
        let eps = 0.5;
        let (lo, hix, hiy) = (
            RECENTER_PAD - eps,
            vw - RECENTER_PAD + eps,
            vh - RECENTER_PAD + eps,
        );
        assert!(
            x0 >= lo && x1 <= hix,
            "x out of view: {x0}..{x1} in 0..{vw}"
        );
        assert!(
            y0 >= lo && y1 <= hiy,
            "y out of view: {y0}..{y1} in 0..{vh}"
        );
    }

    #[test]
    fn settle_view_leaves_a_fitting_centered_target_put() {
        // A small rect already comfortably inside: neither zoom nor pan moves.
        let target = Rect {
            x: 375.0,
            y: 280.0,
            w: 50.0,
            h: 40.0,
        };
        let (z, p) = settle_view(target, vp(800., 600.), CONTENT, 1.0, (0.0, 0.0));
        assert!((z - 1.0).abs() < 1e-4, "zoom moved: {z}");
        assert!(p.0.abs() < 1e-3 && p.1.abs() < 1e-3, "pan moved: {p:?}");
    }

    #[test]
    fn settle_view_only_zooms_out_to_enclose_a_big_target() {
        // A rect larger than the viewport at the current zoom forces a zoom-out,
        // never below the whole-graph fit, and ends up fully framed.
        let target = Rect {
            x: 0.0,
            y: 0.0,
            w: 2000.0,
            h: 1500.0,
        };
        let (z, p) = settle_view(target, vp(800., 600.), CONTENT, 1.0, (0.0, 0.0));
        assert!(z < 1.0, "expected zoom-out, got {z}");
        assert!(
            z >= fit_zoom(vp(800., 600.), CONTENT) - 1e-4,
            "below whole-graph fit: {z}"
        );
        assert_enclosed(target, 800., 600., z, p);
    }

    #[test]
    fn settle_view_never_zooms_in_on_a_small_target() {
        // A node opening smaller than the current zoom keeps the zoom — the
        // disorienting blow-up-to-fill case is gone.
        let target = Rect {
            x: 100.0,
            y: 100.0,
            w: 30.0,
            h: 20.0,
        };
        let (z, _) = settle_view(target, vp(800., 600.), CONTENT, 0.5, (0.0, 0.0));
        assert!(z <= 0.5 + 1e-4, "zoomed in: {z}");
    }

    #[test]
    fn settle_view_pans_an_offscreen_target_into_view_without_rezoom() {
        // A small rect off to the right fits at the current zoom, so the zoom
        // holds and only the pan shifts it back into frame.
        let target = Rect {
            x: 1000.0,
            y: 100.0,
            w: 50.0,
            h: 40.0,
        };
        let (z, p) = settle_view(target, vp(800., 600.), CONTENT, 1.0, (0.0, 0.0));
        assert!((z - 1.0).abs() < 1e-4, "zoom moved: {z}");
        assert!(p.0 < 0.0, "expected a leftward pan, got {p:?}");
        assert_enclosed(target, 800., 600., z, p);
    }

    /// The window in layout space that the culling tests measure against.
    const WINDOW: Rect = Rect {
        x: 0.0,
        y: 0.0,
        w: 10.0,
        h: 10.0,
    };

    #[test]
    fn a_route_is_kept_only_where_it_runs_through_the_window() {
        // A long diagonal whose bounding box swallows the window, but which
        // passes well clear of it: the bounds alone would keep it.
        let across = [(0.0, 1000.0), (1000.0, 0.0)];
        assert!(WINDOW.overlaps(&Rect {
            x: 0.0,
            y: 0.0,
            w: 1000.0,
            h: 1000.0
        }));
        assert!(!WINDOW.crossed_by(&across));
        assert!(WINDOW.crossed_by(&[(-100.0, 5.0), (100.0, 5.0)]));
        // A route entirely inside, and one that only clips a corner leg.
        assert!(WINDOW.crossed_by(&[(2.0, 2.0), (8.0, 8.0)]));
        assert!(WINDOW.crossed_by(&[(-5.0, -5.0), (-5.0, 5.0), (5.0, 5.0)]));
        assert!(!WINDOW.crossed_by(&[(-5.0, -5.0), (-5.0, 50.0), (-1.0, 50.0)]));
    }

    #[test]
    fn a_straight_run_along_an_axis_still_registers() {
        // A vertical or horizontal leg spans no area, so an area test alone
        // would drop it.
        assert!(WINDOW.crossed_by(&[(5.0, -50.0), (5.0, 50.0)]));
        assert!(WINDOW.crossed_by(&[(-50.0, 5.0), (50.0, 5.0)]));
        assert!(!WINDOW.crossed_by(&[(50.0, -50.0), (50.0, 50.0)]));
    }

    #[test]
    fn ease_out_cubic_endpoints_and_deceleration() {
        assert!(ease_out_cubic(0.0).abs() < 1e-6, "{}", ease_out_cubic(0.0));
        assert!(
            (ease_out_cubic(1.0) - 1.0).abs() < 1e-6,
            "{}",
            ease_out_cubic(1.0)
        );
        // Ease-out: past the halfway mark in eased progress before halfway in time.
        assert!(ease_out_cubic(0.5) > 0.5, "{}", ease_out_cubic(0.5));
        // Monotonically increasing.
        let mut prev = ease_out_cubic(0.0);
        for i in 1..=10 {
            let e = ease_out_cubic(i as f32 / 10.0);
            assert!(e >= prev, "not monotonic at {i}: {e} < {prev}");
            prev = e;
        }
    }

    #[test]
    fn enclosed_boxes_keeps_only_fully_visible() {
        let boxes = vec![
            lbox(0, 100.0, 100.0, 50.0, 40.0),  // fully inside
            lbox(1, 760.0, 100.0, 100.0, 40.0), // spills past the right edge
            lbox(2, 100.0, 580.0, 50.0, 40.0),  // spills past the bottom edge
            lbox(3, 200.0, 200.0, 30.0, 30.0),  // fully inside
        ];
        let mut ids = enclosed_boxes(
            &boxes,
            vp(800., 600.),
            Camera {
                zoom: 1.0,
                pan: (0.0, 0.0),
            },
        );
        ids.sort_by_key(|id| id.0);
        assert_eq!(
            ids,
            vec![bviz_graph_layout::Id(0), bviz_graph_layout::Id(3)]
        );
    }

    /// The slack `clamp_pan` allows past the flush position on an axis.
    fn clearance(vp_axis: f32, extent: f32) -> f32 {
        (1.0 - FIT_MARGIN) * vp_axis.min(extent)
    }

    #[test]
    fn clamp_pan_keeps_smaller_content_within_the_viewport() {
        // Content smaller than the viewport (zoom 0.05) stays inside, give or take the
        // edge clearance, so the pan stays in -clearance..=(viewport - content + clearance).
        let viewport = vp(800., 600.);
        let zoom = 0.05;
        let (cw, ch) = (CONTENT.0 * zoom, CONTENT.1 * zoom);
        let (mx, my) = (clearance(800., cw), clearance(600., ch));
        assert_eq!(
            clamp_pan((-9999.0, -9999.0), zoom, viewport, CONTENT),
            (-mx, -my)
        );
        assert_eq!(
            clamp_pan((9999.0, 9999.0), zoom, viewport, CONTENT),
            (800.0 - cw + mx, 600.0 - ch + my)
        );
        // A pan within the bound is left untouched, including one that overscrolls
        // into the clearance.
        assert_eq!(
            clamp_pan((-50.0, -40.0), zoom, viewport, CONTENT),
            (-50.0, -40.0)
        );
        assert_eq!(
            clamp_pan((100.0, 80.0), zoom, viewport, CONTENT),
            (100.0, 80.0)
        );
    }

    #[test]
    fn clamp_pan_keeps_larger_content_covering_the_viewport() {
        // Content larger than the viewport (zoom 1.0) may pull a clearance-sized gutter
        // past an edge but no further, so the pan stays in
        // (viewport - content - clearance)..=clearance.
        let viewport = vp(800., 600.);
        let (cw, ch) = (CONTENT.0, CONTENT.1);
        let (mx, my) = (clearance(800., cw), clearance(600., ch));
        assert_eq!(
            clamp_pan((9999.0, 9999.0), 1.0, viewport, CONTENT),
            (mx, my)
        );
        assert_eq!(
            clamp_pan((-9999.0, -9999.0), 1.0, viewport, CONTENT),
            (800.0 - cw - mx, 600.0 - ch - my)
        );
        // The flush-cover position sits inside the bound, so it is left alone.
        assert_eq!(clamp_pan((0.0, 0.0), 1.0, viewport, CONTENT), (0.0, 0.0));
    }

    #[test]
    fn clamp_pan_never_carries_a_small_graph_off_an_edge() {
        let viewport = vp(800., 600.);
        let content = (60.0, 40.0);
        let near = clamp_pan((-9999.0, -9999.0), 1.0, viewport, content);
        let far = clamp_pan((9999.0, 9999.0), 1.0, viewport, content);
        assert!(
            near.0 + content.0 > 0. && near.1 + content.1 > 0.,
            "off the near edge: {near:?}"
        );
        assert!(far.0 < 800. && far.1 < 600., "off the far edge: {far:?}");
    }

    /// Float slack in the anchor assertions, far below a pixel of visible drift.
    const ANCHOR_TOLERANCE: f32 = 0.05;

    /// A canvas laid out away from the window origin, so a step that neglects to
    /// take the canvas offset off the cursor lands visibly wrong.
    fn canvas(w: f32, h: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(10.), px(20.)), vp(w, h))
    }

    /// Cursor positions over `bounds` covering its center, its four edges and its
    /// four corners.
    fn cursors(bounds: Bounds<Pixels>) -> impl Iterator<Item = Point<Pixels>> {
        const ALONG: [f32; 3] = [0.02, 0.5, 0.98];
        let (w, h) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
        let (ox, oy) = (f32::from(bounds.origin.x), f32::from(bounds.origin.y));
        ALONG.into_iter().flat_map(move |a| {
            ALONG
                .into_iter()
                .map(move |b| point(px(ox + a * w), px(oy + b * h)))
        })
    }

    /// How far the layout point under `cursor` travels on screen across one wheel
    /// step: the drift the cursor anchoring exists to hold at zero.
    fn anchor_drift(
        cursor: Point<Pixels>,
        camera: Camera,
        notches: f32,
        bounds: Bounds<Pixels>,
        content: (f32, f32),
    ) -> (f32, f32) {
        let (gx, gy) = from_screen(cursor, bounds.origin, camera);
        let after = zoom_step(cursor, camera, notches, bounds, content);
        let landed = to_screen(gx as f64, gy as f64, bounds, after);
        (
            f32::from(landed.x) - f32::from(cursor.x),
            f32::from(landed.y) - f32::from(cursor.y),
        )
    }

    /// Asserts a step in and a step out hold their anchor at every cursor over
    /// `bounds`, naming `state` in the failure so the camera it started from is
    /// identifiable.
    fn assert_anchored(state: &str, camera: Camera, bounds: Bounds<Pixels>, content: (f32, f32)) {
        for cursor in cursors(bounds) {
            for notches in [1.0, -1.0] {
                let (dx, dy) = anchor_drift(cursor, camera, notches, bounds, content);
                assert!(
                    dx.abs() < ANCHOR_TOLERANCE && dy.abs() < ANCHOR_TOLERANCE,
                    "{state}: {notches:+} notches at {cursor:?} slipped the anchor by ({dx}, {dy}) px"
                );
            }
        }
    }

    /// The camera showing `content` centered in `bounds` at `zoom`.
    fn centered(zoom: f32, bounds: Bounds<Pixels>, content: (f32, f32)) -> Camera {
        let (vw, vh) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
        Camera {
            zoom,
            pan: ((vw - content.0 * zoom) / 2., (vh - content.1 * zoom) / 2.),
        }
    }

    #[test]
    fn a_wheel_step_holds_its_anchor_while_the_graph_fits_the_window() {
        let bounds = canvas(800., 600.);
        let (zoom, pan) = fit_view(bounds.size, CONTENT);
        assert_anchored("fitted", Camera { zoom, pan }, bounds, CONTENT);
    }

    #[test]
    fn a_wheel_step_holds_its_anchor_when_one_axis_overflows() {
        // A wide graph zoomed until its width spills past the window while its
        // height still fits, so one scroll bar is up.
        const WIDE: (f32, f32) = (4000.0, 900.0);
        let bounds = canvas(800., 600.);
        let zoom = 0.4;
        assert!(WIDE.0 * zoom > 800. && WIDE.1 * zoom < 600.);
        assert_anchored("wide graph", centered(zoom, bounds, WIDE), bounds, WIDE);
    }

    #[test]
    fn a_wheel_step_holds_its_anchor_when_both_axes_overflow() {
        let bounds = canvas(800., 600.);
        let zoom = 0.5;
        assert!(CONTENT.0 * zoom > 800. && CONTENT.1 * zoom > 600.);
        let (cw, ch) = (CONTENT.0 * zoom, CONTENT.1 * zoom);
        for (state, pan) in [
            ("centered", ((800. - cw) / 2., (600. - ch) / 2.)),
            ("flush at the near edges", (0.0, 0.0)),
            ("flush at the far edges", (800. - cw, 600. - ch)),
            ("a gutter past the near edges", (100.0, 80.0)),
        ] {
            assert_anchored(state, Camera { zoom, pan }, bounds, CONTENT);
        }
    }

    #[test]
    fn every_step_of_a_wheel_run_holds_its_anchor() {
        // What a wheel actually sends: notch after notch at one cursor, from a
        // view scrolled off the near edges of a graph larger than the window.
        let bounds = canvas(800., 600.);
        let cursor = point(px(10. + 600.), px(20. + 450.));
        let mut camera = Camera {
            zoom: 0.5,
            pan: (100.0, 80.0),
        };
        for step in 0..4 {
            let (dx, dy) = anchor_drift(cursor, camera, -1.0, bounds, CONTENT);
            assert!(
                dx.abs() < ANCHOR_TOLERANCE && dy.abs() < ANCHOR_TOLERANCE,
                "step {step} slipped the anchor by ({dx}, {dy}) px"
            );
            camera = zoom_step(cursor, camera, -1.0, bounds, CONTENT);
        }
    }

    #[test]
    fn a_wheel_run_into_max_zoom_stops_there_holding_its_anchor() {
        let bounds = canvas(800., 600.);
        let camera = Camera {
            zoom: MAX_ZOOM * 0.98,
            pan: (-4000.0, -3000.0),
        };
        let cursor = point(px(10. + 500.), px(20. + 200.));
        assert_eq!(
            zoom_step(cursor, camera, 5.0, bounds, CONTENT).zoom,
            MAX_ZOOM
        );
        let (dx, dy) = anchor_drift(cursor, camera, 5.0, bounds, CONTENT);
        assert!(
            dx.abs() < ANCHOR_TOLERANCE && dy.abs() < ANCHOR_TOLERANCE,
            "({dx}, {dy}) px"
        );
    }

    #[test]
    fn a_wheel_run_pinned_in_a_corner_keeps_the_graph_in_view() {
        // A cursor in the corner asks the content to shrink away into it, which the
        // pan bound refuses past its clearance: the anchor gives before the graph
        // leaves the window.
        let bounds = canvas(800., 600.);
        let cursor = point(px(10. + 784.), px(20. + 588.));
        let mut camera = Camera {
            zoom: 0.5,
            pan: (0.0, 0.0),
        };
        for _ in 0..20 {
            camera = zoom_step(cursor, camera, -1.0, bounds, CONTENT);
            let (w, h) = (CONTENT.0 * camera.zoom, CONTENT.1 * camera.zoom);
            let on_screen = |pan: f32, extent: f32, vp: f32| (pan + extent).min(vp) - pan.max(0.);
            let (sx, sy) = (
                on_screen(camera.pan.0, w, 800.),
                on_screen(camera.pan.1, h, 600.),
            );
            let eps = 1e-2;
            assert!(
                sx + eps >= FIT_MARGIN * w.min(800.) && sy + eps >= FIT_MARGIN * h.min(600.),
                "only ({sx}, {sy}) px of graph left on screen at zoom {}",
                camera.zoom
            );
        }
    }

    #[test]
    fn a_wheel_run_into_the_fit_zoom_stops_there_holding_its_anchor() {
        let bounds = canvas(800., 600.);
        let fit = fit_zoom(bounds.size, CONTENT);
        let camera = centered(fit * 1.05, bounds, CONTENT);
        let cursor = point(px(10. + 400.), px(20. + 300.));
        assert_eq!(zoom_step(cursor, camera, -5.0, bounds, CONTENT).zoom, fit);
        let (dx, dy) = anchor_drift(cursor, camera, -5.0, bounds, CONTENT);
        assert!(
            dx.abs() < ANCHOR_TOLERANCE && dy.abs() < ANCHOR_TOLERANCE,
            "({dx}, {dy}) px"
        );
    }
}
