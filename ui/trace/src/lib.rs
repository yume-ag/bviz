//! Loads a Bazel `--profile` trace (`.profile.gz` or `.json`) into
//! `bviz_trace_profile` and opens a window rendering its thread tracks as a
//! flame-chart timeline: time on the x axis, stacked call depth on the y axis,
//! with horizontal zoom/pan and vertical scroll.

mod chart;
mod counters;
mod critical_path;
mod flame_panel;
mod groups;
mod model;
mod rebuild;
mod regions;
mod row_step;
mod stats_panel;
#[cfg(test)]
mod test_profile;
mod theme;
mod timeline;
mod track_label;
mod track_order;
mod tracks_menu;

use bviz_label::names::RepoNames;
use chart::{format_duration, format_time, nice_step, span_seconds};

pub use critical_path::CriticalPathPanel;
pub use flame_panel::FlamePanel;
use groups::{ChartLanes, GroupChrome, group_label, lanes_headed_by_pool};
use model::{SourceReplaced, TraceModel};
use regions::{CountersStrip, DividerGrab, LaneBand, LaneHeights, Regions, clamp_gutter_width};
pub use stats_panel::StatsPanel;
use theme::Palette;
use timeline::{HoveredSlice, TIMELINE_GESTURES, Timeline, zoom_floor};
use track_label::{pool_member_label, track_label};
use tracks_menu::TracksMenu;

use bviz_invocation::{InvocationId, Recorded, Session};
use bviz_trace_layout::{LaneLayout, LaneMetrics, TimeSelection, TimeView};
use bviz_trace_profile::{Profile, Slice, TimeSpan};
use bviz_ui_components::gesture::{
    Button as PointerButton, Clicks, Entry, GestureRow, Gestures, Input, Motion, Place, Pointer,
};
use bviz_ui_components::{
    Axis, Button, DEFAULT_MIN_THUMB, Divider, DividerSide, DragEnd, KeyScope, MenuItem, Nesting,
    OverflowRows, Pending, Priority, SCROLLBAR_THICKNESS, Scrollbar, ToolbarGroup, cut_off_note,
    declare_scopes, find_key_bindings, legend_key_bindings, overlay_canvas, text_input, track_drag,
    zoom_key_bindings,
};
use bviz_ui_playback::{DocumentClock, unaligned_note};
use bviz_ui_trace_args::{PROFILE_LONG, TraceArgs, TraceSource};
use bviz_ui_workspace::{
    Member, Panel, PanelEvent, PanelKind, Role, Titled, ViewKind, ViewSpec, document_label,
};
use clap::Parser;
use gpui::{
    AnyElement, App, AvailableSpace, Bounds, ContentMask, Context, Div, Edges, Element, ElementId,
    Entity, EventEmitter, FocusHandle, Focusable, GlobalElementId, Hsla, InspectorElementId,
    IntoElement, KeyBinding, LayoutId, MouseDownEvent, Pixels, Point, Position, Render,
    ScrollHandle, ShapedLine, SharedString, Style, Subscription, TextAlign, TransformationMatrix,
    Window, div, fill, point, prelude::*, px, relative, size,
};
use std::borrow::Cow;

gpui::actions!(
    bviz_trace,
    [
        /// Select all visible tracks
        SelectAllTracks,
        /// Clear time selection
        ClearTimeSelection,
        /// Choose previous row
        ChoosePrevious,
        /// Choose next row
        ChooseNext,
        /// Clear chosen row
        Unchoose,
        /// Show whole timeline
        ShowWholeTimeline,
        /// Show whole flame graph
        ShowWholeFlameGraph,
        /// Filter tracks
        FilterTracks,
        /// Close tracks menu
        CloseTracksMenu,
    ]
);

pub fn register_keys(cx: &mut App) {
    declare_scopes(
        cx,
        [
            TRACE_KEY_CONTEXT,
            TIMELINE_KEY_CONTEXT,
            TRACKS_MENU_KEY_CONTEXT,
            stats_panel::STATS_KEY_CONTEXT,
            flame_panel::FLAME_KEY_CONTEXT,
            critical_path::KEY_CONTEXT,
        ],
    );
    let timeline = Some(TIMELINE_KEY_CONTEXT.id());
    let critical_path = Some(critical_path::KEY_CONTEXT.id());
    let stats = Some(stats_panel::STATS_KEY_CONTEXT.id());
    let timeline_canvas = text_input::unless_typing(TIMELINE_KEY_CONTEXT);
    let timeline_canvas = Some(timeline_canvas.as_str());
    let flame_canvas = text_input::unless_typing(flame_panel::FLAME_KEY_CONTEXT);
    let flame_canvas = Some(flame_canvas.as_str());
    cx.bind_keys([
        KeyBinding::new("secondary-a", SelectAllTracks, timeline),
        KeyBinding::new("escape", ClearTimeSelection, timeline),
        KeyBinding::new(
            "escape",
            CloseTracksMenu,
            Some(TRACKS_MENU_KEY_CONTEXT.id()),
        ),
        KeyBinding::new("up", ChoosePrevious, critical_path),
        KeyBinding::new("down", ChooseNext, critical_path),
        KeyBinding::new("escape", Unchoose, critical_path),
        KeyBinding::new("up", ChoosePrevious, stats),
        KeyBinding::new("down", ChooseNext, stats),
        KeyBinding::new("escape", Unchoose, stats),
        KeyBinding::new("0", ShowWholeTimeline, timeline_canvas),
        KeyBinding::new("0", ShowWholeFlameGraph, flame_canvas),
        KeyBinding::new("d", flame_panel::CycleDirection, flame_canvas),
        KeyBinding::new("w", flame_panel::CycleWeight, flame_canvas),
        KeyBinding::new("k", flame_panel::ToggleKinds, flame_canvas),
    ]);
    cx.bind_keys(zoom_key_bindings(TIMELINE_KEY_CONTEXT));
    cx.bind_keys(zoom_key_bindings(flame_panel::FLAME_KEY_CONTEXT));
    cx.bind_keys(legend_key_bindings(flame_panel::FLAME_KEY_CONTEXT));
    cx.bind_keys(find_key_bindings(TIMELINE_KEY_CONTEXT, FilterTracks));
}

/// The key context of the trace panel, enclosing every sub-surface's.
const TRACE_KEY_CONTEXT: KeyScope = KeyScope::new("Trace", "Timeline");

/// The key context scoping the timeline's own bindings, narrowing
/// [`TRACE_KEY_CONTEXT`]. The timeline element carries it, so a key pressed in
/// the tracks menu's filter field types there instead of dispatching one of them.
/// Its keys are the panel's own, so the help lists them under the panel's name.
const TIMELINE_KEY_CONTEXT: KeyScope = KeyScope::within(
    &TRACE_KEY_CONTEXT,
    Nesting::Inside,
    "TraceTimeline",
    TRACE_KEY_CONTEXT.label(),
);

/// The key context of the open tracks menu.
const TRACKS_MENU_KEY_CONTEXT: KeyScope = KeyScope::within(
    &TRACE_KEY_CONTEXT,
    Nesting::Inside,
    "TracksMenu",
    "Tracks menu",
);

/// Vertical sizing of the stacked thread and counter lanes, in pixels. The lane
/// header is a thin top margin above the depth rows — the names live in the
/// gutter — and adjacent lanes are parted by a hairline rule rather than a wide
/// gap.
const METRICS: LaneMetrics = LaneMetrics {
    header_height: 10.0,
    row_height: 18.0,
    lane_gap: 2.0,
    counter_height: 36.0,
};

/// Font size of a gutter lane or counter name, fixed rather than derived from the
/// thin lane header so the names stay legible.
const GUTTER_LABEL_SIZE: f32 = 11.0;

/// Narrowest width a slice or flame node paints at; one that would render thinner
/// becomes a marker of this width.
const MIN_SLICE_PX: f32 = 1.0;

/// Only slices at least this wide carry a text label; narrower ones would have no
/// room for legible text.
const LABEL_MIN_PX: f32 = 28.0;

/// Inset of a slice's label from its left and right edges.
const SLICE_TEXT_PAD: f32 = 3.0;

/// Deepest horizontal zoom, in pixels per microsecond.
const MAX_PX_PER_US: f64 = 50.0;

/// Empty space kept reachable past each end of the timeline so its start and end
/// stand clear of the window edges.
const EDGE_MARGIN_PX: f32 = 24.0;

/// How far the left-edge seam is mixed from a box's fill toward the background:
/// enough to read as a muted divider against any fill on either theme.
const SLICE_SEAM_MIX: f32 = 0.35;

/// Initial width of the left label gutter, holding lane and counter names clear
/// of the time content so phase-marker lines never cross them; resizable by its
/// drag handle.
const DEFAULT_GUTTER_WIDTH: f32 = 170.0;
/// Narrowest the gutter drags to, keeping room for a truncated name.
const MIN_GUTTER: f32 = 80.0;
/// Width of the vertical drag handle at the gutter's right edge.
const GUTTER_HANDLE_WIDTH: f32 = 6.0;

/// Smallest height the flame section keeps when the divider is dragged down.
const FLAME_MIN_HEIGHT: f32 = 60.0;

/// Initial height of the counters strip, chosen before the profile is read and
/// so sized for a handful of counter lanes; the divider resizes it, and a
/// profile with more lanes than fit is scrolled through.
const DEFAULT_COUNTERS_HEIGHT: f32 = 160.0;

/// How far a press must travel before it is read as a drag rather than a click.
const MIN_DRAG_PX: f32 = 3.0;

/// Height a counter lane stands at: its header band over a chart area of the
/// height [`METRICS`] declares a counter chart to be. Every lane keeps it
/// however short the strip is, so a strip that cannot hold them all scrolls
/// instead of shrinking each plot until none of them can be read.
const COUNTER_LANE_HEIGHT: f32 = METRICS.header_height + METRICS.counter_height;

