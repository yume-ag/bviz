mod gestures;
mod input;
mod paint;
mod selection;
mod slices;
mod view;

use crate::counters::track_count;
use crate::groups::{GroupChrome, RowAnchor, anchor_at, anchor_offset, chart_rows};
use crate::model::{ChoiceChanged, TraceModel, WorkingLanesDerived};
use crate::regions::{CountersStrip, Regions};
use crate::theme::Palette;
use crate::{
    DEFAULT_COUNTERS_HEIGHT, DEFAULT_GUTTER_WIDTH, EDGE_MARGIN_PX, TimelineBars, TracePanel,
    drawn_scroll, scroll_request, scrollbar_geom, slice_tooltip,
};
use bviz_trace_layout::TimeView;
use bviz_trace_profile::{Slice, TimeSpan};
use bviz_ui_components::{
    CardAnchor, HoverCard, ShownCard, cached_layer, hover_card_canvas, overlay_canvas,
};
use bviz_ui_playback::Clock;
use gpui::{
    App, Bounds, Context, Entity, FocusHandle, Focusable, IntoElement, Pixels, Point, Render,
    Subscription, WeakEntity, Window, div, prelude::*,
};
use input::TimelineDrag;
use paint::{SliceCanvas, overlay_paint};
use slices::SliceDrawCache;
use std::rc::Rc;
use view::{Follow, PLAYHEAD_FOLLOW_INSET, framing, scroll_revealing, scrolled_row};

pub(crate) use gestures::TIMELINE_GESTURES;
pub(crate) use slices::HoveredSlice;
pub(crate) use view::zoom_floor;

/// The flame chart as it currently stands, read against the lane scroll's
/// standing anchor.
struct AnchoredChart {
    chrome: Rc<GroupChrome>,
    /// Where the anchored row sits in the chart, before the ceiling bounds it.
    held: f32,
    ceiling: f32,
}

/// The flame-chart timeline: time on the x axis, stacked thread lanes on the y
/// axis, the counter plots, the ruler, gutter, phase markers, and scroll bars.
/// Owns the timeline's view state, reading the document and selection from
/// [`TraceModel`] and the strip/gutter layout from [`TracePanel`]. The slices are
/// painted by [`SliceCanvas`], the time selection, crosshair, and tooltip by an
/// overlay above it.
pub(crate) struct Timeline {
    /// Focus for the trace content, so [`SelectAllTracks`] dispatches here; a
    /// descendant of the window focus, so the window's own actions fire.
    content_focus: FocusHandle,
    /// Read for the document and lanes, written for selection and grouping.
    model: Entity<TraceModel>,
    /// The owning view, read for the counters-strip and gutter layout its
    /// divider and gutter handle drag.
    shell: WeakEntity<TracePanel>,
    /// Horizontal time-to-pixel mapping (pan and zoom). Prepaint settles it
    /// against the painted width — the fit, the resize rescale, and the zoom
    /// floor all need a width the timeline learns only there — and the same
    /// frame's paint then reads the result.
    view: TimeView,
    /// Which chart row the scrolled lanes' top edge sits in, and where within it;
    /// [`Self::lane_scroll`] turns it into the pixel offset the frame draws at.
    scroll: RowAnchor,
    /// How far the counters strip is scrolled up, in pixels, held past its
    /// ceiling so a strip resized shorter and back returns to where it stood;
    /// [`Self::counter_scroll`] is the offset the frame draws at.
    counter_scroll_held: f32,
    /// Viewport bounds captured during canvas paint, so handlers can map a window
    /// cursor to viewport space and bound the zoom to a fit.
    viewport: Bounds<Pixels>,
    /// Whether the initial fit-to-window view has been set; latched during the
    /// first paint, when the viewport size is first known.
    fitted: bool,
    follow: Follow,
    /// A stretch of time asked to be shown before the first paint, which that
    /// paint's fit frames in place of the whole profile.
    framed_on_fit: Option<TimeSpan>,
    /// The timeline's memoized culled draw list; see [`SliceDrawCache`].
    slice_draw: Option<SliceDrawCache>,
    /// The card describing the slice under the pointer.
    card: HoverCard<Slice>,
    /// The cursor position while it hovers the time-content column and no drag is
    /// in progress, drawn as a crosshair; `None` otherwise.
    crosshair: Option<Point<Pixels>>,
    drag: TimelineDrag,
    last_mouse: Point<Pixels>,
    /// The layer painting the slices, gutter, ruler, markers, scroll bars, and
    /// counter plots; notified through [`Self::invalidate_canvas`].
    canvas: Entity<SliceCanvas>,
    /// Repaints the overlay for a selection change made in another pane. Only
    /// the overlay draws from the model's selection state that the timeline does
    /// not itself move, so the slice scene replays rather than rebuilding.
    _model_observer: Subscription,
    /// Repaints the overlay when the session learns something of the
    /// invocation, which the hover tooltip names a slice's configuration by.
    _session_observer: Subscription,
    /// Redraws the counter plots once the working lanes are there to plot.
    _working_lanes_derived: Subscription,
    /// Repaints the overlay, which draws the playhead, as a gesture moves the
    /// document's clock, and follows the playhead again from there.
    _clock_changed: Subscription,
    _choice_changed: Subscription,
}

