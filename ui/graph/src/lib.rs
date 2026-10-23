//! Loads a Bazel `aquery`, `cquery`, or `query` `--output=streamed_proto` file
//! through `bviz_graph_layout`, lays the graph out, and opens a window
//! rendering the result.

use bviz_graph_layout::Id;
use bviz_invocation::Session;
use bviz_ui_components::gesture::{
    GestureRow, Pointer, pinch_factor, scroll_pixels, wheel_zoom_factor,
};
use bviz_ui_components::{
    DragEnd, Dropdown, DropdownEvent, Host, KeyScope, LegendState, MultiSelect, MultiSelectEvent,
    Pending, ScrollbarColors, ShowLegend, ZOOM_KEY_STEP, ZoomIn, ZoomOut, cached_layer,
    declare_scopes, find_key_bindings, legend_gestures, legend_key_bindings, overlay_canvas,
    text_input, zoom_key_bindings,
};
use bviz_ui_graph_args::{AQUERY_LONG, CQUERY_LONG, GraphArgs, GraphSource, QUERY_LONG};
use bviz_ui_playback::Clock;
use bviz_ui_session::Sharing;
use bviz_ui_theme::{DataColors, Theme, ThemeColors};
use bviz_ui_workspace::{
    Document, Member, Panel, PanelEvent, PanelKind, Role, Shared, Titled, ViewKind, ViewSpec,
    document_label,
};
use clap::Parser;
use gpui::{
    AnyElement, AnyView, App, Bounds, Context, Entity, EventEmitter, FocusHandle, Focusable,
    IntoElement, KeyBinding, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    NavigationDirection, PinchEvent, Pixels, Point, Render, Rgba, ScrollWheelEvent, SharedString,
    StyleRefinement, Subscription, WeakEntity, Window, actions, canvas, div, prelude::*,
};
use std::time::Duration;

mod actions;
mod box_menu;
mod boxes;
mod build;
mod camera;
mod chips;
mod colour;
mod column;
mod critical;
mod derive;
mod drag;
mod duration;
mod emphasis;
mod exclusive;
mod filter;
mod framing;
mod fuzzy;
mod hidden;
mod hints;
mod history;
mod holder;
mod hover;
mod hue;
mod jump;
mod legend;
mod missing;
mod mode;
mod model;
mod morph;
mod neighborhood;
mod paint;
mod pan_bench;
mod pins;
mod playback;
mod point;
mod relation;
mod report;
mod reveal;
mod rules;
mod scatter;
mod scroll;
mod selection;
mod spatial;
mod stand_in;
mod status;
mod timecard;
mod timing;
mod toolbar;
mod unit_picker;
mod why;

use camera::{Camera, Rect, from_screen, recenter_rect, scale_about};
use colour::Colouring;
use derive::{Derived, Drawn, Expandable, LayoutLanded, ViewKey};
use drag::{CanvasIntent, Drag, GESTURES, end_drag, pan_by, start_pan, track_gesture};
use emphasis::Emphasis;
pub use exclusive::ExclusivePanel;
use framing::{Framing, Target, build_prepaint};
use legend::Legend;
use mode::LayoutMode;
use model::{
    ExclusionNamed, FilterChanged, GraphModel, MapPlaced, QueryApplied, Reveal, RevealRequested,
    SelectionChanged, SourceReplaced, VerdictsLanded,
};
use morph::{Frame, Morph};
use neighborhood::MarkingMemo;
use paint::{Scene, TitleTarget, canvas_body};
use relation::Relation;
use rules::BoxClass;
pub use scatter::ScatterPanel;
use scroll::{scrollbar_hit, start_scroll_drag};
use selection::Selection;
use spatial::{Indexed, SpatialMemo};

actions!(
    bviz_graph,
    [
        /// Toggle neighbour highlighting
        ToggleNeighborhood,
        /// Toggle transitive reduction
        ToggleReduce,
        /// Toggle map layout
        ToggleMap,
        /// Cycle colouring
        CycleColouring,
        /// Toggle critical path
        ToggleCriticalPath,
        /// Toggle build outcomes
        ToggleOutcomes,
        /// Toggle action timing
        ToggleTiming,
        /// Dismiss card, selection or paths
        Dismiss,
        /// Go back
        JumpBack,
        /// Go forward
        JumpForward,
        /// Show whole graph
        ShowWholeGraph,
        /// Open box menu
        OpenBoxMenu,
    ]
);

/// The key context scoping the graph's own bindings, so a key pressed here
/// reaches this panel and no other.
const KEY_CONTEXT: KeyScope = KeyScope::new("Graph", "Graph");

pub fn register_keys(cx: &mut App) {
    declare_scopes(
        cx,
        [
            KEY_CONTEXT,
            exclusive::KEY_CONTEXT,
            exclusive::ROOTS_KEY_CONTEXT,
            exclusive::OFFERING,
            scatter::KEY_CONTEXT,
            scatter::PICKING,
            scatter::OFFERING,
            jump::KEY_CONTEXT,
            chips::KEY_CONTEXT,
            chips::FIELD_KEY_CONTEXT,
            chips::COMPLETION_KEY_CONTEXT,
            chips::AIMED_KEY_CONTEXT,
        ],
    );
    cx.bind_keys(bindings(Host::CURRENT));
}

/// The panels' bindings in an application running in `host`.
fn bindings(host: Host) -> Vec<KeyBinding> {
    let unless_typing = text_input::unless_typing(KEY_CONTEXT);
    let unless_typing = Some(unless_typing.as_str());
    let jumping = Some(jump::KEY_CONTEXT.id());
    let scatter_canvas = text_input::unless_typing(scatter::KEY_CONTEXT);
    let scatter_canvas = Some(scatter_canvas.as_str());
    // The list's keys stand down in the query bar, whose keys share them.
    let picking = format!(
        "{} && {} && !{}",
        scatter::KEY_CONTEXT,
        scatter::PICKING,
        chips::KEY_CONTEXT
    );
    let picking = Some(picking.as_str());
    let scatter_unpicked = format!(
        "{} && !{}",
        text_input::unless_typing(scatter::KEY_CONTEXT),
        scatter::PICKING
    );
    let scatter_unpicked = Some(scatter_unpicked.as_str());
    let scatter_offering = format!(
        "{} && {} && !{}",
        scatter::KEY_CONTEXT,
        scatter::OFFERING,
        text_input::KEY_CONTEXT
    );
    let scatter_offering = Some(scatter_offering.as_str());
    let exclusive_canvas = text_input::unless_typing(exclusive::KEY_CONTEXT);
    let exclusive_canvas = Some(exclusive_canvas.as_str());
    let exclusive_offering = format!(
        "{} && {} && !{}",
        exclusive::KEY_CONTEXT,
        exclusive::OFFERING,
        text_input::KEY_CONTEXT
    );
    let exclusive_offering = Some(exclusive_offering.as_str());
    let mut bindings = vec![
        KeyBinding::new("h", ToggleNeighborhood, unless_typing),
        KeyBinding::new("r", ToggleReduce, unless_typing),
        KeyBinding::new("m", ToggleMap, unless_typing),
        KeyBinding::new("c", CycleColouring, unless_typing),
        KeyBinding::new("p", ToggleCriticalPath, unless_typing),
        KeyBinding::new("o", ToggleOutcomes, unless_typing),
        KeyBinding::new("t", ToggleTiming, unless_typing),
        KeyBinding::new("escape", Dismiss, unless_typing),
        KeyBinding::new("0", ShowWholeGraph, unless_typing),
        KeyBinding::new("shift-f10", OpenBoxMenu, unless_typing),
        KeyBinding::new("menu", OpenBoxMenu, unless_typing),
        KeyBinding::new("enter", reveal::Accept, unless_typing),
        KeyBinding::new("alt-left", JumpBack, unless_typing),
        KeyBinding::new("alt-right", JumpForward, unless_typing),
        KeyBinding::new("up", jump::Previous, jumping),
        KeyBinding::new("down", jump::Next, jumping),
        KeyBinding::new("enter", jump::Go, jumping),
        KeyBinding::new("secondary-enter", jump::AskWhy, jumping),
        KeyBinding::new("shift-enter", jump::AlwaysShow, jumping),
        KeyBinding::new("alt-enter", jump::AddChip, jumping),
        KeyBinding::new("escape", jump::Close, jumping),
        KeyBinding::new("tab", jump::NextScope, jumping),
        KeyBinding::new("shift-tab", jump::PreviousScope, jumping),
        KeyBinding::new("0", scatter::ShowWholePlot, scatter_canvas),
        KeyBinding::new("u", unit_picker::CyclePoints, scatter_canvas),
        KeyBinding::new("x", scatter::CycleAcross, scatter_canvas),
        KeyBinding::new("y", scatter::CycleUp, scatter_canvas),
        KeyBinding::new("c", CycleColouring, scatter_canvas),
        KeyBinding::new("0", exclusive::ShowWholeBuild, exclusive_canvas),
        KeyBinding::new("backspace", exclusive::FocusUp, exclusive_canvas),
        KeyBinding::new("left", exclusive::SelectPrevious, exclusive_canvas),
        KeyBinding::new("right", exclusive::SelectNext, exclusive_canvas),
        KeyBinding::new("up", exclusive::SelectParent, exclusive_canvas),
        KeyBinding::new("down", exclusive::SelectChild, exclusive_canvas),
        KeyBinding::new("escape", exclusive::Deselect, exclusive_canvas),
        KeyBinding::new("enter", exclusive::FocusSelected, exclusive_canvas),
        // After the binding it shares Enter with, which it outranks while a
        // card stands.
        KeyBinding::new("enter", reveal::Accept, exclusive_offering),
        KeyBinding::new("g", exclusive::ShowSelected, exclusive_canvas),
        KeyBinding::new("u", unit_picker::CyclePoints, exclusive_canvas),
        KeyBinding::new("alt-left", exclusive::FocusBack, exclusive_canvas),
        KeyBinding::new("alt-right", exclusive::FocusForward, exclusive_canvas),
        KeyBinding::new("r", exclusive::EditRoots, exclusive_canvas),
        KeyBinding::new(
            "escape",
            exclusive::CloseRoots,
            Some(exclusive::ROOTS_KEY_CONTEXT.id()),
        ),
        KeyBinding::new(
            "enter",
            exclusive::CloseRoots,
            Some(exclusive::ROOTS_KEY_CONTEXT.id()),
        ),
        KeyBinding::new("escape", scatter::DismissCard, scatter_unpicked),
        KeyBinding::new("g", scatter::ShowSelectedPoint, scatter_unpicked),
        KeyBinding::new("alt-left", JumpBack, scatter_canvas),
        KeyBinding::new("alt-right", JumpForward, scatter_canvas),
        KeyBinding::new("enter", reveal::Accept, scatter_offering),
        KeyBinding::new("up", scatter::AimUp, picking),
        KeyBinding::new("down", scatter::AimDown, picking),
        KeyBinding::new("enter", scatter::SelectAimed, picking),
        KeyBinding::new("escape", scatter::ClosePickList, picking),
    ];
    if host == Host::Browser {
        bindings.extend([
            KeyBinding::new("secondary-k [", JumpBack, unless_typing),
            KeyBinding::new("secondary-k ]", JumpForward, unless_typing),
            KeyBinding::new("secondary-k [", exclusive::FocusBack, exclusive_canvas),
            KeyBinding::new("secondary-k ]", exclusive::FocusForward, exclusive_canvas),
            KeyBinding::new("secondary-k [", JumpBack, scatter_canvas),
            KeyBinding::new("secondary-k ]", JumpForward, scatter_canvas),
        ]);
    }
    bindings.extend(chips::bindings());
    bindings.extend(zoom_key_bindings(KEY_CONTEXT));
    bindings.extend(zoom_key_bindings(scatter::KEY_CONTEXT));
    bindings.extend(legend_key_bindings(KEY_CONTEXT));
    bindings.extend(legend_key_bindings(scatter::KEY_CONTEXT));
    bindings.extend(legend_key_bindings(exclusive::KEY_CONTEXT));
    bindings.extend(find_key_bindings(KEY_CONTEXT, jump::Open));
    bindings.extend(find_key_bindings(scatter::KEY_CONTEXT, jump::Open));
    bindings.extend(find_key_bindings(exclusive::KEY_CONTEXT, jump::Open));
    bindings
}