/// Height of the time ruler's band of tick labels.
const TICK_BAND_HEIGHT: f32 = 20.0;
/// Height of the band under the ticks that the phase markers' labels stand
/// in, so that no label covers a lane.
const MARKER_BAND_HEIGHT: f32 = MARKER_LABEL_SIZE + 4.0;
/// Height of the time ruler at the top of the timeline: its ticks over the
/// phase markers' labels.
const RULER_HEIGHT: f32 = TICK_BAND_HEIGHT + MARKER_BAND_HEIGHT;
/// Rough pixel spacing aimed for between ruler ticks; the actual step is rounded
/// to a 1/2/5 multiple near this.
const RULER_TARGET_TICK_PX: f64 = 120.0;

/// Font size of a phase-marker label.
const MARKER_LABEL_SIZE: f32 = 11.0;
/// The narrowest room a phase-marker label is cut short to; a label with
/// less room is left out, its line still drawn.
const MIN_MARKER_LABEL_WIDTH: f32 = 24.0;

/// Height of the band between the flame chart and the counters strip that a
/// press takes hold of to resize the strip, wide enough to aim a press at where
/// the line down its middle alone would be a pixel high.
const COUNTERS_DIVIDER_HEIGHT: f32 = 7.0;
const COUNTERS_DIVIDER_LINE: f32 = 1.0;
/// Height the band stands at while the strip is shut: room to hold the caret
/// that marks a shut side, and to aim a press at.
const COUNTERS_COLLAPSED_HEIGHT: f32 = 14.0;
/// How far past its resting edge the cursor must be pulled for the counters
/// strip to shut or spring open — past the floor its lanes need to shut, past
/// where it sits shut to open. A fixed distance, so the pull asked for is the
/// same whatever the floor, and far enough that neither happens under an
/// unsteady hand.
const COUNTERS_SNAP_SLOP: f32 = 16.0;

/// Width of the open tracks visibility menu and the tallest its track list grows
/// before it scrolls.
const TRACKS_MENU_WIDTH: f32 = 280.0;
const TRACKS_MENU_MAX_LIST: f32 = 320.0;
/// Tracks a group lists in the menu before it starts folded, chosen to keep a
/// long group from burying the groups under it while a short one stays open.
const TRACKS_MENU_FOLD_ABOVE: usize = 20;

/// Heights of a stats-table header row and a body row.
const STATS_HEADER_HEIGHT: f32 = 24.0;
const STATS_ROW_HEIGHT: f32 = 22.0;
/// Font size of stats-table cells and headers.
const STATS_TEXT_SIZE: f32 = 12.0;
/// Width of each numeric stats column; the key column takes the remainder.
/// Wide enough for the longest formatted value (e.g. "3155.54s", "871.64ms")
/// plus padding, since cells clip rather than grow.
const STATS_NUM_COL_WIDTH: f32 = 104.0;
/// Horizontal padding inside a stats cell.
const STATS_CELL_PAD: f32 = 8.0;

/// Radius of a crosshair value dot, font size of its value labels, and the gap
/// between the crosshair line and a value label.
const CROSSHAIR_DOT_RADIUS: f32 = 3.0;
const CROSSHAIR_VALUE_SIZE: f32 = 11.0;
const CROSSHAIR_LABEL_GAP: f32 = 5.0;

/// What a left-button drag is currently moving.
#[derive(Clone, Copy)]
enum Drag {
    None,
    /// A press on the divider, resizing the counters strip while it travels.
    Divider(DividerGrab),
    /// A press on the gutter handle resizes the gutter.
    Gutter,
}

/// Height of a group header band and of a group's "show more" row.
const GROUP_BAND_HEIGHT: f32 = 18.0;
/// Side of a group band's fold and show-all icons.
const GROUP_ICON: f32 = 12.0;
/// Indent of a group band's gutter label, clearing the fold icon at the gutter's
/// leading edge.
const GROUP_LABEL_INDENT: f32 = GROUP_ICON + SLICE_TEXT_PAD;
/// Indent of a pool header's gutter label, one nesting step inside the column
/// its group band's label starts at.
const FAMILY_LABEL_INDENT: f32 = GROUP_LABEL_INDENT + 10.0;
/// Least gap kept between a header row's name and the count at its trailing edge.
const HEADER_COUNT_GAP: f32 = 6.0;
/// Lanes shown per group before the rest fold behind the band's show-all toggle.
const TOP_N_PER_GROUP: usize = 12;

pub(crate) type Source = bviz_ui_components::Source<Recorded<model::LoadedProfile>>;

/// The trace document's timeline: the flame chart with its gutter, ruler and
/// counters strip, the divider that resizes the strip, and the tracks menu over
/// them. The chart itself is drawn by [`Timeline`]; this view holds the
/// splits the two of them are laid out by and reads the loaded document from the
/// shared [`TraceModel`].
pub struct TracePanel {
    /// The one instance the document's panels each hold a handle to.
    model: Entity<TraceModel>,
    timeline: Entity<Timeline>,
    drag: Drag,
    /// The counters strip as the divider has left it; clamped on use.
    counters: CountersStrip,
    /// Width of the left label gutter, set by its drag handle; clamped on use.
    gutter_width: f32,
    tracks_menu: Entity<TracksMenu>,
    /// Drops the view-local state a freshly loaded document leaves addressing
    /// nothing: the timeline's scroll and cached scene, and the tracks-menu fold
    /// state the model does not seed.
    _source_replaced: Subscription,
}

impl TracePanel {
    pub(crate) fn new(model: Entity<TraceModel>, cx: &mut Context<Self>) -> Self {
        let source_replaced = cx.subscribe(&model, |panel, _, _: &SourceReplaced, cx| {
            panel.timeline.update(cx, |t, _| t.reset_for_new_profile());
            panel
                .tracks_menu
                .update(cx, |m, _| m.reset_for_new_profile());
            panel.invalidate_timeline(cx);
        });
        // A view that reaches back here takes a weak handle, so this panel's own
        // handle to it is what keeps it alive.
        let timeline = {
            let shell = cx.entity().downgrade();
            let model = model.clone();
            cx.new(|cx| Timeline::new(model, shell, cx))
        };
        let tracks_menu = {
            let shell = cx.entity().downgrade();
            let model = model.clone();
            cx.new(|cx| TracksMenu::new(model, shell, cx))
        };
        Self {
            model,
            timeline,
            drag: Drag::None,
            counters: CountersStrip::DEFAULT,
            gutter_width: DEFAULT_GUTTER_WIDTH,
            tracks_menu,
            _source_replaced: source_replaced,
        }
    }

    /// Rebuilds the timeline's cached scene and re-renders this view (so the
    /// divider and gutter handle reposition). A change to the timeline content or
    /// to the strip/gutter layout uses this.
    fn invalidate_timeline(&self, cx: &mut Context<Self>) {
        self.timeline.update(cx, |t, cx| t.invalidate_canvas(cx));
        cx.notify();
    }

    /// The whole trace body: the [`Timeline`] filling the viewport (it paints
    /// the flame chart and counter plots and owns the timeline's mouse/scroll/
    /// hover wiring), a drag overlay that keeps the strip-resize and gutter drags
    /// tracking the cursor, then the divider, the gutter handle, and the tracks
    /// menu over it.
    fn trace_body(&self, palette: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let timeline = div()
            .absolute()
            .inset_0()
            .size_full()
            .child(self.timeline.clone());
        // The strip-resize and gutter drags live on this view; while one runs, a
        // full-bounds canvas re-installs their window-global handlers each frame.
        // It is non-interactive, so it does not intercept the timeline's presses.
        let drag_overlay = {
            let entity = cx.entity();
            overlay_canvas(move |_, window, cx| {
                if matches!(entity.read(cx).drag, Drag::Divider(_) | Drag::Gutter) {
                    track_drag(window, &entity, on_drag, end_drag);
                }
            })
        };

        let inputs = self.region_inputs(cx);
        let mut overlays = RegionOverlays::new(inputs);
        // A profile with no counters has no strip, and so nothing to resize.
        if inputs.n_counters > 0 {
            let panel = cx.entity();
            overlays = overlays.child(move |regions, _, window| {
                (
                    counters_divider(regions, palette, &panel, window).into_any_element(),
                    regions.divider_rect(),
                )
            });
        }
        // The gutter handle spans the full height and so crosses both the chart
        // and the strip, and must take a press over either: it is placed last,
        // and hit-testing follows paint order.
        let panel = cx.entity();
        let overlays = overlays.child(move |regions, body, window| {
            (
                gutter_handle(&panel, window).into_any_element(),
                gutter_handle_rect(regions, body),
            )
        });
        div()
            .id("trace-viewport")
            .relative()
            .flex_1()
            .min_h_0()
            .w_full()
            .overflow_hidden()
            .child(timeline)
            .child(drag_overlay)
            .child(overlays)
            .child(self.tracks_menu.clone())
    }

    /// [`Regions`] for the current viewport (captured by the timeline), strip and
    /// gutter sizes, the loaded profile's counter track count, and the flame lanes'
    /// heights.
    fn region_inputs(&self, cx: &App) -> RegionInputs {
        let model = self.model.read(cx);
        RegionInputs {
            n_counters: model.profile().map_or(0, |p| counters::track_count(p)),
            counters: self.counters,
            gutter_width: self.gutter_width,
            lanes: model
                .chart_lanes()
                .map(|lanes| lanes.heights())
                .unwrap_or_default(),
        }
    }

    /// The split as the timeline last laid it out, which the divider's presses
    /// are aimed at and its drags measured against.
    fn regions(&self, cx: &App) -> Regions {
        self.region_inputs(cx)
            .split(self.timeline.read(cx).viewport())
    }
}

/// What the panel does for a gesture in [`DIVIDER_GESTURES`].
#[derive(Clone, Copy)]
enum DividerIntent {
    Reset,
    Resize,
}