impl Timeline {
    pub(crate) fn new(
        model: Entity<TraceModel>,
        shell: WeakEntity<TracePanel>,
        cx: &mut Context<Self>,
    ) -> Self {
        let content_focus = cx.focus_handle();
        let parent = cx.entity().downgrade();
        let canvas = cx.new(|_| SliceCanvas { parent });
        let model_observer = cx.observe(&model, |_, _, cx| cx.notify());
        let session = model.read(cx).session().clone();
        let session_observer = cx.observe(&session, |_, _, cx| cx.notify());
        let clock = model.read(cx).clock().clone();
        let clock_changed = cx.observe(&clock, |timeline, _, cx| {
            timeline.follow = Follow::Playhead;
            cx.notify();
        });
        let working_lanes_derived = cx
            .subscribe(&model, |timeline, _, _: &WorkingLanesDerived, cx| {
                timeline.invalidate_canvas(cx)
            });
        let choice_changed = cx.subscribe(&model, |timeline, _, _: &ChoiceChanged, cx| {
            timeline.reveal_chosen(cx);
            timeline.invalidate_canvas(cx);
        });
        Self {
            content_focus,
            model,
            shell,
            view: TimeView {
                start_us: 0.0,
                px_per_us: 1.0,
            },
            scroll: RowAnchor::Start,
            counter_scroll_held: 0.0,
            viewport: Bounds::default(),
            fitted: false,
            follow: Follow::Playhead,
            framed_on_fit: None,
            slice_draw: None,
            card: HoverCard::new(cx, |timeline| &mut timeline.card, Self::carded),
            crosshair: None,
            drag: TimelineDrag::None,
            last_mouse: Point::default(),
            canvas,
            _model_observer: model_observer,
            _session_observer: session_observer,
            _working_lanes_derived: working_lanes_derived,
            _clock_changed: clock_changed,
            _choice_changed: choice_changed,
        }
    }

    /// Frames the time of what is chosen of the critical path and scrolls
    /// the lanes to the row its action span stands in. A step's own slice
    /// stands in the pinned row, which no scroll moves.
    fn reveal_chosen(&mut self, cx: &mut Context<Self>) {
        let (time, span, row) = {
            let model = self.model.read(cx);
            let (Some(profile), Some(lanes), Some((_, item))) =
                (model.profile(), model.chart_lanes(), model.chosen(cx))
            else {
                return;
            };
            let row = scrolled_row(profile, &lanes.scrolled, &item.slices());
            (item.time(profile), profile.span(), row)
        };
        let regions = self.regions(cx);
        match self.fitted {
            true => self.view = framing(time, span, regions.time_w),
            false => self.framed_on_fit = Some(time),
        }
        self.follow = Follow::Released;
        let scrolled = regions.scrolled_band(self.lane_scroll(cx));
        if let Some(row) = row
            && let Some(y) = scroll_revealing(row, scrolled.scroll, scrolled.height)
        {
            self.scroll_to(y, cx);
        }
    }

    /// Rebuilds the slice scene along with the overlay; every change to what the
    /// slices show routes through here, and a change that leaves them alone
    /// notifies alone, so the slice scene replays from its cache.
    pub(crate) fn invalidate_canvas(&self, cx: &mut Context<Self>) {
        self.canvas.update(cx, |_, cx| cx.notify());
        cx.notify();
    }

    /// Pages the time view to show the playhead again where it has played out
    /// of sight. Only a playing clock moves the view, and neither while a drag
    /// holds it nor once the reader has moved it ([`Follow::Released`]), so a
    /// reader exploring the timeline keeps where they are.
    fn follow_playhead(&mut self, cx: &mut Context<Self>) {
        if !self.fitted || !matches!(self.drag, TimelineDrag::None) {
            return;
        }
        let (span, at_us) = {
            let model = self.model.read(cx);
            let playing = model
                .clock()
                .read(cx)
                .timeline()
                .is_some_and(|timeline| timeline.is_playing());
            if !playing {
                return;
            }
            let (Some(profile), Some(at_us)) = (model.profile(), model.playhead_us(cx)) else {
                return;
            };
            (profile.span(), at_us)
        };
        if !(span.start_us..=span.end_us).contains(&at_us) {
            return;
        }
        let time_w = self.regions(cx).time_w;
        let x = self.view.x_of(at_us);
        if !self.follow.pages((0.0..=time_w).contains(&x)) {
            return;
        }
        self.view.pan_px(time_w * PLAYHEAD_FOLLOW_INSET - x);
        self.view.clamp_to(span, time_w, EDGE_MARGIN_PX);
        self.invalidate_canvas(cx);
    }