/// Padding around the graph, in layout pixels.
const MARGIN: f64 = 16.0;

/// The share of the canvas a card floating over it may span. The cards sit at
/// opposite corners, so a share leaving a gap between two of them keeps either
/// from covering the other however narrow the canvas.
const CARD_MAX_SHARE: f32 = 0.45;

/// Largest zoom-in factor. The smallest is computed per graph so the whole graph
/// always fits the viewport.
const MAX_ZOOM: f32 = 4.0;

/// The most a frame request zooms in to show what it names, at which a label
/// reads at a comfortable size however small the box framed.
const MAX_FRAME_ZOOM: f32 = 1.5;

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
    /// Fill of a container box.
    container: Rgba,
    /// Fill of the outermost package box, which stands one step further off the
    /// canvas than `container` so nesting reads outermost first.
    package: Rgba,
    background: Rgba,
    /// Accent every emphasis is drawn in: the border picking out the selected
    /// box and the just-toggled one, and the wash over a marked box's fill.
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

/// A view of a Bazel build graph, drawn under a layout mode and camera of its
/// own.
pub struct GraphPanel {
    focus_handle: FocusHandle,
    /// The graph drawn here, shared with every other view of it.
    model: Entity<GraphModel>,
    /// The part of that graph this panel draws, and the state it draws it at.
    derived: Derived,
    framing: Framing,
    drag: Drag,
    /// Whether the last press on the canvas was the first of its run and
    /// landed clear of every box, which a double click showing the whole graph
    /// must begin with: a first click on a box opens or closes it, and the
    /// boxes moving under the second may leave it on empty space.
    pressed_clear: bool,
    emphasis: Emphasis,
    morph: Morph,
    mode: LayoutMode,
    layout_picker: Entity<Dropdown>,
    /// Carries the picker's selections into [`GraphPanel::set_mode`].
    _layout_choices: Subscription,
    colouring: Colouring,
    colour_picker: Entity<Dropdown>,
    /// Carries the picker's selections into [`colour::set`].
    _colour_choices: Subscription,
    /// The box classes the loaded graph carries, in the order the boxes menu's
    /// rows stand.
    box_classes: Vec<BoxClass>,
    boxes: Entity<MultiSelect>,
    /// Carries a toggled boxes-menu row into the box classes the panel unwraps.
    _box_choices: Subscription,
    /// The query bar, whose chips scope what the document's views draw.
    chips: Entity<chips::ChipBar>,
    /// Puts away the paths drawn in the filter's place as the panel's own bar
    /// applies a query.
    _query_applied: Subscription,
    jump: Entity<jump::JumpPalette>,
    jump_opened_at: jump::graph::OpenedAt,
    /// Lands what the palette picks.
    _jumped: Subscription,
    /// Redraws as the graph is read.
    _changed: Subscription,
    /// Repaints the drawing, and only it, as a gesture moves the document's
    /// clock while the boxes are coloured by playback.
    _clock_moved: Subscription,
    /// Derives what the selected mode draws as the read lands: a derivation
    /// asked for while the graph was still being read could not start.
    _source_replaced: Subscription,
    /// Derives the levels of the panel's restriction as the map lands.
    _map_placed: Subscription,
    /// Draws the graph through the document's filter as it changes.
    _filter_changed: Subscription,
    /// Counts what the document's filter shows for the query bar's status
    /// line as its verdicts land.
    _verdicts_landed: Subscription,
    /// Names on the hidden card the chip leaving its node out.
    _exclusion_named: Subscription,
    /// Marks a node another view selects, leaving the camera where it is.
    _selection_changed: Subscription,
    /// Whether the workspace shows the panel.
    shown: bool,
    _reveal_requested: Subscription,
    /// The box whose neighborhood a hover marks: the box under the cursor,
    /// or the one the jump palette previews.
    hovered: Option<Id>,
    box_card: hover::BoxCard,
    hover_marking: MarkingMemo,
    spatial: SpatialMemo,
    /// The box whose related nodes stay marked, and how far that marking reaches;
    /// set by a secondary click, which puts up the box's menu.
    selection: Selection,
    box_menu: Option<box_menu::BoxMenu>,
    /// Whether hovering a box marks its neighborhood; off until the `h` key or
    /// the toolbar toggle turns it on.
    neighborhood_enabled: bool,
    /// Whether the boxes standing for the invocation's critical path wear its
    /// halo.
    critical_path_enabled: bool,
    missing: missing::Missing,
    /// How the card listing the overrides in force is scrolled.
    pins: pins::Pins,
    reveal: reveal::Reveal,
    jumps: reveal::Jumps,
    report: report::Card,
    timecard: timecard::Card,
    /// The dependency paths asked for, which the panel draws in place of its
    /// filter.
    why: why::Why,
    /// The titles the canvas drew, rewritten by every paint and resolved against
    /// a pointer by [`box_at`].
    titles: Vec<TitleTarget>,
    /// The layer drawing the graph; notified only through `invalidate_canvas`
    /// and `invalidate_canvas_only`.
    canvas: Entity<GraphCanvas>,
    toolbar: Entity<ChromeLayer>,
    overlays: Entity<ChromeLayer>,
    _chrome_follows: Subscription,
    legend: Legend,
    legend_state: Entity<LegendState>,
    /// Taken by the first render, which starts it.
    bench: Option<pan_bench::Bench>,
}

impl GraphPanel {
    fn camera(&self) -> Camera {
        self.framing.camera()
    }

    /// The layout the selected mode draws, or `None` while it is being derived.
    fn drawn(&self) -> Option<Drawn> {
        self.derived.drawn(self.mode)
    }

    /// The zoom a frame resolves `drawn`'s view at. Where that is a map whose
    /// camera awaits its fit, the fit picks the level, so this is the zoom the
    /// fit gives, and `None` while the canvas it fits to is unmeasured.
    fn view_zoom(&self, drawn: &Drawn) -> Option<f32> {
        match drawn {
            Drawn::Map { .. } if !self.framing.fitted() => {
                self.framing.pending_fit(drawn.content())
            }
            _ => Some(self.camera().zoom),
        }
    }

    /// Tells the query bar how many points the document's filter shows, in
    /// the finest unit the graph holds, where the panel is shown and the
    /// filter narrows the graph, asking for the filter's verdicts where they
    /// have yet to land.
    fn restate_shown(&mut self, cx: &mut Context<Self>) {
        if !self.shown {
            return;
        }
        let shown = match self.model.read(cx).filter().narrows() {
            false => None,
            true => {
                self.model.update(cx, |model, cx| model.verdicts(cx));
                let model = self.model.read(cx);
                let finest = (model.loaded())
                    .and_then(|loaded| loaded.shared.inputs.inputs().last())
                    .map(point::finest_unit);
                finest.zip(model.judgement()).and_then(|(unit, judgement)| {
                    let counted = judgement.shown(point::unit_kinds(unit))?;
                    Some(chips::Shown {
                        shown: counted.shown,
                        of: counted.of,
                        many: point::unit_names(unit).many,
                        note: None,
                    })
                })
            }
        };
        self.chips.update(cx, |bar, cx| bar.set_shown(shown, cx));
    }

    /// The boxes the selected mode's layout lets the user open or close.
    fn expandable(&self) -> Expandable {
        self.derived.expandable(self.mode)
    }

    /// How far the drawing being eased onto its successor has travelled at this
    /// instant; `None` at rest, where every box sits at its own layout rect. The
    /// paint and every gesture that names a box place through this one frame, so
    /// what the cursor lands on is what is drawn under it.
    fn morph_frame(&self) -> Option<Frame> {
        self.morph.at(bviz_time::now())
    }