/// The gestures of the gutter handle and the counters divider.
const DIVIDER_GESTURES: Gestures<DividerIntent> = Gestures::new(&[
    Entry::new(
        Input::new(Motion::Press(PointerButton::Left, Clicks::Double)),
        Place::On("a divider"),
        "Reset to default size",
        DividerIntent::Reset,
    ),
    Entry::new(
        Input::drag(PointerButton::Left),
        Place::On("a divider"),
        "Resize gutter or counters",
        DividerIntent::Resize,
    ),
]);

/// A thin overlay strip at the gutter's right edge.
fn gutter_handle(panel: &Entity<TracePanel>, window: &Window) -> Divider {
    Divider::new(
        "trace-gutter-handle",
        gpui::Axis::Horizontal,
        window.listener_for(panel, |this, e: &MouseDownEvent, _, cx| {
            DIVIDER_GESTURES.dispatch(Pointer::press(e), |intent| {
                match intent {
                    DividerIntent::Reset => this.gutter_width = DEFAULT_GUTTER_WIDTH,
                    DividerIntent::Resize => this.drag = Drag::Gutter,
                }
                true
            });
            cx.stop_propagation();
            this.invalidate_timeline(cx);
        }),
    )
}

/// The band between the flame chart and the counters strip.
fn counters_divider(
    regions: &Regions,
    palette: Palette,
    panel: &Entity<TracePanel>,
    window: &Window,
) -> Divider {
    let divider = Divider::new(
        "trace-counters-divider",
        gpui::Axis::Vertical,
        window.listener_for(panel, |this, e: &MouseDownEvent, _, cx| {
            let counters_top = this.regions(cx).counters_top;
            DIVIDER_GESTURES.dispatch(Pointer::press(e), |intent| {
                match intent {
                    DividerIntent::Reset => {
                        this.counters = CountersStrip::DEFAULT;
                        this.drag = Drag::None;
                    }
                    DividerIntent::Resize => {
                        let grab = this.counters.grab(f32::from(e.position.y), counters_top);
                        this.drag = Drag::Divider(grab);
                    }
                }
                true
            });
            cx.stop_propagation();
            this.invalidate_timeline(cx);
        }),
    )
    .line(px(COUNTERS_DIVIDER_LINE), palette.divider);
    match regions.shows_counters() {
        true => divider,
        false => divider.collapsed(DividerSide::End, palette.muted_label),
    }
}

/// The chip's label for the `window` a shown aggregate covers, of a build
/// spanning `span_us`. Absent while no aggregate is shown, or the shown one
/// covers the whole build.
fn time_window_summary(window: Option<&TimeSelection>, span_us: i64) -> Option<String> {
    let window = window.filter(|w| !w.is_empty())?;
    let mut text = format!(
        "window: {} of {}",
        format_duration(window.total_us()),
        format_duration(span_us)
    );
    let regions = window.len();
    if regions > 1 {
        text.push_str(&format!(" · {regions} regions"));
    }
    Some(text)
}

/// The window the flame graph and stats cover, as a chip carrying the control
/// that widens the next aggregate back to the whole timeline. Absent while no
/// aggregate is shown, or the shown one covers the whole build.
fn time_window_chip(model: &Entity<TraceModel>, palette: Palette, cx: &App) -> Option<Div> {
    let summary = {
        let m = model.read(cx);
        let profile = m.profile()?;
        let span = profile.span();
        let shown = m.aggregates();
        time_window_summary(
            shown.as_deref().map(|a| &a.window),
            span.end_us - span.start_us,
        )?
    };
    let model = model.clone();
    Some(
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .pl_2()
            .flex_shrink_0()
            .rounded_md()
            .bg(palette.selection)
            .border_1()
            .border_color(palette.accent)
            .text_color(palette.label)
            .child(summary)
            .child(
                Button::new("trace-time-window-clear", move |_, _, cx| {
                    clear_time_window(&model, cx)
                })
                .ghost()
                .compact()
                .icon("x")
                .keys_for_focused(&ClearTimeSelection),
            ),
    )
}

/// [`time_window_chip`] as a toolbar group, which leaves the row for the
/// overflow menu as the row clearing the window, its keys named where `focus`
/// stands.
pub(crate) fn time_window_group(
    model: &Entity<TraceModel>,
    palette: Palette,
    focus: &FocusHandle,
    window: &Window,
    cx: &App,
) -> Option<ToolbarGroup> {
    let chip = time_window_chip(model, palette, cx)?;
    let model = model.clone();
    let row = MenuItem::action(
        &ClearTimeSelection,
        Some(focus),
        window,
        cx,
        move |_, cx| clear_time_window(&model, cx),
    );
    Some(ToolbarGroup::new(
        TIME_WINDOW,
        chip,
        OverflowRows::row(row, true),
    ))
}

/// How long the time window chip holds out against its toolbar narrowing:
/// past every control, as what the panel shows is read against it.
const TIME_WINDOW: Priority = Priority::MAX;

/// Widens the next aggregate of `model` back to the whole timeline.
fn clear_time_window(model: &Entity<TraceModel>, cx: &mut App) {
    model.update(cx, |m, cx| {
        m.set_time_selection(TimeSelection::default(), cx);
    });
}

/// What the window chrome says about the loaded document, the same from whichever
/// of its panels is on show.
pub(crate) fn profile_status(model: &Entity<TraceModel>, cx: &App) -> SharedString {
    model
        .read(cx)
        .source()
        .status("No profile", |loaded| {
            let p = loaded.get().profile();
            let slices: usize = p.threads().iter().map(|t| t.slices.len()).sum();
            format!(
                "{} threads · {} slices · {:.3}s",
                p.threads().len(),
                slices,
                span_seconds(p.span()),
            )
        })
        .into()
}

impl EventEmitter<PanelEvent> for TracePanel {}

impl Focusable for TracePanel {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.timeline.focus_handle(cx)
    }
}

impl Panel for TracePanel {
    fn kind() -> PanelKind {
        PanelKind {
            view: ViewKind::new("trace"),
            key_context: TRACE_KEY_CONTEXT,
            role: Role::Timeline,
        }
    }

    fn gestures() -> Vec<GestureRow> {
        TIMELINE_GESTURES
            .rows()
            .chain(DIVIDER_GESTURES.rows())
            .collect()
    }

    fn titled() -> Titled {
        Titled::File
    }

    fn status(&self, cx: &App) -> Option<SharedString> {
        Some(profile_status(&self.model, cx))
    }

    fn set_active(&mut self, active: bool, window: &mut Window, cx: &mut Context<Self>) {
        match active {
            true => self.focus_handle(cx).focus(window, cx),
            // The tooltip and crosshair anchor to a cursor that is no longer over
            // the chart.
            false => self.timeline.update(cx, |t, _| t.clear_hover()),
        }
    }
}

impl Render for TracePanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = bviz_ui_theme::theme(window, cx);
        let palette = Palette::from_theme(&theme);
        // Take the placeholder before the `&mut self` build calls below, so the
        // model borrow it is read through ends first.
        let placeholder = self.model.read(cx).source().placeholder(&theme.colors);
        let chrome = div()
            .key_context(Self::kind().key_context)
            .track_focus(&self.focus_handle(cx))
            .flex_1()
            .flex()
            .flex_col()
            .overflow_hidden();
        match placeholder {
            Some(placeholder) => chrome.child(placeholder),
            None => {
                let unaligned = self
                    .model
                    .read(cx)
                    .unaligned()
                    .then(|| unaligned_note("profile", &theme.colors));
                let cut_off = self
                    .model
                    .read(cx)
                    .cut_off()
                    .then(|| cut_off_note("profile", "events", &theme.colors));
                chrome
                    .on_action(cx.listener(|view, _: &FilterTracks, window, cx| {
                        view.tracks_menu
                            .update(cx, |menu, cx| menu.open_filtering(window, cx));
                    }))
                    .children(cut_off)
                    .children(unaligned)
                    .child(self.trace_body(palette, cx))
            }
        }
    }
}

/// What [`Regions`] needs beyond the bounds it splits.
#[derive(Clone, Copy)]
struct RegionInputs {
    n_counters: usize,
    counters: CountersStrip,
    gutter_width: f32,
    lanes: LaneHeights,
}

impl RegionInputs {
    fn split(&self, bounds: Bounds<Pixels>) -> Regions {
        Regions::new(
            bounds,
            self.n_counters,
            self.counters,
            self.gutter_width,
            self.lanes,
        )
    }
}

/// Builds an overlay against the split it belongs to and the body's own bounds,
/// giving the element together with the rect it stands in, so what an overlay
/// shows and where it sits are read from one split.
type OverlayInRegions =
    Box<dyn Fn(&Regions, Bounds<Pixels>, &Window) -> (AnyElement, Bounds<Pixels>)>;

fn gutter_handle_rect(r: &Regions, body: Bounds<Pixels>) -> Bounds<Pixels> {
    Bounds::new(
        point(
            px(r.content_left - GUTTER_HANDLE_WIDTH / 2.0),
            body.origin.y,
        ),
        size(px(GUTTER_HANDLE_WIDTH), body.size.height),
    )
}

/// The trace body's overlays — the counters divider and the gutter handle —
/// laid out from the body's own bounds. Each belongs at a pixel offset within the
/// [`Regions`] split, which is defined only once the body has a size, so the
/// split is resolved here during prepaint and each child built against it. A
/// size measured elsewhere is known only after a paint, and would put both what
/// the children show and where they sit a frame behind the body they overlay.
struct RegionOverlays {
    inputs: RegionInputs,
    children: Vec<OverlayInRegions>,
}

impl RegionOverlays {
    fn new(inputs: RegionInputs) -> Self {
        Self {
            inputs,
            children: Vec::new(),
        }
    }

    fn child(
        mut self,
        build: impl Fn(&Regions, Bounds<Pixels>, &Window) -> (AnyElement, Bounds<Pixels>) + 'static,
    ) -> Self {
        self.children.push(Box::new(build));
        self
    }
}

