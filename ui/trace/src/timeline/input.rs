//! Pointer, wheel and key handling, and the drags they start.

use super::Timeline;
use super::gestures::{TIMELINE_GESTURES, TimeBand, TimelineIntent};
use super::selection::{Grab, RegionDrag, region_grab, region_press, time_at_x};
use super::slices::HoveredSlice;
use super::view::held_zoom;
use crate::groups::GroupHit;
use crate::regions::LaneBand;
use crate::{
    ClearTimeSelection, EDGE_MARGIN_PX, GROUP_BAND_HEIGHT, GROUP_ICON, MIN_DRAG_PX, SLICE_TEXT_PAD,
    SelectAllTracks, ShowWholeTimeline, TIMELINE_KEY_CONTEXT, thumb_offset,
};
use bviz_trace_layout::{LaneLayout, TimeSelection, TimeView};
use bviz_trace_profile::{Slice, TimeSpan};
use bviz_ui_components::gesture::{Pointer, pinch_factor, scroll_pixels, wheel_zoom_factor};
use bviz_ui_components::{DragEnd, ZOOM_KEY_STEP, ZoomIn, ZoomOut};
use gpui::{
    App, Context, Div, MouseButton, MouseDownEvent, MouseMoveEvent, PinchEvent, Pixels, Point,
    ScrollWheelEvent, Stateful, Window, prelude::*, px,
};

/// What a left-button drag over the timeline is currently moving.
#[derive(Clone, Copy)]
pub(super) enum TimelineDrag {
    None,
    /// A press on the flame chart pans the timeline (and scrolls its lanes).
    Pan(PanDrag),
    /// A press on the lane vertical, time horizontal, or counters vertical
    /// scroll thumb/track, holding the grab offset from the thumb's leading edge.
    VThumb(f32),
    HThumb(f32),
    CountersThumb(f32),
    /// A press on the ruler, or a shift-press on the flame lanes, selecting the
    /// time the flame graph and stats aggregate over.
    TimeRegion(RegionDrag),
}

/// A pan in progress.
#[derive(Clone, Copy)]
pub(super) struct PanDrag {
    /// Window position of the press, which the click threshold measures from.
    press: Point<Pixels>,
    /// Whether the cursor has passed [`MIN_DRAG_PX`] from the press, latched
    /// so a pan that returns to the press point stays a pan.
    moved: bool,
    /// The click a release short of [`MIN_DRAG_PX`] makes of the press.
    click: Pointer,
}

impl PanDrag {
    fn at(press: &MouseDownEvent) -> PanDrag {
        PanDrag {
            press: press.position,
            moved: false,
            click: Pointer::completing(press),
        }
    }

    fn follow(&mut self, to: Point<Pixels>) {
        let dx = f32::from(to.x - self.press.x).abs();
        let dy = f32::from(to.y - self.press.y).abs();
        self.moved |= dx.max(dy) > MIN_DRAG_PX;
    }

    /// Whether the pan, ended as `end`, was a click: let go of where it was
    /// pressed, give or take the click threshold.
    fn clicked(self, end: DragEnd) -> bool {
        !self.moved && matches!(end, DragEnd::Released)
    }
}

impl TimelineDrag {
    /// The region drag under way, which the overlay previews and a release
    /// commits, so what is drawn during a drag is what lands.
    pub(super) fn region(&self) -> Option<RegionDrag> {
        match *self {
            TimelineDrag::TimeRegion(drag) => Some(drag),
            _ => None,
        }
    }

    /// The selection to paint over `standing` while the drag runs.
    pub(super) fn preview(&self, standing: &TimeSelection) -> TimeSelection {
        self.region()
            .map_or_else(|| standing.clone(), |drag| drag.applied(standing))
    }

    /// Abandons a region drag under way, reporting whether there was one. It
    /// commits nothing, so the selection stands as the press found it.
    pub(super) fn cancel_region(&mut self) -> bool {
        let live = self.region().is_some();
        if live {
            *self = TimelineDrag::None;
        }
        live
    }
}

impl Timeline {
    /// The slice under the pointer, which the card describes.
    pub(super) fn carded(&self, cx: &App) -> Option<Slice> {
        let at = self.card.pointer()?;
        Some(*self.slice_at(at, cx)?.slice())
    }

