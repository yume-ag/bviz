//! A scatter plot of a graph's actions, configured targets or targets by two
//! of: how much of the build each pulls in, how much of it depends on it, and
//! how long its actions took.

mod axis;
mod hue;
mod landing;
mod paint;
mod pick;
mod placed;
mod plot;

use crate::CycleColouring;
use crate::Palette;
use crate::chips::{self, ChipBar};
use crate::colour;
use crate::drag::{DOUBLE_CLICK, past_slop};
use crate::exclusive::NOTICE_SEPARATOR;
use crate::hidden::{self, Offering, Unshown};
use crate::history::{Jumps, Stop};
use crate::holder::Holder;
use crate::hue::Hue;
use crate::jump::{self, JumpEvent, JumpPalette, Stands};
use crate::model::{
    ExclusionNamed, FilterChanged, GraphModel, Loaded, MetricsLanded, SelectionChanged,
    SourceReplaced, VerdictsLanded,
};
use crate::paint::UNMARKED_FADE;
use crate::point::{
    configuration_readings, identify, mnemonic, point_label, primary_output, unit_names,
};
use crate::reveal;
use crate::spawn_bytes::{NodeBytes, Unsized};
use crate::timing::{NodeTimes, Untimed};
use crate::toolbar::PickerGroup;
use crate::unit_picker::CyclePoints;
use crate::unit_picker::UnitPicker;
use axis::{Axes, Axis, Carried, Clocks, Overlay, Reported, Sizes};
use bviz_bazel_flags::TRANSITIONS;
use bviz_graph_layout::metrics::{Measurement, Metrics, Scope, Sizing, Unit};
use bviz_graph_layout::select::Verdicts;
use bviz_graph_layout::{Capabilities, Id, Input, InputSet, Kind, Support};
use bviz_invocation::Revision;
use bviz_time::Instant;
use bviz_ui_components::LabelLine;
use bviz_ui_components::gesture::{self, Clicks, Entry, GestureRow, Gestures, Held, Motion, Place};
use bviz_ui_components::{
    CardAnchor, DragEnd, Dropdown, DropdownEvent, HoverCard, KeyScope, Legend, LegendEntry,
    LegendState, ListScroll, MenuItem, Nesting, OneRow, OverflowRows, Presentation, Priority,
    SCROLLBAR_THICKNESS, ShowLegend, Swatch, Switch, ToggleGroup, ToggleSegment, ToolbarGroup,
    ZOOM_KEY_STEP, ZoomIn, ZoomOut, cached_layer, centered_spinner, defer_hover_card,
    defer_tooltip, legend_gestures, list_scrollbar, load_in_background, menu_chrome,
    tooltip_extent, track_drag,
};
use bviz_ui_components::{KeyStyle, key_text_within};
use bviz_ui_workspace::{Panel, PanelEvent, PanelKind, Role, ViewKind};
use gpui::{
    App, Bounds, Context, Div, Entity, EventEmitter, FocusHandle, Focusable, IntoElement,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, NavigationDirection, PinchEvent,
    Pixels, Point, Render, Rgba, ScrollStrategy, ScrollWheelEvent, SharedString, Size, Stateful,
    Subscription, Task, WeakEntity, Window, canvas, div, point, prelude::*, px, uniform_list,
};
use hue::{ColourBy, Colouring};
use landing::{Landing, PointIndex};
use menu_chrome::{ROW_INSET, SHEET_BORDER};
use pick::{Extents, PickList, PickRow};
use placed::Placed;
use plot::{MIN_RADIUS, Mark, Picked, Plotted, Spot, Zoom};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

/// The key context scoping the scatter plot's own bindings, so a key pressed
/// here reaches this panel and no other.
pub(crate) const KEY_CONTEXT: KeyScope = KeyScope::new("GraphScatter", "Scatter");

/// The identifier the scatter plot's key context carries while a list of the
/// points under a click is up, which the list's own bindings require.
pub(crate) const PICKING: KeyScope =
    KeyScope::within(&KEY_CONTEXT, Nesting::State, "picking", "Pick list");

/// The identifier the scatter plot's key context carries while a card offers
/// to show a node a jump could not land on.
pub(crate) const OFFERING: KeyScope = KeyScope::within(
    &KEY_CONTEXT,
    Nesting::State,
    "scatterOffering",
    "Hidden card",
);

gpui::actions!(
    bviz_graph,
    [
        /// Show whole plot
        ShowWholePlot,
        /// Aim at previous point
        AimUp,
        /// Aim at next point
        AimDown,
        /// Select point
        SelectAimed,
        /// Show point in graph
        ShowSelectedPoint,
        /// Close pick list
        ClosePickList,
        /// Cycle across axis
        CycleAcross,
        /// Cycle up axis
        CycleUp,
        /// Dismiss card or selection
        DismissCard,
    ]
);

/// How long the ring marking the points a jump lands on takes to spread
/// out and fade.
const PULSE: Duration = Duration::from_millis(600);

/// How wide the card offering to show a node stands.
const CARD_WIDTH: Pixels = px(300.);

/// What the panel reads while the graph is being measured.
const MEASURING: &str = "Computing metrics…";

/// The points of a graph, placed by two of their measures.
pub struct ScatterPanel {
    focus: FocusHandle,
    /// Measures the graph the first time this panel is shown.
    model: Entity<GraphModel>,
    /// Whether the workspace is showing the panel, which a graph landing while
    /// it is shown is measured for at once.
    shown: bool,
    /// What the plot measures, from the time the graph is read.
    choice: Option<Choice>,
    /// The measured points as drawn, from the time they land.
    plot: Option<Plot>,
    /// The part of each axis the plot drawn shows, which outlasts a replot in
    /// the same unit, since a filter moves no mark, and is the whole of each
    /// axis for the first plot of a unit.
    zoom: Zoom,
    /// The layer drawing the marks; notified through
    /// [`Self::invalidate_canvas`].
    canvas: Entity<ScatterCanvas>,
    /// The document's query bar, as this panel stands it.
    chips: Entity<ChipBar>,
    jump: Entity<JumpPalette>,
    /// The part of each axis shown as the palette opened, which closing it
    /// unpicked returns to.
    opened_zoom: Option<OpenedZoom>,
    /// When the last jump landed, which the marked points pulse from.
    pulsed: Option<Instant>,
    offering: Offering,
    /// The points jumped to, and the part of each axis shown before the
    /// first jump.
    jumps: Jumps<Zoom>,
    legend: Entity<LegendState>,
    /// Where the plot was last laid out, which a pointer is resolved against.
    bounds: Option<Bounds<Pixels>>,
    /// The press over the plot, until the button comes up again.
    press: Option<Press>,
    /// The card describing the point under the pointer.
    card: HoverCard<Id>,
    /// How far the list of the points under a click has scrolled.
    pick_scroll: ListScroll,
    /// Whether the last click selected a row of the list of points, whose
    /// double click's second press then lands on the plot the list stood over.
    row_selected: bool,
    /// Closes the list of the points under a click as focus leaves the panel;
    /// made the first time the panel is shown, where the window is in hand.
    closes_on_blur: Option<Subscription>,
    /// Plots the points through the graph's filter, absent while nothing is
    /// being plotted, so that one plotting runs at a time.
    plotting: Option<Task<()>>,
    /// Redraws as the graph is read and measured.
    _changed: Subscription,
    _source_replaced: Subscription,
    _metrics_landed: Subscription,
    _filter_changed: Subscription,
    _verdicts_landed: Subscription,
    _exclusion_named: Subscription,
    /// Marks the points of a node another view selects, leaving the axes as
    /// they are.
    _selection_changed: Subscription,
    /// Lands what the palette picks.
    _jumped: Subscription,
}

impl ScatterPanel {
    /// The mark the hover layer rings, hovered or aimed at on a list of
    /// points, in the plot it is drawn in, and what the card describing it
    /// reads while that is shown.
    fn ringed(&self) -> Option<(Arc<Plotted>, usize, Option<Vec<String>>)> {
        let plot = self.plot.as_ref()?;
        let (mark, lines) = match &plot.pointer {
            Pointer::Away => return None,
            Pointer::Hover(hover) => {
                let point = plot.plotted.marks[hover.mark].target.id;
                let lines = self.card.shows(&point).then(|| hover.lines.clone());
                (hover.mark, lines)
            }
            Pointer::Picking(pick) => (pick.rows[pick.aim?].mark, None),
        };
        Some((Arc::clone(&plot.plotted), mark, lines))
    }

    pub(crate) fn new(model: Entity<GraphModel>, cx: &mut Context<Self>) -> Self {
        let changed = cx.observe(&model, |view, _, cx| {
            if let Some(choice) = &view.choice {
                choice.explain(&view.model, cx);
            }
            view.replot(cx);
            cx.notify();
        });
        let source_replaced = cx.subscribe(&model, |view, _, _: &SourceReplaced, cx| {
            view.plot = None;
            view.pulsed = None;
            (view.chips).update(cx, |bar, cx| bar.set_unmarked(None, cx));
            view.opened_zoom = None;
            view.offering.clear();
            view.jumps = Jumps::default();
            view.restate_shown(cx);
            view.plotting = None;
            view.choice = Choice::of(&view.model, &view.focus, cx);
            if view.shown {
                view.measure(cx);
            }
        });
        let metrics_landed = cx.subscribe(&model, |view, _, landed: &MetricsLanded, cx| {
            let (scope, reference) = view.scopes(cx).unzip();
            if [scope, reference].contains(&Some(landed.key)) {
                view.replot(cx);
            }
        });
        let filter_changed = cx.subscribe(&model, |view, _, _: &FilterChanged, cx| {
            view.close_pick(cx);
            view.replot(cx);
        });
        let verdicts_landed =
            cx.subscribe(&model, |view, _, _: &VerdictsLanded, cx| view.replot(cx));
        let exclusion_named = cx.subscribe(&model, |_, _, _: &ExclusionNamed, cx| cx.notify());
        let selection_changed = cx.subscribe(&model, |view, _, event: &SelectionChanged, cx| {
            if event.by != cx.entity_id() {
                view.pulsed = None;
                view.remark(cx);
            }
        });
        let parent = cx.entity().downgrade();
        let focus = cx.focus_handle();
        let legend = cx.new(|cx| LegendState::new(focus.clone(), cx));
        let choice = Choice::of(&model, &focus, cx);
        let chips =
            cx.new(|cx| ChipBar::new(model.clone(), Holder::Scatter, focus.clone(), None, cx));
        let jump = Self::palette(&model, &focus, &chips, cx);
        let jumped = cx.subscribe(&jump, |view, _, event: &JumpEvent, cx| {
            view.jumped(*event, cx)
        });
        Self {
            focus,
            legend,
            chips,
            jump,
            opened_zoom: None,
            pulsed: None,
            offering: Offering::new(model.clone()),
            jumps: Jumps::default(),
            choice,
            model,
            shown: false,
            plot: None,
            zoom: Zoom::WHOLE,
            canvas: cx.new(|_| ScatterCanvas { parent }),
            bounds: None,
            press: None,
            card: HoverCard::new(cx, |view| &mut view.card, Self::carded),
            pick_scroll: ListScroll::new(),
            row_selected: false,
            closes_on_blur: None,
            plotting: None,
            _changed: changed,
            _source_replaced: source_replaced,
            _metrics_landed: metrics_landed,
            _filter_changed: filter_changed,
            _verdicts_landed: verdicts_landed,
            _exclusion_named: exclusion_named,
            _selection_changed: selection_changed,
            _jumped: jumped,
        }
    }