impl IntoElement for RegionOverlays {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for RegionOverlays {
    type RequestLayoutState = ();
    type PrepaintState = Vec<AnyElement>;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        // The overlays cover the body without displacing the timeline beneath them.
        let style = Style {
            position: Position::Absolute,
            inset: Edges::all(px(0.).into()),
            size: size(relative(1.).into(), relative(1.).into()),
            ..Default::default()
        };
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Vec<AnyElement> {
        let regions = self.inputs.split(bounds);
        let mut children = Vec::with_capacity(self.children.len());
        for build in &self.children {
            let (mut child, rect) = build(&regions, bounds, window);
            let space = size(
                AvailableSpace::Definite(rect.size.width),
                AvailableSpace::Definite(rect.size.height),
            );
            child.prepaint_as_root(rect.origin, space, window, cx);
            children.push(child);
        }
        children
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        children: &mut Vec<AnyElement>,
        window: &mut Window,
        cx: &mut App,
    ) {
        for child in children {
            child.paint(window, cx);
        }
    }
}

/// Routes an in-progress drag to its target: resizing the counters strip or the
/// gutter. The flame-chart timeline's, flame-graph panel's, and tracks menu's own
/// drags are routed inside [`Timeline`], [`FlamePanel`], and [`TracksMenu`].
fn on_drag(
    view: &mut TracePanel,
    position: Point<Pixels>,
    _window: &mut Window,
    cx: &mut Context<TracePanel>,
) {
    match view.drag {
        Drag::Divider(mut grab) => {
            let vp = view.timeline.read(cx).viewport();
            let vh = f32::from(vp.size.height);
            let counters_bottom = f32::from(vp.origin.y) + vh;
            view.counters = grab.dragged_to(f32::from(position.y), counters_bottom, vh);
            view.drag = Drag::Divider(grab);
        }
        Drag::Gutter => {
            let vp = view.timeline.read(cx).viewport();
            view.gutter_width = clamp_gutter_width(
                f32::from(position.x) - f32::from(vp.origin.x),
                f32::from(vp.size.width),
            );
        }
        Drag::None => {}
    }
    // Strip and gutter resizes move the timeline's split, so they rebuild it.
    view.invalidate_timeline(cx);
}

fn end_drag(
    view: &mut TracePanel,
    _: Point<Pixels>,
    _: DragEnd,
    _: &mut Window,
    cx: &mut Context<TracePanel>,
) {
    if let Drag::Divider(grab) = view.drag {
        view.counters = grab.released(view.counters);
    }
    view.drag = Drag::None;
    view.invalidate_timeline(cx);
}

/// The scroll offset a thumb at `pos` (grabbed at `grab`) maps to, or `None`
/// when the bar is not currently painted.
fn thumb_offset(bar: Option<Scrollbar>, pos: Point<Pixels>, grab: f32) -> Option<f32> {
    Some(bar?.offset_at(pos, grab))
}

/// The offset a view holding `held` draws at, bounded by the end of its content
/// at `ceiling`.
fn drawn_scroll(held: f32, ceiling: f32) -> f32 {
    held.clamp(0.0, ceiling)
}

/// Where a gesture asking for `request` leaves a view holding `held`, and `None`
/// for a gesture that moves nothing on screen. A held position past the ceiling
/// — a place the content has shrunk away from, which comes back when it grows
/// again — draws at the ceiling, so a further gesture into the bound asks for
/// the pixels already shown: taking it would spend the held position and invent
/// an overscroll past the end.
fn scroll_request(held: f32, ceiling: f32, request: f32) -> Option<f32> {
    let target = drawn_scroll(request, ceiling);
    (target != drawn_scroll(held, ceiling)).then_some(target)
}

/// Scrolls a list `handle` so a downward thumb `offset` maps to a matching
/// negative vertical scroll.
fn set_list_thumb(handle: &ScrollHandle, offset: f32) {
    let mut o = handle.offset();
    o.y = px(-offset);
    handle.set_offset(o);
}

/// The left x of a `label_w`-wide label placed `gap` right of `anchor`, flipped
/// to `gap` left of it when the right placement would spill past `right_limit`.
pub(crate) fn label_x(anchor: f32, label_w: f32, gap: f32, right_limit: f32) -> f32 {
    if anchor + gap + label_w > right_limit {
        anchor - gap - label_w
    } else {
        anchor + gap
    }
}

/// Where a phase marker's label stands in the marker band: from `left`,
/// within `width`.
#[derive(Clone, Copy, Debug, PartialEq)]
struct MarkerLabel {
    left: f32,
    width: f32,
}

/// Places the labels of markers at `lines`, each its x and its label's full
/// width in order of x, in the one row of the marker band between
/// `left_limit` and `right_limit`. A label stands right of its line up to the next line; where
/// it does not fit there, left of its line where that keeps clear of the
/// label and line before it; and otherwise cut short to the room right of its
/// line, or left out where that room is narrower than
/// [`MIN_MARKER_LABEL_WIDTH`].
fn place_marker_labels(
    lines: &[(f32, f32)],
    gap: f32,
    left_limit: f32,
    right_limit: f32,
) -> Vec<Option<MarkerLabel>> {
    let mut placed = Vec::with_capacity(lines.len());
    let mut clear_from = left_limit;
    for (i, &(x, width)) in lines.iter().enumerate() {
        let next = lines.get(i + 1).map_or(right_limit, |&(next, _)| next);
        let room = next - x - 2.0 * gap;
        let label = if width <= room {
            Some(MarkerLabel {
                left: x + gap,
                width,
            })
        } else if x - gap - width >= clear_from {
            Some(MarkerLabel {
                left: x - gap - width,
                width,
            })
        } else if room >= MIN_MARKER_LABEL_WIDTH {
            Some(MarkerLabel {
                left: x + gap,
                width: room,
            })
        } else {
            None
        };
        clear_from = match label {
            Some(label) => label.left + label.width + gap,
            None => x + gap,
        };
        placed.push(label);
    }
    placed
}

/// The window x of each build-phase boundary on the time axis, in order of
/// time, with its name; any off-screen left out.
fn shown_markers(
    profile: &Profile,
    tv: TimeView,
    regions: Regions,
) -> impl Iterator<Item = (f32, &str)> {
    profile.phase_markers().iter().filter_map(move |marker| {
        let x = tv.x_of(marker.start_us);
        (0.0..=regions.time_w)
            .contains(&x)
            .then(|| (regions.content_left + x, profile.str(marker.name)))
    })
}

/// Draws each build-phase boundary as a vertical line down through both
/// sections, confined to the time axis.
fn paint_markers(
    profile: &Profile,
    tv: TimeView,
    regions: Regions,
    show_counters: bool,
    palette: &Palette,
    window: &mut Window,
) {
    for (x, _) in shown_markers(profile, tv, regions) {
        window.paint_quad(fill(
            Bounds::new(
                point(px(x), px(regions.flame_top)),
                size(px(1.), px(regions.flame_h())),
            ),
            palette.marker,
        ));
        // Continue the marker line through the counters strip, which shares the
        // time axis, so a phase boundary reads across both.
        if show_counters {
            window.paint_quad(fill(
                Bounds::new(
                    point(px(x), px(regions.counters_top)),
                    size(px(1.), px(regions.counters_h())),
                ),
                palette.marker,
            ));
        }
    }
}

/// Draws the build-phase boundaries into the ruler's marker band: each
/// line's stretch there and its name as [`place_marker_labels`] places it.
/// Painted over the ruler, whose band it is.
fn paint_marker_band(
    profile: &Profile,
    tv: TimeView,
    regions: Regions,
    palette: &Palette,
    window: &mut Window,
    cx: &mut App,
) {
    let band_top = regions.flame_top - MARKER_BAND_HEIGHT;
    let font = bviz_ui_assets::text_metrics(window).font;
    let fs = px(MARKER_LABEL_SIZE);
    let mut shown: Vec<(f32, &str, ShapedLine)> = shown_markers(profile, tv, regions)
        .map(|(x, name)| {
            let line = bviz_ui_assets::shape_line(
                window,
                name.to_string(),
                font.clone(),
                fs,
                palette.marker.into(),
            );
            (x, name, line)
        })
        .collect();
    shown.sort_by(|(a, ..), (b, ..)| a.total_cmp(b));
    let lines: Vec<(f32, f32)> = shown
        .iter()
        .map(|(x, _, line)| (*x, f32::from(line.width())))
        .collect();
    let placed = place_marker_labels(
        &lines,
        SLICE_TEXT_PAD,
        regions.content_left,
        regions.time_right,
    );
    let label_top = band_top + (MARKER_BAND_HEIGHT - MARKER_LABEL_SIZE) / 2.0;
    for ((x, name, line), label) in shown.into_iter().zip(placed) {
        window.paint_quad(fill(
            Bounds::new(
                point(px(x), px(band_top)),
                size(px(1.), px(MARKER_BAND_HEIGHT)),
            ),
            palette.marker,
        ));
        let Some(label) = label else {
            continue;
        };
        let line = match label.width < f32::from(line.width()) {
            true => bviz_ui_components::shape_truncated(
                name,
                label.width,
                font.clone(),
                fs,
                palette.marker.into(),
                window,
            ),
            false => line,
        };
        let _ = line.paint(
            point(px(label.left), px(label_top)),
            fs,
            TextAlign::Left,
            None,
            window,
            cx,
        );
    }
}

/// Tick positions (viewport x) and labels across the visible span, spaced near
/// `RULER_TARGET_TICK_PX` at a 1/2/5 step.
fn ruler_ticks(tv: TimeView, width: f32) -> Vec<(f32, String)> {
    if tv.px_per_us <= 0.0 || width <= 0.0 {
        return Vec::new();
    }
    let step = nice_step(RULER_TARGET_TICK_PX / tv.px_per_us);
    let end_us = tv.start_us + width as f64 / tv.px_per_us;
    let mut t = (tv.start_us / step).ceil() * step;
    let mut ticks = Vec::new();
    while t <= end_us {
        ticks.push((
            ((t - tv.start_us) * tv.px_per_us) as f32,
            format_time(t, step),
        ));
        t += step;
    }
    ticks
}

/// Drops a faint vertical line from each ruler tick down through both sections,
/// confined to the content column.
fn paint_gridlines(
    ticks: &[(f32, String)],
    content_left: f32,
    top: f32,
    bottom: f32,
    palette: &Palette,
    window: &mut Window,
) {
    let height = px(bottom - top);
    for (x, _) in ticks {
        let rect = Bounds::new(point(px(content_left + *x), px(top)), size(px(1.), height));
        window.paint_quad(fill(rect, palette.gridline));
    }
}

/// Paints the ruler band along the top edge spanning the full width, with a tick
/// mark and label in the content column at each tick.
fn paint_ruler(
    ticks: &[(f32, String)],
    regions: Regions,
    bounds: Bounds<Pixels>,
    palette: &Palette,
    window: &mut Window,
    cx: &mut App,
) {
    let band = Bounds::new(bounds.origin, size(bounds.size.width, px(RULER_HEIGHT)));
    window.paint_quad(fill(band, palette.header_band));
    let font = bviz_ui_assets::text_metrics(window).font;
    let fs = px(TICK_BAND_HEIGHT - 2. * SLICE_TEXT_PAD);
    let ty = bounds.origin.y + px((TICK_BAND_HEIGHT - f32::from(fs)) / 2.);
    for (x, label) in ticks {
        let tx = px(regions.content_left + *x);
        let tick = Bounds::new(
            point(tx, bounds.origin.y + px(TICK_BAND_HEIGHT - 4.)),
            size(px(1.), px(4.)),
        );
        window.paint_quad(fill(tick, palette.label));
        let line = bviz_ui_assets::shape_line(
            window,
            label.clone(),
            font.clone(),
            fs,
            palette.label.into(),
        );
        let _ = line.paint(
            point(tx + px(2.), ty),
            fs,
            TextAlign::Left,
            None,
            window,
            cx,
        );
    }
}

/// The timeline's scroll bars, each `None` where its content fits and so has
/// nothing to scroll.
#[derive(Clone, Copy, Default)]
struct TimelineBars {
    /// Maps to the flame lanes' pixel scroll.
    flame: Option<Scrollbar>,
    /// Maps to the view's start time within the profile span.
    time: Option<Scrollbar>,
    /// Maps to the counters strip's pixel scroll.
    counters: Option<Scrollbar>,
}

impl TimelineBars {
    fn paint(&self, palette: &Palette, window: &mut Window) {
        for bar in [self.flame, self.time, self.counters].into_iter().flatten() {
            bar.paint(palette.scroll.track, palette.scroll.thumb, window);
        }
    }
}

/// Places the timeline's bars for its view state. The two vertical bars share
/// the column reserved at the content's right edge, each over its own section.
fn scrollbar_geom(
    span: TimeSpan,
    tv: TimeView,
    scrolled_total: f32,
    regions: Regions,
    scroll_y: f32,
    counter_scroll: f32,
) -> TimelineBars {
    let scrolled = regions.scrolled_band(scroll_y);
    let vbar_left = px(regions.content_right - SCROLLBAR_THICKNESS);

    let span_us = (span.end_us - span.start_us) as f32;
    let visible_us = regions.time_w / tv.px_per_us as f32;
    // Pan and the wheel reach EDGE_MARGIN_PX of empty space past each span end,
    // so the bar spans that padded range and its offset is measured from the
    // padded start, letting the thumb reach the same margins.
    let margin_us = EDGE_MARGIN_PX / tv.px_per_us as f32;

    let flame = Scrollbar::new(
        Axis::Vertical,
        Bounds::new(
            point(vbar_left, px(scrolled.top)),
            size(px(SCROLLBAR_THICKNESS), px(scrolled.height)),
        ),
        scrolled_total,
        scrolled.height,
        scroll_y,
        DEFAULT_MIN_THUMB,
    );
    let counters = Scrollbar::new(
        Axis::Vertical,
        Bounds::new(
            point(vbar_left, px(regions.counters_top)),
            size(px(SCROLLBAR_THICKNESS), px(regions.counters_h())),
        ),
        regions.counters_total(),
        regions.counters_h(),
        counter_scroll,
        DEFAULT_MIN_THUMB,
    );
    // The time horizontal track runs the full time axis along the flame lanes'
    // bottom edge, ending left of the reserved vertical-bar column. Rounding the
    // extents to microseconds can leave a view sitting on the zoom floor a hair
    // wider than its own span, so the bar is gated on the floor itself.
    let time = match tv.px_per_us > zoom_floor(span, regions.time_w) {
        true => Scrollbar::new(
            Axis::Horizontal,
            Bounds::new(
                point(
                    px(regions.content_left),
                    px(regions.flame_bottom - SCROLLBAR_THICKNESS),
                ),
                size(px(regions.time_w), px(SCROLLBAR_THICKNESS)),
            ),
            span_us + 2.0 * margin_us,
            visible_us,
            tv.start_us as f32 - (span.start_us as f32 - margin_us),
            DEFAULT_MIN_THUMB,
        ),
        false => None,
    };

    TimelineBars {
        flame,
        time,
        counters,
    }
}

struct GutterPaint<'a> {
    left: f32,
    width: f32,
    avail: f32,
    font: gpui::Font,
    fs: Pixels,
    palette: &'a Palette,
}