    fn new(model: Entity<GraphModel>, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        let mode = LayoutMode::default();
        let layout_picker = cx.new(|cx| {
            Dropdown::new(
                "layout-mode",
                LayoutMode::ALL.map(LayoutMode::label),
                mode.index(),
                cx,
            )
            .tooltip("Graph layout")
            .keys_for(&ToggleMap, &focus_handle)
        });
        // The picker notifies on every repaint it makes, and only a selection
        // should move the view off its layout.
        let layout_choices = cx.subscribe(&layout_picker, |view, _, event, cx| {
            let DropdownEvent::Changed(index) = event;
            if let Some(mode) = LayoutMode::at(*index) {
                view.set_mode(mode, cx);
            }
        });
        let colouring = Colouring::default();
        let offered = colour::Offered::by(model.read(cx));
        let hint = colour::hint(model.read(cx));
        let colour_picker = cx.new(|cx| {
            let mut picker = Dropdown::new(
                "colouring",
                Colouring::ALL.map(Colouring::label),
                colouring.index(),
                cx,
            )
            .tooltip(hint)
            .keys_for(&CycleColouring, &focus_handle);
            colour::offer(&mut picker, offered, cx);
            picker
        });
        let colour_choices = cx.subscribe(&colour_picker, |view, _, event, cx| {
            let DropdownEvent::Changed(index) = event;
            if let Some(colouring) = Colouring::at(*index) {
                colour::set(view, colouring, cx);
            }
        });
        let boxes = cx.new(|cx| {
            MultiSelect::new("graph-boxes", boxes::NAME, Vec::new(), cx).tooltip("Box types drawn")
        });
        let box_choices = cx.subscribe(&boxes, |view, _, event: &MultiSelectEvent, cx| {
            boxes::choose_boxes(view, event, cx);
        });
        let focus_handle = cx.focus_handle();
        let drawn = jump::graph::drawn_reader(cx);
        let chips = cx.new(|cx| {
            chips::ChipBar::new(
                model.clone(),
                holder::Holder::Graph,
                focus_handle.clone(),
                Some(drawn),
                cx,
            )
        });
        let jump = jump::graph::palette(model.clone(), focus_handle.clone(), chips.clone(), cx);
        let jumped = cx.subscribe(&jump, |view, _, event: &jump::JumpEvent, cx| {
            jump::graph::landed(view, *event, cx)
        });
        let query_applied = cx.subscribe(&model, |view, _, applied: &QueryApplied, cx| {
            if applied.by == view.chips.entity_id() {
                derive::show_filter(view, cx);
            }
        });
        let parent = cx.entity().downgrade();
        let canvas = cx.new(|_| GraphCanvas {
            parent: parent.clone(),
        });
        let toolbar = cx.new(|_| ChromeLayer {
            parent: parent.clone(),
            draw: GraphPanel::draw_toolbar,
        });
        let overlays = cx.new(|_| ChromeLayer {
            parent,
            draw: |view, palette, _, cx| view.draw_overlays(palette, cx),
        });
        let chrome_follows = cx.observe_self(|view, cx| {
            mode::sync(view, cx);
            view.toolbar.update(cx, |_, cx| cx.notify());
            view.overlays.update(cx, |_, cx| cx.notify());
            view.box_card.restate(cx);
        });
        let changed = cx.observe(&model, |view, _, cx| {
            colour::sync(view, cx);
            view.invalidate_canvas(cx);
        });
        let clock = model.read(cx).clock().clone();
        let clock_moved = cx.observe(&clock, |view, _, cx| {
            if view.colouring == Colouring::Playback {
                view.invalidate_canvas_only(cx);
            }
        });
        let source_replaced = cx.subscribe(&model, |view, _, _: &SourceReplaced, cx| {
            view.take_up_source(cx)
        });
        let map_placed = cx.subscribe(&model, |view, _, _: &MapPlaced, cx| {
            derive::placed(view, cx)
        });
        let filter_changed = cx.subscribe(&model, |view, _, event: &FilterChanged, cx| {
            derive::filter_changed(view, event.frame, cx);
            view.restate_shown(cx);
            cx.notify();
        });
        let verdicts_landed = cx.subscribe(&model, |view, _, _: &VerdictsLanded, cx| {
            view.restate_shown(cx)
        });
        let exclusion_named = cx.subscribe(&model, |_, _, _: &ExclusionNamed, cx| cx.notify());
        let selection_changed = cx.subscribe(&model, |view, _, event: &SelectionChanged, cx| {
            if event.by != cx.entity_id() {
                selection::remark(view, cx);
            }
        });
        let reveal_requested = cx.subscribe(&model, |view, _, event: &RevealRequested, cx| {
            if event.panel == cx.entity_id() {
                derive::catch_up(view, cx);
                match event.reveal {
                    Reveal::Activate => reveal::reveal(view, event.node, cx),
                    Reveal::InPlace => reveal::show_in_place(view, event.node, cx),
                    Reveal::AskWhy => {
                        why::ask_why(view, event.node, cx);
                        cx.emit(PanelEvent::Activate);
                    }
                }
            }
        });
        let derived = Derived::new();
        let legend_state = cx.new(|cx| LegendState::new(focus_handle.clone(), cx));
        let panel = cx.entity();
        model.update(cx, |model, cx| model.defer_to(&panel, cx));
        let mut view = Self {
            focus_handle,
            model,
            derived,
            framing: Framing::new(),
            drag: Drag::None,
            pressed_clear: false,
            emphasis: Emphasis::none(),
            morph: Morph::none(),
            mode,
            layout_picker,
            _layout_choices: layout_choices,
            colouring,
            colour_picker,
            _colour_choices: colour_choices,
            box_classes: Vec::new(),
            boxes,
            _box_choices: box_choices,
            chips,
            _query_applied: query_applied,
            jump,
            jump_opened_at: None,
            _jumped: jumped,
            _changed: changed,
            _clock_moved: clock_moved,
            _source_replaced: source_replaced,
            _map_placed: map_placed,
            _filter_changed: filter_changed,
            _verdicts_landed: verdicts_landed,
            _exclusion_named: exclusion_named,
            _selection_changed: selection_changed,
            shown: false,
            _reveal_requested: reveal_requested,
            hovered: None,
            box_card: hover::BoxCard::new(cx),
            hover_marking: MarkingMemo::default(),
            spatial: SpatialMemo::default(),
            selection: Selection::none(),
            box_menu: None,
            neighborhood_enabled: NEIGHBOURHOOD_AT_REST,
            critical_path_enabled: CRITICAL_PATH_AT_REST,
            missing: missing::Missing::new(),
            pins: pins::Pins::new(),
            reveal: reveal::Reveal::Idle,
            jumps: reveal::Jumps::default(),
            report: report::Card::new(),
            timecard: timecard::Card::new(),
            why: why::Why::new(),
            titles: Vec::new(),
            canvas,
            toolbar,
            overlays,
            _chrome_follows: chrome_follows,
            legend: Legend::default(),
            legend_state,
            bench: pan_bench::Bench::from_env(),
        };
        // A panel minted over a graph its document has already read, as a
        // closed view reopened is, sees no read land.
        view.take_up_source(cx);
        view
    }

    /// Draws the graph the document holds, once it is read, and offers the box
    /// classes it carries.
    fn take_up_source(&mut self, cx: &mut Context<Self>) {
        box_menu::close(self, cx);
        derive::opened(self, cx);
        boxes::adopt(self, cx);
    }

    /// Rebuilds the graph scene along with the chrome around it; every change to
    /// what the canvas draws routes through here or
    /// [`invalidate_canvas_only`](Self::invalidate_canvas_only), and a change
    /// that leaves the drawing alone notifies alone, so the scene replays from
    /// its cache.
    pub(crate) fn invalidate_canvas(&self, cx: &mut Context<Self>) {
        self.canvas.update(cx, |_, cx| cx.notify());
        cx.notify();
    }

    /// Rebuilds the graph scene alone, for a change nothing a [`ChromeLayer`]
    /// draws depends on.
    pub(crate) fn invalidate_canvas_only(&self, cx: &mut Context<Self>) {
        self.canvas.update(cx, |_, cx| cx.notify());
    }

    /// Moves the camera by `step`, repainting only if the camera changed, so a
    /// gesture the content's bounds hold in place draws no frame.
    pub(crate) fn steer(&mut self, step: impl FnOnce(&mut Framing), cx: &mut Context<Self>) {
        let before = self.camera();
        step(&mut self.framing);
        if self.camera() != before {
            self.invalidate_canvas_only(cx);
            self.drawing_moved(cx);
        }
    }

    /// Lets go of the box the card describes as the drawing moves under a
    /// pointer that did not move it, which need not rest on that box any more.
    /// A drag keeps it, as the drawing carries the box along with the pointer.
    pub(crate) fn drawing_moved(&mut self, cx: &mut Context<Self>) {
        if matches!(self.drag, Drag::None) {
            self.box_card.forget(cx);
        }
    }

    /// Brings this drawing onto a layered layout the graph has landed.
    ///
    /// A morph eases one layered drawing onto its successor, so it says nothing
    /// about a map's levels, which the camera alone selects between: armed while
    /// the map is on screen it would pump repaints of a canvas that draws none of
    /// it.
    pub(crate) fn settle_onto(&mut self, landed: &LayoutLanded, cx: &mut Context<Self>) {
        let replaced = landed
            .replaced
            .clone()
            .filter(|_| self.mode == LayoutMode::Layered);
        if let Some(replaced) = replaced {
            morph::begin(self, replaced, &landed.current, cx);
        }
        self.framing.request_recenter();
        emphasis::begin_fade(self, cx);
    }

    /// Frames the layer drawing the selected mode's layout, or a spinner while
    /// that layout is still being computed — the toolbar stays above either, so
    /// the picker remains reachable.
    fn viewport(
        &mut self,
        palette: Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let laying_out = || bviz_ui_components::centered_spinner("Laying out…", &palette.colors);
        let Some(drawn) = self.drawn() else {
            let entity = cx.entity();
            return laying_out()
                .relative()
                .child(overlay_canvas(move |bounds, _, cx| {
                    entity.update(cx, |view, _| view.framing.measure(bounds));
                }))
                .into_any_element();
        };
        let zoom = self.view_zoom(&drawn);
        let underived = zoom.is_some_and(|zoom| drawn.view(zoom).is_none());
        let entity = cx.entity();
        div()
            .id("graph-viewport")
            .relative()
            .size_full()
            .overflow_hidden()
            .child(cached_layer(self.canvas.clone()))
            .children(underived.then(|| div().absolute().size_full().flex().child(laying_out())))
            .children(self.derived.too_dense(self.mode).map(|too_dense| {
                let notice = format!(
                    "{} connectors are more than a layered layout takes on ({}). \
                     Filter the graph further, or draw it as a map.",
                    too_dense.connectors,
                    build::MAX_LAID_OUT_CONNECTORS,
                );
                div()
                    .absolute()
                    .size_full()
                    .flex()
                    .child(bviz_ui_components::centered_notice(notice, &palette.colors))
            }))
            .children(
                boxes::every_class_hidden(self.derived.unwrapped(), &self.box_classes)
                    .filter(|_| zoom.is_some_and(|zoom| drawn.withholds_most(zoom)))
                    .map(|notice| {
                        div()
                            .absolute()
                            .size_full()
                            .flex()
                            .child(bviz_ui_components::centered_notice(notice, &palette.colors))
                    }),
            )
            .child(cached_layer(self.overlays.clone()))
            .child(cached_layer(self.box_card.layer()))
            .children(box_menu::menu(self, window))
            // A replayed scene installs the handlers recorded with it, which a
            // press's own frame has yet to make; the panel's own paint is what
            // installs them for the gesture that press begins.
            .child(overlay_canvas(move |_, window, cx| {
                track_gesture(&entity, window, cx)
            }))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::on_secondary_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .hover_listener_mode(gpui::HoverListenerMode::InputModalityIndependent)
            .on_hover(cx.listener(Self::on_hover))
            .on_scroll_wheel(cx.listener(Self::on_scroll_wheel))
            .on_pinch(cx.listener(Self::on_pinch))
            // Track the shared focus handle here too (the chrome already tracks
            // it on an ancestor) so a press on the canvas brings the focus back
            // here from whichever control last took it. Key events bubble from
            // here up through the strip's own bindings to the chrome's.
            .track_focus(&self.focus_handle)
            .into_any_element()
    }

    /// The key to the view drawn, placed in the toolbar, blank where nothing
    /// drawn is keyed.
    fn legend(&mut self, palette: &Palette, cx: &Context<Self>) -> bviz_ui_components::Legend {
        self.keyed(palette, cx)
            .unwrap_or_else(|| legend::blank(&self.legend_state))
    }

    /// The key to the view drawn, where it keys anything.
    fn keyed(
        &mut self,
        palette: &Palette,
        cx: &Context<Self>,
    ) -> Option<bviz_ui_components::Legend> {
        let drawn = self.drawn()?;
        let zoom = self.view_zoom(&drawn)?;
        let loaded = self.model.read(cx).loaded()?;
        let (statuses, resolved) = (loaded.statuses(), loaded.taken_up());
        let coloured = legend::Coloured {
            colouring: self.colouring,
            times: loaded.times().ok(),
            timed: loaded.timed(),
            states: loaded.states(),
            attempted: loaded.attempted(),
        };
        let haloed = legend::Haloed {
            nodes: self.critical_path_enabled.then(|| loaded.critical()),
            traced: loaded.traced(),
        };
        self.legend.strip(
            &drawn,
            zoom,
            statuses,
            resolved,
            coloured,
            haloed,
            &self.legend_state,
            palette,
        )
    }