    /// Whether `pos` lies over a time-axis region the crosshair shows on: the
    /// flame lanes, and the counters strip where the profile has counters.
    fn in_content(&self, pos: Point<Pixels>, cx: &App) -> bool {
        if self.model.read(cx).chart_lanes().is_none() {
            return false;
        }
        let regions = self.regions(cx);
        regions.in_flame(pos) || regions.in_counters(pos)
    }

    /// Whether `pos` lies over the time axis's content clear of every slice
    /// and scroll bar.
    fn empty_at(&self, pos: Point<Pixels>, cx: &App) -> bool {
        let bars = self.scroll_bars(cx);
        let on_bar = [bars.flame, bars.time, bars.counters]
            .into_iter()
            .flatten()
            .any(|bar| bar.track_hit(pos));
        self.in_content(pos, cx) && !on_bar && self.slice_at(pos, cx).is_none()
    }

    /// Fits the whole span of the profile to the time axis.
    fn show_whole(&mut self, cx: &App) {
        let Some(span) = self.model.read(cx).profile().map(|profile| profile.span()) else {
            return;
        };
        let before = self.view;
        self.view = TimeView::fit(span, self.regions(cx).time_w, EDGE_MARGIN_PX);
        self.release_if_moved(before);
    }

    /// The slice under a window-space cursor: the box the last paint drew there,
    /// found in the memoized draw list, so a hover names what is on screen, and
    /// the document it was drawn from. `None` until the first paint has culled a
    /// draw list.
    pub(super) fn slice_at(&self, pos: Point<Pixels>, cx: &App) -> Option<HoveredSlice> {
        let regions = self.regions(cx);
        if !regions.in_flame(pos) {
            return None;
        }
        self.slice_draw.as_ref()?.hit(
            f32::from(pos.x) - regions.content_left,
            f32::from(pos.y) - regions.flame_top,
        )
    }

    /// The group header band or show-more row at `pos`, if the press lands on one
    /// (across the full chart width, gutter included).
    fn group_row_at(&self, pos: Point<Pixels>, cx: &App) -> Option<GroupHit> {
        let model = self.model.read(cx);
        model.chart_lanes()?;
        let chrome = model.group_chrome()?;
        let regions = self.regions(cx);
        let (_, gutter_width) = self.shell_layout(cx);
        let x = f32::from(pos.x);
        let gutter_left = regions.content_left - gutter_width;
        if x < gutter_left || x >= regions.content_right {
            return None;
        }
        let content_y = regions
            .scrolled_band(self.lane_scroll(cx))
            .stack_y(f32::from(pos.y))?;
        let hits = |top: f32| content_y >= top && content_y < top + GROUP_BAND_HEIGHT;
        let band = chrome.bands.iter().find(|b| hits(b.top))?;
        let icon_left = regions.content_left - GROUP_ICON - 2.0 * SLICE_TEXT_PAD;
        if band.capped && !band.collapsed && x >= icon_left && x < regions.content_left {
            Some(GroupHit::ShowAll(band.group))
        } else {
            Some(GroupHit::Collapse(band.group))
        }
    }

    /// The track whose gutter label lies under `pos`, if the press hit the
    /// track-label column. `None` elsewhere or over a lane's empty tail.
    fn track_label_at(&self, pos: Point<Pixels>, cx: &App) -> Option<usize> {
        let lanes = self.model.read(cx).chart_lanes()?;
        let regions = self.regions(cx);
        let (_, gutter_width) = self.shell_layout(cx);
        let x = f32::from(pos.x);
        let gutter_left = regions.content_left - gutter_width;
        if x < gutter_left || x >= regions.content_left {
            return None;
        }
        let y = f32::from(pos.y);
        let in_band = |layout: &LaneLayout, band: LaneBand| {
            let content_y = band.stack_y(y)?;
            layout
                .lanes
                .iter()
                .find(|l| content_y >= l.top && content_y < l.top + l.height)
                .map(|l| l.track)
        };
        in_band(&lanes.pinned, regions.pinned_band())
            .or_else(|| in_band(&lanes.scrolled, regions.scrolled_band(self.lane_scroll(cx))))
    }

    /// Answers [`SelectAllTracks`] by selecting every currently visible track.
    fn select_all_visible(&mut self, _: &SelectAllTracks, _: &mut Window, cx: &mut Context<Self>) {
        if self.model.update(cx, |m, cx| m.select_all_visible(cx)) {
            self.invalidate_canvas(cx);
        }
    }