    /// Drops the state that addresses the document being replaced, so nothing
    /// survives the swap still pointing into the old profile.
    pub(crate) fn reset_for_new_profile(&mut self) {
        self.slice_draw = None;
        self.framed_on_fit = None;
        self.scroll = RowAnchor::Start;
        self.counter_scroll_held = 0.0;
        self.drag = TimelineDrag::None;
        self.follow = Follow::Playhead;
    }

    /// Releases the time view from following the playhead where the reader
    /// moved it from `before`.
    fn release_if_moved(&mut self, before: TimeView) {
        if self.view != before {
            self.follow = Follow::Released;
        }
    }

    /// The counters strip and gutter width the timeline is split by, held by the
    /// owning view because its divider and gutter handle drag them.
    fn shell_layout(&self, cx: &App) -> (CountersStrip, f32) {
        match self.shell.upgrade() {
            Some(shell) => {
                let s = shell.read(cx);
                (s.counters, s.gutter_width)
            }
            None => (
                CountersStrip::Open(DEFAULT_COUNTERS_HEIGHT),
                DEFAULT_GUTTER_WIDTH,
            ),
        }
    }

    /// The viewport bounds last captured during paint.
    pub(crate) fn viewport(&self) -> Bounds<Pixels> {
        self.viewport
    }

    /// [`Regions`] for the current viewport, strip and gutter sizes, the loaded
    /// profile's counter track count, and the flame lanes' heights.
    fn regions(&self, cx: &App) -> Regions {
        let (n_counters, lanes) = {
            let model = self.model.read(cx);
            let n_counters = model.profile().map_or(0, |p| track_count(p));
            let lanes = model.chart_lanes().map(|l| l.heights()).unwrap_or_default();
            (n_counters, lanes)
        };
        let (counters, gutter_width) = self.shell_layout(cx);
        Regions::new(self.viewport, n_counters, counters, gutter_width, lanes)
    }

    /// The bars for the timeline's view state; all `None` while no profile with a
    /// lane layout is loaded.
    fn scroll_bars(&self, cx: &App) -> TimelineBars {
        let (span, scrolled_total) = {
            let model = self.model.read(cx);
            let (Some(profile), Some(lanes)) = (model.profile(), model.chart_lanes()) else {
                return TimelineBars::default();
            };
            (profile.span(), lanes.scrolled.total_height)
        };
        scrollbar_geom(
            span,
            self.view,
            scrolled_total,
            self.regions(cx),
            self.lane_scroll(cx),
            self.counter_scroll(cx),
        )
    }

    /// The chart the lane scroll addresses; `None` while no profile with a lane
    /// layout is loaded. Reads the shell for its layout, so it must not run while
    /// the shell is leased.
    fn anchored_chart(&self, cx: &App) -> Option<AnchoredChart> {
        let model = self.model.read(cx);
        model.chart_lanes()?;
        let chrome = model.group_chrome()?;
        let held = anchor_offset(chart_rows(&chrome), self.scroll);
        let ceiling = self.regions(cx).max_lane_scroll();
        Some(AnchoredChart {
            chrome,
            held,
            ceiling,
        })
    }

    /// How far the lanes are scrolled up, in pixels. The sole reading of the
    /// vertical offset, so what is painted, what is culled, and what is
    /// hit-tested agree.
    fn lane_scroll(&self, cx: &App) -> f32 {
        self.anchored_chart(cx)
            .map_or(0.0, |chart| drawn_scroll(chart.held, chart.ceiling))
    }

    /// Re-anchors the lane scroll to the row a gesture asking for `y` pixels down
    /// the chart lands in.
    fn scroll_to(&mut self, y: f32, cx: &App) {
        let Some(chart) = self.anchored_chart(cx) else {
            return;
        };
        let Some(target) = scroll_request(chart.held, chart.ceiling, y) else {
            return;
        };
        self.scroll = anchor_at(chart_rows(&chart.chrome), target);
    }

    /// Scrolls the lanes down by a gesture's `dy` pixels.
    fn scroll_by(&mut self, dy: f32, cx: &App) {
        self.scroll_to(self.lane_scroll(cx) - dy, cx);
    }