    /// The view a frame draws, where one is derived.
    fn shown_key(&self) -> Option<ViewKey> {
        let drawn = self.drawn()?;
        Some(drawn.shown(self.view_zoom(&drawn)?)?.key())
    }

    fn draw_toolbar(
        &mut self,
        palette: &Palette,
        window: &mut Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        let controls = Controls {
            legend: self.legend(palette, cx),
        };
        toolbar::toolbar(self, controls, window, cx).into_any_element()
    }

    fn draw_overlays(&self, palette: &Palette, cx: &Context<Self>) -> AnyElement {
        div()
            .size_full()
            .child(
                column::Column::new()
                    .held(reveal::card(self, palette, cx))
                    .held(pins::panel(self, palette, cx))
                    .folding(report::card(self, palette, cx))
                    .folding(timecard::card(self, palette, cx))
                    .folding(missing::card(self, palette, cx))
                    .foot(why::card(self, palette, cx)),
            )
            .children(self.jump.read(cx).is_open().then(|| self.jump.clone()))
            .into_any_element()
    }

    /// Switches which layout is drawn, starting the newly selected one's if it
    /// has not been computed yet.
    fn set_mode(&mut self, mode: LayoutMode, cx: &mut Context<Self>) {
        if self.switch_mode(mode, cx) {
            derive::ensure_layout(self, cx);
            self.invalidate_canvas(cx);
        }
    }

    /// Switches which layout is drawn, leaving it to the caller to derive what
    /// that mode draws; reports whether the mode changed. The two span
    /// different extents, so the view refits.
    ///
    /// The hover, the emphasis and the morph all describe the layout being left:
    /// an emphasis carried across would let a relayout still in flight settle the
    /// incoming view onto a box picked out in the other one, and highlight it,
    /// and a morph would ease boxes of the layout left behind over the one
    /// arriving.
    fn switch_mode(&mut self, mode: LayoutMode, cx: &mut Context<Self>) -> bool {
        if self.mode == mode {
            return false;
        }
        self.mode = mode;
        self.hovered = None;
        self.box_card.forget(cx);
        self.emphasis.clear();
        self.morph.clear();
        self.framing.refit();
        selection::remark(self, cx);
        true
    }

    fn on_mouse_down(&mut self, e: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        // The palette occludes the canvas, so a press reaching here landed
        // outside it.
        jump::graph::dismiss(self, cx);
        let clear = self.box_id_at(e.position, cx).is_none();
        let pressed_clear = std::mem::replace(&mut self.pressed_clear, clear && e.click_count == 1);
        // A press on a scroll-bar gutter drives the thumb instead of panning the
        // content under it.
        if let Some(axis) = scrollbar_hit(self, e.position) {
            start_scroll_drag(self, axis, e.position, cx);
            return;
        }
        GESTURES.dispatch(Pointer::press(e), |intent| match intent {
            CanvasIntent::ShowWhole if clear && pressed_clear => {
                self.show_whole(cx);
                true
            }
            CanvasIntent::Pan => {
                start_pan(self, e, cx);
                true
            }
            _ => false,
        });
    }

    /// Eases the view onto the whole of what the panel draws.
    fn show_whole(&mut self, cx: &mut Context<Self>) {
        self.framing.request_frame(framing::Frame::Drawing);
        self.invalidate_canvas_only(cx);
    }

    fn on_secondary_down(
        &mut self,
        e: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        jump::graph::dismiss(self, cx);
        let hit = self.box_id_at(e.position, cx);
        GESTURES.dispatch(Pointer::press(e), |intent| match (intent, hit) {
            (CanvasIntent::BoxMenu, Some(node)) => {
                box_menu::open(self, node, e.position, window, cx);
                true
            }
            (CanvasIntent::Deselect, None) => {
                selection::select(self, None, cx);
                true
            }
            _ => false,
        });
    }

    fn on_navigate_down(&mut self, e: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        GESTURES.dispatch(Pointer::press(e), |intent| match intent {
            CanvasIntent::GoBack => {
                self.go_back(cx);
                true
            }
            CanvasIntent::GoForward => {
                self.go_forward(cx);
                true
            }
            _ => false,
        });
    }

    fn go_back(&mut self, cx: &mut Context<Self>) {
        jump::graph::dismiss(self, cx);
        reveal::back(self, cx);
    }

    fn go_forward(&mut self, cx: &mut Context<Self>) {
        jump::graph::dismiss(self, cx);
        reveal::forward(self, cx);
    }

    /// Ends the drag on the release the element receives directly, which the
    /// window-global handlers can miss: they are installed only once a frame
    /// paints with a drag in progress, so a press-release quick enough to finish
    /// before that paint would leave the drag running.
    fn on_mouse_up(&mut self, e: &MouseUpEvent, window: &mut Window, cx: &mut Context<Self>) {
        end_drag(self, e.position, DragEnd::Released, window, cx);
    }

    fn on_mouse_move(&mut self, e: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        // A pan moves the view under a stationary pointer, so the box it is over
        // is the one already resolved.
        if matches!(self.drag, Drag::Pan(_)) {
            return;
        }
        GESTURES.dispatch(Pointer::HOVER, |intent| match intent {
            CanvasIntent::Hover => {
                self.hover_box_at(e.position, cx);
                true
            }
            _ => false,
        });
    }

    /// What `read` reads of the box drawn under the window-space `position`,
    /// if any.
    fn read_box_at<T>(
        &mut self,
        position: Point<Pixels>,
        read: impl FnOnce(&bviz_graph_layout::Box) -> T,
        cx: &mut Context<Self>,
    ) -> Option<T> {
        let drawn = self.drawn()?;
        let camera = self.camera();
        let vp = self.framing.viewport();
        let morph = self.morph_frame();
        self.spatial
            .indexed(&drawn, camera.zoom, cx)
            .and_then(|(_, g)| box_at(&self.titles, &g, morph.as_ref(), vp, camera, position))
            .map(read)
    }

    /// The box drawn under the window-space `position`, if any.
    fn box_id_at(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) -> Option<Id> {
        self.read_box_at(position, |b| b.id, cx)
    }

    /// Takes the pointer at `position` to be over the box drawn there, which
    /// the card describes and a hover marks the neighborhood of.
    fn hover_box_at(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        if self.drawn().is_none() {
            return;
        }
        let described = self.box_card.described();
        let landed = self.read_box_at(
            position,
            |b| match described == Some(b.id) {
                true => hover::Landed::Described(b.id),
                false => hover::Landed::Fresh(hover::Pointed::of(b)),
            },
            cx,
        );
        let hit = landed.as_ref().map(hover::Landed::id);
        self.box_card.hover(position, landed, cx);
        if hit != self.hovered {
            self.hovered = hit;
            self.repaint_hover(cx);
        }
    }

    /// Drops the hover when the pointer leaves the canvas, so a node's
    /// neighborhood does not stay marked while the cursor is over the toolbar or
    /// the window chrome — the move handler is bounds-gated and never fires there.
    /// gpui reports the canvas unhovered while a button is held, so the drag guard
    /// keeps a pan from dropping the marked set it started with.
    fn on_hover(&mut self, over: &bool, _: &mut Window, cx: &mut Context<Self>) {
        if *over || !matches!(self.drag, Drag::None) {
            return;
        }
        self.box_card.forget(cx);
        if self.hovered.take().is_some() {
            self.repaint_hover(cx);
        }
    }

    /// Repaints the drawing for a change of the hovered box, which it shows
    /// only while hovering marks a neighborhood.
    fn repaint_hover(&self, cx: &mut Context<Self>) {
        if self.neighborhood_enabled {
            self.invalidate_canvas_only(cx);
        }
    }

    fn on_scroll_wheel(&mut self, e: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let delta = scroll_pixels(e);
        let (dx, dy) = (f32::from(delta.x), f32::from(delta.y));
        let moved = GESTURES.dispatch(Pointer::scroll(e), |intent| {
            match intent {
                CanvasIntent::Zoom => self.zoom_about(e.position, wheel_zoom_factor(e), cx),
                CanvasIntent::PanAcross => pan_by(self, (dx + dy, 0.0), cx),
                CanvasIntent::Scroll => pan_by(self, (dx, dy), cx),
                _ => return false,
            }
            true
        });
        if moved {
            self.hover_box_at(e.position, cx);
        }
    }

    fn on_pinch(&mut self, e: &PinchEvent, _: &mut Window, cx: &mut Context<Self>) {
        let moved = GESTURES.dispatch(Pointer::pinch(e), |intent| match intent {
            CanvasIntent::Zoom => {
                self.zoom_about(e.position, pinch_factor(e), cx);
                true
            }
            _ => false,
        });
        if moved {
            self.hover_box_at(e.position, cx);
        }
    }

    /// Zooms `factor` times closer about the middle of the view.
    fn zoom_centred(&mut self, factor: f32, cx: &mut Context<Self>) {
        self.zoom_about(self.framing.viewport().center(), factor, cx);
    }

    /// Zooms `factor` times closer about the window-space point `at`.
    fn zoom_about(&mut self, at: Point<Pixels>, factor: f32, cx: &mut Context<Self>) {
        let Some(content) = self.drawn().map(|d| d.content()) else {
            return;
        };
        let viewport = self.framing.viewport();
        let scaled = scale_about(at, self.camera(), factor, viewport, content);
        self.steer(|framing| framing.move_to(scaled), cx);
    }

    /// Puts away the innermost thing the reader set up: a reveal pending,
    /// then the selection where the panel marks it, then the dependency paths
    /// asked for, and last a selection the panel marks nothing for.
    fn dismiss(&mut self, cx: &mut Context<Self>) {
        let selected = self.model.read(cx).selected().is_some();
        let marked = self.selection.node().is_some();
        if reveal::dismiss(self) {
            cx.notify();
        } else if marked || (selected && !self.why.asked()) {
            selection::select(self, None, cx);
        } else {
            why::clear(self, cx);
        }
    }

    fn toggle_neighborhood(&mut self, cx: &mut Context<Self>) {
        self.neighborhood_enabled = !self.neighborhood_enabled;
        self.invalidate_canvas(cx);
    }

    /// Whether the invocation's critical path lands on the graph, which is
    /// where its halo has anything to draw.
    fn traced(&self, cx: &App) -> bool {
        self.model
            .read(cx)
            .loaded()
            .is_some_and(|loaded| loaded.critical().any())
    }

    fn toggle_critical_path(&mut self, cx: &mut Context<Self>) {
        if self.traced(cx) {
            self.critical_path_enabled = !self.critical_path_enabled;
            self.invalidate_canvas(cx);
        }
    }

    /// Widens or narrows what the selected box marks.
    fn set_relation(&mut self, relation: Relation, cx: &mut Context<Self>) {
        self.selection.set_relation(relation);
        self.invalidate_canvas(cx);
    }
}

impl EventEmitter<PanelEvent> for GraphPanel {}

impl Focusable for GraphPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Panel for GraphPanel {
    fn kind() -> PanelKind {
        PanelKind {
            view: ViewKind::new("graph"),
            key_context: KEY_CONTEXT,
            role: Role::Canvas,
        }
    }

    fn gestures() -> Vec<GestureRow> {
        GESTURES
            .rows()
            .chain(jump::gestures())
            .chain(chips::gestures())
            .chain(pins::gestures())
            .chain(report::gestures())
            .chain(timecard::gestures())
            .chain(missing::gestures())
            .chain(legend_gestures())
            .collect()
    }