    /// Answers [`ClearTimeSelection`] by backing out of the innermost gesture: a
    /// region drag under way is abandoned, failing that what is chosen of the
    /// critical path is let go of, and failing that the aggregate widens back
    /// to the whole timeline.
    fn clear_time_selection(
        &mut self,
        _: &ClearTimeSelection,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let backed_out = self.drag.cancel_region()
            || self.model.update(cx, |m, cx| {
                m.unchoose(cx) || m.set_time_selection(TimeSelection::default(), cx)
            });
        if backed_out {
            cx.notify();
        }
    }

    /// The instant a window-space cursor points at; `None` while no profile is
    /// loaded.
    fn time_at(&self, pos: Point<Pixels>, cx: &App) -> Option<i64> {
        let profile = self.model.read(cx).profile()?;
        let x_px = f32::from(pos.x) - self.regions(cx).content_left;
        Some(time_at_x(self.view, profile.span(), x_px))
    }

    /// Begins a time-region drag from a press at `pos` where it lands on the
    /// band `on`, reporting whether it did. [`region_press`] resolves what it
    /// edits, a press that `toggles` membership or not.
    fn press_time_region(
        &mut self,
        pos: Point<Pixels>,
        on: TimeBand,
        toggles: bool,
        cx: &App,
    ) -> bool {
        let regions = self.regions(cx);
        let x = f32::from(pos.x);
        let on_band = match on {
            TimeBand::Ruler => regions.in_ruler(pos),
            TimeBand::Lanes => regions.in_flame(pos),
        };
        if !on_band {
            return false;
        }
        let Some(profile) = self.model.read(cx).profile() else {
            return false;
        };
        let bounds = profile.span();
        let at = time_at_x(self.view, bounds, x - regions.content_left);
        let (edit, anchor_us) = {
            let sel = self.model.read(cx).time_selection();
            let grab = match on {
                TimeBand::Ruler => Grab::Ruler(region_grab(sel, self.view, regions, bounds, at, x)),
                TimeBand::Lanes => Grab::Lanes,
            };
            region_press(sel, at, toggles, grab)
        };
        self.crosshair = None;
        self.drag = TimelineDrag::TimeRegion(RegionDrag {
            edit,
            anchor_us,
            cursor_us: at,
            press_x: x,
            moved: false,
        });
        true
    }

    /// Lands a finished time-region drag on the model. Only a release carries
    /// the click that clears the selection; an abandoned drag lands what it was
    /// already showing.
    fn commit_time_region(&mut self, drag: RegionDrag, end: DragEnd, cx: &mut Context<Self>) {
        let standing = self.model.read(cx).time_selection();
        let selection = match end {
            DragEnd::Released => drag.released(standing),
            DragEnd::Abandoned => drag.applied(standing),
        };
        self.model
            .update(cx, |m, cx| m.set_time_selection(selection, cx));
    }

    /// Begins a timeline drag from `press`: the lane vertical or time
    /// horizontal scroll thumb/track if the press hit one, otherwise a pan.
    fn press(&mut self, press: &MouseDownEvent, cx: &App) {
        let pos = press.position;
        self.crosshair = None;
        let bars = self.scroll_bars(cx);
        if let Some(bar) = bars.flame
            && bar.track_hit(pos)
        {
            let grab = bar.grab(pos);
            self.drag = TimelineDrag::VThumb(grab);
            self.scroll_thumb_to(pos, grab, cx);
            return;
        }
        if let Some(bar) = bars.time
            && bar.track_hit(pos)
        {
            let grab = bar.grab(pos);
            self.drag = TimelineDrag::HThumb(grab);
            self.pan_thumb_to(pos, grab, cx);
            return;
        }
        if let Some(bar) = bars.counters
            && bar.track_hit(pos)
        {
            let grab = bar.grab(pos);
            self.drag = TimelineDrag::CountersThumb(grab);
            self.counters_thumb_to(pos, grab, cx);
            return;
        }
        self.drag = TimelineDrag::Pan(PanDrag::at(press));
        self.last_mouse = pos;
    }

    /// Maps the lane vertical thumb drag back to a scroll offset.
    fn scroll_thumb_to(&mut self, pos: Point<Pixels>, grab: f32, cx: &App) {
        if let Some(offset) = thumb_offset(self.scroll_bars(cx).flame, pos, grab) {
            self.scroll_to(offset, cx);
        }
    }