    /// The jump palette the plot stands, ranking its points first.
    fn palette(
        model: &Entity<GraphModel>,
        focus: &FocusHandle,
        chips: &Entity<ChipBar>,
        cx: &mut Context<Self>,
    ) -> Entity<JumpPalette> {
        let panel = cx.weak_entity();
        let drawn: chips::Drawn = Box::new(move |cx: &App| {
            let panel = panel.upgrade();
            let plot = panel
                .as_ref()
                .and_then(|panel| panel.read(cx).plot.as_ref());
            plot.map_or_else(Default::default, |plot| Arc::clone(plot.index.drawn()))
        });
        let panel = cx.weak_entity();
        let standing = Box::new(move |id, cx: &App| {
            let Some(panel) = panel.upgrade() else {
                return Stands::Here;
            };
            let panel = panel.read(cx);
            let (Some(plot), Some(loaded)) = (&panel.plot, panel.model.read(cx).loaded()) else {
                return Stands::Here;
            };
            let shared = &loaded.shared;
            (plot.index).stands(&shared.base, &shared.facets, &plot.verdicts, id)
        });
        let (model, focus, chips) = (model.clone(), focus.clone(), chips.clone());
        cx.new(|cx| {
            JumpPalette::new(
                model,
                Holder::Scatter,
                focus,
                chips,
                Some(drawn),
                standing,
                cx,
            )
        })
    }