    fn titled() -> Titled {
        Titled::File
    }

    fn status(&self, cx: &App) -> Option<SharedString> {
        Some(
            derive::status(
                self.model.read(cx).source(),
                self.drawn()
                    .and_then(|drawn| self.view_zoom(&drawn).map(|zoom| (drawn, zoom))),
                self.derived.over_budget(),
            )
            .into(),
        )
    }

    fn set_active(&mut self, active: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.shown = active;
        if active {
            derive::catch_up(self, cx);
            self.restate_shown(cx);
            self.focus_handle.focus(window, cx);
            let panel = cx.entity();
            self.model
                .update(cx, |model, cx| model.defer_to(&panel, cx));
        } else {
            // The card and the hover belong to a pointer no longer over the
            // drawing.
            self.box_card.forget(cx);
            if self.hovered.take().is_some() {
                self.repaint_hover(cx);
            }
        }
    }
}

impl Render for GraphPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _span = pan_bench::span(pan_bench::Phase::PanelRender);
        if let Some(bench) = self.bench.take() {
            bench.start(cx.entity(), window);
        }
        let palette = Palette::from_theme(&bviz_ui_theme::theme(window, cx));
        if self.legend.retarget(self.shown_key()) {
            let toolbar = self.toolbar.clone();
            window.on_next_frame(move |_, cx| toolbar.update(cx, |_, cx| cx.notify()));
        }
        let chrome = div()
            .key_context(Self::kind().key_context)
            .on_action(
                cx.listener(|view, _: &ToggleNeighborhood, _, cx| view.toggle_neighborhood(cx)),
            )
            .on_action(
                cx.listener(|view, _: &ToggleReduce, _, cx| mode::toggle_reduction(view, cx)),
            )
            .on_action(cx.listener(|view, _: &ToggleMap, _, cx| mode::toggle_map(view, cx)))
            .on_action(cx.listener(|view, _: &CycleColouring, _, cx| colour::cycle(view, cx)))
            .on_action(
                cx.listener(|view, _: &ToggleCriticalPath, _, cx| view.toggle_critical_path(cx)),
            )
            .on_action(cx.listener(|view, _: &ToggleOutcomes, _, cx| report::toggle(view, cx)))
            .on_action(cx.listener(|view, _: &ToggleTiming, _, cx| timecard::toggle(view, cx)))
            .on_action(cx.listener(|view, _: &Dismiss, _, cx| view.dismiss(cx)))
            .on_action(cx.listener(|view, _: &reveal::Accept, _, cx| reveal::accept(view, cx)))
            .on_action(cx.listener(|view, _: &JumpBack, _, cx| view.go_back(cx)))
            .on_action(cx.listener(|view, _: &JumpForward, _, cx| view.go_forward(cx)))
            .on_action(
                cx.listener(|view, _: &jump::Open, window, cx| jump::graph::open(view, window, cx)),
            )
            .on_action(cx.listener(|view, _: &chips::Focus, window, cx| {
                view.chips
                    .update(cx, |bar, cx| chips::focus(bar, window, cx));
            }))
            .on_action(cx.listener(|view, _: &chips::Apply, _, cx| {
                view.chips.update(cx, chips::apply_staged);
            }))
            .on_action(cx.listener(|view, _: &chips::CopyStack, _, cx| {
                view.chips.update(cx, chips::copy_stack);
            }))
            .on_action(cx.listener(|view, _: &ShowWholeGraph, _, cx| view.show_whole(cx)))
            .on_action(cx.listener(|view, _: &OpenBoxMenu, window, cx| {
                box_menu::open_selected(view, window, cx)
            }))
            .on_action(cx.listener(|view, _: &ShowLegend, window, cx| {
                view.legend_state
                    .update(cx, |legend, cx| legend.show(window, cx));
            }))
            .on_action(cx.listener(|view, _: &ZoomIn, _, cx| view.zoom_centred(ZOOM_KEY_STEP, cx)))
            .on_action(
                cx.listener(|view, _: &ZoomOut, _, cx| {
                    view.zoom_centred(ZOOM_KEY_STEP.recip(), cx)
                }),
            )
            .on_mouse_down(
                MouseButton::Navigate(NavigationDirection::Back),
                cx.listener(Self::on_navigate_down),
            )
            .on_mouse_down(
                MouseButton::Navigate(NavigationDirection::Forward),
                cx.listener(Self::on_navigate_down),
            )
            .track_focus(&self.focus_handle)
            .flex_1()
            .flex()
            .flex_col()
            .overflow_hidden();
        let placeholder = self.model.read(cx).source().placeholder(&palette.colors);
        match placeholder {
            Some(placeholder) => chrome.child(placeholder),
            None => chrome
                .child(
                    AnyView::from(self.toolbar.clone()).cached(
                        StyleRefinement::default()
                            .w_full()
                            .h(toolbar::height())
                            .flex_none(),
                    ),
                )
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .overflow_hidden()
                        .child(self.viewport(palette, window, cx)),
                ),
        }
    }
}

/// A part of the panel's chrome, drawn in a layer of its own so that a frame
/// drawn for a change only the canvas shows replays it. Every notify of the
/// panel's own notifies each such layer through an observer, which gpui does
/// not run for a notify issued while a frame is drawn, so nothing the panel
/// renders, prepaints or paints may notify it.
struct ChromeLayer {
    parent: WeakEntity<GraphPanel>,
    draw: fn(&mut GraphPanel, &Palette, &mut Window, &Context<GraphPanel>) -> AnyElement,
}

impl Render for ChromeLayer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _span = pan_bench::span(pan_bench::Phase::ChromeRender);
        let Some(parent) = self.parent.upgrade() else {
            return div().into_any_element();
        };
        let palette = Palette::from_theme(&bviz_ui_theme::theme(window, cx));
        let draw = self.draw;
        parent.update(cx, |view, cx| draw(view, &palette, window, cx))
    }
}

/// The layer the graph is drawn in, so that a frame drawn for any other reason
/// replays it rather than laying the drawing out again.
struct GraphCanvas {
    /// What the drawing is made from, and what its prepaint and paint write
    /// back through.
    parent: WeakEntity<GraphPanel>,
}

impl Render for GraphCanvas {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(parent) = self.parent.upgrade() else {
            return div().into_any_element();
        };
        let palette = Palette::from_theme(&bviz_ui_theme::theme(window, cx));
        let view = parent.read(cx);
        let Some(drawn) = view.drawn() else {
            return div().into_any_element();
        };
        let clock = view.model.read(cx).clock();
        let at = clock.read(cx).now();
        let playing = (view.colouring == Colouring::Playback).then_some(clock);
        Clock::keep_pace(playing, window, cx);
        let content = drawn.content();
        let recenter_target = match view.view_zoom(&drawn).and_then(|zoom| drawn.view(zoom)) {
            None => Target::Pending,
            Some(g) => Target::Resolved(recenter_rect(g, &view.emphasis)),
        };
        let scene = Scene {
            expandable: view.expandable(),
            palette,
            faded: view.derived.faded(),
            hovered: view.hovered.filter(|_| view.neighborhood_enabled),
            selected: view.selection.node(),
            highlight: view.emphasis.highlight(),
            colouring: view.colouring,
            at,
            critical_path: view.critical_path_enabled,
            drawn,
        };
        let prepaint = build_prepaint(parent.clone(), content, recenter_target);
        canvas(prepaint, canvas_body(scene, parent))
            .size_full()
            .into_any_element()
    }
}

/// The toolbar's parts that take more to build than a shared borrow of the
/// panel: the panel itself mutably.
struct Controls {
    legend: bviz_ui_components::Legend,
}

/// Whether a freshly opened graph marks the neighbourhood of the box under
/// the pointer.
const NEIGHBOURHOOD_AT_REST: bool = false;

/// Whether a freshly opened graph haloes its critical path, where it has one.
const CRITICAL_PATH_AT_REST: bool = true;

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
/// the cursor in `indexed` under `camera` and `morph`. Since a title is always
/// painted within its own box, that leaves a set recorded from a frame the view
/// has moved on from — a relayout, a mode switch, a camera step — resolving to
/// the box under the cursor instead.
fn box_at<'a>(
    titles: &[TitleTarget],
    indexed: &Indexed<'a>,
    morph: Option<&Frame>,
    viewport: Bounds<Pixels>,
    camera: Camera,
    pos: Point<Pixels>,
) -> Option<&'a bviz_graph_layout::Box> {
    let (gx, gy) = from_screen(pos, viewport.origin, camera);
    let over: Vec<_> = boxes_over(indexed, morph, gx, gy).collect();
    let titled = paint::title_at(titles, pos)
        .and_then(|id| over.iter().find(|(b, _)| b.id == id))
        .map(|&(b, _)| b);
    titled.or_else(|| innermost(over))
}

/// Opens or closes the box under `pos` (window px) and settles the view to keep
/// it in frame. A box the drawn layout holds no handle on, or a `pos` off the
/// canvas, is left alone.
fn click_box_at(view: &mut GraphPanel, pos: Point<Pixels>, cx: &mut Context<GraphPanel>) {
    let vp = view.framing.viewport();
    if !vp.contains(&pos) {
        return;
    }
    let camera = view.camera();
    let Some(drawn) = view.drawn() else {
        return;
    };
    let Some((_, g)) = view.spatial.indexed(&drawn, camera.zoom, cx) else {
        return;
    };
    let morph = view.morph_frame();
    let expandable = view.expandable();
    let hit =
        box_at(&view.titles, &g, morph.as_ref(), vp, camera, pos).filter(|b| expandable.holds(b));
    let Some(id) = hit.map(|b| b.id) else {
        return;
    };
    // The relayout places the boxes afresh, so the box resolved here need not be
    // the one the cursor ends up over.
    view.hovered = None;
    view.box_card.forget(cx);
    derive::toggle_expansion(view, id, cx);
    view.emphasis.toggled(id);
    view.invalidate_canvas(cx);
}

/// The smallest-area (in layout space) of the boxes `over` draws, so an inner
/// member or header wins over its enclosing container; the first of equals.
fn innermost<'a>(
    over: impl IntoIterator<Item = (&'a bviz_graph_layout::Box, Rect)>,
) -> Option<&'a bviz_graph_layout::Box> {
    over.into_iter()
        .min_by(|(_, a), (_, b)| (a.w * a.h).total_cmp(&(b.w * b.h)))
        .map(|(b, _)| b)
}

/// The boxes drawn over `(gx, gy)` under `morph`, in the drawing's order, each
/// with the rect it is drawn at. The boxes a morph is fading out are absent from
/// the drawing, so a gesture passes through them to whatever the new drawing
/// puts there.
fn boxes_over<'a>(
    indexed: &Indexed<'a>,
    morph: Option<&Frame>,
    gx: f32,
    gy: f32,
) -> impl Iterator<Item = (&'a bviz_graph_layout::Box, Rect)> {
    let at = Rect {
        x: gx as f64,
        y: gy as f64,
        w: 0.0,
        h: 0.0,
    };
    indexed
        .boxes_near(&at, morph)
        .filter(move |(_, rect, _)| rect.covers(gx, gy))
        .map(|(b, rect, _)| (b, rect))
}