    /// Maps the counters vertical thumb drag back to a scroll offset.
    fn counters_thumb_to(&mut self, pos: Point<Pixels>, grab: f32, cx: &App) {
        if let Some(offset) = thumb_offset(self.scroll_bars(cx).counters, pos, grab) {
            self.scroll_counters_to(offset, cx);
        }
    }

    /// Maps a horizontal thumb drag back to a pan start time, then clamps the view.
    fn pan_thumb_to(&mut self, pos: Point<Pixels>, grab: f32, cx: &App) {
        if self.model.read(cx).chart_lanes().is_none() {
            return;
        }
        let Some(profile) = self.model.read(cx).profile().cloned() else {
            return;
        };
        let span = profile.span();
        let regions = self.regions(cx);
        let Some(offset) = thumb_offset(self.scroll_bars(cx).time, pos, grab) else {
            return;
        };
        // The bar's offset is measured from the span start less the reachable
        // margin, so map it back through the same shift before clamping.
        let margin_us = EDGE_MARGIN_PX / self.view.px_per_us as f32;
        self.view.start_us = (span.start_us as f32 - margin_us) as f64 + offset as f64;
        self.view.clamp_to(span, regions.time_w, EDGE_MARGIN_PX);
    }

    fn on_scroll_wheel(&mut self, e: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.model.read(cx).chart_lanes().is_none() {
            return;
        }
        let Some(profile) = self.model.read(cx).profile().cloned() else {
            return;
        };
        let span = profile.span();
        let regions = self.regions(cx);
        let time_w = regions.time_w;
        let content_left = regions.content_left;
        let before = self.view;
        let delta = scroll_pixels(e);
        let (dx, dy) = (f32::from(delta.x), f32::from(delta.y));
        TIMELINE_GESTURES.dispatch(Pointer::scroll(e), |intent| {
            match intent {
                TimelineIntent::Zoom => {
                    self.scale_view(
                        f32::from(e.position.x) - content_left,
                        f64::from(wheel_zoom_factor(e)),
                        span,
                        time_w,
                    );
                }
                TimelineIntent::PanTime => self.view.pan_px(dx + dy),
                TimelineIntent::Scroll => {
                    // Both sections share the time axis, so a wheel pans
                    // either; the vertical half goes to whichever the cursor
                    // is over.
                    match regions.in_counters(e.position) {
                        true => self.scroll_counters_by(dy, cx),
                        false => self.scroll_by(dy, cx),
                    }
                    self.view.pan_px(dx);
                }
                _ => return false,
            }
            self.view.clamp_to(span, time_w, EDGE_MARGIN_PX);
            true
        });
        self.release_if_moved(before);
        self.invalidate_canvas(cx);
    }

    /// Scales the time axis `factor` times closer about `x` px into the time
    /// axis, holding the zoom between the fit of `span` to `time_w` and the
    /// closest the axis goes.
    fn scale_view(&mut self, x: f32, factor: f64, span: TimeSpan, time_w: f32) {
        self.view.zoom_at(x, factor);
        self.view.px_per_us = held_zoom(self.view.px_per_us, span, time_w);
    }

    fn on_pinch(&mut self, e: &PinchEvent, _: &mut Window, cx: &mut Context<Self>) {
        TIMELINE_GESTURES.dispatch(Pointer::pinch(e), |intent| match intent {
            TimelineIntent::Zoom => {
                self.zoom_about(e.position.x, pinch_factor(e).into(), cx);
                true
            }
            _ => false,
        });
    }

    /// Zooms the time axis `factor` times closer about the middle of the view.
    fn zoom_centred(&mut self, factor: f64, cx: &mut Context<Self>) {
        let regions = self.regions(cx);
        self.zoom_about(px(regions.content_left + regions.time_w / 2.), factor, cx);
    }

    /// Zooms the time axis `factor` times closer about the window-space `x`.
    fn zoom_about(&mut self, x: Pixels, factor: f64, cx: &mut Context<Self>) {
        if self.model.read(cx).chart_lanes().is_none() {
            return;
        }
        let Some(span) = self.model.read(cx).profile().map(|profile| profile.span()) else {
            return;
        };
        let regions = self.regions(cx);
        let before = self.view;
        self.scale_view(
            f32::from(x) - regions.content_left,
            factor,
            span,
            regions.time_w,
        );
        self.view.clamp_to(span, regions.time_w, EDGE_MARGIN_PX);
        self.release_if_moved(before);
        self.invalidate_canvas(cx);
    }