    /// Opens the palette over the plot, noting the zoom it opened at.
    fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_pick(cx);
        let unit = self.scopes(cx).map(|(scope, _)| scope.unit());
        let scope = unit.map_or_else(jump::Scope::default, jump::Scope::of_unit);
        if self
            .jump
            .update(cx, |palette, cx| palette.open(scope, window, cx))
        {
            self.opened_zoom = self.plot.as_ref().map(|plot| OpenedZoom {
                zoom: self.zoom,
                unit: plot.scope.unit(),
                axes: plot.plotted.axes,
            });
        }
    }

    /// Lands what the palette asks of the plot.
    fn jumped(&mut self, event: JumpEvent, cx: &mut Context<Self>) {
        if let JumpEvent::Preview(id) = event {
            self.preview(id, cx);
            return;
        }
        let opened_zoom = self.opened_zoom.take();
        if let Some(plot) = &mut self.plot {
            plot.previewed.clear();
        }
        match event {
            JumpEvent::Preview(_) => {}
            JumpEvent::Go(id) => {
                let origin = match self.selected(cx) {
                    Some(selected) => Stop::Node(selected),
                    None => Stop::View(self.zoom),
                };
                self.jumps.visit(id, Some(origin));
                self.select(Some(id), cx);
                self.land(id, cx);
            }
            JumpEvent::AskWhy(id) => self.model.update(cx, |model, cx| model.ask_why(id, cx)),
            JumpEvent::AlwaysShow(id) => {
                // The pin goes on the point the filter hides, which may stand
                // for `id` rather than be it.
                let pinned = match self.landing(id, cx) {
                    Some(Landing::Hidden(point, _)) => point,
                    _ => id,
                };
                self.offering.always_show(id, pinned, cx);
            }
            JumpEvent::Closed => {
                let drawn_alike = |opened: &OpenedZoom| {
                    (self.plot.as_ref()).is_some_and(|plot| {
                        plot.scope.unit() == opened.unit && plot.plotted.axes == opened.axes
                    })
                };
                if let Some(opened) = opened_zoom.filter(drawn_alike) {
                    self.show_zoom(opened.zoom, cx);
                }
            }
            JumpEvent::Dismissed => {}
        }
        cx.notify();
    }

    /// What a jump to `id` lands on in the plot drawn; none while no plot is.
    fn landing(&self, id: Id, cx: &App) -> Option<Landing> {
        let plot = self.plot.as_ref()?;
        let shared = &self.model.read(cx).loaded()?.shared;
        Some((plot.index).landing(&shared.base, &shared.facets, &plot.verdicts, id))
    }

    /// Rings the points a jump to `id` would land on, and brings them into
    /// view below the palette, leaving what is selected as it is.
    fn preview(&mut self, id: Id, cx: &mut Context<Self>) {
        let marks = match self.landing(id, cx) {
            Some(Landing::Marks(marks)) => marks,
            _ => Vec::new(),
        };
        let clear_below = self.jump.read(cx).painted().map(|painted| painted.bottom());
        self.zoom_onto(&marks, clear_below, cx);
        if let Some(plot) = &mut self.plot {
            plot.previewed = marks;
        }
        cx.notify();
    }

    /// Marks the points `id` stands for, brings them into view and pulses
    /// them, listing them where there are several. Where none is drawn, the
    /// card says why and offers to show the node another way; with no plot
    /// drawn yet, the jump waits for one.
    fn land(&mut self, id: Id, cx: &mut Context<Self>) {
        self.offering.clear();
        let Some(landing) = self.landing(id, cx) else {
            self.offering.wait_for(id);
            return;
        };
        let unshown = match landing {
            Landing::Marks(marks) => {
                self.zoom_onto(&marks, None, cx);
                self.pulsed = Some(bviz_time::now());
                let spot = (self.area().zip(self.plot.as_ref()))
                    .map(|(area, plot)| area.spot(area.place(&plot.plotted.marks[marks[0]])));
                let several = (marks.len() > 1)
                    .then(|| Picked::of(marks.clone()))
                    .flatten();
                if let Some((spot, picked)) = spot.zip(several) {
                    self.open_pick(spot, picked, cx);
                }
                self.invalidate_canvas(cx);
                return;
            }
            Landing::Hidden(..) | Landing::Absent(_) => unshown(landing),
        };
        if let Some(unshown) = unshown {
            self.offering.stand(id, unshown, cx);
        }
        cx.notify();
    }

    /// The node every view of the graph marks as the one selected.
    fn selected(&self, cx: &App) -> Option<Id> {
        self.model.read(cx).selected()
    }

    /// Selects `node`, or nothing for `None`, for every view of the graph,
    /// and marks its points.
    fn select(&mut self, node: Option<Id>, cx: &mut Context<Self>) {
        self.pulsed = None;
        let by = cx.entity_id();
        self.model
            .update(cx, |model, cx| model.select(node, by, cx));
        self.remark(cx);
    }

    /// Marks the points of the node selected in the plot drawn, resolving
    /// them once for each node selected in each plot, and has the query bar's
    /// status line say where the plot marks none.
    fn remark(&mut self, cx: &mut Context<Self>) {
        let selected = self.selected(cx);
        let Some(plot) = &self.plot else {
            return;
        };
        if plot.selection.as_ref().map(|(id, _)| *id) != selected {
            let selection = selected.and_then(|id| Some((id, self.landing(id, cx)?)));
            if let Some(plot) = &mut self.plot {
                plot.selection = selection;
            }
            self.invalidate_canvas(cx);
        }
        let unmarked = (self.plot.as_ref())
            .and_then(|plot| plot.selection.as_ref())
            .and_then(|(_, landing)| landing.unmarked());
        self.chips
            .update(cx, |bar, cx| bar.set_unmarked(unmarked, cx));
    }

    /// Takes up a plot drawn afresh: marks the points of the node selected,
    /// moves the card to where its node now lands, and lands the jump waiting
    /// for the plot.
    fn replotted(&mut self, cx: &mut Context<Self>) {
        self.remark(cx);
        let offer = self.offering.offer();
        let now = offer
            .and_then(|offer| self.landing(offer.node, cx))
            .and_then(unshown);
        self.offering.redrawn(now, cx);
        if let Some(id) = self.offering.take_waiting() {
            self.land(id, cx);
        }
    }

    /// Returns to the point jumped to before the last, or to the axes shown
    /// before the first jump.
    fn go_back(&mut self, cx: &mut Context<Self>) {
        if let Some(stop) = self.jumps.back() {
            self.revisit(stop, cx);
        }
    }

    /// Returns to the point gone back from last.
    fn go_forward(&mut self, cx: &mut Context<Self>) {
        if let Some(stop) = self.jumps.forward() {
            self.revisit(stop, cx);
        }
    }

    /// Lands on `stop` again, leaving the jumps as they stand.
    fn revisit(&mut self, stop: Stop<Zoom>, cx: &mut Context<Self>) {
        match stop {
            Stop::Node(id) => {
                self.select(Some(id), cx);
                self.land(id, cx);
            }
            Stop::View(zoom) => {
                self.offering.clear();
                self.select(None, cx);
                self.show_zoom(zoom, cx);
            }
        }
        cx.notify();
    }

    /// Answers the card as its key does, leaving the key to the plot where
    /// the card offers only to close.
    fn accept(&mut self, cx: &mut Context<Self>) {
        match self.offering.accept(cx) {
            true => cx.notify(),
            false => cx.propagate(),
        }
    }

    /// Puts away the card, or with none the selection.
    fn dismiss(&mut self, cx: &mut Context<Self>) {
        if !self.offering.dismiss() {
            self.select(None, cx);
        }
        cx.notify();
    }

    /// Shows the part of each axis that brings `marks` into view below the
    /// height `clear_below`, or below the top of the plot.
    fn zoom_onto(&mut self, marks: &[usize], clear_below: Option<Pixels>, cx: &mut Context<Self>) {
        let (Some(area), Some(plot)) = (self.area(), &self.plot) else {
            return;
        };
        if marks.is_empty() {
            return;
        }
        let clear_below = clear_below.unwrap_or(area.frame().top());
        let zoom = area.onto(
            marks.iter().map(|&mark| &plot.plotted.marks[mark]),
            clear_below,
        );
        self.show_zoom(zoom, cx);
    }

    /// Shows the plot at `zoom`, naming nothing under the pointer.
    fn show_zoom(&mut self, zoom: Zoom, cx: &mut Context<Self>) {
        if zoom != self.zoom {
            self.zoom = zoom;
            self.close_pick(cx);
            self.invalidate_canvas(cx);
        }
    }

    /// The scope the plot measures in, and the scope of its unit over every
    /// input loaded that it is scaled to, once the graph is read.
    fn scopes(&self, cx: &App) -> Option<(Scope, Scope)> {
        self.choice.as_ref().map(|choice| choice.scopes(cx))
    }

    /// Has the graph measured in the chosen scope and the one it is scaled to,
    /// and brings the plot up to date where it already is.
    fn measure(&mut self, cx: &mut Context<Self>) {
        self.close_pick(cx);
        if let Some((scope, reference)) = self.scopes(cx) {
            self.model
                .update(cx, |model, cx| model.want_metrics(&[scope, reference], cx));
        }
        self.replot(cx);
    }

    /// Steps the picker `of` the choice to its next option.
    fn cycle(&self, of: impl FnOnce(&Choice) -> Entity<Dropdown>, cx: &mut Context<Self>) {
        if let Some(picker) = self.choice.as_ref().map(of) {
            picker.update(cx, |picker, cx| picker.cycle(cx));
        }
    }

    /// Measures the graph's points in the unit chosen, over the inputs last
    /// enabled for it.
    fn choose_unit(&mut self, cx: &mut Context<Self>) {
        if let Some(choice) = &self.choice {
            choice.name_colourings(cx);
            choice.explain(&self.model, cx);
        }
        self.measure(cx);
        cx.notify();
    }

    /// Colours the marks `by` what the picker offers.
    fn choose_colour(&mut self, by: ColourBy, cx: &mut Context<Self>) {
        let Some(choice) = &mut self.choice else {
            return;
        };
        if choice.colour == by {
            return;
        }
        choice.colour = by;
        self.close_pick(cx);
        self.replot(cx);
        cx.notify();
    }

    /// Places the points by the axes `choose` makes of those chosen.
    fn choose_axes(&mut self, choose: impl FnOnce(Axes) -> Axes, cx: &mut Context<Self>) {
        let Some(choice) = &mut self.choice else {
            return;
        };
        let axes = choose(choice.axes);
        if choice.axes == axes {
            return;
        }
        choice.axes = axes;
        choice.show_axes(cx);
        self.close_pick(cx);
        self.replot(cx);
        cx.notify();
    }

    /// Offers the choices reading an overlay only where the unit plotted
    /// carries it as `carried` says, taking down a list of points drawn by one
    /// it falls back from.
    fn offer(&mut self, carried: Carried, cx: &mut Context<Self>) {
        let Some(choice) = &mut self.choice else {
            return;
        };
        let fell_back = choice.offer(carried, cx);
        choice.explain(&self.model, cx);
        if fell_back {
            self.close_pick(cx);
            cx.notify();
        }
    }

    /// Enables `input` where it is disabled and disables it where it is
    /// enabled, unless that leaves no input describing the unit plotted.
    fn toggle_input(&mut self, input: Input, cx: &mut Context<Self>) {
        let Some(choice) = &mut self.choice else {
            return;
        };
        let inputs = choice.picker.chosen(cx).inputs();
        let inputs = match inputs.contains(input) {
            true => inputs.without(input),
            false => Some(inputs.with(input)),
        };
        if !inputs.is_some_and(|inputs| choice.picker.draw_over(inputs, cx)) {
            return;
        }
        self.measure(cx);
        cx.notify();
    }

    /// Plots the measured points through the verdicts of the document's
    /// filter, where the plot drawn is not of the chosen scope, axes and
    /// colouring, not drawn through those verdicts, or not of the overlays the
    /// graph holds, once the choices reading an overlay are offered as the
    /// unit's measure carries it. A hidden panel waits until it is shown, and a
    /// plotting running replots once it lands. Plotting passes over every
    /// point, so this runs off the UI thread, and the plot drawn stays until
    /// the new one lands.
    fn replot(&mut self, cx: &mut Context<Self>) {
        if !self.shown || self.plotting.is_some() {
            return;
        }
        let Some((_, reference)) = self.scopes(cx) else {
            return;
        };
        let model = self.model.read(cx);
        let carried = model
            .loaded()
            .zip(model.metrics(reference))
            .map(|(loaded, reference)| carried(loaded, reference));
        if let Some(carried) = carried {
            self.offer(carried, cx);
        }
        let Some(((scope, reference), choice)) = self.scopes(cx).zip(self.choice.as_ref()) else {
            return;
        };
        let (axes, colour) = (choice.axes, choice.colour);
        let Some(verdicts) = self.model.update(cx, |model, cx| model.verdicts(cx)) else {
            return;
        };
        let model = self.model.read(cx);
        let Some(loaded) = model.loaded() else {
            return;
        };
        let (timed, logged) = (loaded.timed(), loaded.logged());
        let (Some(metrics), Some(reference)) = (model.metrics(scope), model.metrics(reference))
        else {
            return;
        };
        if self.plot.as_ref().is_some_and(|plot| {
            Arc::ptr_eq(&plot.verdicts, &verdicts)
                && plot.scope == scope
                && plot.plotted.axes == axes
                && plot.colour == colour
                && plot.timed == timed
                && plot.logged == logged
        }) {
            return;
        }
        let clocks = Clocks::of(loaded.times(), &reference.points);
        let sizes = Sizes::of(loaded.bytes(), &reference.points);
        let (metrics, reference) = (Arc::clone(metrics), Arc::clone(reference));
        let shared = loaded.shared.clone();
        let Some(writers) = model.writers() else {
            return;
        };
        self.plotting = Some(load_in_background(
            cx,
            move || {
                let plots = |point: &Metrics| plotted_as(scope.unit(), point.kind);
                let colouring = Colouring::of(colour, scope.unit(), &reference, &shared.facets);
                let hue = |point: &Metrics| colouring.hue(point, &shared.facets, &clocks);
                let verdict = |id| verdicts.get(id);
                let reported = |id| Reported {
                    time: clocks.time(id),
                    bytes: sizes.get(id),
                };
                let plotted =
                    Plotted::of(&metrics, &reference, axes, reported, plots, verdict, hue);
                let writers = Arc::clone(writers.wait());
                let index = PointIndex::of(&metrics, &plotted, plots, &shared.base, writers);
                let points = metrics.points.iter().filter(|point| {
                    plots(point) && axes.place(point, reported(point.id)).is_some()
                });
                let legend = colouring.legend(points, &shared.facets, &clocks);
                (plotted, index, legend, verdicts)
            },
            move |view, (plotted, index, legend, verdicts), cx| {
                view.plotting = None;
                if view.plot.as_ref().is_none_or(|plot| {
                    plot.scope.unit() != scope.unit()
                        || plot.plotted.axes != axes
                        || (plot.timed != timed && axes.reads(Overlay::Times))
                        || (plot.logged != logged && axes.reads(Overlay::Bytes))
                }) {
                    view.zoom = Zoom::WHOLE;
                }
                view.plot = Some(Plot {
                    plotted: Arc::new(plotted),
                    index,
                    legend,
                    pointer: Pointer::Away,
                    verdicts,
                    scope,
                    colour,
                    timed,
                    logged,
                    selection: None,
                    previewed: Vec::new(),
                });
                view.replotted(cx);
                view.restate_shown(cx);
                view.invalidate_canvas(cx);
                view.replot(cx);
            },
        ));
    }

    /// Tells the query bar how many points of its unit the plot drawn shows
    /// of those it plots.
    fn restate_shown(&self, cx: &mut Context<Self>) {
        let shown = self.plot.as_ref().map(|plot| chips::Shown {
            shown: plot.plotted.marks.len(),
            of: plot.plotted.plotted,
            many: unit_names(plot.scope.unit()).many,
            note: None,
        });
        self.chips.update(cx, |bar, cx| bar.set_shown(shown, cx));
    }

    /// Repaints the marks along with the layer over them; every change to what
    /// the marks show routes through here.
    fn invalidate_canvas(&self, cx: &mut Context<Self>) {
        self.canvas.update(cx, |_, cx| cx.notify());
        cx.notify();
    }

    /// Where the plot places values, where it is drawn.
    fn area(&self) -> Option<plot::DataArea> {
        paint::data_area(&self.plot.as_ref()?.plotted, self.zoom, self.bounds?)
    }

    /// The mark a pointer at `at` picks, where the plot is drawn.
    fn pick(&self, at: Point<Pixels>) -> Option<Picked> {
        let marks = &self.plot.as_ref()?.plotted.marks;
        self.area()?.pick(marks, at)
    }

    /// Shows the plot at `zoom`, and names what the pointer at `at` is over
    /// there unless a press is in progress, whose release names it.
    fn zoom_to(&mut self, zoom: Zoom, at: Point<Pixels>, cx: &mut Context<Self>) {
        if zoom == self.zoom {
            return;
        }
        self.zoom = zoom;
        self.close_pick(cx);
        self.invalidate_canvas(cx);
        if self.press.is_none() {
            self.hover_at(at, cx);
        }
    }

    fn on_scroll_wheel(&mut self, e: &ScrollWheelEvent, cx: &mut Context<Self>) {
        let delta = gesture::scroll_pixels(e);
        GESTURES.dispatch(gesture::Pointer::scroll(e), |intent| {
            let by = match intent {
                ScatterIntent::Zoom => {
                    self.zoom_about(e.position, gesture::wheel_zoom_factor(e), cx);
                    return true;
                }
                ScatterIntent::PanAcross => point(delta.x + delta.y, px(0.)),
                ScatterIntent::Scroll => delta,
                _ => return false,
            };
            if let Some(area) = self.area() {
                self.zoom_to(area.panned(by), e.position, cx);
            }
            true
        });
    }

    fn on_pinch(&mut self, e: &PinchEvent, cx: &mut Context<Self>) {
        GESTURES.dispatch(gesture::Pointer::pinch(e), |intent| match intent {
            ScatterIntent::Zoom => {
                self.zoom_about(e.position, gesture::pinch_factor(e), cx);
                true
            }
            _ => false,
        });
    }

    /// Zooms `factor` times closer about the window-space point `at`.
    fn zoom_about(&mut self, at: Point<Pixels>, factor: f32, cx: &mut Context<Self>) {
        if let Some(area) = self.area() {
            self.zoom_to(area.zoomed(at, factor), at, cx);
        }
    }

    /// Zooms `factor` times closer about the middle of the plot, naming what
    /// the pointer is over there.
    fn zoom_centred(&mut self, factor: f32, window: &Window, cx: &mut Context<Self>) {
        if let Some(area) = self.area() {
            let zoom = area.zoomed(area.frame().center(), factor);
            self.zoom_to(zoom, window.mouse_position(), cx);
        }
    }

    /// Acts on a press as [`GESTURES`] has it. A double click shows the whole
    /// of each axis only clear of every mark: on a mark it shows in the graph
    /// the point its first click selected. A press beside an open list of
    /// points closes it and goes on as a press on the plot. The second press
    /// of a double click whose first put the list up or selected one of its
    /// rows goes no further.
    fn on_mouse_down(&mut self, e: &MouseDownEvent, cx: &mut Context<Self>) {
        let row_selected = std::mem::take(&mut self.row_selected);
        if e.click_count >= DOUBLE_CLICK && (row_selected || self.picking()) {
            return;
        }
        let closed_list = self.picking();
        self.close_pick(cx);
        GESTURES.dispatch(gesture::Pointer::press(e), |intent| match intent {
            ScatterIntent::ShowWholePlot if self.pick(e.position).is_none() => {
                self.press = None;
                self.zoom_to(Zoom::WHOLE, e.position, cx);
                true
            }
            ScatterIntent::Pan => {
                self.press = Some(Press {
                    from: e.position,
                    last: e.position,
                    dragged: false,
                    click: gesture::Pointer::completing(e),
                    closed_list,
                });
                // The frame this asks for installs the handlers carrying the
                // drag.
                cx.notify();
                true
            }
            _ => false,
        });
    }

    /// Walks the jumps as a mouse's back or forward button asks.
    fn on_navigate(&mut self, e: &MouseDownEvent, cx: &mut Context<Self>) {
        GESTURES.dispatch(gesture::Pointer::press(e), |intent| match intent {
            ScatterIntent::GoBack => {
                self.go_back(cx);
                true
            }
            ScatterIntent::GoForward => {
                self.go_forward(cx);
                true
            }
            _ => false,
        });
    }

    /// Follows the pointer to the mark it picks, repainting only where that
    /// changes what the tooltip reads. An open list of points holds the
    /// pointer, which names nothing else while it is up.
    fn hover_at(&mut self, at: Point<Pixels>, cx: &mut Context<Self>) {
        let picked = self.pick(at);
        let point = picked
            .as_ref()
            .and_then(|picked| self.point_of(picked.mark));
        self.card.hover(at, point.as_ref());
        let model = self.model.read(cx);
        let Some(plot) = &mut self.plot else {
            return;
        };
        let named = match &plot.pointer {
            Pointer::Picking(_) => return,
            Pointer::Away => None,
            Pointer::Hover(hover) => Some((hover.mark, hover.others)),
        };
        if named == picked.as_ref().map(|picked| (picked.mark, picked.others())) {
            return;
        }
        let unit = plot.scope.unit();
        let hover = picked.and_then(|picked| {
            let mark = &plot.plotted.marks[picked.mark];
            let lines = describe(mark, unit, &plot.plotted, picked.others(), model.loaded()?);
            Some(Hover {
                mark: picked.mark,
                others: picked.others(),
                lines,
            })
        });
        plot.pointer = hover.map_or(Pointer::Away, Pointer::Hover);
        cx.notify();
    }

    /// Carries the zoom along with a press dragged past a click's reach.
    fn drag_to(&mut self, at: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(press) = &mut self.press else {
            return;
        };
        let starts = !press.dragged && past_slop(press.from, at);
        press.dragged |= starts;
        if !press.dragged {
            return;
        }
        let by = at - press.last;
        press.last = at;
        self.card.drag(at);
        if starts {
            cx.notify();
        }
        if let Some(area) = self.area() {
            self.zoom_to(area.panned(by), at, cx);
        }
    }

    /// Ends the press, which where it was a click acts as [`GESTURES`] has
    /// it. Taking the press makes a second call for the same release a no-op.
    fn end_press(&mut self, at: Point<Pixels>, end: DragEnd, cx: &mut Context<Self>) {
        let Some(press) = self.press.take() else {
            return;
        };
        cx.notify();
        self.hover_at(at, cx);
        if press.dragged || !matches!(end, DragEnd::Released) {
            return;
        }
        GESTURES.dispatch(press.click, |intent| match intent {
            ScatterIntent::Select => self.select_at(at, cx),
            ScatterIntent::Deselect => {
                if !press.closed_list {
                    self.select(None, cx);
                }
                true
            }
            ScatterIntent::Reveal => self.reveal_at(at, cx),
            _ => false,
        });
    }

    /// Selects the point under `at`, or lists the points there where it lands
    /// on several; reports whether a point is there.
    fn select_at(&mut self, at: Point<Pixels>, cx: &mut Context<Self>) -> bool {
        let (Some(picked), Some(area)) = (self.pick(at), self.area()) else {
            return false;
        };
        match picked.covered() {
            &[mark] => self.select_mark(mark, cx),
            _ => self.open_pick(area.spot(at), picked, cx),
        }
        true
    }

    /// Selects the point the plotted mark `mark` stands for, whichever was
    /// selected before, so the first click of a double click that goes on to
    /// show it in the graph leaves it selected.
    fn select_mark(&mut self, mark: usize, cx: &mut Context<Self>) {
        let point = self.point_of(mark);
        if point.is_some() {
            self.select(point, cx);
        }
    }

    /// Selects the point under `at`, where only one is, and shows the
    /// graph's drawing of it; reports whether it did, which it can only with
    /// a graph panel open.
    fn reveal_at(&mut self, at: Point<Pixels>, cx: &mut Context<Self>) -> bool {
        let point = match self.pick(at).as_ref().map(Picked::covered) {
            Some(&[mark]) => self.point_of(mark),
            _ => None,
        };
        let Some(point) = point.filter(|_| self.model.read(cx).reveals()) else {
            return false;
        };
        self.select(Some(point), cx);
        self.model.update(cx, |model, cx| model.reveal(point, cx));
        true
    }

    /// Shows the graph's drawing of the node selected, where a graph panel is
    /// open.
    fn show_selected(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = self.selected(cx) {
            self.model.update(cx, |model, cx| model.reveal(id, cx));
        }
    }

    /// Drops the card, which names the mark under a pointer that has left.
    fn clear_hover(&mut self, cx: &mut Context<Self>) {
        let mut cleared = self.card.leave();
        if let Some(plot) = &mut self.plot
            && let Pointer::Hover(_) = plot.pointer
        {
            plot.pointer = Pointer::Away;
            cleared = true;
        }
        if cleared {
            cx.notify();
        }
    }

    /// The point the card describes.
    fn carded(&self, _: &App) -> Option<Id> {
        match &self.plot.as_ref()?.pointer {
            Pointer::Hover(hover) => self.point_of(hover.mark),
            Pointer::Away | Pointer::Picking(_) => None,
        }
    }

    /// The point the plotted mark `mark` stands for, which names it across
    /// replots that renumber the marks.
    fn point_of(&self, mark: usize) -> Option<Id> {
        Some(self.plot.as_ref()?.plotted.marks[mark].target.id)
    }

    /// Whether a list of the points under a click is up.
    fn picking(&self) -> bool {
        self.plot
            .as_ref()
            .is_some_and(|plot| matches!(plot.pointer, Pointer::Picking(_)))
    }

    /// Puts up the list of every point `picked` covers under a click at `spot`.
    fn open_pick(&mut self, spot: Spot, picked: Picked, cx: &mut Context<Self>) {
        let model = self.model.read(cx);
        let (Some(plot), Some(loaded)) = (&mut self.plot, model.loaded()) else {
            return;
        };
        let (unit, covered) = (plot.scope.unit(), picked.covered());
        let mut named = Vec::with_capacity(covered.len());
        let mut readings = Vec::with_capacity(covered.len());
        for &mark in covered {
            let (mnemonic, label, read) =
                pick_reading(&plot.plotted.marks[mark].target, unit, loaded);
            named.push((mark, mnemonic, label));
            readings.push(read);
        }
        let keys: Vec<(Option<&str>, &str)> = named
            .iter()
            .map(|(_, mnemonic, label)| (*mnemonic, label.as_str()))
            .collect();
        let hints = crate::hints::hints(&keys, readings);
        let mut rows: Vec<PickRow> = named
            .iter()
            .zip(hints)
            .map(|((mark, mnemonic, label), hint)| {
                let drawn = &plot.plotted.marks[*mark];
                let point = &drawn.target;
                PickRow {
                    mark: *mark,
                    mnemonic: mnemonic.map(|mnemonic| mnemonic.to_owned().into()),
                    label: label.clone().into(),
                    hint: hint.map(SharedString::from),
                    measures: plot.plotted.axes.values(point, drawn.reported).into(),
                    x: drawn.x,
                    y: drawn.y,
                    tooltip: describe(drawn, unit, &plot.plotted, 0, loaded),
                }
            })
            .collect();
        pick::order(&mut rows);
        plot.pointer = Pointer::Picking(PickList {
            spot,
            rows: rows.into(),
            aim: None,
            extents: None,
        });
        self.pick_scroll.to_top();
        cx.notify();
    }

    /// Takes down the list of the points under a click, where one is up.
    fn close_pick(&mut self, cx: &mut Context<Self>) {
        if let Some(plot) = &mut self.plot
            && let Pointer::Picking(_) = plot.pointer
        {
            plot.pointer = Pointer::Away;
            cx.notify();
        }
    }

    /// Aims at the list's row `row`, which rings its mark and describes its
    /// point beside the list.
    fn aim_at(&mut self, row: usize, cx: &mut Context<Self>) {
        let Some(Pointer::Picking(pick)) = self.plot.as_mut().map(|plot| &mut plot.pointer) else {
            return;
        };
        if pick.aim == Some(row) || row >= pick.rows.len() {
            return;
        }
        pick.aim = Some(row);
        cx.notify();
    }

    /// Moves the aim `by` rows through the list, and brings the row it lands
    /// on into view.
    fn step_aim(&mut self, by: isize, cx: &mut Context<Self>) {
        let Some(Pointer::Picking(pick)) = self.plot.as_ref().map(|plot| &plot.pointer) else {
            return;
        };
        if let Some(row) = pick::stepped(pick.aim, by, pick.rows.len()) {
            self.aim_at(row, cx);
            let scroll = self.pick_scroll.uniform_handle();
            scroll.scroll_to_item(row, ScrollStrategy::Nearest);
        }
    }

    /// Selects the point on the list's row `row`, and takes the list down.
    fn select_row(&mut self, row: usize, cx: &mut Context<Self>) {
        let Some(Pointer::Picking(pick)) = self.plot.as_ref().map(|plot| &plot.pointer) else {
            return;
        };
        let Some(mark) = pick.rows.get(row).map(|listed| listed.mark) else {
            return;
        };
        self.close_pick(cx);
        self.select_mark(mark, cx);
        self.row_selected = true;
    }

    /// Selects the point on the row aimed at.
    fn select_aimed(&mut self, cx: &mut Context<Self>) {
        let Some(Pointer::Picking(pick)) = self.plot.as_ref().map(|plot| &plot.pointer) else {
            return;
        };
        if let Some(row) = pick.aim {
            self.select_row(row, cx);
        }
    }
}

