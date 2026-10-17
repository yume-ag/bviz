use crate::chart::format_duration;
use crate::model::{AggregateBuildStarted, SourceReplaced, TraceModel};
use crate::theme::{self, Palette, RemainderPaint};
use crate::{
    METRICS, MIN_SLICE_PX, STATS_TEXT_SIZE, ShowWholeFlameGraph, drawn_scroll, profile_status,
    scroll_request, thumb_offset, time_window_group,
};
use bviz_trace_layout::{
    FlameBox, FlameBoxContent, FlameDirection, FlameLabels, FlameMetrics, FlameMode,
    FlamePlacement, FlameRemainder, FlameView, FlameWeight, RemainderSiblings, ScopeAggregate,
    flame_height, lay_out_flame,
};
use bviz_ui_components::gesture::{
    self, Clicks, Entry, GestureRow, Gestures, Held, Input, Motion, Place, Pointer, pinch_factor,
};
use bviz_ui_components::{
    Axis, CardAnchor, DEFAULT_MIN_THUMB, DragEnd, HoverCard, KeyScope, Legend, LegendState,
    MenuItem, OneRow, OverflowRows, Presentation, Priority, SCROLLBAR_THICKNESS, Scrollbar,
    ShowLegend, ShownCard, Switch, ToggleGroup, ToggleSegment, ToolbarGroup, ZOOM_KEY_STEP, ZoomIn,
    ZoomOut, cached_layer, centered_spinner, hover_card_canvas, legend_gestures, overlay_canvas,
    paint_labeled_box, track_drag,
};
use bviz_ui_workspace::{Panel, PanelEvent, PanelKind, Role, ViewKind};
use gpui::{
    AnyElement, App, Bounds, ContentMask, Context, Div, Entity, EventEmitter, FocusHandle,
    Focusable, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent, PinchEvent, Pixels, Point,
    Render, ScrollWheelEvent, SharedString, Stateful, Subscription, WeakEntity, Window, canvas,
    div, fill, point, prelude::*, px, quad, size,
};
use std::rc::Rc;
use std::sync::Arc;

gpui::actions!(
    bviz_trace,
    [
        /// Cycle direction
        CycleDirection,
        /// Cycle weight
        CycleWeight,
        /// Toggle merging by kind
        ToggleKinds,
    ]
);

/// The key context scoping the flame graph's own bindings, so a key pressed here
/// reaches this panel and no other.
pub(crate) const FLAME_KEY_CONTEXT: KeyScope = KeyScope::new("TraceFlameGraph", "Flame Graph");

/// What the flame graph does for a gesture in [`GESTURES`].
#[derive(Clone, Copy)]
enum FlameIntent {
    ShowWhole,
    Zoom,
    PanAcross,
    Scroll,
    Pan,
    Describe,
}

const GESTURES: Gestures<FlameIntent> = Gestures::new(&[
    Entry::new(
        Input::new(Motion::Press(gesture::Button::Left, Clicks::Double)),
        Place::Empty,
        "Show whole flame graph",
        FlameIntent::ShowWhole,
    ),
    Entry::new(
        Input::SCROLL.holding(Held::SECONDARY),
        Place::Anywhere,
        "Zoom about pointer",
        FlameIntent::Zoom,
    ),
    Entry::new(
        Input::PINCH,
        Place::Anywhere,
        "Zoom about pointer",
        FlameIntent::Zoom,
    ),
    Entry::new(
        Input::SCROLL.holding(Held::SHIFT),
        Place::Anywhere,
        "Pan across",
        FlameIntent::PanAcross,
    ),
    Entry::new(Input::SCROLL, Place::Anywhere, "Pan", FlameIntent::Scroll),
    Entry::new(
        Input::drag(gesture::Button::Left),
        Place::Anywhere,
        "Pan",
        FlameIntent::Pan,
    ),
    Entry::new(
        Input::HOVER,
        Place::On("a box"),
        "Describe box",
        FlameIntent::Describe,
    ),
]);

/// The directions the toolbar offers, in the order it shows them.
const DIRECTIONS: [FlameDirection; 2] = [FlameDirection::TopDown, FlameDirection::BottomUp];

fn direction_segment(direction: FlameDirection) -> ToggleSegment {
    let (label, hint) = match direction {
        FlameDirection::TopDown => ("top-down", "Top-down: callees"),
        FlameDirection::BottomUp => ("bottom-up", "Bottom-up: callers"),
    };
    ToggleSegment::new(label).tooltip(hint)
}

/// How long each mode choice of the toolbar holds out against the panel
/// narrowing, the direction longest.
const DIRECTION: Priority = 4;
const WEIGHT: Priority = 3;
const KINDS: Priority = 2;

/// The words naming each choice, beside it, on its collapsed trigger and on its
/// row of the overflow menu.
const DIRECTION_NAME: &str = "direction";
const WEIGHT_NAME: &str = "weight";
const LABELS_NAME: &str = "labels";

/// The weights the toolbar offers, in the order it shows them.
const WEIGHTS: [FlameWeight; 2] = [FlameWeight::Duration, FlameWeight::WallShare];

fn weight_segment(weight: FlameWeight) -> ToggleSegment {
    let (label, hint) = match weight {
        FlameWeight::Duration => ("summed time", "Size by summed time"),
        FlameWeight::WallShare => ("wall time", "Size by wall time"),
    };
    ToggleSegment::new(label).tooltip(hint)
}

/// The choice among `choices` after `standing`, round to the first.
fn after<T: Copy + PartialEq>(choices: &[T], standing: T) -> T {
    let at = choices.iter().position(|&choice| choice == standing);
    choices[at.map_or(0, |at| (at + 1) % choices.len())]
}

/// The flame graph's laid-out boxes together with the inputs that determine
/// them, so a paint reuses them unless one changed. The retained
/// `ScopeAggregate` both keeps the graph alive for the boxes' name handles and
/// serves as a cache key: swapping in another selection's aggregate changes the
/// `Arc`, so `Arc::ptr_eq` detects a stale layout. The layout is viewport-
/// relative — it depends on the zoomed width `content_w`, the horizontal scroll
/// `scroll_x`, and the viewport width `view_w` — so all three are keys. The
/// vertical `scroll_y` is not: it only offsets a box's y at paint.
pub(crate) struct FlameBoxCache {
    aggregates: Arc<ScopeAggregate>,
    content_w: f64,
    scroll_x: f64,
    view_w: f32,
    boxes: Rc<Vec<FlameBox>>,
}

impl FlameBoxCache {
    /// Lays the boxes out from `aggregates`'s own graph, so a hit's box and the
    /// graph it names that box through are the same aggregate's.
    fn new(
        aggregates: Arc<ScopeAggregate>,
        content_w: f64,
        scroll_x: f64,
        view_w: f32,
        metrics: &FlameMetrics,
    ) -> Self {
        let boxes = Rc::new(lay_out_flame(
            &aggregates.flame,
            content_w,
            FlamePlacement {
                scroll_x,
                view_w,
                metrics,
                min_px: MIN_SLICE_PX,
            },
        ));
        Self {
            aggregates,
            content_w,
            scroll_x,
            view_w,
            boxes,
        }
    }

    /// The memoized boxes when every key matches, `None` otherwise.
    fn matching(
        &self,
        aggregates: &Arc<ScopeAggregate>,
        content_w: f64,
        scroll_x: f64,
        view_w: f32,
    ) -> Option<&Rc<Vec<FlameBox>>> {
        (Arc::ptr_eq(&self.aggregates, aggregates)
            && self.content_w == content_w
            && self.scroll_x == scroll_x
            && self.view_w == view_w)
            .then_some(&self.boxes)
    }