    /// Tracks the cursor for the slice card.
    fn update_hover(&mut self, pos: Point<Pixels>, cx: &mut Context<Self>) {
        let slice = self.slice_at(pos, cx).map(|hovered| *hovered.slice());
        if self.card.hover(pos, slice.as_ref()) {
            cx.notify();
        }
    }

    /// Tracks the cursor for the tooltip and the crosshair while not dragging.
    /// Only the overlay reads these, so the slices need no rebuild.
    fn on_mouse_move(&mut self, e: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !matches!(self.drag, TimelineDrag::None) {
            return;
        }
        TIMELINE_GESTURES.dispatch(Pointer::HOVER, |intent| match intent {
            TimelineIntent::Read => {
                self.read_at(e.position, cx);
                true
            }
            _ => false,
        });
    }

    fn read_at(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let in_content = self.in_content(position, cx);
        let was_crosshair = self.crosshair.is_some();
        self.crosshair = in_content.then_some(position);
        self.update_hover(position, cx);
        if in_content || was_crosshair {
            cx.notify();
        }
    }

    /// Acts on `intent` for the press `e`, declining where the press is not on
    /// the intent's place.
    fn press_for(
        &mut self,
        intent: TimelineIntent,
        e: &MouseDownEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        match intent {
            TimelineIntent::ShowAllOfGroup => match self.group_row_at(e.position, cx) {
                Some(GroupHit::ShowAll(group)) => {
                    self.model.update(cx, |m, _| m.toggle_group_show_all(group));
                }
                _ => return false,
            },
            TimelineIntent::FoldGroup => match self.group_row_at(e.position, cx) {
                Some(GroupHit::Collapse(group)) => {
                    self.model
                        .update(cx, |m, _| m.toggle_group_collapsed(group));
                }
                _ => return false,
            },
            TimelineIntent::PickTrack(pick) => match self.track_label_at(e.position, cx) {
                Some(track) => self
                    .model
                    .update(cx, |m, cx| m.select_track(track, pick, cx)),
                None => return false,
            },
            TimelineIntent::SelectTime { on, toggles } => {
                let took = self.press_time_region(e.position, on, toggles, cx);
                if took {
                    cx.notify();
                }
                return took;
            }
            TimelineIntent::ShowWhole if self.empty_at(e.position, cx) => self.show_whole(cx),
            TimelineIntent::Pan => self.press(e, cx),
            _ => return false,
        }
        self.invalidate_canvas(cx);
        true
    }

    /// The timeline's pointer wiring, acting as [`TIMELINE_GESTURES`] has it,
    /// a press on a scroll thumb or track driving that bar instead of panning.
    /// Holds the content focus and the chart's actions, so after a press here
    /// [`SelectAllTracks`] selects all tracks and [`ClearTimeSelection`]
    /// widens the aggregate to the whole timeline.
    pub(super) fn content_handlers(
        &self,
        root: Stateful<Div>,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        root.key_context(TIMELINE_KEY_CONTEXT)
            .track_focus(&self.content_focus)
            .on_action(cx.listener(Self::select_all_visible))
            .on_action(cx.listener(Self::clear_time_selection))
            .on_action(cx.listener(|this, _: &ShowWholeTimeline, _, cx| {
                this.show_whole(cx);
                this.invalidate_canvas(cx);
            }))
            .on_action(cx.listener(|this, _: &ZoomIn, _, cx| {
                this.zoom_centred(ZOOM_KEY_STEP.into(), cx);
            }))
            .on_action(cx.listener(|this, _: &ZoomOut, _, cx| {
                this.zoom_centred(ZOOM_KEY_STEP.recip().into(), cx);
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, e: &MouseDownEvent, window, cx| {
                    // Take content focus so `SelectAllTracks` dispatches here after
                    // a chart interaction (a click may have moved focus to the filter).
                    this.content_focus.focus(window, cx);
                    TIMELINE_GESTURES
                        .dispatch(Pointer::press(e), |intent| this.press_for(intent, e, cx));
                }),
            )
            .on_scroll_wheel(cx.listener(Self::on_scroll_wheel))
            .on_pinch(cx.listener(Self::on_pinch))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .hover_listener_mode(gpui::HoverListenerMode::InputModalityIndependent)
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                if !*hovered {
                    let cleared = this.crosshair.take().is_some() | this.card.leave();
                    if cleared {
                        cx.notify();
                    }
                }
            }))
    }
}