/// The part of each axis a plot showed as the palette opened, with what
/// the plot measured and on which axes, which a zoom only means alike.
#[derive(Clone, Copy)]
struct OpenedZoom {
    zoom: Zoom,
    unit: Unit,
    axes: Axes,
}

/// Why the plot shows no part of a node a jump to which lands as `landing`;
/// none where it lands on marks.
fn unshown(landing: Landing) -> Option<Unshown> {
    match landing {
        Landing::Marks(_) => None,
        Landing::Hidden(point, by) => Some(Unshown::Hidden { point, by }),
        Landing::Absent(absent) => Some(Unshown::Absent(absent.describe())),
    }
}

/// The choice of what the plot measures, which the strip offers.
struct Choice {
    /// The inputs the graph was read from.
    loaded: InputSet,
    picker: UnitPicker,
    axes: Axes,
    /// Which overlays some point of the unit plotted carries, by which the
    /// choices reading them are offered.
    carried: Carried,
    across_picker: Entity<Dropdown>,
    _across_picked: Subscription,
    up_picker: Entity<Dropdown>,
    _up_picked: Subscription,
    colour: ColourBy,
    colour_picker: Entity<Dropdown>,
    _colour_picked: Subscription,
}

impl Choice {
    /// The finest unit the graph `model` has read is measured in, over every
    /// input describing it; absent until the graph is read.
    fn of(
        model: &Entity<GraphModel>,
        focus: &FocusHandle,
        cx: &mut Context<ScatterPanel>,
    ) -> Option<Self> {
        let loaded = model.read(cx).loaded()?;
        let inputs = loaded.shared.inputs;
        // Until the unit is measured, which overlays any of its points carries
        // is taken from which any node of the graph does.
        let carried = Carried {
            times: loaded.times().is_ok(),
            bytes: loaded.bytes().is_ok(),
            inputs: loaded.bytes().is_ok_and(NodeBytes::inputs_measured),
        };
        let picker = UnitPicker::new(
            "scatter-unit",
            "Point unit",
            inputs,
            None,
            focus,
            ScatterPanel::choose_unit,
            cx,
        )?;
        let axes = Axes::OPENING;
        let names = Axis::ALL.map(Axis::name);
        let across_picker = cx.new(|cx| {
            Dropdown::new("scatter-across", names, axes.x().index(), cx)
                .keys_for(&CycleAcross, focus)
        });
        let across_picked = cx.subscribe(&across_picker, |view, _, event, cx| {
            let DropdownEvent::Changed(index) = event;
            if let Some(&axis) = Axis::ALL.get(*index) {
                view.choose_axes(|axes| axes.across(axis), cx);
            }
        });
        let up_picker = cx.new(|cx| {
            Dropdown::new("scatter-up", names, axes.y().index(), cx).keys_for(&CycleUp, focus)
        });
        let up_picked = cx.subscribe(&up_picker, |view, _, event, cx| {
            let DropdownEvent::Changed(index) = event;
            if let Some(&axis) = Axis::ALL.get(*index) {
                view.choose_axes(|axes| axes.up(axis), cx);
            }
        });
        let unit = picker.chosen(cx).unit();
        let names = ColourBy::ALL.map(|by| by.label(unit));
        let colour_picker = cx.new(|cx| {
            Dropdown::new("scatter-colour", names, ColourBy::OPENING.index(), cx)
                .keys_for(&CycleColouring, focus)
        });
        let colour_picked = cx.subscribe(&colour_picker, |view, _, event, cx| {
            let DropdownEvent::Changed(index) = event;
            if let Some(&by) = ColourBy::ALL.get(*index) {
                view.choose_colour(by, cx);
            }
        });
        let mut choice = Self {
            loaded: inputs,
            picker,
            axes,
            across_picker,
            _across_picked: across_picked,
            up_picker,
            _up_picked: up_picked,
            carried,
            colour: ColourBy::OPENING,
            colour_picker,
            _colour_picked: colour_picked,
        };
        choice.offer(carried, cx);
        choice.explain(model, cx);
        Some(choice)
    }