    /// The box under a window-space `pos`, which maps to a height in the tree
    /// via the vertical scroll `scroll_y` and to an x relative to `fl`'s content
    /// column. `None` when the cursor lies off that rect, no box is drawn there, or
    /// a key no longer matches. Every change to the panel's view, scroll, or
    /// bounds rebuilds the canvas scene, whose paint re-memoizes ahead of the
    /// overlay that reads this, so a hit test taken during paint names a box of
    /// the layout on screen.
    fn hit(
        &self,
        aggregates: &Arc<ScopeAggregate>,
        fl: FlameLayout,
        scroll_y: f32,
        pos: Point<Pixels>,
    ) -> Option<HoveredBox> {
        if !fl.rect().contains(&pos) {
            return None;
        }
        let boxes = self.matching(
            aggregates,
            fl.view.content_px(fl.width),
            fl.view.scroll_px(fl.width),
            fl.width,
        )?;
        let content_y = f32::from(pos.y) - fl.top + scroll_y;
        let vx = f32::from(pos.x) - fl.left;
        let boxed = *boxes
            .iter()
            .find(|b| content_y >= b.y && content_y < b.y + b.h && vx >= b.x && vx < b.x + b.w)?;
        Some(HoveredBox {
            aggregates: self.aggregates.clone(),
            boxed,
        })
    }
}

/// A hovered flame box paired with the aggregate a frame's name handle resolves
/// through.
struct HoveredBox {
    aggregates: Arc<ScopeAggregate>,
    boxed: FlameBox,
}

/// The flame graph's current geometry, shared by hit-testing and the
/// scroll/zoom/pan handlers so they map the cursor the same way the paint does.
#[derive(Clone, Copy)]
struct FlameLayout {
    view: FlameView,
    total_h: f32,
    left: f32,
    top: f32,
    width: f32,
    height: f32,
}

impl FlameLayout {
    /// Places a graph `total_h` tall, viewed through `view`, in `bounds`: the
    /// content rect spans the full body with no gutter and no time axis, less a
    /// reserved vertical-bar column (when the tree's rows overflow the visible
    /// height) and a reserved horizontal-bar strip (when the zoomed width
    /// overflows the visible width). Both reserves may apply at once, leaving the
    /// bottom-right corner to neither bar.
    fn new(bounds: Bounds<Pixels>, total_h: f32, view: FlameView) -> Self {
        // Zoom > 1 overflows horizontally, so a bottom strip is reserved for the
        // h-bar; that shrinks the height the rows must clear, which is what decides
        // the v-bar reserve. Resolve in that order.
        let hbar_reserve = if view.zoom > 1.0 {
            SCROLLBAR_THICKNESS
        } else {
            0.0
        };
        let height = (f32::from(bounds.size.height) - hbar_reserve).max(0.0);
        let vbar_reserve = if total_h > height {
            SCROLLBAR_THICKNESS
        } else {
            0.0
        };
        let width = (f32::from(bounds.size.width) - vbar_reserve).max(0.0);
        Self {
            view,
            total_h,
            left: f32::from(bounds.origin.x),
            top: f32::from(bounds.origin.y),
            width,
            height,
        }
    }

    fn rect(&self) -> Bounds<Pixels> {
        Bounds::new(
            point(px(self.left), px(self.top)),
            size(px(self.width), px(self.height)),
        )
    }
}

/// What a left-button drag over the flame-graph panel is currently moving.
#[derive(Clone, Copy)]
enum FlameDrag {
    None,
    /// A press inside the panel pans it.
    Pan,
    /// A press on the panel's vertical or horizontal scroll thumb/track, holding
    /// that grab offset from the thumb's leading edge.
    VThumb(f32),
    HThumb(f32),
}

/// The flame metrics matching the chart's lane row height, so the graph's rows
/// align with the rest of the view.
fn flame_metrics() -> FlameMetrics {
    FlameMetrics {
        row_height: METRICS.row_height,
    }
}

/// Lays out the graph's visible window, reusing the [`FlameBoxCache`] in `cache`
/// unless a key changed.
fn flame_boxes(
    cache: &mut Option<FlameBoxCache>,
    aggregates: &Arc<ScopeAggregate>,
    content_w: f64,
    scroll_x: f64,
    view_w: f32,
    metrics: &FlameMetrics,
) -> Rc<Vec<FlameBox>> {
    if let Some(boxes) = cache
        .as_ref()
        .and_then(|c| c.matching(aggregates, content_w, scroll_x, view_w))
    {
        return boxes.clone();
    }
    cache
        .insert(FlameBoxCache::new(
            aggregates.clone(),
            content_w,
            scroll_x,
            view_w,
            metrics,
        ))
        .boxes
        .clone()
}

/// Narrowest a remainder is drawn with its outline. Any narrower and the
/// outline would leave none of the wash showing that tells it from a frame.
const REMAINDER_OUTLINE_MIN_PX: f32 = 3.0;

const REMAINDER_OUTLINE_PX: f32 = 1.0;

/// Paints a remainder's box: the wash and the outline of
/// [`Palette::remainder`], labelled with [`remainder_label`].
fn paint_remainder(
    rect: Bounds<Pixels>,
    remainder: &FlameRemainder,
    palette: &Palette,
    window: &mut Window,
    cx: &mut App,
) {
    let RemainderPaint {
        outline,
        wash,
        border,
    } = palette.remainder(remainder.role());
    if f32::from(rect.size.width) < REMAINDER_OUTLINE_MIN_PX {
        window.paint_quad(fill(rect, wash));
        return;
    }
    paint_labeled_box(
        rect,
        wash,
        remainder_label(remainder).as_str(),
        bviz_ui_theme::label_on(wash),
        palette.label_box(),
        window,
        cx,
    );
    window.paint_quad(quad(
        rect,
        px(0.),
        gpui::transparent_black(),
        px(REMAINDER_OUTLINE_PX),
        outline,
        border,
    ));
}

/// How many frames a remainder stands for, as others where its parent holds
/// frames drawn beside it.
fn remainder_label(remainder: &FlameRemainder) -> String {
    let frames = remainder.frames();
    match remainder.siblings() {
        RemainderSiblings::None => format!("({frames} frames)"),
        RemainderSiblings::Wider => format!("({frames} others)"),
    }
}

/// The tooltip's lines for `hovered`. A frame gives its name and what the name
/// leaves open about its spans (see [`FlameGraph::kind_or_category`]) where
/// that is not the name itself, then its figures; a remainder, and the depth
/// drawn beneath one, gives what the remainder's label counts and their total.
fn flame_tooltip_lines(hovered: &HoveredBox) -> Vec<String> {
    let HoveredBox { aggregates, boxed } = hovered;
    let flame = &aggregates.flame;
    let root_total_us = flame.root_total_us();
    let pct = |total_us: i64| {
        if root_total_us > 0 {
            100.0 * total_us as f64 / root_total_us as f64
        } else {
            0.0
        }
    };
    match boxed.content {
        FlameBoxContent::Frame(frame) => {
            let detail = format!(
                "total {} · self {} · {} span{} · {:.1}% of view",
                format_duration(frame.total_us),
                format_duration(flame.span_self_us(frame.node)),
                frame.count,
                if frame.count == 1 { "" } else { "s" },
                pct(frame.total_us),
            );
            let context = flame
                .kind_or_category(frame.node)
                .filter(|&c| c != frame.name);
            [
                Some(flame.str(frame.name).to_string()),
                context.map(|c| flame.str(c).to_string()),
                Some(detail),
            ]
            .into_iter()
            .flatten()
            .collect()
        }
        FlameBoxContent::Remainder(remainder) | FlameBoxContent::RemainderDepth(remainder) => {
            let total_us = flame.remainder_total_us(&remainder);
            vec![
                remainder_label(&remainder),
                format!(
                    "too narrow · total {} · {:.1}% of view",
                    format_duration(total_us),
                    pct(total_us),
                ),
            ]
        }
    }
}