    /// How far the counters strip is scrolled up, in pixels. The sole reading of
    /// the strip's offset, so its plots, their gutter names, and its bar agree.
    fn counter_scroll(&self, cx: &App) -> f32 {
        drawn_scroll(
            self.counter_scroll_held,
            self.regions(cx).max_counter_scroll(),
        )
    }

    /// Moves the counters strip to a gesture asking for `y` pixels down its lanes.
    fn scroll_counters_to(&mut self, y: f32, cx: &App) {
        let ceiling = self.regions(cx).max_counter_scroll();
        if let Some(target) = scroll_request(self.counter_scroll_held, ceiling, y) {
            self.counter_scroll_held = target;
        }
    }

    /// Scrolls the counters strip down by a gesture's `dy` pixels.
    fn scroll_counters_by(&mut self, dy: f32, cx: &App) {
        self.scroll_counters_to(self.counter_scroll(cx) - dy, cx);
    }

    /// Clears the slice card and crosshair, so a tab switch drops them
    /// instead of leaving them until the next mouse move.
    pub(crate) fn clear_hover(&mut self) {
        self.card.leave();
        self.crosshair = None;
    }
}

impl Focusable for Timeline {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.content_focus.clone()
    }
}

impl Render for Timeline {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.follow_playhead(cx);
        let (profile, lanes) = {
            let model = self.model.read(cx);
            Clock::keep_pace(Some(model.clock()), window, cx);
            let Some(profile) = model.profile() else {
                return div().into_any_element();
            };
            let Some(lanes) = model.chart_lanes() else {
                return div().into_any_element();
            };
            (profile.clone(), lanes)
        };
        let palette = Palette::from_theme(&bviz_ui_theme::theme(window, cx));
        let entity = cx.entity();
        let slices = cached_layer(self.canvas.clone());
        let overlay = overlay_canvas(overlay_paint(profile, lanes, entity.clone(), palette));
        let card = hover_card_canvas(palette.colors.tooltip(), move |_, _, cx| {
            let timeline = entity.read(cx);
            let at = timeline.card.pointer()?;
            let hovered = timeline.slice_at(at, cx)?;
            if !timeline.card.shows(hovered.slice()) {
                return None;
            }
            let model = timeline.model.read(cx);
            let session = Some(model.session().read(cx));
            let repos = model.repos()?;
            Some(ShownCard {
                lines: slice_tooltip(&hovered, session, repos).into(),
                anchor: CardAnchor::Pointer(at),
            })
        });
        let root = div()
            .id("trace-content")
            .relative()
            .size_full()
            .overflow_hidden()
            .child(slices)
            .child(overlay)
            .child(card);
        self.content_handlers(root, cx).into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bviz_trace_profile::Profile;
    use std::sync::Arc;

    pub(super) const TEST_TIDS: [i64; 2] = [1, 2];
    pub(super) const TEST_VIEW: TimeView = TimeView {
        start_us: 0.0,
        px_per_us: 0.1,
    };

    /// One thread's name metadata followed by its `spans`, each `(start_us,
    /// dur_us)`, as `X` events named `t{tid}s{i}` in category `c{tid}s{i}` —
    /// unique across a profile of distinct tids, so either identifies both the
    /// thread and the slice within it.
    pub(super) fn thread_events(
        tid: i64,
        spans: impl IntoIterator<Item = (i64, i64)>,
    ) -> Vec<String> {
        let mut events = vec![format!(
            r#"{{"name":"thread_name","ph":"M","tid":{tid},"args":{{"name":"skyframe-evaluator-execution-{tid}"}}}}"#,
        )];
        events.extend(spans.into_iter().enumerate().map(|(i, (start_us, dur_us))| {
            format!(
                r#"{{"cat":"c{tid}s{i}","name":"t{tid}s{i}","ph":"X","ts":{start_us},"dur":{dur_us},"tid":{tid}}}"#,
            )
        }));
        events
    }

    pub(super) fn profile_of_events(events: Vec<String>) -> Arc<Profile> {
        let json = format!(r#"{{"traceEvents":[{}]}}"#, events.join(","));
        Arc::new(
            Profile::from_bytes(json.as_bytes())
                .expect("parse test profile")
                .into_inner(),
        )
    }

    /// A profile of two threads, each carrying `per_thread` back-to-back
    /// millisecond slices.
    pub(super) fn profile_of(per_thread: usize) -> Arc<Profile> {
        profile_of_events(
            TEST_TIDS
                .iter()
                .flat_map(|&tid| {
                    thread_events(tid, (0..per_thread as i64).map(|i| (i * 1000, 1000)))
                })
                .collect(),
        )
    }
}