/// Paints the opaque left gutter — a single column spanning the flame chart and
/// the counters strip — over any content that panned or scrolled under it, then
/// the names left-aligned within it, each clipped to the band it scrolls in:
/// thread names track the flame lanes' vertical scroll, counter names the
/// strip's.
#[allow(clippy::too_many_arguments)]
fn paint_gutter(
    profile: &Profile,
    lanes: &ChartLanes,
    chrome: &GroupChrome,
    regions: Regions,
    gutter_width: f32,
    scroll_y: f32,
    counter_scroll: f32,
    show_counters: bool,
    selected: &[bool],
    shared_names: &[bool],
    palette: &Palette,
    window: &mut Window,
    cx: &mut App,
) {
    let gutter_left = regions.content_left - gutter_width;
    let gutter_bottom = regions.counters_bottom;
    paint_gutter_frame(
        regions,
        gutter_left,
        gutter_width,
        gutter_bottom,
        palette,
        window,
    );

    let font = bviz_ui_assets::text_metrics(window).font;
    let fs = px(GUTTER_LABEL_SIZE);
    let avail = gutter_width - 2.0 * SLICE_TEXT_PAD;
    let g = GutterPaint {
        left: gutter_left,
        width: gutter_width,
        avail,
        font,
        fs,
        palette,
    };

    // Thread and group labels stand in the band their lanes are drawn in, so
    // clip them to it.
    let gutter = GutterLanes {
        profile,
        selected,
        shared_names,
    };
    let pinned = regions.pinned_band();
    window.with_content_mask(
        Some(ContentMask {
            bounds: pinned.rect(gutter_left, regions.content_left),
        }),
        |window| {
            let unheaded = std::iter::repeat(false);
            paint_thread_labels(&g, &gutter, &lanes.pinned, unheaded, pinned, window, cx);
        },
    );
    let scrolled = regions.scrolled_band(scroll_y);
    window.with_content_mask(
        Some(ContentMask {
            bounds: scrolled.rect(gutter_left, regions.content_left),
        }),
        |window| {
            let headed = lanes_headed_by_pool(chrome);
            paint_thread_labels(&g, &gutter, &lanes.scrolled, headed, scrolled, window, cx);
            paint_group_bands(&g, chrome, scrolled, window, cx);
            paint_family_bands(&g, chrome, scrolled, window, cx);
        },
    );

    if show_counters {
        let counters_mask = ContentMask {
            bounds: Bounds::new(
                point(px(gutter_left), px(regions.counters_top)),
                size(px(gutter_width), px(regions.counters_h())),
            ),
        };
        window.with_content_mask(Some(counters_mask), |window| {
            paint_counter_labels(&g, profile, regions, counter_scroll, window, cx);
        });
    }
}

/// Fills the gutter's opaque background and draws a divider at its right edge,
/// under the drag handle, spanning the full height so the resizable line reads as
/// one across the flame chart and the counters strip.
fn paint_gutter_frame(
    regions: Regions,
    gutter_left: f32,
    gutter_width: f32,
    gutter_bottom: f32,
    palette: &Palette,
    window: &mut Window,
) {
    let strip = Bounds::new(
        point(px(gutter_left), px(regions.flame_top)),
        size(px(gutter_width), px(gutter_bottom - regions.flame_top)),
    );
    window.paint_quad(fill(strip, palette.header_band));
    window.paint_quad(fill(
        Bounds::new(
            point(px(regions.content_left - 1.0), px(regions.flame_top)),
            size(px(1.0), px(gutter_bottom - regions.flame_top)),
        ),
        palette.divider,
    ));
}

/// Shapes `name` truncated to `avail` and paints it left-aligned at the gutter's
/// text inset, vertically offset to `ty`.
fn paint_gutter_label(g: &GutterPaint, name: &str, ty: f32, window: &mut Window, cx: &mut App) {
    let gutter_left = g.left;
    let avail = g.avail;
    let font = &g.font;
    let fs = g.fs;
    let palette = g.palette;
    let line = bviz_ui_components::shape_truncated(
        name,
        avail,
        font.clone(),
        fs,
        palette.label.into(),
        window,
    );
    let _ = line.paint(
        point(px(gutter_left + SLICE_TEXT_PAD), px(ty)),
        fs,
        TextAlign::Left,
        None,
        window,
        cx,
    );
}

/// The state a flame lane's gutter row is drawn from.
struct GutterLanes<'a> {
    profile: &'a Profile,
    selected: &'a [bool],
    shared_names: &'a [bool],
}