/// Routes an in-progress timeline drag to its target: panning the chart, moving
/// a scroll thumb, or stretching a time region.
pub(super) fn timeline_on_drag(
    timeline: &mut Timeline,
    position: Point<Pixels>,
    _window: &mut Window,
    cx: &mut Context<Timeline>,
) {
    let moves_slices = !matches!(timeline.drag, TimelineDrag::TimeRegion(_));
    let before = timeline.view;
    match timeline.drag {
        TimelineDrag::Pan(mut pan) => {
            pan.follow(position);
            timeline.drag = TimelineDrag::Pan(pan);
            let dx = f32::from(position.x) - f32::from(timeline.last_mouse.x);
            let dy = f32::from(position.y) - f32::from(timeline.last_mouse.y);
            if timeline.model.read(cx).chart_lanes().is_none() {
                return;
            }
            let Some(profile) = timeline.model.read(cx).profile().cloned() else {
                return;
            };
            let span = profile.span();
            let regions = timeline.regions(cx);
            timeline.view.pan_px(dx);
            timeline.view.clamp_to(span, regions.time_w, EDGE_MARGIN_PX);
            timeline.scroll_by(dy, cx);
            timeline.last_mouse = position;
        }
        TimelineDrag::VThumb(grab) => timeline.scroll_thumb_to(position, grab, cx),
        TimelineDrag::HThumb(grab) => timeline.pan_thumb_to(position, grab, cx),
        TimelineDrag::CountersThumb(grab) => timeline.counters_thumb_to(position, grab, cx),
        TimelineDrag::TimeRegion(mut drag) => {
            if let Some(at) = timeline.time_at(position, cx) {
                drag.cursor_us = at;
                drag.moved |= (f32::from(position.x) - drag.press_x).abs() > MIN_DRAG_PX;
                timeline.drag = TimelineDrag::TimeRegion(drag);
            }
        }
        TimelineDrag::None => {}
    }
    timeline.release_if_moved(before);
    timeline.card.drag(position);
    if moves_slices {
        timeline.invalidate_canvas(cx);
    } else {
        cx.notify();
    }
}

pub(super) fn timeline_end_drag(
    timeline: &mut Timeline,
    position: Point<Pixels>,
    end: DragEnd,
    _: &mut Window,
    cx: &mut Context<Timeline>,
) {
    timeline.update_hover(position, cx);
    match std::mem::replace(&mut timeline.drag, TimelineDrag::None) {
        TimelineDrag::TimeRegion(drag) => {
            timeline.commit_time_region(drag, end, cx);
            cx.notify();
        }
        TimelineDrag::Pan(pan) if pan.clicked(end) => {
            TIMELINE_GESTURES.dispatch(pan.click, |intent| match intent {
                TimelineIntent::Unchoose => {
                    timeline.model.update(cx, |model, cx| model.unchoose(cx));
                    true
                }
                _ => false,
            });
            timeline.invalidate_canvas(cx);
        }
        _ => timeline.invalidate_canvas(cx),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::point;

    #[test]
    fn a_pan_let_go_of_within_the_click_threshold_is_a_click() {
        let press = point(px(100.), px(100.));
        let mut pan = PanDrag::at(&MouseDownEvent {
            position: press,
            ..Default::default()
        });
        pan.follow(point(px(100. + MIN_DRAG_PX), px(100. - MIN_DRAG_PX)));
        assert!(pan.clicked(DragEnd::Released));
        assert!(!pan.clicked(DragEnd::Abandoned), "no release, no click");

        pan.follow(point(px(100.), px(101. + MIN_DRAG_PX)));
        pan.follow(press);
        assert!(!pan.clicked(DragEnd::Released), "moved, then came back");
    }

    #[test]
    fn cancelling_with_no_drag_under_way_reports_nothing_to_abandon() {
        assert!(!TimelineDrag::None.cancel_region());
        let press = MouseDownEvent::default();
        assert!(!TimelineDrag::Pan(PanDrag::at(&press)).cancel_region());
    }
}