/// The aggregated flame-graph panel: time-free work-class-colored boxes filling
/// the body, zoom/pan on the x axis and vertical scroll on the y, with its own
/// scroll bars and a hover tooltip. Owns its view state as plain fields,
/// reading the aggregate from [`TraceModel`]. The boxes are painted by
/// [`FlameCanvas`], the tooltip by an overlay above it.
pub struct FlamePanel {
    /// Focus for the panel, so the window's own actions dispatch while it is the
    /// panel on show.
    focus: FocusHandle,
    /// Read for the current selection's aggregate and whether a rebuild is
    /// running, and written by the mode controls on its toolbar.
    model: Entity<TraceModel>,
    /// Horizontal window into the aggregated flame graph's normalized width (the
    /// panel's zoom and pan); a fraction so a resize stays invertible.
    flame_view: FlameView,
    /// Where the panel's top edge sits in the tree, in pixels from its top. Held
    /// unbounded, so a position past the end of a graph that is momentarily
    /// shorter returns the view once the graph grows back.
    flame_scroll_y: f32,
    /// The panel's own bounds, captured during its canvas prepaint, so handlers
    /// map the cursor within the panel rather than the whole window; `None`
    /// until the panel has been laid out once.
    flame_bounds: Option<Bounds<Pixels>>,
    /// The panel's memoized box layout; see [`FlameBoxCache`].
    flame_boxes: Option<FlameBoxCache>,
    /// The card describing the box under the pointer.
    card: HoverCard<FlameBoxContent>,
    drag: FlameDrag,
    last_mouse: Point<Pixels>,
    legend: Entity<LegendState>,
    /// The layer painting the boxes and scroll bars; notified through
    /// [`Self::invalidate_canvas`].
    canvas: Entity<FlameCanvas>,
    /// Re-syncs the panel with the model's aggregate, which lands asynchronously.
    _model_observer: Subscription,
    /// Raises the rebuild pill when a build starts, which no state the panel
    /// renders from would otherwise reveal.
    _build_start: Subscription,
    /// Returns the panel to the fit-to-window view when a different document
    /// loads, whose graph a zoom held over the old one addresses nothing in.
    _source_replaced: Subscription,
}

impl FlamePanel {
    pub(crate) fn new(model: Entity<TraceModel>, cx: &mut Context<Self>) -> Self {
        let parent = cx.entity().downgrade();
        let canvas = cx.new(|_| FlameCanvas { parent });
        let model_observer = cx.observe(&model, |this, _, cx| {
            this.reset_aggregate_derived();
            this.invalidate_canvas(cx);
        });
        // The boxes still address the shown graph; only the toolbar's cue of the
        // build needs the frame.
        let build_start = cx.subscribe(&model, |_, _, _: &AggregateBuildStarted, cx| cx.notify());
        let source_replaced = cx.subscribe(&model, |this, _, _: &SourceReplaced, cx| {
            this.reset_view();
            this.invalidate_canvas(cx);
        });
        let focus = cx.focus_handle();
        let legend = cx.new(|cx| LegendState::new(focus.clone(), cx));
        Self {
            focus,
            model,
            flame_view: FlameView::FIT,
            flame_scroll_y: 0.0,
            flame_bounds: None,
            flame_boxes: None,
            card: HoverCard::new(cx, |panel| &mut panel.card, Self::carded),
            drag: FlameDrag::None,
            last_mouse: Point::default(),
            canvas,
            legend,
            _model_observer: model_observer,
            _build_start: build_start,
            _source_replaced: source_replaced,
        }
    }

    /// Rebuilds the box scene along with the overlay; every change to what the
    /// boxes show routes through here.
    pub(crate) fn invalidate_canvas(&self, cx: &mut Context<Self>) {
        self.canvas.update(cx, |_, cx| cx.notify());
        cx.notify();
    }

    /// Drops the memoized box layout, which addresses the graph that produced it.
    fn reset_aggregate_derived(&mut self) {
        self.flame_boxes = None;
    }

    /// Resets the panel to the fit-to-window view at the top of the tree: a zoom,
    /// pan, and scroll held for one graph address nothing in a graph whose
    /// geometry is unrelated to it.
    pub(crate) fn reset_view(&mut self) {
        self.flame_view = FlameView::FIT;
        self.flame_scroll_y = 0.0;
        self.reset_aggregate_derived();
    }

    /// Clears the box tooltip, so a pane bringing another panel forward drops it
    /// instead of leaving it until the next mouse move.
    fn clear_hover(&mut self) {
        self.card.leave();
    }