/// A joint graph being read and laid out.
pub struct Load {
    built: Pending<model::Read>,
    title: SharedString,
}

impl From<GraphSource> for Load {
    fn from(source: GraphSource) -> Self {
        let title = title(&source);
        Self {
            built: Pending::start(move || model::build(source)),
            title,
        }
    }
}

/// Names the file the graph was read from, and how many more it merges.
fn title(source: &GraphSource) -> SharedString {
    let (first, rest) = source.first_and_rest();
    match rest.len() {
        0 => document_label(&first.path),
        more => format!("{} +{more}", document_label(&first.path)).into(),
    }
}

/// The view a graph opens with, by which a document is told to hold one.
pub type LeadPanel = GraphPanel;

/// The views drawing the graph `load` reads, which tells `session` what the
/// graph knows of its document's invocation, and follow `clock`. The graph
/// derives the action graph only where `sharing` lets another file read it.
pub fn open(
    load: impl Into<Load>,
    session: Entity<Session>,
    clock: Entity<Clock>,
    sharing: Sharing,
    _window: &mut Window,
    cx: &mut App,
) -> Member {
    let load = load.into();
    let label = load.title;
    let model = cx.new(|cx| GraphModel::reading(load.built, session, clock, sharing, cx));
    let graph = ViewSpec::new({
        let model = model.clone();
        move |cx| cx.new(|cx| LeadPanel::new(model.clone(), cx))
    });
    let scatter = ViewSpec::new({
        let model = model.clone();
        move |cx| cx.new(|cx| ScatterPanel::new(model.clone(), cx))
    });
    let exclusive = ViewSpec::new(move |cx| cx.new(|cx| ExclusivePanel::new(model.clone(), cx)));
    Member::new(label, graph, vec![scatter, exclusive])
}

/// Renders a Bazel build graph from one or more `--output=streamed_proto`
/// files. Each flag selects a file and the query that produced it; passing
/// several merges them into one joint graph, with shared entities collapsed.
#[derive(Parser)]
#[command(name = "graph", version, about)]
struct Cli {
    #[command(flatten)]
    graph: GraphArgs,
}