/// Paints the label of each of `layout`'s lanes, drawn in `band` where
/// [`LaneBand::label_centre`] places it. `headed` says, lane by lane,
/// whether a pool header stands over it. A selected lane's row carries a faint
/// accent tint and a leading accent bar, so which tracks feed the flame graph
/// reads at a glance.
fn paint_thread_labels(
    g: &GutterPaint,
    lanes: &GutterLanes,
    layout: &LaneLayout,
    headed: impl Iterator<Item = bool>,
    band: LaneBand,
    window: &mut Window,
    cx: &mut App,
) {
    let gutter_left = g.left;
    let gutter_width = g.width;
    let palette = g.palette;
    for (lane, headed) in layout.lanes.iter().zip(headed) {
        let top = band.y_of(lane.top);
        if top + lane.height < band.top || top > band.bottom() {
            continue;
        }
        if lanes.selected.get(lane.track).copied().unwrap_or(false) {
            window.paint_quad(fill(
                Bounds::new(
                    point(px(gutter_left), px(top)),
                    size(px(gutter_width), px(lane.height)),
                ),
                palette.selection,
            ));
            window.paint_quad(fill(
                Bounds::new(
                    point(px(gutter_left), px(top)),
                    size(px(2.0), px(lane.height)),
                ),
                palette.accent,
            ));
        }
        let Some(centre) = band.label_centre(lane.top, lane.height, GUTTER_LABEL_SIZE) else {
            continue;
        };
        let ty = centre - GUTTER_LABEL_SIZE / 2.0;
        let label = if headed {
            pool_member_label(lanes.profile, lanes.shared_names, lane.track)
        } else {
            track_label(lanes.profile, lanes.shared_names, lane.track)
        };
        paint_gutter_label(g, &label, ty, window, cx);
    }
}

/// How many of a header row's lanes the chart draws of those it holds — the
/// total alone where it draws none.
fn header_count(shown: Option<usize>, total: usize) -> String {
    match shown {
        Some(shown) => format!("{shown}/{total}"),
        None => format!("{total}"),
    }
}

/// A header row of the flame chart's gutter.
struct HeaderRow<'a> {
    /// Where the row sits in the chart, before the lane scroll moves it.
    top: f32,
    name: &'a str,
    count: &'a str,
    /// Width held at the row's leading edge, ahead of the name.
    indent: f32,
    /// Width held at the row's trailing edge for an icon drawn over it.
    reserved: f32,
    color: Hsla,
}

/// Paints a header row of the scrolled lanes' `band`: its count set against the
/// gutter's trailing edge, clear of what the row reserves there, and its name
/// truncated into the width left between the two. Returns the window y the
/// row's text and icons centre on, and nothing where the band shows too little
/// of the row to hold its icons whole.
fn paint_header_row(
    g: &GutterPaint,
    band: LaneBand,
    row: HeaderRow,
    window: &mut Window,
    cx: &mut App,
) -> Option<f32> {
    let centre = band.label_centre(row.top, GROUP_BAND_HEIGHT, GROUP_ICON)?;
    let ty = centre - GUTTER_LABEL_SIZE / 2.0;
    let shape = |text: &str, avail: f32, window: &mut Window| {
        bviz_ui_components::shape_truncated(text, avail, g.font.clone(), g.fs, row.color, window)
    };
    let count = shape(row.count, g.avail - row.indent - row.reserved, window);
    let count_width = f32::from(count.width());
    let name_avail = g.avail - row.indent - row.reserved - count_width - HEADER_COUNT_GAP;
    let name = shape(row.name, name_avail, window);
    let paint = |line: gpui::ShapedLine, x: f32, window: &mut Window, cx: &mut App| {
        let _ = line.paint(
            point(px(x), px(ty)),
            g.fs,
            TextAlign::Left,
            None,
            window,
            cx,
        );
    };
    paint(name, g.left + SLICE_TEXT_PAD + row.indent, window, cx);
    let count_x = g.left + g.width - SLICE_TEXT_PAD - row.reserved - count_width;
    paint(count, count_x, window, cx);
    Some(centre)
}

/// Paints each group header band's caret, class name, and shown/total lane
/// count; a capped, expanded group shows a caret-double icon at the gutter's right
/// edge — down to show every lane, up to show only the leading ones.
fn paint_group_bands(
    g: &GutterPaint,
    chrome: &GroupChrome,
    band: LaneBand,
    window: &mut Window,
    cx: &mut App,
) {
    let gutter_left = g.left;
    let gutter_width = g.width;
    let palette = g.palette;
    for group in &chrome.bands {
        let show_icon = group.capped && !group.collapsed;
        let count = header_count((!group.collapsed).then_some(group.shown), group.total);
        let Some(centre) = paint_header_row(
            g,
            band,
            HeaderRow {
                top: group.top,
                name: group_label(group.class),
                count: &count,
                indent: GROUP_LABEL_INDENT,
                reserved: if show_icon {
                    GROUP_ICON + SLICE_TEXT_PAD
                } else {
                    0.0
                },
                color: palette.label.into(),
            },
            window,
            cx,
        ) else {
            continue;
        };
        paint_band_icon(
            if group.collapsed {
                "caret-right"
            } else {
                "caret-down"
            },
            gutter_left + SLICE_TEXT_PAD,
            centre,
            palette.label.into(),
            window,
            cx,
        );
        if show_icon {
            let name = if group.show_all {
                "caret-double-up"
            } else {
                "caret-double-down"
            };
            paint_band_icon(
                name,
                gutter_left + gutter_width - GROUP_ICON - SLICE_TEXT_PAD,
                centre,
                palette.marker.into(),
                window,
                cx,
            );
        }
    }
}

/// Paints one of a group band's icons, square and vertically centred on window
/// y `centre`.
fn paint_band_icon(
    name: &str,
    left: f32,
    centre: f32,
    color: Hsla,
    window: &mut Window,
    cx: &mut App,
) {
    let iy = centre - GROUP_ICON / 2.0;
    let _ = window.paint_svg(
        Bounds::new(
            point(px(left), px(iy)),
            size(px(GROUP_ICON), px(GROUP_ICON)),
        ),
        SharedString::from(format!("assets/regular/{name}.svg")),
        None,
        TransformationMatrix::default(),
        color,
        cx,
    );
}

/// Paints each pool header's name and shown/total lane count, indented under the
/// group band it hangs from.
fn paint_family_bands(
    g: &GutterPaint,
    chrome: &GroupChrome,
    band: LaneBand,
    window: &mut Window,
    cx: &mut App,
) {
    for family in &chrome.families {
        let count = header_count(Some(family.shown), family.total);
        paint_header_row(
            g,
            band,
            HeaderRow {
                top: family.top,
                name: &family.name,
                count: &count,
                indent: FAMILY_LABEL_INDENT,
                reserved: 0.0,
                color: g.palette.muted_label.into(),
            },
            window,
            cx,
        );
    }
}

/// Paints each counter's name at its strip lane's chart baseline, so it
/// associates with low values rather than floating at the top.
fn paint_counter_labels(
    g: &GutterPaint,
    profile: &Profile,
    regions: Regions,
    counter_scroll: f32,
    window: &mut Window,
    cx: &mut App,
) {
    for gl in regions.counter_lanes(counter_scroll) {
        let lane_bottom = gl.top + METRICS.header_height + gl.chart_h;
        let ty = lane_bottom - GUTTER_LABEL_SIZE - SLICE_TEXT_PAD;
        paint_gutter_label(g, counters::track_name(profile, gl.lane), ty, window, cx);
    }
}

/// Paints a slice quad and, if wide enough, its truncated name.
#[allow(clippy::too_many_arguments)]
fn paint_slice(
    profile: &Profile,
    slice: &Slice,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    content_left: f32,
    palette: &Palette,
    window: &mut Window,
    cx: &mut App,
) {
    let rect = Bounds::new(point(px(content_left + x), px(y)), size(px(w), px(h)));
    let fill = palette.role_fill(slice.role);
    bviz_ui_components::paint_labeled_box(
        rect,
        fill,
        profile.str(slice.name),
        bviz_ui_theme::label_on(fill),
        palette.label_box(),
        window,
        cx,
    );
}

/// How the configuration a profile names by `checksum` reads: by the name
/// `session` knows it under, and by the checksum itself where the profile is
/// part of no session or the session knows no configuration of that checksum.
fn configuration_label<'a>(checksum: &'a str, session: Option<&Session>) -> Cow<'a, str> {
    session
        .and_then(|session| session.configurations().name(checksum))
        .map_or(Cow::Borrowed(checksum), Cow::Owned)
}

/// The lines of the tooltip for a hovered slice: its name, its category and
/// duration, and its args.
fn slice_tooltip(
    hovered: &HoveredSlice,
    session: Option<&Session>,
    repos: &RepoNames,
) -> Vec<String> {
    let (profile, slice, detail) = (hovered.profile(), hovered.slice(), hovered.detail());
    let category = profile.str(detail.category);
    let took = format_duration(slice.dur_us);
    let timing = match hovered.self_us() {
        Some(self_us) => format!("{category} · {took} · self {}", format_duration(self_us)),
        None => format!("{category} · {took}"),
    };
    let mut lines = vec![profile.str(slice.name).to_string(), timing];
    if let Some(target) = profile.opt_str(detail.target) {
        let (shown, written) = repos.shown_with_written(target);
        lines.push(shown);
        lines.extend(written);
    }
    let configuration = (profile.opt_str(detail.configuration))
        .map(|checksum| configuration_label(checksum, session).to_string());
    let ran: Vec<String> = [
        profile.opt_str(detail.mnemonic).map(str::to_owned),
        configuration,
    ]
    .into_iter()
    .flatten()
    .collect();
    if !ran.is_empty() {
        lines.push(ran.join(" · "));
    }
    for (key, value) in profile.slice_others(detail) {
        lines.push(format!("{} {}", profile.str(*key), profile.str(*value)));
    }
    lines
}

/// A profile being decoded and parsed.
pub struct Load {
    profile: Pending<Recorded<Profile>>,
    title: SharedString,
}

/// The invocation the profile `input` holds names itself as written by, read
/// from the input's head alone.
pub fn invocation_id(input: &bviz_input::Input) -> Option<InvocationId> {
    bviz_trace_profile::probe_invocation_id(input)
}