    /// Has each picker offering a choice that reads an overlay say what the
    /// graph `model` holds of it.
    fn explain(&self, model: &Entity<GraphModel>, cx: &mut Context<ScatterPanel>) {
        let unit = self.picker.chosen(cx).unit();
        let overlays = Overlays::of(model.read(cx).loaded());
        let notes = [
            (
                &self.across_picker,
                axis_note(overlays, self.carried, unit, ACROSS_HINT),
            ),
            (
                &self.up_picker,
                axis_note(overlays, self.carried, unit, UP_HINT),
            ),
            (
                &self.colour_picker,
                times_note(overlays.times, self.carried.times, unit, COLOUR_HINT),
            ),
        ];
        for (picker, note) in notes {
            picker.update(cx, |picker, cx| picker.set_tooltip(note, cx));
        }
    }

    /// Names each colouring the picker offers as it reads for the unit
    /// plotted.
    fn name_colourings(&self, cx: &mut Context<ScatterPanel>) {
        let unit = self.picker.chosen(cx).unit();
        self.colour_picker.update(cx, |picker, cx| {
            for by in ColourBy::ALL {
                picker.set_label(by.index(), by.label(unit), cx);
            }
        });
    }

    /// Shows on each axis's picker the measure chosen for it.
    fn show_axes(&self, cx: &mut Context<ScatterPanel>) {
        let axes = self.axes;
        let pickers = [(&self.across_picker, axes.x()), (&self.up_picker, axes.y())];
        for (picker, axis) in pickers {
            picker.update(cx, |picker, cx| picker.set_selected(axis.index(), cx));
        }
    }

    /// Offers the choices reading an overlay only where the unit plotted
    /// carries it as `carried` says, falling back from any of them chosen
    /// where it does not, and shows what is chosen as chosen. Whether a
    /// choice fell back.
    fn offer(&mut self, carried: Carried, cx: &mut Context<ScatterPanel>) -> bool {
        self.carried = carried;
        let timed = carried.times;
        let available = self.axes.available(carried);
        let axes_fall = available != self.axes;
        self.axes = available;
        let colour_falls = !timed && self.colour == ColourBy::Runner;
        if colour_falls {
            self.colour = ColourBy::Identity;
        }
        for picker in [&self.across_picker, &self.up_picker] {
            picker.update(cx, |picker, cx| {
                for offered in Axis::ALL {
                    picker.set_available(offered.index(), offered.available(carried), cx);
                }
            });
        }
        self.show_axes(cx);
        let colour = self.colour;
        self.colour_picker.update(cx, |picker, cx| {
            for offered in ColourBy::ALL {
                let available = timed || offered != ColourBy::Runner;
                picker.set_available(offered.index(), available, cx);
            }
            picker.set_selected(colour.index(), cx);
        });
        axes_fall || colour_falls
    }

    /// The scope the plot measures in, and that scope's unit over every loaded
    /// input describing it, whose measure the plot is scaled to.
    fn scopes(&self, cx: &App) -> (Scope, Scope) {
        let scope = self.picker.chosen(cx);
        (scope, scope.widened(self.loaded))
    }
}

/// How the toggle for one loaded input stands against the unit plotted.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Toggle {
    On,
    Off,
    /// On, and the only input on describing the unit, which a measure needs.
    Lone,
    /// The input describes nothing of the unit plotted.
    Ignored,
}

impl Toggle {
    /// How the toggle for the loaded input `input` stands against a plot
    /// measuring in `scope`.
    fn of(scope: Scope, input: Input) -> Self {
        let unit = scope.unit();
        let inputs = scope.inputs();
        if !unit.inputs().contains(input) {
            Toggle::Ignored
        } else if !inputs.contains(input) {
            Toggle::Off
        } else if inputs.without(input).is_none() {
            Toggle::Lone
        } else {
            Toggle::On
        }
    }
}

fn input_name(input: Input) -> &'static str {
    match input {
        Input::Query => "query",
        Input::Cquery => "cquery",
        Input::Aquery => "aquery",
    }
}

/// The measured points as drawn, and what among them the pointer is on.
struct Plot {
    plotted: Arc<Plotted>,
    /// Where each node stands among the points `plotted` draws.
    index: PointIndex,
    /// Each hue the plotted points are drawn in, and what it stands for.
    legend: Vec<(SharedString, Hue)>,
    pointer: Pointer,
    /// What `plotted` was drawn through.
    verdicts: Arc<Verdicts>,
    /// What `plotted` measures.
    scope: Scope,
    /// What the marks of `plotted` are coloured by.
    colour: ColourBy,
    /// Which of its session's timings `plotted` was drawn with.
    timed: Revision,
    /// Which of its session's logged spawns `plotted` was drawn with.
    logged: Revision,
    /// What the node selected lands on in this plot, and that node.
    selection: Option<(Id, Landing)>,
    /// The marks standing for the node the palette's keys are on.
    previewed: Vec<usize>,
}

impl Plot {
    /// The marks standing for the node selected.
    fn selected(&self) -> &[usize] {
        match &self.selection {
            Some((_, Landing::Marks(marks))) => marks,
            _ => &[],
        }
    }
}

/// What the panel does for a gesture in [`GESTURES`].
#[derive(Clone, Copy)]
enum ScatterIntent {
    ShowWholePlot,
    Pan,
    Select,
    Deselect,
    Reveal,
    SelectListed,
    Zoom,
    PanAcross,
    Scroll,
    Describe,
    GoBack,
    GoForward,
}

const POINT: Place = Place::On("a point");

const SHOW_WHOLE: Entry<ScatterIntent> = Entry::new(
    gesture::Input::new(Motion::Press(gesture::Button::Left, Clicks::Double)),
    Place::Empty,
    "Show whole plot",
    ScatterIntent::ShowWholePlot,
);

const GESTURES: Gestures<ScatterIntent> = Gestures::new(&[
    SHOW_WHOLE,
    Entry::new(
        gesture::Input::drag(gesture::Button::Left),
        Place::Anywhere,
        "Pan",
        ScatterIntent::Pan,
    ),
    Entry::new(
        gesture::Input::new(Motion::Click(gesture::Button::Left, Clicks::Single)),
        POINT,
        "Select point or list points",
        ScatterIntent::Select,
    ),
    Entry::new(
        gesture::Input::new(Motion::Click(gesture::Button::Left, Clicks::Single)),
        Place::Empty,
        "Clear selection",
        ScatterIntent::Deselect,
    ),
    Entry::new(
        gesture::Input::new(Motion::Click(gesture::Button::Left, Clicks::Double)),
        POINT,
        "Show point in graph",
        ScatterIntent::Reveal,
    ),
    Entry::new(
        gesture::Input::click(gesture::Button::Left),
        Place::On("a listed point"),
        "Select point",
        ScatterIntent::SelectListed,
    ),
    Entry::new(
        gesture::Input::SCROLL.holding(Held::SECONDARY),
        Place::Anywhere,
        "Zoom about pointer",
        ScatterIntent::Zoom,
    ),
    Entry::new(
        gesture::Input::PINCH,
        Place::Anywhere,
        "Zoom about pointer",
        ScatterIntent::Zoom,
    ),
    Entry::new(
        gesture::Input::SCROLL.holding(Held::SHIFT),
        Place::Anywhere,
        "Pan across",
        ScatterIntent::PanAcross,
    ),
    Entry::new(
        gesture::Input::SCROLL,
        Place::Anywhere,
        "Pan",
        ScatterIntent::Scroll,
    ),
    Entry::new(
        gesture::Input::HOVER,
        POINT,
        "Describe point",
        ScatterIntent::Describe,
    ),
    Entry::new(
        gesture::Input::press(gesture::Button::Back),
        Place::Anywhere,
        "Go back",
        ScatterIntent::GoBack,
    ),
    Entry::new(
        gesture::Input::press(gesture::Button::Forward),
        Place::Anywhere,
        "Go forward",
        ScatterIntent::GoForward,
    ),
]);

/// A press over the plot, which is a click until it is dragged past
/// [`CLICK_SLOP`](crate::drag::CLICK_SLOP) and pans the plot from then on.
struct Press {
    from: Point<Pixels>,
    /// Where the pointer was at the last step of the pan.
    last: Point<Pixels>,
    dragged: bool,
    /// The click a release short of [`CLICK_SLOP`] makes of the press.
    click: gesture::Pointer,
    /// Whether the press took down a list of points, which is all a click
    /// clear of every mark then does.
    closed_list: bool,
}

/// What among a plot's marks the pointer is on.
enum Pointer {
    Away,
    Hover(Hover),
    /// A click landing on several marks put up the list of their points,
    /// which holds the pointer until it closes.
    Picking(PickList),
}

/// A mark under the pointer, by its index among the plotted marks, and what its
/// tooltip reads.
struct Hover {
    mark: usize,
    /// How many other marks are under the pointer with it.
    others: usize,
    lines: Vec<String>,
}

/// Whether a measured point of `kind` is plotted as a point of `unit`: a
/// source file is a point of a measure of actions, but no action.
fn plotted_as(unit: Unit, kind: Kind) -> bool {
    unit != Unit::Action || kind == Kind::Action
}

/// How many points of `unit` the plot plots; how many of them the filter
/// shows is the query bar's to say.
fn plotted_status(plotted: &Plotted, unit: Unit) -> String {
    let drawn = format!("{} {}", plotted.plotted, unit_names(unit).many);
    match plotted.estimated {
        true => format!("{drawn}, counts estimated"),
        false => drawn,
    }
}