    /// The panel's toolbar: the controls choosing which graph is aggregated,
    /// then the window it covers, ending in the toggle opening the legend for
    /// the colours it is drawn in.
    fn toolbar(&self, palette: Palette, window: &Window, cx: &mut Context<Self>) -> OneRow {
        let mode = self.model.read(cx).flame_mode();
        let at_rest = FlameMode::default();
        let view = cx.entity();
        let focus = &self.focus;
        let set_mode = move |to: FlameMode, cx: &mut App| {
            view.update(cx, |this, cx| this.set_mode(to, cx));
        };
        let direction = || {
            let set_mode = set_mode.clone();
            ToggleGroup::one_of(
                "flame-direction",
                DIRECTION_NAME,
                DIRECTIONS,
                mode.direction,
                |direction| direction_segment(direction).keys_for(&CycleDirection, focus),
                move |direction, _, cx| set_mode(FlameMode { direction, ..mode }, cx),
            )
            .tooltip("Flame direction")
        };
        let weight = || {
            let set_mode = set_mode.clone();
            ToggleGroup::one_of(
                "flame-weight",
                WEIGHT_NAME,
                WEIGHTS,
                mode.weight,
                |weight| weight_segment(weight).keys_for(&CycleWeight, focus),
                move |weight, _, cx| set_mode(FlameMode { weight, ..mode }, cx),
            )
            .tooltip("Box size")
        };
        let kinds = || {
            let set_mode = set_mode.clone();
            ToggleGroup::any_of(
                "flame-kinds",
                LABELS_NAME,
                [Switch::new(
                    ToggleSegment::new("kinds")
                        .tooltip("Merge spans by kind")
                        .keys_for(&ToggleKinds, focus),
                    mode.labels == FlameLabels::Kind,
                    move |kinds, _, cx| {
                        let labels = match kinds {
                            true => FlameLabels::Kind,
                            false => FlameLabels::Name,
                        };
                        set_mode(FlameMode { labels, ..mode }, cx)
                    },
                )],
            )
        };
        let choice = |priority, name: &'static str, toggles: &dyn Fn() -> ToggleGroup, set| {
            let row = MenuItem::submenu(name, toggles().submenu(window));
            ToolbarGroup::named(priority, name, OverflowRows::row(row, set), toggles)
                .or(toggles().presentation(Presentation::Collapsed))
        };
        let direction = choice(
            DIRECTION,
            DIRECTION_NAME,
            &direction,
            mode.direction != at_rest.direction,
        );
        let weight = choice(WEIGHT, WEIGHT_NAME, &weight, mode.weight != at_rest.weight);
        let kinds_row = MenuItem::submenu(LABELS_NAME, kinds().submenu(window));
        let kinds_set = mode.labels != at_rest.labels;
        let kinds = ToolbarGroup::named(
            KINDS,
            LABELS_NAME,
            OverflowRows::row(kinds_row, kinds_set),
            kinds,
        );
        let legend =
            Legend::new("flame-legend", &self.legend).entries(theme::role_legend(&palette));
        // While a rebuild for a new selection runs off the thread, the prior
        // graph stays shown and interactive.
        let building = self.model.read(cx).aggregate_building();
        OneRow::new("flame-controls")
            .text_size(px(STATS_TEXT_SIZE))
            .group(direction)
            .group(weight)
            .group(kinds)
            .when_some(
                time_window_group(&self.model, palette, focus, window, cx),
                |row, chip| row.group(chip.parted()),
            )
            .when(building, |row| row.busy("flame-rebuild", "updating…"))
            .trailing(legend.toolbar_group(window, cx))
    }

    /// Applies a mode chosen on the toolbar, leaving the view alone for a click
    /// that re-chooses the standing mode. The graph the next aggregate lays out
    /// has a geometry unrelated to the one a zoom and scroll were held over, so
    /// they return to the fit.
    fn set_mode(&mut self, mode: FlameMode, cx: &mut Context<Self>) {
        if !self.model.update(cx, |m, cx| m.set_flame_mode(mode, cx)) {
            return;
        }
        self.reset_view();
        self.invalidate_canvas(cx);
    }

    /// The current panel geometry, or `None` until the graph exists and the panel
    /// has been laid out.
    fn layout(&self, cx: &App) -> Option<FlameLayout> {
        let aggregates = self.model.read(cx).aggregates()?;
        let total_h = flame_height(&aggregates.flame, &flame_metrics());
        Some(FlameLayout::new(
            self.flame_bounds?,
            total_h,
            self.flame_view,
        ))
    }

    /// The bar for the tree's rows against the visible height; none while the
    /// layout is unknown.
    fn vbar(&self, cx: &App) -> Option<Scrollbar> {
        let fl = self.layout(cx)?;
        Scrollbar::new(
            Axis::Vertical,
            Bounds::new(
                point(px(fl.left + fl.width), px(fl.top)),
                size(px(SCROLLBAR_THICKNESS), px(fl.height)),
            ),
            fl.total_h,
            fl.height,
            self.flame_scroll(cx),
            DEFAULT_MIN_THUMB,
        )
    }

    /// The bar for the zoomed width against the visible width; none while the
    /// layout is unknown.
    fn hbar(&self, cx: &App) -> Option<Scrollbar> {
        let fl = self.layout(cx)?;
        Scrollbar::new(
            Axis::Horizontal,
            Bounds::new(
                point(px(fl.left), px(fl.top + fl.height)),
                size(px(fl.width), px(SCROLLBAR_THICKNESS)),
            ),
            fl.view.content_px(fl.width) as f32,
            fl.width,
            fl.view.scroll_px(fl.width) as f32,
            DEFAULT_MIN_THUMB,
        )
    }

    /// The pixel ceiling for the panel's vertical scroll: the tree's height past
    /// the panel's on-screen height (which excludes any reserved horizontal-bar
    /// strip, so the last row clears that bar), or `None` until the layout is
    /// known.
    fn max_scroll(&self, cx: &App) -> Option<f32> {
        self.layout(cx).map(|fl| (fl.total_h - fl.height).max(0.0))
    }

    /// How far down the tree the panel draws, in pixels; see [`drawn_scroll`].
    /// The sole reading of the vertical offset, so what is painted and what is
    /// hit-tested agree.
    fn flame_scroll(&self, cx: &App) -> f32 {
        let Some(max) = self.max_scroll(cx) else {
            return self.flame_scroll_y;
        };
        drawn_scroll(self.flame_scroll_y, max)
    }

    /// Moves the panel's top edge to `y` pixels down the tree; see
    /// [`scroll_request`]. This is a gesture entry point rather than an absolute
    /// seek: while a position past the tree's end is held, `scroll_to(0.0)` is a
    /// no-op, so a caller that means "go to the top" assigns the offset instead.
    fn scroll_to(&mut self, y: f32, cx: &App) {
        let Some(max) = self.max_scroll(cx) else {
            return;
        };
        if let Some(y) = scroll_request(self.flame_scroll_y, max, y) {
            self.flame_scroll_y = y;
        }
    }

    /// Moves the panel's top edge up by a gesture's `dy` pixels.
    fn scroll_by(&mut self, dy: f32, cx: &App) {
        self.scroll_to(self.flame_scroll(cx) - dy, cx);
    }

    /// The box under a window-space cursor, resolved against the memoized layout;
    /// see [`FlameBoxCache::hit`]. `None` until the graph exists and the panel
    /// has painted once.
    fn box_at(&self, pos: Point<Pixels>, cx: &App) -> Option<HoveredBox> {
        let fl = self.layout(cx)?;
        let aggregates = self.model.read(cx).aggregates()?;
        self.flame_boxes
            .as_ref()?
            .hit(&aggregates, fl, self.flame_scroll(cx), pos)
    }

    /// Tracks the cursor for the box card.
    fn update_hover(&mut self, pos: Point<Pixels>, cx: &mut Context<Self>) {
        let boxed = self.box_at(pos, cx).map(|hovered| hovered.boxed.content);
        if self.card.hover(pos, boxed.as_ref()) {
            cx.notify();
        }
    }

    /// What the box under the pointer stands for, which the card describes.
    fn carded(&self, cx: &App) -> Option<FlameBoxContent> {
        let at = self.card.pointer()?;
        Some(self.box_at(at, cx)?.boxed.content)
    }

    /// The box the card describes, with the graph it names the box through,
    /// and the pointer it stands beside, while the card is shown.
    fn hovered_box(&self, cx: &App) -> Option<(Point<Pixels>, HoveredBox)> {
        let at = self.card.pointer()?;
        let hovered = self.box_at(at, cx)?;
        self.card
            .shows(&hovered.boxed.content)
            .then_some((at, hovered))
    }

    /// Whether `pos` lies over the graph clear of every box and scroll bar.
    fn empty_at(&self, pos: Point<Pixels>, cx: &App) -> bool {
        let on_bar = [self.vbar(cx), self.hbar(cx)]
            .into_iter()
            .flatten()
            .any(|bar| bar.track_hit(pos));
        self.layout(cx).is_some_and(|fl| fl.rect().contains(&pos))
            && !on_bar
            && self.box_at(pos, cx).is_none()
    }

    /// Fits the whole width of the graph to the panel, leaving the vertical
    /// scroll where it is.
    fn show_whole(&mut self) {
        self.flame_view = FlameView::FIT;
    }

    /// Begins a drag at `pos`: the vertical or horizontal scroll thumb/track if the
    /// press hit one, otherwise a pan of the panel.
    fn press(&mut self, pos: Point<Pixels>, cx: &App) {
        if let Some(bar) = self.vbar(cx)
            && bar.track_hit(pos)
        {
            let grab = bar.grab(pos);
            self.drag = FlameDrag::VThumb(grab);
            self.scroll_thumb_to(pos, grab, cx);
            return;
        }
        if let Some(bar) = self.hbar(cx)
            && bar.track_hit(pos)
        {
            let grab = bar.grab(pos);
            self.drag = FlameDrag::HThumb(grab);
            self.hthumb_to(pos, grab, cx);
            return;
        }
        self.drag = FlameDrag::Pan;
        self.last_mouse = pos;
    }

    /// Maps the vertical thumb drag back to a scroll offset.
    fn scroll_thumb_to(&mut self, pos: Point<Pixels>, grab: f32, cx: &App) {
        if let Some(offset) = thumb_offset(self.vbar(cx), pos, grab) {
            self.scroll_to(offset, cx);
        }
    }

    /// Maps a horizontal thumb drag to its normalized scroll.
    fn hthumb_to(&mut self, pos: Point<Pixels>, grab: f32, cx: &App) {
        let (Some(offset), Some(fl)) = (thumb_offset(self.hbar(cx), pos, grab), self.layout(cx))
        else {
            return;
        };
        let mut fv = fl.view;
        fv.set_scroll_px(offset, fl.width);
        self.flame_view = fv;
    }

    fn on_scroll_wheel(&mut self, e: &ScrollWheelEvent, cx: &mut Context<Self>) {
        let Some(fl) = self.layout(cx) else {
            return;
        };
        let delta = gesture::scroll_pixels(e);
        let (dx, dy) = (f32::from(delta.x), f32::from(delta.y));
        let (mut fv, width) = (fl.view, fl.width);
        GESTURES.dispatch(Pointer::scroll(e), |intent| {
            match intent {
                FlameIntent::Zoom => {
                    let factor = f64::from(gesture::wheel_zoom_factor(e));
                    self.zoom_about(e.position.x, factor, cx);
                }
                FlameIntent::PanAcross => {
                    fv.pan_px(dx + dy, width);
                    self.flame_view = fv;
                }
                FlameIntent::Scroll => {
                    self.scroll_by(dy, cx);
                    fv.pan_px(dx, width);
                    self.flame_view = fv;
                }
                _ => return false,
            }
            true
        });
        self.invalidate_canvas(cx);
    }

    fn on_pinch(&mut self, e: &PinchEvent, cx: &mut Context<Self>) {
        GESTURES.dispatch(Pointer::pinch(e), |intent| match intent {
            FlameIntent::Zoom => {
                self.zoom_about(e.position.x, pinch_factor(e).into(), cx);
                true
            }
            _ => false,
        });
    }

    /// Zooms `factor` times closer about the middle of the view.
    fn zoom_centred(&mut self, factor: f64, cx: &mut Context<Self>) {
        if let Some(fl) = self.layout(cx) {
            self.zoom_about(px(fl.left + fl.width / 2.), factor, cx);
        }
    }

    /// Zooms `factor` times closer about the window-space `x`.
    fn zoom_about(&mut self, x: Pixels, factor: f64, cx: &mut Context<Self>) {
        let Some(fl) = self.layout(cx) else {
            return;
        };
        let mut fv = fl.view;
        fv.zoom_at(f32::from(x) - fl.left, fl.width, factor);
        self.flame_view = fv;
        self.invalidate_canvas(cx);
    }

    /// The panel's pointer wiring, acting as [`GESTURES`] has it, a press on a
    /// scroll bar driving the bar instead of panning. Presses and the wheel
    /// stop propagation so they don't also reach the flame chart's handlers
    /// underneath, which leaves a press to focus the panel itself.
    fn panel_handlers(&self, root: Stateful<Div>, cx: &mut Context<Self>) -> Stateful<Div> {
        root.on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, e: &MouseDownEvent, window, cx| {
                this.focus.focus(window, cx);
                GESTURES.dispatch(Pointer::press(e), |intent| match intent {
                    FlameIntent::ShowWhole if this.empty_at(e.position, cx) => {
                        this.show_whole();
                        true
                    }
                    FlameIntent::Pan => {
                        this.press(e.position, cx);
                        true
                    }
                    _ => false,
                });
                cx.stop_propagation();
                this.invalidate_canvas(cx);
            }),
        )
        .on_scroll_wheel(cx.listener(|this, e: &ScrollWheelEvent, _, cx| {
            this.on_scroll_wheel(e, cx);
            cx.stop_propagation();
        }))
        .on_pinch(cx.listener(|this, e: &PinchEvent, _, cx| {
            this.on_pinch(e, cx);
            cx.stop_propagation();
        }))
        .on_mouse_move(cx.listener(|this, e: &MouseMoveEvent, _, cx| {
            if matches!(this.drag, FlameDrag::None) {
                GESTURES.dispatch(Pointer::HOVER, |intent| match intent {
                    FlameIntent::Describe => {
                        this.update_hover(e.position, cx);
                        true
                    }
                    _ => false,
                });
            }
        }))
        .hover_listener_mode(gpui::HoverListenerMode::InputModalityIndependent)
        .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
            if !*hovered && this.card.leave() {
                cx.notify();
            }
        }))
    }
}