impl From<bviz_input::Input> for Load {
    fn from(input: bviz_input::Input) -> Self {
        let title = document_label(std::path::Path::new(input.name()));
        Self {
            profile: Pending::start(move || Profile::from_input(input).map_err(|e| e.to_string())),
            title,
        }
    }
}

impl From<TraceSource> for Load {
    fn from(source: TraceSource) -> Self {
        let title = document_label(&source.path);
        Self {
            profile: Pending::start(move || {
                Profile::from_path(&source.path).map_err(|e| e.to_string())
            }),
            title,
        }
    }
}

/// The views rendering the profile `load` reads: the timeline, the flame graph
/// and stats that aggregate whatever tracks it has selected, and the critical
/// path. Where it is opened as part of an invocation's `session`,
/// it names what the profile holds an id of by what the session knows, and
/// plays on `clock` where its document plays back.
pub fn open(
    load: impl Into<Load>,
    session: Option<Entity<Session>>,
    clock: Option<DocumentClock>,
    _window: &mut Window,
    cx: &mut App,
) -> Member {
    let load = load.into();
    let label = load.title;
    let model = cx.new(|cx| TraceModel::reading(load.profile, session, clock, cx));
    let timeline = ViewSpec::new({
        let model = model.clone();
        move |cx| cx.new(|cx| TracePanel::new(model.clone(), cx))
    });
    let flame = ViewSpec::new({
        let model = model.clone();
        move |cx| cx.new(|cx| FlamePanel::new(model.clone(), cx))
    });
    let stats = ViewSpec::new({
        let model = model.clone();
        move |cx| cx.new(|cx| StatsPanel::new(model.clone(), cx))
    });
    let critical_path =
        ViewSpec::new(move |cx| cx.new(|cx| CriticalPathPanel::new(model.clone(), cx)));
    Member::new(label, timeline, vec![flame, stats, critical_path])
}

/// Renders a Bazel `--profile` trace as a flame-chart timeline. Each profile
/// given opens a timeline of its own.
#[derive(Parser)]
#[command(name = "trace", version, about)]
struct Cli {
    #[command(flatten)]
    trace: TraceArgs,
}