/// What the tooltip over `mark`, a point of `unit` drawn in `plotted`, reads,
/// where a pick naming it covers `others` points besides: what the point is,
/// then its measures.
fn describe(
    mark: &Mark,
    unit: Unit,
    plotted: &Plotted,
    others: usize,
    loaded: &Loaded,
) -> Vec<String> {
    let (point, reported) = (&mark.target, mark.reported);
    let mut lines = Vec::with_capacity(TOOLTIP_LINES);
    identify(point.id, point.kind, unit, &loaded.shared, &mut lines);
    lines.push(plotted.axes.readings(point, reported));
    lines.extend(plotted.axes.unplaced(point, reported));
    if let Hue::Ran(ran) = mark.hue {
        lines.push(ran.label().to_owned());
    }
    lines.push(format!(
        "fan-in {} · fan-out {}",
        point.fan_in, point.fan_out
    ));
    if plotted.sizing == Sizing::Actions {
        let names = unit_names(Unit::Action);
        let actions = match point.actions {
            1 => names.one,
            _ => names.many,
        };
        lines.push(format!("{} {actions}", point.actions));
    }
    if others > 0 {
        lines.push(format!("+{others} more here"));
    }
    lines
}

/// What a row of the list of points under a click reads for `point`, a point
/// of `unit` — for an action its mnemonic, and the label of its target — and
/// each reading that could tell it apart from points whose rows read alike,
/// most compact first: for an action, the name of the file it writes, the
/// readings of its configuration, that file's path in its configuration's
/// output tree, and the whole path; for a configured target, the readings of
/// its configuration.
fn pick_reading<'a>(
    point: &Metrics,
    unit: Unit,
    loaded: &'a Loaded,
) -> (Option<&'a str>, String, Vec<Option<String>>) {
    let shared = &loaded.shared;
    let label = point_label(point.id, shared);
    let [short, whole] = configuration_readings(point.id, shared);
    match unit {
        Unit::Action => {
            let output = primary_output(point.id, shared);
            let file = output.map(|output| output.rsplit('/').next().unwrap_or(output));
            let readings = vec![
                file.map(str::to_owned),
                short,
                whole,
                output.map(|output| pick::in_output_tree(output).to_owned()),
                output.map(str::to_owned),
            ];
            (Some(mnemonic(point.id, shared)), label, readings)
        }
        Unit::ConfiguredTarget => (None, label, vec![short, whole]),
        Unit::Target => (None, label, Vec::new()),
    }
}

/// The most lines a tooltip reads.
const TOOLTIP_LINES: usize = 10;

impl EventEmitter<PanelEvent> for ScatterPanel {}

impl Focusable for ScatterPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Panel for ScatterPanel {
    fn kind() -> PanelKind {
        PanelKind {
            view: ViewKind::new("graph-scatter"),
            key_context: KEY_CONTEXT,
            role: Role::Canvas,
        }
    }

    fn gestures() -> Vec<GestureRow> {
        (GESTURES.rows())
            .chain(jump::gestures())
            .chain(chips::gestures())
            .chain(legend_gestures())
            .collect()
    }

    fn status(&self, cx: &App) -> Option<SharedString> {
        let model = self.model.read(cx);
        let status = model.source().status("No graph", |_| match &self.plot {
            None => MEASURING.into(),
            Some(plot) => {
                let zoomed = (self.zoom != Zoom::WHOLE).then(|| show_whole_hint(cx));
                let parts = [Some(plotted_status(&plot.plotted, plot.scope.unit()))];
                let parts = parts.into_iter().chain([unplaced(plot), zoomed]);
                parts.flatten().collect::<Vec<_>>().join(NOTICE_SEPARATOR)
            }
        });
        Some(status.into())
    }

    fn set_active(&mut self, active: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.shown = active;
        match active {
            true => {
                if self.closes_on_blur.is_none() {
                    let blurred =
                        cx.on_focus_out(&self.focus, window, |view, _, _, cx| view.close_pick(cx));
                    self.closes_on_blur = Some(blurred);
                }
                self.focus.focus(window, cx);
                self.measure(cx);
            }
            false => {
                self.clear_hover(cx);
                self.close_pick(cx);
                self.jump.update(cx, |palette, cx| palette.dismiss(cx));
                if let Some(plot) = &mut self.plot {
                    plot.previewed.clear();
                }
            }
        }
    }
}

impl Render for ScatterPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::from_theme(&bviz_ui_theme::theme(window, cx));
        let mut key_context = Self::kind().key_context.to_string();
        if self.picking() {
            key_context = format!("{key_context} {PICKING}");
        }
        if self.offering.offer().is_some() {
            key_context = format!("{key_context} {OFFERING}");
        }
        let root = div()
            .key_context(key_context.as_str())
            .on_action(cx.listener(|view, _: &ShowWholePlot, window, cx| {
                view.zoom_to(Zoom::WHOLE, window.mouse_position(), cx);
            }))
            .on_action(cx.listener(|view, _: &ZoomIn, window, cx| {
                view.zoom_centred(ZOOM_KEY_STEP, window, cx);
            }))
            .on_action(cx.listener(|view, _: &ZoomOut, window, cx| {
                view.zoom_centred(ZOOM_KEY_STEP.recip(), window, cx);
            }))
            .on_action(cx.listener(|view, _: &AimUp, _, cx| view.step_aim(-1, cx)))
            .on_action(cx.listener(|view, _: &AimDown, _, cx| view.step_aim(1, cx)))
            .on_action(cx.listener(|view, _: &SelectAimed, _, cx| view.select_aimed(cx)))
            .on_action(cx.listener(|view, _: &ShowSelectedPoint, _, cx| view.show_selected(cx)))
            .on_action(cx.listener(|view, _: &ClosePickList, _, cx| view.close_pick(cx)))
            .on_action(
                cx.listener(|view, _: &jump::Open, window, cx| view.open_palette(window, cx)),
            )
            .on_action(cx.listener(|view, _: &DismissCard, _, cx| view.dismiss(cx)))
            .on_action(cx.listener(|view, _: &crate::JumpBack, _, cx| view.go_back(cx)))
            .on_action(cx.listener(|view, _: &crate::JumpForward, _, cx| view.go_forward(cx)))
            .on_action(cx.listener(|view, _: &reveal::Accept, _, cx| view.accept(cx)))
            .on_action(cx.listener(|view, _: &ShowLegend, window, cx| {
                view.legend.update(cx, |legend, cx| legend.show(window, cx));
            }))
            .on_action(cx.listener(|view, _: &CyclePoints, _, cx| {
                if let Some(choice) = &view.choice {
                    choice.picker.cycle(cx);
                }
            }))
            .on_action(cx.listener(|view, _: &CycleAcross, _, cx| {
                view.cycle(|choice| choice.across_picker.clone(), cx);
            }))
            .on_action(cx.listener(|view, _: &CycleUp, _, cx| {
                view.cycle(|choice| choice.up_picker.clone(), cx);
            }))
            .on_action(cx.listener(|view, _: &CycleColouring, _, cx| {
                view.cycle(|choice| choice.colour_picker.clone(), cx);
            }))
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
            .track_focus(&self.focus)
            .flex_1()
            .flex()
            .flex_col()
            .size_full()
            .overflow_hidden();
        let model = self.model.read(cx);
        if let Some(placeholder) = model.source().placeholder(&palette.colors) {
            return root.child(placeholder);
        }
        let scope = self.choice.as_ref().map(|choice| choice.picker.chosen(cx));
        let legend = legend(&self.legend, self.plot.as_ref(), &palette);
        let root = root.children(self.choice.as_ref().zip(scope).map(|(choice, scope)| {
            let measuring = self
                .plot
                .as_ref()
                .is_some_and(|plot| plot.scope != scope || plot.plotted.axes != choice.axes);
            let shown = Shown {
                scope,
                overlays: Overlays::of(model.loaded()),
                measuring,
            };
            strip(choice, shown, legend, &self.focus, window, cx)
        }));
        let chips = &self.chips;
        let root = root.child(OneRow::new("scatter-query-bar").filling(|| chips.clone()));
        let capabilities = model.loaded().map(|loaded| &loaded.shared.capabilities);
        let dropped = scope.zip(capabilities);
        let dropped = dropped.and_then(|(scope, capabilities)| dropped_edges(scope, capabilities));
        let root = root.children(dropped.map(|dropped| notice(dropped, &palette)));
        let row = menu_chrome::row_height(window);
        let extents = match self
            .plot
            .as_mut()
            .map(|plot| (plot.scope.unit(), plot.plotted.axes, &mut plot.pointer))
        {
            Some((unit, axes, Pointer::Picking(pick))) => Some(
                *pick
                    .extents
                    .get_or_insert_with(|| pick_extents(&pick.rows, unit, axes, window)),
            ),
            _ => None,
        };
        let card = hidden::offer_card(
            &self.offering,
            |view: &mut Self| &mut view.offering,
            &palette,
            &self.focus,
            &DismissCard,
            cx,
        );
        let card = card.map(|card| div().absolute().top_2().right_2().w(CARD_WIDTH).child(card));
        let jump = self.jump.read(cx).is_open().then(|| self.jump.clone());
        let Some(plot) = &self.plot else {
            let measuring = centered_spinner(MEASURING, &palette.colors);
            let over = div().relative().flex_1().min_h(px(0.)).child(measuring);
            return root.child(over.children(jump).children(card));
        };
        let list = match (&plot.pointer, extents) {
            (Pointer::Picking(pick), Some(extents)) => {
                let style = PickRowStyle {
                    row,
                    line: window.line_height(),
                };
                let list = pick_list(plot, pick, &self.pick_scroll, style, palette, cx);
                let at = PickAt {
                    plotted: Arc::clone(&plot.plotted),
                    zoom: self.zoom,
                    spot: pick.spot,
                };
                Some(pick_layer(
                    list,
                    pick,
                    at,
                    extents,
                    row,
                    &self.pick_scroll,
                    palette,
                ))
            }
            _ => None,
        };
        let hovering = matches!(plot.pointer, Pointer::Hover(_));
        root.child(
            div()
                .id("scatter-plot")
                .relative()
                .flex_1()
                .min_h(px(0.))
                .child(cached_layer(self.canvas.clone()))
                .when(hovering, |plot| plot.cursor_pointer())
                .child(hover_layer(cx.entity(), palette))
                .children(list)
                .children(card)
                .children(jump)
                .on_mouse_move(cx.listener(|view, e: &MouseMoveEvent, _, cx| {
                    if view.press.is_none() {
                        GESTURES.dispatch(gesture::Pointer::HOVER, |intent| match intent {
                            ScatterIntent::Describe => {
                                view.hover_at(e.position, cx);
                                true
                            }
                            _ => false,
                        });
                    }
                }))
                .on_scroll_wheel(cx.listener(|view, e: &ScrollWheelEvent, _, cx| {
                    view.on_scroll_wheel(e, cx);
                }))
                .on_pinch(cx.listener(|view, e: &PinchEvent, _, cx| view.on_pinch(e, cx)))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|view, e: &MouseDownEvent, _, cx| view.on_mouse_down(e, cx)),
                )
                .on_mouse_down(
                    MouseButton::Navigate(NavigationDirection::Back),
                    cx.listener(|view, e: &MouseDownEvent, _, cx| view.on_navigate(e, cx)),
                )
                .on_mouse_down(
                    MouseButton::Navigate(NavigationDirection::Forward),
                    cx.listener(|view, e: &MouseDownEvent, _, cx| view.on_navigate(e, cx)),
                )
                // The window-global handlers are installed only by a frame
                // painted with the press in progress, which a quick click
                // finishes before.
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|view, e: &MouseUpEvent, _, cx| {
                        view.end_press(e.position, DragEnd::Released, cx);
                    }),
                )
                .hover_listener_mode(gpui::HoverListenerMode::InputModalityIndependent)
                .on_hover(cx.listener(|view, hovered: &bool, _, cx| {
                    if !hovered {
                        view.clear_hover(cx);
                    }
                })),
        )
    }
}