/// Parses the query inputs and opens the graph-viewer window.
pub fn run() {
    bviz_ui_workspace::run_viewer(
        "bviz graph",
        bviz_ui_workspace::WINDOW_SIZE,
        |cli: Cli| cli.graph.source().map(Load::from),
        &format!("one of --{AQUERY_LONG}, --{CQUERY_LONG} or --{QUERY_LONG}"),
        |load: Load, window, cx| {
            let shared = Shared::new(cx);
            let (session, clock) = (shared.session().clone(), shared.clock().clone());
            let member = open(load, session, clock, Sharing::Alone, window, cx);
            Document::alone(member, None, shared)
        },
        &[register_keys],
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::camera::{
        Rect, clamp_pan, ease_out_cubic, fit_view, fit_zoom, settle_view, to_screen,
    };
    use crate::paint::connector::lane_polyline;
    use bviz_graph_layout::Box;
    use bviz_graph_layout::Kind;
    use bviz_ui_components::gesture::ZOOM_PER_NOTCH;
    use bviz_ui_components::keymap_probe::{self, is};
    use gpui::{Bounds, Size, point, px, size};

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

    /// The action `keys` dispatch with focus in the innermost of `contexts`,
    /// under the panel's bindings and those of the text field.
    fn dispatched(keys: &str, contexts: &[&str]) -> Option<std::boxed::Box<dyn gpui::Action>> {
        dispatched_on(Host::Desktop, keys, contexts)
    }

    fn dispatched_on(
        host: Host,
        keys: &str,
        contexts: &[&str],
    ) -> Option<std::boxed::Box<dyn gpui::Action>> {
        let mut bindings = bindings(host);
        bindings.extend(text_input::bindings());
        keymap_probe::dispatched(bindings, keys, contexts)
    }

    const IN_PANEL: &[&str] = &[KEY_CONTEXT.id()];
    const IN_FIELD: &[&str] = &[KEY_CONTEXT.id(), text_input::KEY_CONTEXT.id()];
    const IN_PALETTE: &[&str] = &[
        KEY_CONTEXT.id(),
        jump::KEY_CONTEXT.id(),
        text_input::KEY_CONTEXT.id(),
    ];

    /// A letter typed into any text field is text, not one of the panel's
    /// keys.
    #[test]
    fn the_panels_letters_stand_down_in_any_text_field() {
        assert!(is(dispatched("h", IN_PANEL), ToggleNeighborhood));
        assert!(is(dispatched("r", IN_PANEL), ToggleReduce));
        assert!(is(dispatched("p", IN_PANEL), ToggleCriticalPath));
        assert!(is(dispatched("o", IN_PANEL), ToggleOutcomes));
        assert!(is(dispatched("t", IN_PANEL), ToggleTiming));
        assert!(is(dispatched("m", IN_PANEL), ToggleMap));
        assert!(is(dispatched("c", IN_PANEL), CycleColouring));
        for field in [IN_CHIP_FIELD, IN_FIELD] {
            for letter in ["h", "r", "p", "o", "t", "m", "c"] {
                assert!(dispatched(letter, field).is_none(), "{letter}");
            }
        }
    }

    /// The keys opening a context menu open the selected chip's on the chips,
    /// the selected box's elsewhere, and leave a text field's own to it.
    #[test]
    fn the_menu_keys_open_the_menu_of_what_is_selected_outside_a_text_field() {
        for keys in ["shift-f10", "menu"] {
            assert!(is(dispatched(keys, IN_PANEL), OpenBoxMenu), "{keys}");
            assert!(
                is(dispatched(keys, IN_CHIPS), chips::OpenChipMenu),
                "{keys}"
            );
            for field in [IN_FIELD, IN_CHIP_FIELD, IN_PALETTE] {
                assert!(dispatched(keys, field).is_none(), "{keys}");
            }
        }
    }

    #[test]
    fn slash_opens_the_palette_only_where_it_types_nothing() {
        assert!(is(dispatched("/", IN_PANEL), jump::Open));
        assert!(dispatched("/", IN_CHIP_FIELD).is_none());
        assert!(dispatched("/", IN_PALETTE).is_none());
        assert!(is(dispatched("secondary-f", IN_PANEL), jump::Open));
        assert!(is(dispatched("secondary-f", IN_CHIP_FIELD), jump::Open));
    }

    /// A letter typed into the palette's field is text, and the keys moving
    /// through its rows reach it before the panel's own.
    #[test]
    fn the_palette_takes_its_keys_before_the_panel() {
        assert!(dispatched("h", IN_PALETTE).is_none());
        assert!(is(dispatched("h", IN_PANEL), ToggleNeighborhood));
        assert!(is(dispatched("escape", IN_PALETTE), jump::Close));
        assert!(is(dispatched("escape", IN_PANEL), Dismiss));
        assert!(is(dispatched("enter", IN_PANEL), reveal::Accept));
        assert!(is(dispatched("enter", IN_CHIP_FIELD), chips::Commit));
        assert!(is(dispatched("enter", IN_CHIPS), chips::Edit));
        assert!(is(dispatched("up", IN_PALETTE), jump::Previous));
        assert!(is(dispatched("enter", IN_PALETTE), jump::Go));
        assert!(is(dispatched("secondary-enter", IN_PALETTE), jump::AskWhy));
        assert!(is(dispatched("shift-enter", IN_PALETTE), jump::AlwaysShow));
        assert!(is(dispatched("alt-enter", IN_PALETTE), jump::AddChip));
        assert!(is(dispatched("tab", IN_PALETTE), jump::NextScope));
        assert!(is(dispatched("shift-tab", IN_PALETTE), jump::PreviousScope));
    }

    /// The panel's own jumps and the exclusive view's foci are walked by the
    /// same keys, each in its own panel, and a text field keeps them.
    #[test]
    fn alt_arrows_walk_the_history_of_the_panel_holding_focus() {
        assert!(is(dispatched("alt-left", IN_PANEL), JumpBack));
        assert!(is(dispatched("alt-right", IN_PANEL), JumpForward));
        for keys in ["alt-left", "alt-right"] {
            for field in [IN_PALETTE, IN_CHIP_FIELD] {
                assert!(!is(dispatched(keys, field), JumpBack), "{keys}");
                assert!(!is(dispatched(keys, field), JumpForward), "{keys}");
            }
        }
        let scatter = &[scatter::KEY_CONTEXT.id()];
        assert!(is(dispatched("alt-left", scatter), JumpBack));
        assert!(is(dispatched("alt-right", scatter), JumpForward));
        let exclusive = &[exclusive::KEY_CONTEXT.id()];
        assert!(is(dispatched("alt-left", exclusive), exclusive::FocusBack));
        assert!(is(
            dispatched("alt-right", exclusive),
            exclusive::FocusForward
        ));
    }

    /// In a browser the same histories are walked by chords as well, which a
    /// text field keeps as it keeps the arrows.
    #[test]
    fn a_browser_walks_the_histories_by_chords_too() {
        let browser = |keys, contexts| dispatched_on(Host::Browser, keys, contexts);
        assert!(is(browser("alt-left", IN_PANEL), JumpBack));
        assert!(is(browser("secondary-k [", IN_PANEL), JumpBack));
        assert!(is(browser("secondary-k ]", IN_PANEL), JumpForward));
        let scatter = &[scatter::KEY_CONTEXT.id()];
        assert!(is(browser("secondary-k [", scatter), JumpBack));
        assert!(is(browser("secondary-k ]", scatter), JumpForward));
        let exclusive = &[exclusive::KEY_CONTEXT.id()];
        assert!(is(
            browser("secondary-k [", exclusive),
            exclusive::FocusBack
        ));
        assert!(is(
            browser("secondary-k ]", exclusive),
            exclusive::FocusForward
        ));
        for keys in ["secondary-k [", "secondary-k ]"] {
            for field in [IN_PALETTE, IN_CHIP_FIELD] {
                assert!(!is(browser(keys, field), JumpBack), "{keys}");
                assert!(!is(browser(keys, field), JumpForward), "{keys}");
            }
            assert!(dispatched(keys, IN_PANEL).is_none(), "{keys}");
        }
    }

    /// The exclusive-deps panel's roots are edited by `r`, `q` reaching the
    /// query bar as in every view, and both are text inside the roots' field,
    /// which Escape and Enter close.
    #[test]
    fn the_roots_are_edited_by_r() {
        const IN_EXCLUSIVE: &[&str] = &[exclusive::KEY_CONTEXT.id()];
        const IN_ROOTS: &[&str] = &[
            exclusive::KEY_CONTEXT.id(),
            exclusive::ROOTS_KEY_CONTEXT.id(),
            text_input::KEY_CONTEXT.id(),
        ];
        assert!(is(dispatched("r", IN_EXCLUSIVE), exclusive::EditRoots));
        assert!(is(dispatched("q", IN_EXCLUSIVE), chips::Focus));
        assert!(dispatched("r", IN_ROOTS).is_none());
        assert!(dispatched("q", IN_ROOTS).is_none());
        for keys in ["escape", "enter"] {
            assert!(
                is(dispatched(keys, IN_ROOTS), exclusive::CloseRoots),
                "{keys}"
            );
        }
        for keys in ["secondary-f", "/"] {
            assert!(is(dispatched(keys, IN_EXCLUSIVE), jump::Open), "{keys}");
        }
    }

    /// While a card stands in the exclusive deps view, Enter answers it and
    /// Escape puts it away; with none, they act on the selected cell.
    #[test]
    fn the_exclusive_view_answers_its_card_ahead_of_its_cells() {
        const IN_EXCLUSIVE: &[&str] = &[exclusive::KEY_CONTEXT.id()];
        const OFFERING: &[&str] = &["GraphExclusive exclusiveOffering"];
        assert!(is(
            dispatched("enter", IN_EXCLUSIVE),
            exclusive::FocusSelected
        ));
        assert!(is(dispatched("enter", OFFERING), reveal::Accept));
        assert!(is(dispatched("escape", OFFERING), exclusive::Deselect));
        let in_palette = &[
            exclusive::KEY_CONTEXT.id(),
            jump::KEY_CONTEXT.id(),
            text_input::KEY_CONTEXT.id(),
        ];
        assert!(is(dispatched("enter", in_palette), jump::Go));
        assert!(is(dispatched("up", in_palette), jump::Previous));
        assert!(dispatched("r", in_palette).is_none());
    }

    /// The scatter plot opens the palette by the find keys, and while a card
    /// stands Enter answers it, the list of points under a click still
    /// taking Enter and Escape while it is up.
    #[test]
    fn the_scatter_plot_finds_and_answers_its_card() {
        const IN_SCATTER: &[&str] = &[scatter::KEY_CONTEXT.id()];
        const OFFERING: &[&str] = &["GraphScatter scatterOffering"];
        const PICKING_TOO: &[&str] = &["GraphScatter scatterOffering picking"];
        for keys in ["secondary-f", "/"] {
            assert!(is(dispatched(keys, IN_SCATTER), jump::Open), "{keys}");
        }
        assert!(is(dispatched("escape", IN_SCATTER), scatter::DismissCard));
        assert!(dispatched("enter", IN_SCATTER).is_none());
        assert!(is(dispatched("enter", OFFERING), reveal::Accept));
        assert!(is(dispatched("enter", PICKING_TOO), scatter::SelectAimed));
        assert!(is(dispatched("g", IN_SCATTER), scatter::ShowSelectedPoint));
        assert!(is(
            dispatched("escape", PICKING_TOO),
            scatter::ClosePickList
        ));
        let in_palette = &[
            scatter::KEY_CONTEXT.id(),
            jump::KEY_CONTEXT.id(),
            text_input::KEY_CONTEXT.id(),
        ];
        assert!(is(dispatched("enter", in_palette), jump::Go));
        assert!(is(dispatched("escape", in_palette), jump::Close));
        assert!(dispatched("/", in_palette).is_none());
    }

    /// The field's word keys reach it inside the bar, beside the bar's own
    /// keys that share their arrow or backspace.
    #[test]
    fn the_query_field_moves_and_deletes_by_word() {
        let field = |key: &str| {
            let keys = format!("{}-{key}", text_input::WORD_MODIFIER);
            dispatched(&keys, IN_CHIP_FIELD)
        };
        assert!(is(field("left"), text_input::WordLeft));
        assert!(is(field("right"), text_input::WordRight));
        assert!(is(field("shift-left"), text_input::SelectWordLeft));
        assert!(is(field("shift-right"), text_input::SelectWordRight));
        assert!(is(field("backspace"), text_input::DeleteWordLeft));
        assert!(is(field("delete"), text_input::DeleteWordRight));
        assert!(is(
            dispatched("secondary-shift-backspace", IN_CHIP_FIELD),
            chips::ClearQuery
        ));
    }

    const IN_CHIPS: &[&str] = &[KEY_CONTEXT.id(), chips::KEY_CONTEXT.id()];
    const IN_CHIP_FIELD: &[&str] = &[
        KEY_CONTEXT.id(),
        chips::KEY_CONTEXT.id(),
        chips::FIELD_KEY_CONTEXT.id(),
        text_input::KEY_CONTEXT.id(),
    ];

    /// The action `keys` dispatch with focus in the query field while its
    /// completion's list stands open, with a row `aimed` at or not.
    fn completing(keys: &str, aimed: bool) -> Option<std::boxed::Box<dyn gpui::Action>> {
        let mut bindings = bindings(Host::Desktop);
        bindings.extend(text_input::bindings());
        keymap_probe::dispatched_within(
            bindings,
            keys,
            &chips::completion_stack(holder::Holder::Graph, aimed),
        )
    }

    /// While the list completing the field's word stands open, its keys
    /// move through it, accept a row and close it ahead of the field's own;
    /// the field's text keys and the panel's apply still reach through.
    #[test]
    fn an_open_completion_takes_the_keys_ahead_of_the_field() {
        for aimed in [false, true] {
            assert!(is(completing("up", aimed), chips::CompletePrevious));
            assert!(is(completing("down", aimed), chips::CompleteNext));
            assert!(is(completing("tab", aimed), chips::AcceptCompletion));
            assert!(is(completing("escape", aimed), chips::CloseCompletion));
            assert!(is(completing("secondary-enter", aimed), chips::Apply));
            assert!(is(completing("left", aimed), text_input::Left));
            assert!(completing("q", aimed).is_none());
        }
        for closed in ["up", "down", "tab"] {
            assert!(
                !dispatched(closed, IN_CHIP_FIELD)
                    .is_some_and(|action| action.name().contains("Completion")),
                "{closed}"
            );
        }
    }

    /// Enter commits the field as typed until the arrow keys move onto a
    /// row, and accepts that row once they have.
    #[test]
    fn enter_accepts_a_completion_only_once_a_row_is_aimed_at() {
        assert!(is(completing("enter", false), chips::Commit));
        assert!(is(completing("enter", true), chips::AcceptCompletion));
    }

    /// `q` reaches the query bar from the panel and is text in a field; the
    /// field commits and leaves by its own keys and edits by the text field's.
    #[test]
    fn the_query_bar_is_reached_by_q_and_its_field_keeps_its_text_keys() {
        assert!(is(dispatched("q", IN_PANEL), chips::Focus));
        assert!(dispatched("q", IN_FIELD).is_none());
        assert!(dispatched("q", IN_CHIP_FIELD).is_none());
        assert!(is(dispatched("enter", IN_CHIP_FIELD), chips::Commit));
        assert!(is(dispatched("escape", IN_CHIP_FIELD), chips::Leave));
        assert!(is(
            dispatched("secondary-enter", IN_CHIP_FIELD),
            chips::Apply
        ));
        assert!(is(
            dispatched("backspace", IN_CHIP_FIELD),
            text_input::Backspace
        ));
        assert!(is(dispatched("left", IN_CHIP_FIELD), text_input::Left));
        for typed in ["space", "!", "delete"] {
            assert!(
                !dispatched(typed, IN_CHIP_FIELD)
                    .is_some_and(|action| action.name().starts_with("bviz_graph_chips")),
                "{typed}"
            );
        }
    }

    #[test]
    fn a_selected_chip_takes_the_keys_acting_on_chips() {
        assert!(is(dispatched("left", IN_CHIPS), chips::SelectPrevious));
        assert!(is(dispatched("right", IN_CHIPS), chips::SelectNext));
        assert!(is(dispatched("space", IN_CHIPS), chips::Toggle));
        assert!(is(dispatched("!", IN_CHIPS), chips::Negate));
        assert!(is(dispatched("alt-up", IN_CHIPS), chips::MoveBack));
        assert!(is(dispatched("alt-down", IN_CHIPS), chips::MoveForward));
        assert!(is(dispatched("delete", IN_CHIPS), chips::Remove));
        assert!(is(dispatched("backspace", IN_CHIPS), chips::Remove));
        assert!(is(dispatched("enter", IN_CHIPS), chips::Edit));
        assert!(is(dispatched("escape", IN_CHIPS), chips::Leave));
        assert!(is(dispatched("secondary-enter", IN_CHIPS), chips::Apply));
        assert!(is(dispatched("h", IN_CHIPS), ToggleNeighborhood));
        assert!(is(dispatched("secondary-c", IN_CHIPS), chips::CopyChip));
        assert!(is(
            dispatched("secondary-c", IN_CHIP_FIELD),
            text_input::Copy
        ));
        assert!(is(
            dispatched("secondary-shift-c", IN_CHIPS),
            chips::CopyStack
        ));
        assert!(is(
            dispatched("secondary-shift-c", IN_CHIP_FIELD),
            chips::CopyStack
        ));
        assert!(is(
            dispatched("secondary-shift-c", IN_PANEL),
            chips::CopyStack
        ));
        assert!(is(
            dispatched("secondary-shift-backspace", IN_CHIPS),
            chips::ClearQuery
        ));
        assert!(is(
            dispatched("secondary-shift-backspace", IN_CHIP_FIELD),
            chips::ClearQuery
        ));
        assert!(dispatched("secondary-shift-backspace", IN_PANEL).is_none());
        assert!(is(dispatched("secondary-z", IN_CHIPS), chips::RevertQuery));
        assert!(is(
            dispatched("secondary-z", IN_CHIP_FIELD),
            chips::RevertQuery
        ));
        assert!(dispatched("secondary-z", IN_PANEL).is_none());
        assert!(dispatched("secondary-shift-c", IN_FIELD).is_none());
    }

    /// No keystroke is bound twice under one predicate, where the later
    /// binding would silently take the key from the earlier.
    #[test]
    fn no_key_is_bound_twice_in_one_context() {
        for host in [Host::Desktop, Host::Browser] {
            let mut seen = std::collections::HashSet::new();
            for binding in bindings(host) {
                let keys: Vec<String> =
                    binding.keystrokes().iter().map(|k| k.to_string()).collect();
                let predicate = binding.predicate().map(|p| p.to_string());
                assert!(
                    seen.insert((keys.clone(), predicate.clone())),
                    "{host:?}: {keys:?} bound twice under {predicate:?}"
                );
            }
        }
    }

    /// Each key reaching the query bar from the panel and from within the bar
    /// is one binding, which the keys help lists once.
    #[test]
    fn the_bars_keys_reaching_it_from_the_panel_are_bound_once() {
        let bindings = bindings(Host::Desktop);
        let bound = |action: &dyn gpui::Action| {
            bindings
                .iter()
                .filter(|binding| binding.action().partial_eq(action))
                .count()
        };
        assert_eq!(bound(&chips::Apply), 1);
        assert_eq!(bound(&chips::CopyStack), 1);
    }

    /// Each view standing a query bar is the holder whose scope its keys
    /// reach the bar from.
    #[test]
    fn every_holder_is_the_view_it_names() {
        use holder::Holder;
        assert_eq!(Holder::Graph.scope(), GraphPanel::kind().key_context);
        assert_eq!(Holder::Scatter.scope(), ScatterPanel::kind().key_context);
        assert_eq!(
            Holder::Exclusive.scope(),
            ExclusivePanel::kind().key_context
        );
    }

    /// The scatter plot's pick list leaves its keys to the query bar while
    /// the bar holds the focus.
    #[test]
    fn the_pick_list_leaves_its_keys_to_the_query_bar() {
        let picking = [scatter::KEY_CONTEXT.id(), scatter::PICKING.id()];
        let picking_context = picking.join(" ");
        let listing = &[picking_context.as_str()];
        assert!(is(dispatched("down", listing), scatter::AimDown));
        let in_bar = &[picking_context.as_str(), chips::KEY_CONTEXT.id()];
        assert!(!is(dispatched("down", in_bar), scatter::AimDown));
        assert!(!is(dispatched("enter", in_bar), scatter::SelectAimed));
    }

    /// Every view holding the query bar reaches it by the same keys, from
    /// outside its text fields and from within the bar.
    #[test]
    fn the_bars_keys_reach_it_in_every_view_holding_it() {
        for holder in holder::Holder::ALL.map(holder::Holder::scope) {
            let outside = &[holder.id()];
            let on_chips = &[holder.id(), chips::KEY_CONTEXT.id()];
            let in_field = &[
                holder.id(),
                chips::KEY_CONTEXT.id(),
                chips::FIELD_KEY_CONTEXT.id(),
                text_input::KEY_CONTEXT.id(),
            ];
            assert!(is(dispatched("q", outside), chips::Focus), "{holder}");
            assert!(
                is(dispatched("secondary-enter", outside), chips::Apply),
                "{holder}"
            );
            assert!(
                is(dispatched("secondary-shift-c", outside), chips::CopyStack),
                "{holder}"
            );
            assert!(
                is(dispatched("left", on_chips), chips::SelectPrevious),
                "{holder}"
            );
            assert!(is(dispatched("enter", in_field), chips::Commit), "{holder}");
            assert!(is(dispatched("escape", in_field), chips::Leave), "{holder}");
        }
    }

    /// A staged stack is applied from anywhere in the panel but the text
    /// fields, the palette keeping its own secondary-Enter.
    #[test]
    fn secondary_enter_applies_the_stack_outside_the_palette() {
        assert!(is(dispatched("secondary-enter", IN_PANEL), chips::Apply));
        assert!(is(dispatched("secondary-enter", IN_PALETTE), jump::AskWhy));
        assert!(dispatched("secondary-enter", IN_FIELD).is_none());
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
        emphasis.toggled(bviz_graph_layout::Id(7));
        let g = bviz_graph_layout::LaidOut {
            boxes: vec![lbox(7, 10.0, 20.0, 30.0, 40.0)],
            ..Default::default()
        };
        assert!(recenter_rect(&g, &emphasis).is_some());
        emphasis.clear();
        assert!(recenter_rect(&g, &emphasis).is_none());
    }

    #[test]
    fn a_settle_frames_the_toggled_box_and_not_what_sits_around_it() {
        // A toggled box with a neighbour far off to the side. Framing the pair
        // would bound a rect wide enough to force a zoom-out, and an expansion
        // parts neighbours like this every time; the settle takes the toggled
        // box alone.
        let mut emphasis = Emphasis::none();
        emphasis.toggled(bviz_graph_layout::Id(7));
        let g = bviz_graph_layout::LaidOut {
            boxes: vec![
                lbox(7, 10.0, 20.0, 30.0, 40.0),
                lbox(1, 5000.0, 4000.0, 30.0, 40.0),
            ],
            ..Default::default()
        };
        let rect = recenter_rect(&g, &emphasis).expect("the toggled box is drawn");
        assert_eq!(
            (rect.x, rect.y, rect.w, rect.h),
            (10.0, 20.0, 30.0, 40.0),
            "the settle reached past the toggled box"
        );
        // And so the settle that rect feeds leaves the reader's zoom alone, where
        // one bounding the neighbour too would have to zoom out to enclose it.
        let (z, _) = settle_view(rect, vp(800., 600.), CONTENT, 1.0, (0.0, 0.0), 1.0);
        assert!((z - 1.0).abs() < 1e-4, "the toggle moved the zoom to {z}");
    }

    #[test]
    fn hit_box_picks_innermost_and_none_outside() {
        // An outer container (id 0) with a small inner box (id 1) nested in it.
        let g = bviz_graph_layout::LaidOut {
            boxes: vec![
                lbox(0, 0.0, 0.0, 100.0, 100.0),
                lbox(1, 20.0, 20.0, 30.0, 30.0),
            ],
            ..Default::default()
        };
        let boxes = Indexed::of(&g);
        // Inside both rects: the smaller (inner) wins.
        assert_eq!(
            innermost(boxes_over(&boxes, None, 30.0, 30.0)).unwrap().id,
            bviz_graph_layout::Id(1)
        );
        // Inside only the container.
        assert_eq!(
            innermost(boxes_over(&boxes, None, 5.0, 5.0)).unwrap().id,
            bviz_graph_layout::Id(0)
        );
        // Outside everything.
        assert!(innermost(boxes_over(&boxes, None, 200.0, 200.0)).is_none());
        assert!(innermost(boxes_over(&boxes, None, -1.0, 50.0)).is_none());
    }

    #[test]
    fn lane_polyline_offsets_every_vertex_in_parallel() {
        // An L route: right to (100,0), then down to (100,100).
        let mut out = Vec::new();
        let lane = lane_polyline(&[(0.0, 0.0), (100.0, 0.0), (100.0, 100.0)], 4.0, &mut out);
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
        assert_eq!(lane_polyline(&route, 0.0, &mut Vec::new()), route);
    }

    #[test]
    fn lane_polyline_opposite_signs_nest_on_opposite_sides() {
        let route = vec![(0.0, 0.0), (100.0, 0.0), (100.0, 100.0)];
        let plus = lane_polyline(&route, 4.0, &mut Vec::new())[1];
        let minus = lane_polyline(&route, -4.0, &mut Vec::new())[1];
        // The two lanes never cross: their corners fall on opposite sides.
        assert_ne!(plus.1.signum(), minus.1.signum(), "{plus:?} {minus:?}");
    }

    #[test]
    fn lane_polyline_parallel_shifts_a_straight_run() {
        // A straight two-point edge shifts whole, both ends offset perpendicular.
        let mut out = Vec::new();
        let lane = lane_polyline(&[(0.0, 0.0), (100.0, 0.0)], 4.0, &mut out);
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
        let (z, p) = settle_view(target, vp(800., 600.), CONTENT, 1.0, (0.0, 0.0), 1.0);
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
        let (z, p) = settle_view(target, vp(800., 600.), CONTENT, 1.0, (0.0, 0.0), 1.0);
        assert!(z < 1.0, "expected zoom-out, got {z}");
        assert!(
            z >= fit_zoom(vp(800., 600.), CONTENT) - 1e-4,
            "below whole-graph fit: {z}"
        );
        assert_enclosed(target, 800., 600., z, p);
    }

    #[test]
    fn settle_view_never_zooms_in_past_the_readers_own_zoom() {
        // A box far smaller than the window does not blow up to fill it: the
        // settle rises no further than the zoom the reader set.
        let target = Rect {
            x: 100.0,
            y: 100.0,
            w: 30.0,
            h: 20.0,
        };
        let (z, _) = settle_view(target, vp(800., 600.), CONTENT, 0.5, (0.0, 0.0), 0.5);
        assert!(z <= 0.5 + 1e-4, "zoomed in: {z}");
    }

    #[test]
    fn opening_one_box_after_another_leaves_the_zoom_alone_while_each_one_fits() {
        // A run of expansions, each landing a box that still fits the window. The
        // view holds the reader's zoom throughout: it is the boxes around the
        // toggled one that an expansion spreads out, so chasing those would step
        // the zoom further out at every toggle.
        let chosen = 1.0;
        let mut camera = (chosen, (0.0, 0.0));
        for (i, along) in [200.0, 900.0, 1600.0, 2300.0].into_iter().enumerate() {
            let opened = Rect {
                x: along,
                y: along / 2.,
                w: 300.0,
                h: 220.0,
            };
            camera = settle_view(opened, vp(800., 600.), CONTENT, camera.0, camera.1, chosen);
            assert!(
                (camera.0 - chosen).abs() < 1e-4,
                "expansion {i} moved the zoom to {}",
                camera.0
            );
        }
    }

    #[test]
    fn settling_over_and_over_neither_walks_the_zoom_out_nor_strands_it() {
        // Expanding grows the toggled box past the window, so the view relaxes to
        // show it whole; collapsing shrinks it back within the window, so the view
        // returns to the zoom the reader set. Every repetition of the pair settles
        // at the same two zooms rather than starting from where the last left off.
        let chosen = 1.0;
        let opened = Rect {
            x: 0.0,
            y: 0.0,
            w: 2000.0,
            h: 1500.0,
        };
        let shut = Rect {
            x: 375.0,
            y: 280.0,
            w: 50.0,
            h: 40.0,
        };
        let mut camera = (chosen, (0.0, 0.0));
        let mut settled = Vec::new();
        for _ in 0..4 {
            camera = settle_view(opened, vp(800., 600.), CONTENT, camera.0, camera.1, chosen);
            let relaxed = camera.0;
            camera = settle_view(shut, vp(800., 600.), CONTENT, camera.0, camera.1, chosen);
            settled.push((relaxed, camera.0));
        }
        assert!(
            settled[0].0 < chosen,
            "the open target did not relax the view"
        );
        for (i, &(relaxed, back)) in settled.iter().enumerate() {
            assert!(
                (back - chosen).abs() < 1e-4,
                "toggle {i} came back to {back}, not to the reader's {chosen}"
            );
            assert!(
                (relaxed - settled[0].0).abs() < 1e-4,
                "toggle {i} relaxed to {relaxed}, not to {} as the first did",
                settled[0].0
            );
        }
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
        let (z, p) = settle_view(target, vp(800., 600.), CONTENT, 1.0, (0.0, 0.0), 1.0);
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

    /// The camera a wheel step of `notches` lands on.
    fn zoom_step(
        cursor: Point<Pixels>,
        camera: Camera,
        notches: f32,
        bounds: Bounds<Pixels>,
        content: (f32, f32),
    ) -> Camera {
        scale_about(
            cursor,
            camera,
            ZOOM_PER_NOTCH.powf(notches),
            bounds,
            content,
        )
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

    /// The graph read from `paths`, an aquery, a cquery and a query output in
    /// that order, as many of them as are given.
    fn source(paths: &[&str]) -> GraphSource {
        use bviz_ui_graph_args::InputKind;
        let mut files = bviz_ui_graph_args::GraphFiles::default();
        let kinds = [InputKind::Aquery, InputKind::Cquery, InputKind::Query];
        for (kind, path) in kinds.into_iter().zip(paths) {
            *files.file_mut(kind) = Some(path.into());
        }
        files.source().expect("the paths name a graph")
    }

    /// One graph is drawn however many files it merges, so its title names the
    /// first and counts the rest.
    #[test]
    fn a_graph_of_several_files_is_titled_by_the_first_and_a_count() {
        assert_eq!(title(&source(&["/tmp/aquery.pb"])), "aquery.pb");
        assert_eq!(
            title(&source(&["/tmp/aquery.pb", "/tmp/cquery.pb"])),
            "aquery.pb +1"
        );
        assert_eq!(
            title(&source(&[
                "/tmp/aquery.pb",
                "/tmp/cquery.pb",
                "/tmp/query.pb"
            ])),
            "aquery.pb +2"
        );
    }
}