impl EventEmitter<PanelEvent> for FlamePanel {}

impl Focusable for FlamePanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Panel for FlamePanel {
    fn kind() -> PanelKind {
        PanelKind {
            view: ViewKind::new("trace-flame-graph"),
            key_context: FLAME_KEY_CONTEXT,
            role: Role::Analysis,
        }
    }

    fn gestures() -> Vec<GestureRow> {
        GESTURES.rows().chain(legend_gestures()).collect()
    }

    fn status(&self, cx: &App) -> Option<SharedString> {
        Some(profile_status(&self.model, cx))
    }

    fn set_active(&mut self, active: bool, window: &mut Window, cx: &mut Context<Self>) {
        match active {
            true => self.focus.focus(window, cx),
            // The tooltip anchors to a cursor that is no longer over the graph.
            false => self.clear_hover(),
        }
    }
}

impl Render for FlamePanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::from_theme(&bviz_ui_theme::theme(window, cx));
        let toolbar = self.toolbar(palette, window, cx);
        let body = self.graph_body(palette, cx);
        div()
            .key_context(Self::kind().key_context)
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &ShowWholeFlameGraph, _, cx| {
                this.show_whole();
                this.invalidate_canvas(cx);
            }))
            .on_action(cx.listener(|this, _: &ZoomIn, _, cx| {
                this.zoom_centred(ZOOM_KEY_STEP.into(), cx);
            }))
            .on_action(cx.listener(|this, _: &ZoomOut, _, cx| {
                this.zoom_centred(ZOOM_KEY_STEP.recip().into(), cx);
            }))
            .on_action(cx.listener(|this, _: &ShowLegend, window, cx| {
                this.legend.update(cx, |legend, cx| legend.show(window, cx));
            }))
            .on_action(cx.listener(|this, _: &CycleDirection, _, cx| {
                let mode = this.model.read(cx).flame_mode();
                let direction = after(&DIRECTIONS, mode.direction);
                this.set_mode(FlameMode { direction, ..mode }, cx);
            }))
            .on_action(cx.listener(|this, _: &CycleWeight, _, cx| {
                let mode = this.model.read(cx).flame_mode();
                let weight = after(&WEIGHTS, mode.weight);
                this.set_mode(FlameMode { weight, ..mode }, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleKinds, _, cx| {
                let mode = this.model.read(cx).flame_mode();
                let labels = match mode.labels {
                    FlameLabels::Kind => FlameLabels::Name,
                    FlameLabels::Name => FlameLabels::Kind,
                };
                this.set_mode(FlameMode { labels, ..mode }, cx);
            }))
            .flex()
            .flex_col()
            .size_full()
            .overflow_hidden()
            .child(toolbar)
            .child(body)
    }
}

impl FlamePanel {
    /// The graph itself, or the spinner standing in for it while the first
    /// aggregate for the loaded document is still being built.
    fn graph_body(&mut self, palette: Palette, cx: &mut Context<Self>) -> AnyElement {
        if self.model.read(cx).aggregates().is_none() {
            return centered_spinner("Building flame graph…", &palette.colors).into_any_element();
        }
        let entity = cx.entity();
        let boxes = cached_layer(self.canvas.clone());
        let card = hover_card_canvas(palette.colors.tooltip(), {
            let entity = entity.clone();
            move |_, _, cx| {
                let (at, hovered) = entity.read(cx).hovered_box(cx)?;
                Some(ShownCard {
                    lines: flame_tooltip_lines(&hovered).into(),
                    anchor: CardAnchor::Pointer(at),
                })
            }
        });
        let overlay = overlay_canvas(move |_, window, cx| {
            if !matches!(entity.read(cx).drag, FlameDrag::None) {
                track_drag(window, &entity, flame_on_drag, flame_end_drag);
            }
        });
        self.panel_handlers(
            div()
                .id("flame-graph-panel")
                .relative()
                .flex_1()
                .min_h(px(0.))
                .overflow_hidden()
                .child(boxes)
                .child(overlay)
                .child(card),
            cx,
        )
        .into_any_element()
    }
}

/// Routes an in-progress panel drag to its target: panning the panel, or moving a
/// scroll thumb.
fn flame_on_drag(
    panel: &mut FlamePanel,
    position: Point<Pixels>,
    _window: &mut Window,
    cx: &mut Context<FlamePanel>,
) {
    match panel.drag {
        FlameDrag::Pan => {
            let dx = f32::from(position.x) - f32::from(panel.last_mouse.x);
            let dy = f32::from(position.y) - f32::from(panel.last_mouse.y);
            if let Some(fl) = panel.layout(cx) {
                let mut fv = fl.view;
                fv.pan_px(dx, fl.width);
                panel.flame_view = fv;
            }
            panel.scroll_by(dy, cx);
            panel.last_mouse = position;
        }
        FlameDrag::VThumb(grab) => panel.scroll_thumb_to(position, grab, cx),
        FlameDrag::HThumb(grab) => panel.hthumb_to(position, grab, cx),
        FlameDrag::None => {}
    }
    panel.card.drag(position);
    panel.invalidate_canvas(cx);
}

fn flame_end_drag(
    panel: &mut FlamePanel,
    position: Point<Pixels>,
    _: DragEnd,
    _: &mut Window,
    cx: &mut Context<FlamePanel>,
) {
    panel.drag = FlameDrag::None;
    panel.update_hover(position, cx);
    panel.invalidate_canvas(cx);
}

/// The layer painting the flame-graph panel's boxes and scroll bars, from the
/// view state and aggregate it reads through `parent`.
struct FlameCanvas {
    parent: WeakEntity<FlamePanel>,
}

impl Render for FlameCanvas {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(parent) = self.parent.upgrade() else {
            return div().into_any_element();
        };
        let Some(aggregates) = parent.read(cx).model.read(cx).aggregates() else {
            return div().into_any_element();
        };
        let palette = Palette::from_theme(&bviz_ui_theme::theme(window, cx));
        let prepaint = {
            let parent = parent.clone();
            move |bounds: Bounds<Pixels>, _: &mut Window, cx: &mut App| {
                parent.update(cx, |panel, _| {
                    panel.flame_bounds = Some(bounds);
                });
            }
        };
        let paint = flame_paint(aggregates, parent, palette);
        canvas(prepaint, paint)
            .absolute()
            .inset_0()
            .size_full()
            .into_any_element()
    }
}