/// The layer over the marks that rings the one hovered or aimed at on a list
/// of points, and describes the one hovered, painted apart from the marks so
/// that neither repaints any of them. Its layout records where the plot lies
/// for the pointer handlers; its paint carries a press past the plot's edge.
fn hover_layer(view: Entity<ScatterPanel>, palette: Palette) -> impl IntoElement {
    let prepaint = {
        let view = view.clone();
        move |bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App| {
            view.update(cx, |view, _| view.bounds = Some(bounds));
            let panel = view.read(cx);
            let Some((plotted, mark, Some(lines))) = panel.ringed() else {
                return;
            };
            let Some(area) = paint::data_area(&plotted, panel.zoom, bounds) else {
                return;
            };
            let ring = paint::ring_bounds(&plotted.marks[mark], &area);
            let tooltip = palette.colors.tooltip();
            defer_hover_card(lines, CardAnchor::Item(ring), bounds, tooltip, window, cx);
        }
    };
    let paint = move |bounds: Bounds<Pixels>, (), window: &mut Window, cx: &mut App| {
        let panel = view.read(cx);
        let (zoom, pressed) = (panel.zoom, panel.press.is_some());
        let ringed = panel.ringed();
        let pulse = panel
            .pulsed
            .map(|at| at.elapsed().as_secs_f32() / PULSE.as_secs_f32());
        let pulse = pulse.filter(|&progress| progress < 1.0);
        let marked = panel.plot.as_ref().map(|plot| {
            (
                Arc::clone(&plot.plotted),
                plot.selected().to_vec(),
                plot.previewed.clone(),
            )
        });
        if pressed {
            track_drag(
                window,
                &view,
                |view, at, _, cx| view.drag_to(at, cx),
                |view, at, end, _, cx| view.end_press(at, end, cx),
            );
        }
        if let Some((plotted, selected, previewed)) = marked
            && let Some(area) = paint::data_area(&plotted, zoom, bounds)
        {
            for &mark in &previewed {
                paint::paint_ring(&plotted.marks[mark], &area, &palette, window);
            }
            for &mark in &selected {
                paint::paint_selection(&plotted.marks[mark], &area, &palette, pulse, window);
            }
            if pulse.is_some() && !selected.is_empty() {
                window.request_animation_frame();
            }
        }
        let Some((plotted, mark, _)) = ringed else {
            return;
        };
        if let Some(area) = paint::data_area(&plotted, zoom, bounds) {
            paint::paint_ring(&plotted.marks[mark], &area, &palette, window);
        }
    };
    canvas(prepaint, paint).absolute().inset_0().size_full()
}

/// Where a list of points opens: beside where `spot` is drawn on `plotted` at
/// `zoom`.
struct PickAt {
    plotted: Arc<Plotted>,
    zoom: Zoom,
    spot: Spot,
}

impl PickAt {
    /// Where the spot is drawn in a plot filling `bounds`, or the plot's corner
    /// where it is too small to draw the axes.
    fn within(&self, bounds: Bounds<Pixels>) -> Point<Pixels> {
        paint::data_area(&self.plotted, self.zoom, bounds)
            .map_or(bounds.origin, |area| area.at_spot(self.spot))
    }
}

/// `list`, the list of the points of `pick`, standing where `at` places it in
/// whatever bounds the plot is drawn, with the tooltip of the row aimed at
/// beside it.
fn pick_layer(
    list: impl IntoElement,
    pick: &PickList,
    at: PickAt,
    extents: Extents,
    row: Pixels,
    scroll: &ListScroll,
    palette: Palette,
) -> Placed {
    let rows = pick.rows.len();
    let at = Rc::new(at);
    let frame = {
        let at = Rc::clone(&at);
        move |bounds| pick::frame(at.within(bounds), rows, row, extents, bounds)
    };
    let placed = Placed::new(list, frame);
    let Some(aimed) = pick.aim else {
        return placed;
    };
    let lines = pick.rows[aimed].tooltip.clone();
    let scroll = scroll.handle().clone();
    placed.beside(move |list, bounds, window, cx| {
        let level = pick::row_top(scroll.bounds(), scroll.offset().y, row, aimed);
        let at = at.within(bounds);
        let place = move |extent, frame| pick::flank_within(at, list, extent, level, bounds, frame);
        let tooltip = palette.colors.tooltip();
        defer_tooltip(lines, place, tooltip, window, cx);
    })
}

/// Room between the parts a row of the list of points under a click reads.
const PICK_GAP: Pixels = px(8.);

/// What the header of a list of `rows` points of `unit` reads over the points.
fn pick_heading(rows: usize, unit: Unit) -> String {
    format!("{rows} {} here", unit_names(unit).many)
}

/// How large the list of points listing `rows` of `unit` placed by `axes` and
/// the tooltips of its rows stand, under the window's inherited text style.
fn pick_extents(rows: &[PickRow], unit: Unit, axes: Axes, window: &mut Window) -> Extents {
    let tooltip = rows.iter().fold(Size::default(), |largest, row| {
        largest.max(&tooltip_extent(&row.tooltip, window))
    });
    Extents {
        list_width: pick_width(rows, unit, axes, window),
        tooltip,
    }
}

/// How wide the list of points listing `rows` of `unit` placed by `axes` stands
/// to read each row and its header whole, under the window's inherited text
/// style.
fn pick_width(rows: &[PickRow], unit: Unit, axes: Axes, window: &mut Window) -> Pixels {
    let mut measure =
        |text: &SharedString| menu_chrome::widest(window, std::iter::once(text.clone()));
    let heading =
        measure(&pick_heading(rows.len(), unit).into()) + PICK_GAP + measure(&axes.names().into());
    let widest = rows.iter().fold(heading, |widest, row| {
        let label = measure(&row.label);
        let parts = [
            row.mnemonic.as_ref(),
            row.hint.as_ref(),
            Some(&row.measures),
        ];
        let rest = parts
            .into_iter()
            .flatten()
            .map(|part| PICK_GAP + measure(part));
        let width =
            px(SWATCH_DIAMETER) + PICK_GAP + label + rest.fold(px(0.), |sum, part| sum + part);
        widest.max(width)
    });
    widest + ROW_INSET * 2. + px(SCROLLBAR_THICKNESS) + SHEET_BORDER * 2.
}

/// How tall each row of a list of points and a line of its text stand.
#[derive(Clone, Copy)]
struct PickRowStyle {
    row: Pixels,
    line: Pixels,
}

/// The list of the points under a click, `pick`, filling the rect it is given,
/// its rows drawing only those in view.
fn pick_list(
    plot: &Plot,
    pick: &PickList,
    scroll: &ListScroll,
    style: PickRowStyle,
    palette: Palette,
    cx: &mut Context<ScatterPanel>,
) -> impl IntoElement + use<> {
    let colors = &palette.colors;
    let unit = plot.scope.unit();
    let rows = Rc::clone(&pick.rows);
    let plotted = Arc::clone(&plot.plotted);
    let aimed = pick.aim;
    let view = cx.entity();
    let listed = uniform_list("scatter-pick-rows", rows.len(), move |range, _, _| {
        range
            .map(|index| {
                let aimed = aimed == Some(index);
                pick_row(index, &rows[index], &plotted, aimed, style, &view, &palette)
            })
            .collect::<Vec<_>>()
    })
    .flex_1()
    .w_full()
    .track_scroll(scroll.uniform_handle());
    let header = menu_chrome::header(colors)
        .flex_none()
        .h(style.row)
        .justify_between()
        .gap(PICK_GAP)
        .text_color(colors.text_muted)
        .child(pick_heading(pick.rows.len(), unit))
        .child(
            div()
                .mr(px(SCROLLBAR_THICKNESS))
                .child(plot.plotted.axes.names()),
        );
    let sheet = div()
        .size_full()
        .occlude()
        .flex()
        .flex_col()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|view, e: &MouseDownEvent, window, cx| {
                view.focus.focus(window, cx);
                view.pick_scroll.press(e.position);
                cx.stop_propagation();
                cx.notify();
            }),
        )
        .child(header)
        .child(
            div()
                .relative()
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .child(listed)
                .child(list_scrollbar(
                    cx.entity(),
                    |view: &ScatterPanel| &view.pick_scroll,
                    |view: &mut ScatterPanel| &mut view.pick_scroll,
                    palette.scroll,
                )),
        );
    menu_chrome::surface(sheet, colors)
}

/// A line of `text` in a row of the list of points, cut short where the row
/// runs out of room for it.
fn pick_part(text: SharedString) -> Div {
    div()
        .min_w_0()
        .overflow_hidden()
        .whitespace_nowrap()
        .child(text)
}

/// The row `index` of a list of points, reading `listed`: the colour its mark
/// is drawn in, what its point is, what tells it apart from the rows reading
/// alike, and its measures.
fn pick_row(
    index: usize,
    listed: &PickRow,
    plotted: &Plotted,
    aimed: bool,
    style: PickRowStyle,
    view: &Entity<ScatterPanel>,
    palette: &Palette,
) -> Stateful<Div> {
    let colors = &palette.colors;
    let mark = &plotted.marks[listed.mark];
    let fade = if mark.faded { UNMARKED_FADE } else { 1.0 };
    let swatch = Rgba {
        a: SWATCH_ALPHA * fade,
        ..mark.hue.color(palette)
    };
    let (aim, select) = (view.clone(), view.clone());
    menu_chrome::clear_of_scrollbar(menu_chrome::aimed_row(
        ("scatter-pick", index).into(),
        colors,
        aimed,
    ))
    .w_full()
    .h(style.row)
    .gap(PICK_GAP)
    .on_mouse_move(move |_, _, cx| aim.update(cx, |view, cx| view.aim_at(index, cx)))
    .on_click(move |e, _, cx| {
        select.update(cx, |view, cx| {
            GESTURES.dispatch(gesture::Pointer::click(e), |intent| match intent {
                ScatterIntent::SelectListed => {
                    view.select_row(index, cx);
                    true
                }
                _ => false,
            });
        });
    })
    .child(
        div()
            .flex_none()
            .size(px(SWATCH_DIAMETER))
            .rounded_full()
            .bg(swatch),
    )
    .children(
        listed
            .mnemonic
            .clone()
            .map(|mnemonic| div().flex_none().whitespace_nowrap().child(mnemonic)),
    )
    .child(
        div()
            .flex()
            .items_center()
            .min_w_0()
            .mr_auto()
            .h(style.line)
            .child(LabelLine::new(listed.label.clone(), colors.text_muted)),
    )
    .children(listed.hint.clone().map(|hint| {
        pick_part(hint)
            .text_ellipsis_start()
            .text_color(colors.text_muted)
    }))
    .child(
        div()
            .flex_none()
            .whitespace_nowrap()
            .text_color(colors.text_muted)
            .child(listed.measures.clone()),
    )
}

/// Opacity of a legend swatch, firmer than a mark's so that a lone swatch still
/// shows the hue a cluster of marks builds up to.
const SWATCH_ALPHA: f32 = 0.7;

/// Diameter of a swatch beside a point of the list of points under a click:
/// twice the smallest mark's, so its hue reads.
const SWATCH_DIAMETER: f32 = 4.0 * MIN_RADIUS;