/// Parses `--profile` and opens the trace-viewer window.
pub fn run() {
    bviz_ui_workspace::run_viewer(
        "bviz trace",
        bviz_ui_workspace::WINDOW_SIZE,
        |cli: Cli| cli.trace.sources().map(Load::from),
        &format!("--{PROFILE_LONG}"),
        |load, window, cx| open(load, None, None, window, cx).into(),
        &[register_keys],
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use bviz_invocation::Configurations;
    use bviz_ui_components::gesture::LINE_SCROLL_PX;

    #[test]
    fn every_divider_gesture_is_reachable() {
        assert_eq!(DIVIDER_GESTURES.unreachable(), []);
    }

    #[test]
    fn every_divider_gesture_fits_the_keys_help() {
        assert_eq!(DIVIDER_GESTURES.overlong(), []);
    }

    /// A session told of a configuration per `(checksum, mnemonic)`.
    pub(crate) fn session_knowing(described: &[(&str, &str)]) -> Session {
        let mut configurations = Configurations::default();
        for (checksum, mnemonic) in described {
            configurations.insert((*checksum).into(), (*mnemonic).into());
        }
        let mut session = Session::default();
        session.add_configurations(configurations);
        session
    }

    /// A session of two configurations sharing a mnemonic and one alone in its
    /// own.
    fn session() -> Session {
        session_knowing(&[
            ("55817e2200e2", "k8-fastbuild"),
            ("91e24984663c", "k8-fastbuild"),
            ("0c3d9a7714be", "k8-opt-exec"),
        ])
    }

    #[test]
    fn a_configuration_the_session_knows_reads_by_its_name() {
        let session = session();
        let label = |checksum| configuration_label(checksum, Some(&session));
        assert_eq!(label("0c3d9a7714be"), "k8-opt-exec");
        assert_eq!(label("55817e2200e2"), "k8-fastbuild 55817e");
    }

    /// A profile names configurations the build event stream may not describe —
    /// one it was not written with, or a word that is no checksum at all — and
    /// a session knows none until its stream is read.
    #[test]
    fn a_configuration_the_session_does_not_know_reads_by_its_checksum() {
        let session = session();
        let label = |checksum| configuration_label(checksum, Some(&session));
        assert_eq!(label("ffffffffffff"), "ffffffffffff");
        assert_eq!(label("system"), "system");
        let unread = Session::default();
        assert_eq!(
            configuration_label("55817e2200e2", Some(&unread)),
            "55817e2200e2"
        );
    }

    #[test]
    fn a_profile_in_no_session_reads_its_configurations_by_checksum() {
        assert_eq!(configuration_label("55817e2200e2", None), "55817e2200e2");
    }

    #[test]
    fn an_input_names_the_invocation_its_profile_was_written_by() {
        let dir = std::env::var_os("TEST_TMPDIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let source = |name: &str, json: &str| {
            let path = std::path::Path::new(&dir).join(name);
            std::fs::write(&path, json).unwrap();
            bviz_input::Input::open(&path).expect("the profile opens")
        };
        let named = source(
            "named.json",
            r#"{"otherData":{"build_id":"a5f7f3e6-4fda"},"traceEvents":[]}"#,
        );
        assert_eq!(invocation_id(&named), InvocationId::new("a5f7f3e6-4fda"));
        let unnamed = source("unnamed.json", r#"{"traceEvents":[]}"#);
        assert_eq!(invocation_id(&unnamed), None);
    }

    /// A window tall enough that the strip stands at the height it is opened to,
    /// split for a profile with `n` counters and lanes short enough that the
    /// flame region never asks for a bar of its own.
    fn counters_split(n: usize, strip: CountersStrip) -> Regions {
        let window = Bounds::new(point(px(0.0), px(0.0)), size(px(1000.0), px(800.0)));
        Regions::new(
            window,
            n,
            strip,
            DEFAULT_GUTTER_WIDTH,
            LaneHeights::default(),
        )
    }

    /// The counters strip's bar for a split scrolled to its top.
    fn counter_bar(regions: Regions) -> Option<Scrollbar> {
        let span = TimeSpan {
            start_us: 0,
            end_us: 1_000,
        };
        let tv = TimeView {
            start_us: 0.0,
            px_per_us: 1.0,
        };
        scrollbar_geom(span, tv, 0.0, regions, 0.0, 0.0).counters
    }

    #[test]
    fn clamp_gutter_width_bounds_to_half_window() {
        // A comfortable request in a wide window is left untouched.
        assert_eq!(clamp_gutter_width(170.0, 1280.0), 170.0);
        // Never narrower than the legibility floor.
        assert_eq!(clamp_gutter_width(10.0, 1280.0), MIN_GUTTER);
        // Never wider than half the window, so the time axis keeps the majority.
        assert_eq!(clamp_gutter_width(900.0, 1000.0), 500.0);
        // In a tiny window the floor wins over the half-window ceiling.
        assert_eq!(clamp_gutter_width(120.0, 100.0), MIN_GUTTER);
    }

    /// gpui's `Bounds::contains` documents inclusive edges and implements
    /// half-open ones; the hover regions are built on the implementation, so a
    /// pin that resolved the contradiction the other way would move which pixel
    /// column and row show a tooltip.
    #[test]
    fn the_flame_region_ends_left_of_the_bar_column() {
        let window = Bounds::new(point(px(0.0), px(0.0)), size(px(1000.0), px(600.0)));
        // Lanes taller than any region this window can hold, so the split
        // reserves a column for the vertical bar.
        let lanes = LaneHeights {
            pinned: 0.0,
            scrolled: 2.0 * f32::from(window.size.height),
        };
        let regions = Regions::new(
            window,
            0,
            CountersStrip::Open(DEFAULT_COUNTERS_HEIGHT),
            DEFAULT_GUTTER_WIDTH,
            lanes,
        );
        let right = regions.content_left + regions.time_w;
        assert_eq!(right, regions.content_right - SCROLLBAR_THICKNESS);

        let at = |x, y| point(px(x), px(y));
        // The gutter's edge, the ruler's bottom, and the last column left of the
        // bar lie inside the region.
        assert!(regions.in_flame(at(regions.content_left, regions.flame_top)));
        assert!(regions.in_flame(at(right - 1.0, regions.flame_bottom - 1.0)));
        // The bar's own column and the row below the lanes lie past it.
        assert!(!regions.in_flame(at(right, regions.flame_top)));
        assert!(!regions.in_flame(at(regions.content_right - 1.0, regions.flame_top)));
        assert!(!regions.in_flame(at(regions.content_left, regions.flame_bottom)));
        // Clear of the region on either axis: in the gutter, and in the ruler.
        assert!(!regions.in_flame(at(regions.content_left - 1.0, regions.flame_top)));
        assert!(!regions.in_flame(at(right - 1.0, regions.flame_top - 1.0)));
    }

    /// The time bar lies along the bottom edge of the flame lanes, inside the
    /// region it pans, with its far edge on the divider.
    #[test]
    fn the_time_bar_lies_along_the_bottom_of_the_flame_lanes() {
        let regions = counters_split(3, CountersStrip::Open(DEFAULT_COUNTERS_HEIGHT));
        let span = TimeSpan {
            start_us: 0,
            end_us: 1_000_000,
        };
        // Zoomed well past the width the span fits, so there is time to pan to.
        let tv = TimeView {
            start_us: 0.0,
            px_per_us: 2.0 * zoom_floor(span, regions.time_w),
        };
        let track = scrollbar_geom(span, tv, 0.0, regions, 0.0, 0.0)
            .time
            .expect("a zoomed view has time to pan to")
            .track();
        assert_eq!(track.size.height, px(SCROLLBAR_THICKNESS));
        assert_eq!(track.origin.y + track.size.height, px(regions.flame_bottom));
        assert!(regions.in_flame(track.origin));
    }

    /// The lane bar scrolls only the lanes below the pinned row, so it spans
    /// their band and measures their stack alone.
    #[test]
    fn the_lane_bar_spans_the_scrolled_lanes_below_the_pinned_row() {
        let window = Bounds::new(point(px(0.0), px(0.0)), size(px(1000.0), px(600.0)));
        let lanes = LaneHeights {
            pinned: 30.0,
            scrolled: 2.0 * f32::from(window.size.height),
        };
        let regions = Regions::new(
            window,
            0,
            CountersStrip::Open(DEFAULT_COUNTERS_HEIGHT),
            DEFAULT_GUTTER_WIDTH,
            lanes,
        );
        let span = TimeSpan {
            start_us: 0,
            end_us: 1_000,
        };
        let tv = TimeView::fit(span, regions.time_w, EDGE_MARGIN_PX);
        let bar = scrollbar_geom(span, tv, lanes.scrolled, regions, 0.0, 0.0)
            .flame
            .expect("lanes overflowing their band");
        let band = regions.scrolled_band(0.0);
        assert_eq!(bar.track().origin.y, px(regions.flame_top + lanes.pinned));
        assert_eq!(bar.track().origin.y, px(band.top));
        assert_eq!(bar.track().size.height, px(band.height));
    }

    /// A view sitting on the zoom floor holds the whole span in sight, so there
    /// is nothing to pan to and no time bar.
    #[test]
    fn a_view_at_the_zoom_floor_has_no_time_bar() {
        let regions = counters_split(3, CountersStrip::Open(DEFAULT_COUNTERS_HEIGHT));
        let span = TimeSpan {
            start_us: 0,
            end_us: 1_000_000,
        };
        let tv = TimeView::fit(span, regions.time_w, EDGE_MARGIN_PX);
        assert!(
            scrollbar_geom(span, tv, 0.0, regions, 0.0, 0.0)
                .time
                .is_none()
        );
    }

    /// The divider stands for as long as there are counters to resize, shut or
    /// open; a profile with none has neither strip nor divider.
    #[test]
    fn only_a_profile_with_counters_has_a_divider() {
        let window = Bounds::new(point(px(0.0), px(0.0)), size(px(1000.0), px(600.0)));
        let split = |n, strip| {
            Regions::new(
                window,
                n,
                strip,
                DEFAULT_GUTTER_WIDTH,
                LaneHeights::default(),
            )
        };

        let none = split(0, CountersStrip::Open(DEFAULT_COUNTERS_HEIGHT));
        assert_eq!(none.divider_rect().size.height, px(0.0));
        assert_eq!(none.counters_h(), 0.0);

        let shut = split(3, CountersStrip::Collapsed { restore: 220.0 });
        assert!(!shut.shows_counters());
        assert_eq!(
            shut.divider_rect().size.height,
            px(COUNTERS_COLLAPSED_HEIGHT)
        );
        // The band spans the content column, so it is there to be pressed
        // wherever the cursor crosses it right of the gutter.
        assert_eq!(
            shut.divider_rect().size.width,
            window.size.width - px(DEFAULT_GUTTER_WIDTH)
        );
        // An open strip leaves a band sized for its line alone, and the flame
        // region gives up the room the strip takes.
        let open = split(3, CountersStrip::Open(DEFAULT_COUNTERS_HEIGHT));
        assert_eq!(open.divider_rect().size.height, px(COUNTERS_DIVIDER_HEIGHT));
        assert!(shut.flame_bottom > open.flame_bottom);
    }

    /// Lanes with room to spare share the strip evenly, each a header band over
    /// its chart.
    #[test]
    fn counter_lanes_share_a_roomy_strip_evenly() {
        let n = 2;
        let strip = 4.0 * COUNTER_LANE_HEIGHT;
        let regions = counters_split(n, CountersStrip::Open(strip));
        let lanes: Vec<_> = regions.counter_lanes(0.0).collect();
        assert_eq!(lanes.len(), n);
        let lane_h = strip / n as f32;
        for (i, lane) in lanes.iter().enumerate() {
            assert_eq!(lane.chart_h, lane_h - METRICS.header_height);
            assert_eq!(lane.top, regions.counters_top + i as f32 * lane_h);
        }
    }

    #[test]
    fn a_scroll_request_moving_nothing_leaves_the_held_position_standing() {
        let ceiling = 400.0;
        // A position the content has shrunk away from, drawn at the ceiling.
        let held = 900.0;
        assert_eq!(drawn_scroll(held, ceiling), ceiling);
        assert_eq!(
            scroll_request(held, ceiling, ceiling + LINE_SCROLL_PX),
            None
        );
        assert_eq!(scroll_request(held, ceiling, ceiling), None);
        // It draws where it stands again once the content grows back past it.
        assert_eq!(drawn_scroll(held, 1000.0), held);
    }

    #[test]
    fn a_scroll_request_that_moves_reports_its_bounded_target() {
        let ceiling = 400.0;
        assert_eq!(scroll_request(900.0, ceiling, 250.0), Some(250.0));
        assert_eq!(
            scroll_request(100.0, ceiling, 100.0 + LINE_SCROLL_PX),
            Some(100.0 + LINE_SCROLL_PX)
        );
        // Past either end the gesture lands on the bound rather than being refused.
        assert_eq!(scroll_request(100.0, ceiling, 5000.0), Some(ceiling));
        assert_eq!(scroll_request(100.0, ceiling, -20.0), Some(0.0));
    }

    #[test]
    fn a_scroll_request_holds_at_the_floor_and_the_ceiling() {
        let ceiling = 400.0;
        assert_eq!(scroll_request(0.0, ceiling, -LINE_SCROLL_PX), None);
        assert_eq!(
            scroll_request(ceiling, ceiling, ceiling + LINE_SCROLL_PX),
            None
        );
        // Content shorter than its viewport pins every gesture to the top.
        assert_eq!(scroll_request(0.0, 0.0, LINE_SCROLL_PX), None);
    }

    #[test]
    fn a_marker_label_stands_right_then_left_of_its_line_then_cut_short_or_left_out() {
        let label = |left, width| Some(MarkerLabel { left, width });
        // Room up to the next line: right of the line.
        assert_eq!(
            place_marker_labels(&[(0.0, 50.0), (100.0, 50.0)], 2.0, 0.0, 500.0),
            [label(2.0, 50.0), label(102.0, 50.0)]
        );
        // Too little room right but clear room left: left of the line.
        assert_eq!(
            place_marker_labels(&[(100.0, 50.0), (110.0, 50.0)], 2.0, 0.0, 500.0),
            [label(48.0, 50.0), label(112.0, 50.0)]
        );
        // Neither: cut short to the room right, or left out with too little.
        assert_eq!(
            place_marker_labels(&[(0.0, 50.0), (40.0, 80.0), (45.0, 50.0)], 2.0, 0.0, 500.0),
            [label(2.0, 36.0), None, label(47.0, 50.0)]
        );
        // The last line's room runs to the right limit.
        assert_eq!(
            place_marker_labels(&[(490.0, 50.0)], 2.0, 0.0, 500.0),
            [label(438.0, 50.0)]
        );
    }

    const BUILD_US: i64 = 40_000_000;

    fn selection(regions: &[(i64, i64)]) -> TimeSelection {
        let mut selection = TimeSelection::default();
        for &(start_us, end_us) in regions {
            selection.insert(
                bviz_trace_layout::span_between(start_us, end_us)
                    .unwrap_or_else(|| panic!("({start_us}, {end_us}) spans no time")),
            );
        }
        selection
    }

    #[test]
    fn a_one_region_window_reads_as_a_share_of_the_build() {
        assert_eq!(
            time_window_summary(Some(&selection(&[(0, 2_500_000)])), BUILD_US).as_deref(),
            Some("window: 2.50s of 40.00s")
        );
    }

    #[test]
    fn several_regions_are_counted_alongside_the_time_they_hold() {
        let three = selection(&[
            (0, 1_000_000),
            (2_000_000, 3_000_000),
            (4_000_000, 5_000_000),
        ]);
        assert_eq!(
            time_window_summary(Some(&three), BUILD_US).as_deref(),
            Some("window: 3.00s of 40.00s · 3 regions")
        );
    }

    #[test]
    fn a_window_far_shorter_than_the_build_keeps_its_own_unit() {
        assert_eq!(
            time_window_summary(Some(&selection(&[(0, 4_200)])), BUILD_US).as_deref(),
            Some("window: 4.20ms of 40.00s")
        );
    }

    #[test]
    fn an_unrestricted_aggregate_carries_no_chip() {
        assert_eq!(
            time_window_summary(Some(&TimeSelection::default()), BUILD_US),
            None
        );
    }

    #[test]
    fn no_chip_stands_for_figures_that_are_not_on_screen_yet() {
        assert_eq!(time_window_summary(None, BUILD_US), None);
    }

    /// A strip with more lanes than it can hold shows a bar over its own band,
    /// in the column reserved at the content's right edge; one whose lanes fit
    /// shows none, and reserves no column for one.
    #[test]
    fn the_counters_bar_stands_only_while_the_lanes_overflow() {
        let n = 10;
        let overflowing = counters_split(n, CountersStrip::Open(DEFAULT_COUNTERS_HEIGHT));
        assert!(overflowing.max_counter_scroll() > 0.0);
        let bar = counter_bar(overflowing).expect("the overflowing lanes are scrolled by a bar");
        assert_eq!(bar.track().origin.y, px(overflowing.counters_top));
        assert_eq!(bar.track().size.height, px(overflowing.counters_h()));
        assert_eq!(
            bar.track().origin.x,
            px(overflowing.content_right - SCROLLBAR_THICKNESS)
        );
        // The plots end left of the bar rather than running under it.
        assert_eq!(
            overflowing.time_right,
            overflowing.content_right - SCROLLBAR_THICKNESS
        );

        let roomy = counters_split(n, CountersStrip::Open(n as f32 * COUNTER_LANE_HEIGHT));
        assert_eq!(roomy.max_counter_scroll(), 0.0);
        assert!(counter_bar(roomy).is_none());
        assert_eq!(roomy.time_right, roomy.content_right);
    }
}