/// The box layer's paint closure: the aggregated flame graph filling the body
/// — each box painted as what it stands for, a frame with a truncated label,
/// clipped to the content rect, scrolled vertically by the panel's scroll and
/// zoomed/panned horizontally by its view — plus the vertical and horizontal
/// scroll bars for whichever axis overflows. No time-axis chrome applies, so
/// none is drawn. It reads the live view state through `parent`.
fn flame_paint(
    aggregates: Arc<ScopeAggregate>,
    parent: Entity<FlamePanel>,
    palette: Palette,
) -> impl FnOnce(Bounds<Pixels>, (), &mut Window, &mut App) {
    move |_, _, window, cx| {
        let graph = &aggregates.flame;
        let metrics = flame_metrics();
        // Read the panel's view state and lay out the visible window, then drop the
        // borrow before the box paint uses `cx` mutably.
        let (fl, scroll_y, content_w, scroll_x, vbar, hbar) = {
            let panel = parent.read(cx);
            let Some(fl) = panel.layout(cx) else {
                return;
            };
            // Lay out only the visible window, viewport-relative: the tree occupies
            // `content_w` pixels at this zoom, scrolled `scroll_x` in, so a box's x
            // is already relative to the content column's left edge. Coordinates are
            // f64 so precision holds at deep zoom.
            let content_w = fl.view.content_px(fl.width);
            let scroll_x = fl.view.scroll_px(fl.width);
            (
                fl,
                panel.flame_scroll(cx),
                content_w,
                scroll_x,
                panel.vbar(cx),
                panel.hbar(cx),
            )
        };
        let boxes = parent.update(cx, |panel, _| {
            flame_boxes(
                &mut panel.flame_boxes,
                &aggregates,
                content_w,
                scroll_x,
                fl.width,
                &metrics,
            )
        });
        let FlameLayout {
            left, top, height, ..
        } = fl;

        let mask = ContentMask { bounds: fl.rect() };
        window.with_content_mask(Some(mask), |window| {
            for b in boxes.iter() {
                let y = top + b.y - scroll_y;
                if y + b.h < top || y > top + height {
                    continue;
                }
                let rect = Bounds::new(point(px(left + b.x), px(y)), size(px(b.w), px(b.h)));
                match b.content {
                    FlameBoxContent::Frame(frame) => {
                        let role_fill = palette.role_fill(frame.role);
                        paint_labeled_box(
                            rect,
                            role_fill,
                            graph.str(frame.name),
                            bviz_ui_theme::label_on(role_fill),
                            palette.label_box(),
                            window,
                            cx,
                        );
                    }
                    FlameBoxContent::Remainder(remainder) => {
                        paint_remainder(rect, &remainder, &palette, window, cx);
                    }
                    FlameBoxContent::RemainderDepth(_) => {
                        window.paint_quad(fill(rect, palette.remainder_depth_fill()));
                    }
                }
            }
        });

        for bar in vbar.into_iter().chain(hbar) {
            bar.paint(palette.scroll.track, palette.scroll.thumb, window);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bviz_trace_layout::{
        FlameDirection, FlameGraph, FlameLabels, FlameMode, FlameWeight, TimeSelection,
    };
    use bviz_trace_profile::Profile;

    #[test]
    fn stepping_a_mode_choice_wraps_round_to_the_first() {
        assert_eq!(
            after(&DIRECTIONS, FlameDirection::TopDown),
            FlameDirection::BottomUp
        );
        assert_eq!(
            after(&DIRECTIONS, FlameDirection::BottomUp),
            FlameDirection::TopDown
        );
    }

    #[test]
    fn every_gesture_is_reachable() {
        assert_eq!(GESTURES.unreachable(), []);
    }

    #[test]
    fn every_gesture_fits_the_keys_help() {
        assert_eq!(GESTURES.overlong(), []);
    }

    /// Width of the content column the fixture layouts are placed for, wide
    /// enough that every fixture frame clears [`MIN_SLICE_PX`].
    const VIEW_W: f32 = 800.0;

    /// A profile of one execution lane whose spans nest two deep, so its graph
    /// has an interior frame with two siblings under it.
    fn fixture() -> Arc<Profile> {
        let mut events = vec![
            r#"{"name":"thread_name","ph":"M","tid":1,"args":{"name":"skyframe-evaluator-execution-1"}}"#
                .to_string(),
        ];
        for (name, ts, dur) in [("build", 0, 1000), ("compile", 0, 400), ("link", 400, 600)] {
            events.push(format!(
                r#"{{"cat":"general information","name":"{name}","ph":"X","ts":{ts},"dur":{dur},"tid":1}}"#,
            ));
        }
        let json = format!(r#"{{"traceEvents":[{}]}}"#, events.join(","));
        Arc::new(
            Profile::from_bytes(json.as_bytes())
                .expect("parse test profile")
                .into_inner(),
        )
    }

    /// A profile of two execution lanes, "link" running alongside the first
    /// half of "compile", so each span's share of the elapsed time differs
    /// from its duration.
    fn overlapping_fixture() -> Arc<Profile> {
        let mut events = Vec::with_capacity(4);
        for (tid, name, dur) in [(1, "compile", 1000), (2, "link", 500)] {
            events.push(format!(
                r#"{{"name":"thread_name","ph":"M","tid":{tid},"args":{{"name":"skyframe-evaluator-execution-{tid}"}}}}"#,
            ));
            events.push(format!(
                r#"{{"cat":"general information","name":"{name}","ph":"X","ts":0,"dur":{dur},"tid":{tid}}}"#,
            ));
        }
        let json = format!(r#"{{"traceEvents":[{}]}}"#, events.join(","));
        Arc::new(
            Profile::from_bytes(json.as_bytes())
                .expect("parse test profile")
                .into_inner(),
        )
    }

    /// The aggregate over every track of `profile` under `weight`, each frame
    /// named by its own span's name so a box's text identifies which span it
    /// stands for.
    fn aggregate_weighing(profile: &Arc<Profile>, weight: FlameWeight) -> Arc<ScopeAggregate> {
        Arc::new(ScopeAggregate::build_for_tracks(
            profile,
            FlameMode {
                direction: FlameDirection::TopDown,
                labels: FlameLabels::Name,
                weight,
            },
            &TimeSelection::default(),
            |_| true,
        ))
    }

    fn aggregate(profile: &Arc<Profile>) -> Arc<ScopeAggregate> {
        aggregate_weighing(profile, FlameWeight::Duration)
    }

    /// The layout of `aggregates` fitted to a [`VIEW_W`]-wide column, memoized as
    /// the panel would, with the geometry it was placed for. The rect runs one
    /// row past the graph, so a probe on an empty row still lands inside it.
    fn laid_out(aggregates: &Arc<ScopeAggregate>) -> (FlameBoxCache, FlameLayout) {
        let metrics = flame_metrics();
        let total_h = flame_height(&aggregates.flame, &metrics);
        let fl = FlameLayout {
            view: FlameView::FIT,
            total_h,
            left: 40.0,
            top: 30.0,
            width: VIEW_W,
            height: total_h + metrics.row_height,
        };
        let cache = FlameBoxCache::new(
            aggregates.clone(),
            fl.view.content_px(fl.width),
            fl.view.scroll_px(fl.width),
            fl.width,
            &metrics,
        );
        (cache, fl)
    }

    /// The name of the frame `b` stands for, `None` for a box standing for
    /// none.
    fn frame_name<'a>(graph: &'a FlameGraph, b: &FlameBox) -> Option<&'a str> {
        match b.content {
            FlameBoxContent::Frame(frame) => Some(graph.str(frame.name)),
            FlameBoxContent::Remainder(_) | FlameBoxContent::RemainderDepth(_) => None,
        }
    }

    /// Where the unscrolled panel draws `b`'s center, in window space.
    fn center_of(fl: FlameLayout, b: &FlameBox) -> Point<Pixels> {
        point(px(fl.left + b.x + b.w / 2.0), px(fl.top + b.y + b.h / 2.0))
    }

    #[test]
    fn a_hit_names_the_box_drawn_there() {
        let profile = fixture();
        let aggregates = aggregate(&profile);
        let (cache, fl) = laid_out(&aggregates);
        let names: Vec<&str> = cache
            .boxes
            .iter()
            .filter_map(|b| frame_name(&aggregates.flame, b))
            .collect();
        assert!(
            names.contains(&"compile") && names.contains(&"link"),
            "{names:?}"
        );
        for b in cache.boxes.iter() {
            let hovered = cache
                .hit(&aggregates, fl, 0.0, center_of(fl, b))
                .expect("the box at its own center");
            assert_eq!(&hovered.boxed, b);
        }
    }

    #[test]
    fn the_tooltip_gives_its_figures_in_the_weight_of_the_graph_hovered() {
        let profile = overlapping_fixture();
        let lines = |weight| {
            let aggregates = aggregate_weighing(&profile, weight);
            let (cache, fl) = laid_out(&aggregates);
            let compile = cache
                .boxes
                .iter()
                .find(|b| frame_name(&aggregates.flame, b) == Some("compile"))
                .expect("the compile frame");
            let hovered = cache
                .hit(&aggregates, fl, 0.0, center_of(fl, compile))
                .expect("the box at its own center");
            flame_tooltip_lines(&hovered)
        };
        assert_eq!(
            lines(FlameWeight::Duration),
            [
                "compile",
                "general information",
                "total 1.00ms · self 1.00ms · 1 span · 66.7% of view"
            ]
        );
        assert_eq!(
            lines(FlameWeight::WallShare),
            [
                "compile",
                "general information",
                "total 750µs · self 750µs · 1 span · 75.0% of view"
            ]
        );
    }

    /// A profile of two execution lanes. On the first, "build" holds "compile"
    /// and then forty spans each a small fraction of a [`VIEW_W`]-wide column's
    /// pixel; on the second, "link" runs alongside those forty, so their share
    /// of the elapsed time is half their duration.
    fn thin_tail_fixture() -> Arc<Profile> {
        const THIN: usize = 40;
        let mut events = Vec::with_capacity(THIN + 5);
        for tid in [1, 2] {
            events.push(format!(
                r#"{{"name":"thread_name","ph":"M","tid":{tid},"args":{{"name":"skyframe-evaluator-execution-{tid}"}}}}"#,
            ));
        }
        let span = |tid: u32, name: &str, ts: usize, dur: usize| {
            format!(
                r#"{{"cat":"general information","name":"{name}","ph":"X","ts":{ts},"dur":{dur},"tid":{tid}}}"#,
            )
        };
        events.push(span(1, "build", 0, 10_000));
        events.push(span(1, "compile", 0, 8_000));
        events.extend((0..THIN).map(|i| span(1, &format!("t{i:02}"), 8_000 + 5 * i, 5)));
        events.push(span(2, "link", 5_000, 5_000));
        let json = format!(r#"{{"traceEvents":[{}]}}"#, events.join(","));
        Arc::new(
            Profile::from_bytes(json.as_bytes())
                .expect("parse test profile")
                .into_inner(),
        )
    }

    #[test]
    fn the_tooltip_of_a_remainder_gives_its_count_and_summed_weight() {
        let profile = thin_tail_fixture();
        let lines = |weight| {
            let aggregates = aggregate_weighing(&profile, weight);
            let (cache, fl) = laid_out(&aggregates);
            let remainder = cache
                .boxes
                .iter()
                .find(|b| matches!(b.content, FlameBoxContent::Remainder(_)))
                .expect("the thin spans' remainder");
            let hovered = cache
                .hit(&aggregates, fl, 0.0, center_of(fl, remainder))
                .expect("the box at its own center");
            assert_eq!(&hovered.boxed, remainder);
            flame_tooltip_lines(&hovered)
        };
        assert_eq!(
            lines(FlameWeight::Duration),
            ["(40 others)", "too narrow · total 200µs · 1.3% of view"]
        );
        assert_eq!(
            lines(FlameWeight::WallShare),
            ["(40 others)", "too narrow · total 100µs · 1.0% of view"]
        );
    }

    /// A profile of one lane on which "build" holds forty spans and nothing
    /// else, each a small fraction of a [`VIEW_W`]-wide column's pixel and
    /// holding one span of its own length.
    fn thin_children_fixture() -> Arc<Profile> {
        const THIN: usize = 40;
        let mut events = vec![
            r#"{"name":"thread_name","ph":"M","tid":1,"args":{"name":"skyframe-evaluator-execution-1"}}"#
                .to_string(),
        ];
        let span = |name: &str, ts: usize, dur: usize| {
            format!(
                r#"{{"cat":"general information","name":"{name}","ph":"X","ts":{ts},"dur":{dur},"tid":1}}"#,
            )
        };
        events.push(span("build", 0, 10_000));
        for i in 0..THIN {
            events.push(span(&format!("t{i:02}"), 5 * i, 5));
            events.push(span(&format!("n{i:02}"), 5 * i, 5));
        }
        let json = format!(r#"{{"traceEvents":[{}]}}"#, events.join(","));
        Arc::new(
            Profile::from_bytes(json.as_bytes())
                .expect("parse test profile")
                .into_inner(),
        )
    }

    #[test]
    fn a_remainder_of_every_child_of_its_parent_counts_frames_and_not_others() {
        let aggregates = aggregate(&thin_children_fixture());
        let (cache, fl) = laid_out(&aggregates);
        let lines_at = |b: &FlameBox| {
            let hovered = cache
                .hit(&aggregates, fl, 0.0, center_of(fl, b))
                .expect("the box at its own center");
            assert_eq!(&hovered.boxed, b);
            flame_tooltip_lines(&hovered)
        };
        let of_kind = |wanted: fn(&FlameBoxContent) -> bool| {
            let mut found = cache.boxes.iter().filter(|b| wanted(&b.content));
            let boxed = found.next().expect("a box of the kind");
            assert!(found.next().is_none());
            boxed
        };
        let remainder = of_kind(|c| matches!(c, FlameBoxContent::Remainder(_)));
        let lines = ["(40 frames)", "too narrow · total 200µs · 2.0% of view"];
        assert_eq!(lines_at(remainder), lines);
        // The row the thin spans' own children reach is the remainder's too,
        // and names none of them.
        let depth = of_kind(|c| matches!(c, FlameBoxContent::RemainderDepth(_)));
        assert_eq!(
            (depth.x, depth.w, depth.depth),
            (remainder.x, remainder.w, remainder.depth + 1)
        );
        assert_eq!(lines_at(depth), lines);
        let names: Vec<&str> = cache
            .boxes
            .iter()
            .filter_map(|b| frame_name(&aggregates.flame, b))
            .collect();
        assert_eq!(names, ["All", "build"]);
    }

    #[test]
    fn the_tooltip_of_the_root_gives_the_whole_view() {
        let aggregates = aggregate(&fixture());
        let (cache, fl) = laid_out(&aggregates);
        let root = cache.boxes.first().expect("the root frame");
        let hovered = cache
            .hit(&aggregates, fl, 0.0, center_of(fl, root))
            .expect("the box at its own center");
        assert_eq!(
            flame_tooltip_lines(&hovered),
            ["All", "total 1.00ms · self 0µs · 0 spans · 100.0% of view"]
        );
    }

    /// A profile of two lanes: a repository fetch holding a download that
    /// names what it fetches, and a wait recorded under another category than
    /// the wait beside the fetch.
    fn kinds_fixture() -> Arc<Profile> {
        let mut events = Vec::new();
        for (tid, spans) in [
            (
                1,
                &[
                    ("Fetching repository", "@@a", 0, 1000),
                    ("general information", "download", 0, 400),
                    ("general information", "fetching: archive a", 0, 100),
                    ("Remote execution setup", "acquiring semaphore", 1000, 500),
                ][..],
            ),
            (
                2,
                &[(
                    "Remote execution upload time",
                    "acquiring semaphore",
                    0,
                    500,
                )][..],
            ),
        ] {
            events.push(format!(
                r#"{{"name":"thread_name","ph":"M","tid":{tid},"args":{{"name":"skyframe-evaluator-{tid}"}}}}"#,
            ));
            for (cat, name, ts, dur) in spans {
                events.push(format!(
                    r#"{{"cat":"{cat}","name":"{name}","ph":"X","ts":{ts},"dur":{dur},"tid":{tid}}}"#,
                ));
            }
        }
        let json = format!(r#"{{"traceEvents":[{}]}}"#, events.join(","));
        Arc::new(
            Profile::from_bytes(json.as_bytes())
                .expect("parse test profile")
                .into_inner(),
        )
    }

    /// The tooltip lines ahead of the figures for the box of `profile`'s
    /// top-down graph under `labels` that is named `name` at `depth`.
    fn heading(profile: &Arc<Profile>, labels: FlameLabels, name: &str, depth: u32) -> Vec<String> {
        let aggregates = Arc::new(ScopeAggregate::build_for_tracks(
            profile,
            FlameMode {
                labels,
                ..FlameMode::default()
            },
            &TimeSelection::default(),
            |_| true,
        ));
        let (cache, fl) = laid_out(&aggregates);
        let boxed = cache
            .boxes
            .iter()
            .find(|b| b.depth == depth && frame_name(&aggregates.flame, b) == Some(name))
            .unwrap_or_else(|| panic!("no {name} box at depth {depth}"));
        let hovered = cache
            .hit(&aggregates, fl, 0.0, center_of(fl, boxed))
            .expect("the box at its own center");
        let mut lines = flame_tooltip_lines(&hovered);
        lines.pop().expect("the figures");
        lines
    }

    #[test]
    fn the_tooltip_gives_a_frame_the_self_time_its_detail_frames_break_out() {
        // The fetch's own time hangs beneath its frame under the repository's
        // name, and is still the frame's self time, as its Stats row has it.
        let profile = kinds_fixture();
        let figures = |weight| {
            let aggregates = Arc::new(ScopeAggregate::build_for_tracks(
                &profile,
                FlameMode {
                    weight,
                    ..FlameMode::default()
                },
                &TimeSelection::default(),
                |_| true,
            ));
            let (cache, fl) = laid_out(&aggregates);
            let flame = &aggregates.flame;
            let fetching = cache
                .boxes
                .iter()
                .find(|b| frame_name(flame, b) == Some("Fetching repository"))
                .expect("the fetch's frame");
            let hovered = cache
                .hit(&aggregates, fl, 0.0, center_of(fl, fetching))
                .expect("the box at its own center");
            let row = aggregates
                .stats
                .iter()
                .find(|row| flame.str(row.key) == "Fetching repository")
                .expect("the fetch's row");
            (flame_tooltip_lines(&hovered).pop(), row.self_us)
        };
        assert_eq!(
            figures(FlameWeight::Duration),
            (
                Some("total 1.00ms · self 600µs · 1 span · 50.0% of view".to_string()),
                600
            )
        );
        // The first half of the fetch shares the elapsed time with the wait
        // on the other lane.
        assert_eq!(
            figures(FlameWeight::WallShare),
            (
                Some("total 750µs · self 550µs · 1 span · 50.0% of view".to_string()),
                600
            )
        );
    }

    #[test]
    fn the_tooltip_of_a_frame_labelled_by_its_category_does_not_repeat_it() {
        let profile = kinds_fixture();
        assert_eq!(
            heading(&profile, FlameLabels::Kind, "Fetching repository", 1),
            ["Fetching repository"]
        );
    }

    #[test]
    fn the_tooltip_of_a_detail_frame_names_the_kind_of_its_instance() {
        let profile = kinds_fixture();
        // The repository's own time, and the time of the download nested in it.
        for depth in [2, 3] {
            assert_eq!(
                heading(&profile, FlameLabels::Kind, "@@a", depth),
                ["@@a", "Fetching repository"],
                "depth {depth}"
            );
        }
        // A kind the instance's name leads with is not its category.
        assert_eq!(
            heading(&profile, FlameLabels::Kind, "fetching: archive a", 4),
            ["fetching: archive a", "fetching"]
        );
    }

    #[test]
    fn the_tooltip_of_an_actions_detail_frame_names_its_category() {
        // A mnemonic is told by neither the name nor the category of the
        // action it labels.
        let json = r#"{"traceEvents":[
            {"name":"thread_name","ph":"M","tid":1,"args":{"name":"skyframe-evaluator-1"}},
            {"cat":"action processing","name":"Compiling a","ph":"X","ts":0,"dur":1000,"tid":1,
             "args":{"mnemonic":"Rustc"}}
        ]}"#;
        let profile = Arc::new(
            Profile::from_bytes(json.as_bytes())
                .expect("parse test profile")
                .into_inner(),
        );
        assert_eq!(
            heading(&profile, FlameLabels::Kind, "Compiling a", 2),
            ["Compiling a", "action processing"]
        );
        assert_eq!(
            heading(&profile, FlameLabels::Kind, "Rustc", 1),
            ["Rustc", "action processing"]
        );
    }

    #[test]
    fn the_tooltip_of_a_frame_labelled_by_name_names_its_category() {
        let profile = kinds_fixture();
        assert_eq!(
            heading(&profile, FlameLabels::Kind, "download", 2),
            ["download", "general information"]
        );
        assert_eq!(
            heading(&profile, FlameLabels::Name, "@@a", 1),
            ["@@a", "Fetching repository"]
        );
    }

    #[test]
    fn the_tooltip_of_a_frame_merging_two_categories_names_neither() {
        let profile = kinds_fixture();
        for labels in [FlameLabels::Kind, FlameLabels::Name] {
            assert_eq!(
                heading(&profile, labels, "acquiring semaphore", 1),
                ["acquiring semaphore"],
                "{labels:?}"
            );
        }
    }

    #[test]
    fn a_hit_off_the_boxes_names_nothing() {
        let profile = fixture();
        let aggregates = aggregate(&profile);
        let (cache, fl) = laid_out(&aggregates);
        let hit = |x, y| {
            cache
                .hit(&aggregates, fl, 0.0, point(px(x), px(y)))
                .is_some()
        };
        let mid_x = fl.left + fl.width / 2.0;
        // The row past the deepest one the graph reaches carries no box.
        assert!(!hit(
            mid_x,
            fl.top + fl.total_h + flame_metrics().row_height / 2.0
        ));
        // Clear of the content rect on either axis.
        assert!(!hit(fl.left - 1.0, fl.top + 1.0));
        assert!(!hit(mid_x, fl.top - 1.0));
    }

    #[test]
    fn a_layout_keyed_to_another_view_names_nothing() {
        let profile = fixture();
        let aggregates = aggregate(&profile);
        let (cache, fl) = laid_out(&aggregates);
        let at = center_of(fl, cache.boxes.first().expect("the root frame"));
        assert!(cache.hit(&aggregates, fl, 0.0, at).is_some());
        // A rebuild yields a distinct allocation, so the boxes address a graph
        // that is no longer the one on screen.
        assert!(cache.hit(&aggregate(&profile), fl, 0.0, at).is_none());
        // A narrower content column is one the boxes were not placed for.
        let resized = FlameLayout {
            width: fl.width - 1.0,
            ..fl
        };
        assert!(cache.hit(&aggregates, resized, 0.0, at).is_none());
    }

    #[test]
    fn the_content_rect_reserves_a_column_and_a_strip_for_the_bars_shown() {
        let bounds = Bounds::new(point(px(40.0), px(30.0)), size(px(800.0), px(400.0)));
        let full_w = f32::from(bounds.size.width);
        let full_h = f32::from(bounds.size.height);
        let zoomed = FlameView {
            left: 0.0,
            zoom: 2.0,
        };
        let rect = |total_h, view| FlameLayout::new(bounds, total_h, view).rect();
        // A graph that fits, at the zoom that fits it: neither bar shows, so the
        // rect is the whole body.
        assert_eq!(rect(full_h, FlameView::FIT), bounds);
        // Zoomed in, the h-bar takes its strip; the rows still clear what is left
        // of the height, so no column goes to the v-bar.
        assert_eq!(
            rect(full_h - SCROLLBAR_THICKNESS, zoomed),
            Bounds::new(
                bounds.origin,
                size(px(full_w), px(full_h - SCROLLBAR_THICKNESS))
            )
        );
        // Rows that fit the body but not the height left by the h-bar's strip
        // overflow, so both bars show and each takes its reserve.
        assert_eq!(
            rect(full_h, zoomed),
            Bounds::new(
                bounds.origin,
                size(
                    px(full_w - SCROLLBAR_THICKNESS),
                    px(full_h - SCROLLBAR_THICKNESS)
                )
            )
        );
    }
}