/// What the strip reads of the plot it chooses.
struct Shown<'a> {
    scope: Scope,
    /// What its overlay-reading pickers explain themselves by.
    overlays: Overlays<'a>,
    /// Whether a new choice is measured over the plot drawn for the last.
    measuring: bool,
}

/// How long each group of the strip holds out against the panel narrowing,
/// the unit plotted longest.
const POINTS: Priority = 5;
const AXES: Priority = 4;
const COLOUR: Priority = 3;
const INPUTS: Priority = 2;

/// The words naming each group, beside it in the strip and on its row of the
/// overflow menu.
const ACROSS_NAME: &str = "across";
const UP_NAME: &str = "up";
const COLOUR_NAME: &str = "colour";
const INPUTS_NAME: &str = "inputs";

/// What each overlay-reading picker's hint names it where the graph places
/// the overlay on its points.
const ACROSS_HINT: &str = "Horizontal axis";
const UP_HINT: &str = "Vertical axis";
const COLOUR_HINT: &str = "Point colouring";

/// The strip choosing what the plot measures, its pickers' keys named where
/// `focus` stands, ending in the toggle opening the plot's `legend`.
fn strip(
    choice: &Choice,
    shown: Shown,
    legend: Legend,
    focus: &FocusHandle,
    window: &Window,
    cx: &Context<ScatterPanel>,
) -> OneRow {
    let Shown {
        scope,
        overlays,
        measuring,
    } = shown;
    let points = (choice.picker).group(POINTS, focus, window, cx);
    let note = |name: &str| Some(axis_note(overlays, choice.carried, scope.unit(), name));
    let opening = Axes::OPENING;
    let across = PickerGroup {
        name: ACROSS_NAME,
        dropdown: &choice.across_picker,
        action: &CycleAcross,
        note: note(ACROSS_HINT),
        set: choice.axes.x() != opening.x(),
    };
    let up = PickerGroup {
        name: UP_NAME,
        dropdown: &choice.up_picker,
        action: &CycleUp,
        note: note(UP_HINT),
        set: choice.axes.y() != opening.y(),
    };
    let colour = PickerGroup {
        name: COLOUR_NAME,
        dropdown: &choice.colour_picker,
        action: &CycleColouring,
        note: Some(times_note(
            overlays.times,
            choice.carried.times,
            scope.unit(),
            COLOUR_HINT,
        )),
        set: choice.colour != ColourBy::OPENING,
    };
    let grouped = |picker: PickerGroup, priority| picker.group(priority, focus, window, cx);
    let switches = || {
        let switches = (choice.loaded.inputs()).map(|input| input_switch(input, scope, cx));
        ToggleGroup::any_of("scatter-inputs", INPUTS_NAME, switches).tooltip("Inputs measured")
    };
    let inputs_set = (choice.loaded.inputs()).any(|input| Toggle::of(scope, input) == Toggle::Off);
    let inputs_row = MenuItem::submenu(INPUTS_NAME, switches().submenu(window));
    let inputs = ToolbarGroup::named(
        INPUTS,
        INPUTS_NAME,
        OverflowRows::row(inputs_row, inputs_set),
        switches,
    )
    .or(switches().presentation(Presentation::Collapsed))
    .parted();
    let legend = legend.toolbar_group(window, cx);
    OneRow::new("scatter-controls")
        .group(points)
        .group(grouped(across, AXES))
        .group(grouped(up, AXES))
        .group(grouped(colour, COLOUR))
        .group(inputs)
        .when(measuring, |row| row.busy("scatter-measuring", "measuring…"))
        .trailing(legend)
}

/// The band beneath the strip saying `text` of what the measure leaves out,
/// wrapped to the panel's width.
fn notice(text: String, palette: &Palette) -> Div {
    div()
        .flex_none()
        .px_2()
        .py_1()
        .bg(palette.colors.surface)
        .border_b_1()
        .border_color(palette.border)
        .text_sm()
        .text_color(palette.colors.warning)
        .child(text)
}

/// What a measure in `scope` of configured targets over cquery says of the
/// cquery edges it leaves out: those a cquery run without the configuration
/// of each dependency links only to the dependency's target, which no
/// configured target holds.
fn dropped_edges(scope: Scope, capabilities: &Capabilities) -> Option<String> {
    if scope.unit() != Unit::ConfiguredTarget || !scope.inputs().contains(Input::Cquery) {
        return None;
    }
    let dropped = match capabilities.support(bviz_graph_layout::Axis::ExecConfiguration) {
        Support::Available => return None,
        Support::Partial { .. } => "some cquery edges left out",
        Support::Unavailable { .. } => "cquery edges left out",
    };
    Some(format!("{dropped} — run the cquery with {TRANSITIONS}"))
}

/// Turns the loaded `input`'s share in the measure in `scope` on and off, and
/// says why where it cannot.
fn input_switch(input: Input, scope: Scope, cx: &Context<ScatterPanel>) -> Switch {
    let name = input_name(input);
    let toggle = Toggle::of(scope, input);
    let many = unit_names(scope.unit()).many;
    let segment = match toggle {
        Toggle::On | Toggle::Off => ToggleSegment::new(name).tooltip(format!("Include {name}")),
        Toggle::Lone => ToggleSegment::new(name).unavailable("Disabled: only input"),
        Toggle::Ignored => ToggleSegment::new(name).unavailable(format!("Disabled: no {many}")),
    };
    let view = cx.entity();
    Switch::new(
        segment,
        matches!(toggle, Toggle::On | Toggle::Lone),
        move |_, _, cx| view.update(cx, |view, cx| view.toggle_input(input, cx)),
    )
}

/// What the status says of the points of `plot` left unplotted, where any are.
fn unplaced(plot: &Plot) -> Option<String> {
    let unplaced = plot.plotted.unplaced;
    if unplaced == 0 {
        return None;
    }
    let names = unit_names(plot.scope.unit());
    let many = match unplaced {
        1 => names.one,
        _ => names.many,
    };
    let axes = plot.plotted.axes;
    let lacked: Vec<&str> = [axes.x(), axes.y()]
        .into_iter()
        .filter(|axis| axis.overlay().is_some())
        .map(Axis::name)
        .collect();
    let lacked = lacked.join(" or ");
    Some(format!("{unplaced} {many} without {lacked} left out"))
}

/// The overlays a graph places on its nodes.
#[derive(Clone, Copy)]
struct Overlays<'a> {
    times: Result<&'a NodeTimes, Untimed>,
    bytes: Result<&'a NodeBytes, Unsized>,
}

impl<'a> Overlays<'a> {
    fn of(loaded: Option<&'a Loaded>) -> Self {
        Overlays {
            times: loaded.map_or(Err(Untimed::NoProfile), Loaded::times),
            bytes: loaded.map_or(Err(Unsized::NoExecLog), Loaded::bytes),
        }
    }
}

/// Which overlays `loaded` places on any point `reference` measures.
fn carried(loaded: &Loaded, reference: &Measurement) -> Carried {
    let points = || reference.points.iter();
    let bytes = (loaded.bytes()).is_ok_and(|bytes| points().any(|p| bytes.get(p.id).is_some()));
    Carried {
        times: (loaded.times()).is_ok_and(|times| points().any(|p| times.get(p.id).is_some())),
        bytes,
        inputs: bytes && loaded.bytes().is_ok_and(NodeBytes::inputs_measured),
    }
}

/// The hint of an axis picker `name`d, which `carried` says of which overlays
/// the graph places on some point of `unit`, as [`colour::overlays_hint`]
/// words it.
fn axis_note(overlays: Overlays, carried: Carried, unit: Unit, name: &str) -> SharedString {
    let hint = colour::overlays_hint(
        name,
        [
            (!carried.times).then(|| times_note(overlays.times, false, unit, name)),
            (!carried.bytes).then(|| bytes_note(overlays.bytes, unit, name)),
        ],
    );
    match carried.bytes {
        true => colour::measuring_reads(hint, overlays.bytes),
        false => hint,
    }
}

/// Why the graph places spawn bytes on none of the points of `unit`, as
/// [`times_note`] says for action time placed on none.
fn bytes_note(bytes: Result<&NodeBytes, Unsized>, unit: Unit, name: &str) -> SharedString {
    match bytes {
        Ok(_) => format!("No spawn bytes on {}", unit_names(unit).many).into(),
        Err(_) => colour::bytes_note(bytes, name),
    }
}

/// The hint of the picker `name`d, offering action time: as the graph's
/// colour picker's, but where the graph places time on none of the points of
/// `unit`, that it places none there.
fn times_note(
    times: Result<&NodeTimes, Untimed>,
    timed: bool,
    unit: Unit,
    name: &str,
) -> SharedString {
    match times {
        Ok(_) if !timed => format!("No action time on {}", unit_names(unit).many).into(),
        _ => colour::note(times, name),
    }
}

/// What a mark's size stands for, as the legend names it.
fn sizing_name(sizing: Sizing) -> &'static str {
    match sizing {
        Sizing::Actions => "actions",
        Sizing::FanIn => "direct dependents",
    }
}

/// The legend of `plot`, open as `state` says: what each colour a mark is
/// drawn in stands for, then what a mark's size stands for; keying nothing
/// until a plot is drawn.
fn legend(state: &Entity<LegendState>, plot: Option<&Plot>, palette: &Palette) -> Legend {
    let legend = Legend::new("scatter-legend", state);
    let Some(plot) = plot else {
        return legend;
    };
    let hues = plot.legend.iter().map(|(name, hue)| {
        let fill = Rgba {
            a: SWATCH_ALPHA,
            ..hue.color(palette)
        };
        LegendEntry::new(Swatch::Dot(fill.into()), name.clone())
    });
    let measure = sizing_name(plot.plotted.sizing);
    // Inked apart from every hue, so the key reads as one of size, not colour.
    let size = Swatch::Sizes(palette.colors.text_muted);
    let size = LegendEntry::new(size, measure).tooltip("Mark size");
    legend.entries(hues.chain([size]))
}

/// How to leave a zoom: by the gesture, or by the keys that show the whole
/// plot where any reach the plot.
fn show_whole_hint(cx: &App) -> String {
    let gesture = SHOW_WHOLE.gesture().text(KeyStyle::NATIVE);
    match key_text_within(&ShowWholePlot, &[KEY_CONTEXT.into()], cx) {
        Some(keys) => format!("{gesture} or press {keys} to show the whole plot"),
        None => format!("{gesture} to show the whole plot"),
    }
}

/// The layer the points are drawn in, cached across frames that leave the
/// plot and zoom unchanged.
struct ScatterCanvas {
    /// The panel whose plot and zoom this draws.
    parent: WeakEntity<ScatterPanel>,
}

impl Render for ScatterCanvas {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::from_theme(&bviz_ui_theme::theme(window, cx));
        let drawn = self.parent.upgrade().and_then(|parent| {
            let panel = parent.read(cx);
            Some((Arc::clone(&panel.plot.as_ref()?.plotted), panel.zoom))
        });
        canvas(
            |_, _, _| {},
            move |bounds: Bounds<Pixels>, (), window, cx| {
                if let Some((plotted, zoom)) = drawn {
                    paint::paint_plot(&plotted, zoom, &palette, bounds, window, cx);
                }
            },
        )
        .size_full()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_gesture_is_reachable() {
        assert_eq!(GESTURES.unreachable(), []);
    }

    #[test]
    fn every_gesture_fits_the_keys_help() {
        assert_eq!(GESTURES.overlong(), []);
    }
}
