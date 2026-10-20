use crate::camera::{
    Camera, Rect, Settle, clamp_pan, drawn_rect, ease_out_cubic, fit_view, fit_zoom, frame_view,
    settle_view, visible_rect,
};
use crate::derive::Drawn;
use crate::pan_bench::{self, Phase};
use crate::{GraphPanel, MAX_ZOOM, SETTLE_DURATION, SETTLE_MIN_PAN_STEP, SETTLE_MIN_ZOOM_STEP};
use bviz_graph_layout::Id;
use gpui::{App, Bounds, Entity, Pixels, Window, px};

/// What a frame request moves the view onto.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Frame {
    /// What the panel draws, for a gesture that named what to look at.
    Drawing,
    /// What the panel draws, where it covers so little of the window that the
    /// camera has nothing left to hold onto.
    Escaping,
    /// The box of one node, for a gesture that named it.
    Node(Id),
}

/// What a request to move the view resolves to, as a prepaint finds it.
pub(crate) enum Target {
    /// There is no view drawn yet to resolve it at: no drawing, no zoom to
    /// pick a map's level at, or no level derived.
    Pending,
    /// The rect to move the view onto, `None` where the request is answered by
    /// leaving the camera put.
    Resolved(Option<Rect>),
}

/// A view a [`Framing`] stood at, to be returned to.
#[derive(Clone, Copy, PartialEq)]
pub(crate) struct Held {
    camera: Camera,
    chosen_zoom: f32,
    /// The fit the camera stood under, which no other fit's view is of.
    fit: u32,
}

/// Where the graph sits in the window, and the ease that carries it from one
/// placement to the next. Every step over it is a function of this value alone,
/// so none of them needs the entity or the window.
pub(crate) struct Framing {
    camera: Camera,
    /// The zoom the reader put the view at, by the fit to the window or by
    /// working the wheel. A settle may relax the camera below this to show a
    /// toggled box that outgrew the window, and comes back to it once a toggle
    /// leaves that box fitting again, so it is the one part of the framing a
    /// settle never writes.
    chosen_zoom: f32,
    /// Whether the fit-to-window view has been established.
    fitted: bool,
    /// How many fits have been established, which tells the views of one fit,
    /// and of the extent it fitted, from those of another.
    fits: u32,
    /// The canvas bounds as last measured, by a prepaint or by
    /// [`measure`](Self::measure), so hit-testing and the pan/zoom handlers
    /// can work in the same frame the canvas painted.
    viewport: Bounds<Pixels>,
    recenter: bool,
    /// What a prepaint is to move the view onto outright, once the panel has a
    /// drawing and a fitted camera to resolve it at, and how far down the
    /// viewport's top an overlay covers it.
    frame: Option<(Frame, Pixels)>,
    /// The settle ease in flight, stepped once per frame; `None` at rest.
    anim: Option<Settle>,
}

impl Framing {
    /// A framing not yet fitted to a window.
    pub(crate) fn new() -> Self {
        Framing {
            camera: Camera {
                zoom: 1.0,
                pan: (0.0, 0.0),
            },
            chosen_zoom: 1.0,
            fitted: false,
            fits: 0,
            viewport: Bounds::default(),
            recenter: false,
            frame: None,
            anim: None,
        }
    }

    pub(crate) fn camera(&self) -> Camera {
        self.camera
    }

    /// Whether the camera has been fitted to the window since the framing was
    /// made or last [`refit`](Self::refit); until then its zoom is no view's.
    pub(crate) fn fitted(&self) -> bool {
        self.fitted
    }

    pub(crate) fn viewport(&self) -> Bounds<Pixels> {
        self.viewport
    }

    /// Records the canvas bounds while there is no drawing to fit to them, so
    /// the fit the first drawing gets is known before a frame draws it.
    pub(crate) fn measure(&mut self, bounds: Bounds<Pixels>) {
        self.viewport = bounds;
    }

    /// The zoom the next prepaint fits `content` at, where the camera awaits
    /// its fit and the canvas has been measured.
    pub(crate) fn pending_fit(&self, content: (f32, f32)) -> Option<f32> {
        (!self.fitted && !self.viewport.is_empty()).then(|| fit_zoom(self.viewport.size, content))
    }

    /// Moves the camera to a zoom the reader picked, abandoning any settle in
    /// flight: a manual zoom takes the view over from an ease still running. The
    /// zoom becomes the one settles come back to.
    pub(crate) fn move_to(&mut self, camera: Camera) {
        self.camera = camera;
        self.chosen_zoom = camera.zoom;
        self.anim = None;
    }

    /// Slides the camera across the graph, abandoning any settle in flight. The
    /// zoom a settle returns to is left alone: carrying the view sideways is not
    /// the reader asking to be shown the graph at a different size.
    pub(crate) fn pan_to(&mut self, pan: (f32, f32)) {
        self.camera.pan = pan;
        self.anim = None;
    }

    /// Asks the next prepaint to settle the view onto the reframed content.
    pub(crate) fn request_recenter(&mut self) {
        self.recenter = true;
    }

    /// Asks a prepaint to move the view onto what `frame` names.
    pub(crate) fn request_frame(&mut self, frame: Frame) {
        self.request_frame_below(frame, px(0.));
    }

    /// As [`request_frame`](Self::request_frame), under an overlay covering
    /// the viewport's top `covered`.
    pub(crate) fn request_frame_below(&mut self, frame: Frame, covered: Pixels) {
        self.frame = Some((frame, covered));
    }

    /// Where the view stands, for [`return_to`](Self::return_to) to bring it
    /// back to; `None` before the first fit, when it stands at no view.
    pub(crate) fn hold(&self) -> Option<Held> {
        self.fitted.then_some(Held {
            camera: self.camera,
            chosen_zoom: self.chosen_zoom,
            fit: self.fits,
        })
    }

    /// Eases the view back to where `held` found it, dropping any frame or
    /// recenter pending, which would carry it off again. A framing refitted
    /// since is of another extent, which `held` says nothing of, so it is left
    /// where it stands.
    pub(crate) fn return_to(&mut self, held: Held) {
        if !self.fitted || held.fit != self.fits {
            return;
        }
        self.frame = None;
        self.recenter = false;
        self.chosen_zoom = held.chosen_zoom;
        self.ease_to(held.camera.zoom, held.camera.pan);
    }

    /// Drops the established framing, so the next prepaint fits the content
    /// afresh. A layout of a different extent has nothing in common with the
    /// zoom and pan held over from the last one.
    pub(crate) fn refit(&mut self) {
        self.fitted = false;
        self.anim = None;
    }

    /// Arms an ease from where the camera stands onto `(to_zoom, to_pan)`. A move
    /// below the settle steps arms nothing, so a gesture that leaves the view
    /// already framed does not twitch.
    fn ease_to(&mut self, to_zoom: f32, to_pan: (f32, f32)) {
        let (from_zoom, from_pan) = (self.camera.zoom, self.camera.pan);
        let moves = (to_zoom - from_zoom).abs() > SETTLE_MIN_ZOOM_STEP
            || (to_pan.0 - from_pan.0).abs() > SETTLE_MIN_PAN_STEP
            || (to_pan.1 - from_pan.1).abs() > SETTLE_MIN_PAN_STEP;
        if moves {
            self.anim = Some(Settle {
                from_zoom,
                from_pan,
                to_zoom,
                to_pan,
                start: bviz_time::now(),
            });
        }
    }

    /// Records the canvas bounds and establishes the framing they call for: the
    /// first prepaint fits the graph to the window. Reports whether the frame
    /// rendered before it is stale, describing a view that is not drawn (its
    /// status line, its legend): the camera moved, or the fit gave a map's view
    /// the zoom it is resolved at.
    ///
    /// A later prepaint whose bounds changed size keeps the view stable at any
    /// zoom level instead, holding the graph point under the viewport center
    /// fixed and scaling the zoom with the fit, so the same section stays framed
    /// at the same zoom relative to the window rather than snapping to the fully
    /// fitted view. A settle in flight targeted the old size, so it is dropped
    /// rather than eased toward a stale destination.
    fn fit_or_rescale(&mut self, bounds: Bounds<Pixels>, content: (f32, f32)) -> bool {
        let prev = self.viewport;
        let before = self.camera;
        let fitting = !self.fitted;
        self.viewport = bounds;
        if fitting {
            let (z, p) = fit_view(bounds.size, content);
            self.camera = Camera { zoom: z, pan: p };
            self.chosen_zoom = z;
            self.fitted = true;
            self.fits = self.fits.wrapping_add(1);
        } else if prev.size != bounds.size {
            self.anim = None;
            let old_fit = fit_zoom(prev.size, content);
            let new_fit = fit_zoom(bounds.size, content);
            let z_old = self.camera.zoom;
            let (pan_x, pan_y) = self.camera.pan;
            let center_x = (f32::from(prev.size.width) / 2. - pan_x) / z_old;
            let center_y = (f32::from(prev.size.height) / 2. - pan_y) / z_old;
            let z = (z_old * new_fit / old_fit).clamp(new_fit, MAX_ZOOM);
            self.camera.zoom = z;
            self.chosen_zoom = (self.chosen_zoom * new_fit / old_fit).clamp(new_fit, MAX_ZOOM);
            self.camera.pan = clamp_pan(
                (
                    f32::from(bounds.size.width) / 2. - center_x * z,
                    f32::from(bounds.size.height) / 2. - center_y * z,
                ),
                z,
                bounds.size,
                content,
            );
        }
        fitting || (before.zoom, before.pan) != (self.camera.zoom, self.camera.pan)
    }

    /// Answers a pending frame request once `target` is resolved, by arming an
    /// ease onto the rect it resolved to. The zoom it lands at becomes the one
    /// settles come back to, since it is the view a gesture asked for, and it
    /// drops a pending recenter, which would otherwise pull the view off what
    /// was named onto whatever a relayout emphasized.
    fn arm_frame(&mut self, content: (f32, f32), target: Target) {
        let Some((_, covered)) = self.frame else {
            return;
        };
        let Target::Resolved(target) = target else {
            return;
        };
        self.frame = None;
        let Some(target) = target else {
            return;
        };
        self.recenter = false;
        let (zoom, pan) = frame_view(target, self.viewport.size, covered, content);
        self.chosen_zoom = zoom;
        self.ease_to(zoom, pan);
    }

    /// Answers a pending recenter request once `target` is resolved, by arming
    /// an ease onto the rect it resolved to; the request is consumed whether or
    /// not one is armed.
    fn arm_settle(&mut self, content: (f32, f32), target: Target) {
        if !self.recenter {
            return;
        }
        let Target::Resolved(target) = target else {
            return;
        };
        if let Some(target) = target {
            let (zoom, pan) = (self.camera.zoom, self.camera.pan);
            let (to_zoom, to_pan) = settle_view(
                target,
                self.viewport.size,
                content,
                zoom,
                pan,
                self.chosen_zoom,
            );
            self.ease_to(to_zoom, to_pan);
        }
        self.recenter = false;
    }

    /// Advances an in-flight settle, writing the eased zoom and pan and landing
    /// exactly on the target on the last step. Reports whether a frame is due:
    /// every step of a running ease needs the next one, and the step that lands
    /// needs one more if it moved the camera.
    fn step_settle(&mut self) -> bool {
        let Some(a) = self.anim else {
            return false;
        };
        let before = self.camera;
        let t = (a.start.elapsed().as_secs_f32() / SETTLE_DURATION.as_secs_f32()).clamp(0.0, 1.0);
        let e = ease_out_cubic(t);
        self.camera.zoom = a.from_zoom + (a.to_zoom - a.from_zoom) * e;
        self.camera.pan = (
            a.from_pan.0 + (a.to_pan.0 - a.from_pan.0) * e,
            a.from_pan.1 + (a.to_pan.1 - a.from_pan.1) * e,
        );
        if t < 1.0 {
            return true;
        }
        self.anim = None;
        (before.zoom, before.pan) != (self.camera.zoom, self.camera.pan)
    }
}

/// The share of the view the drawing must cover for the camera to be worth
/// holding. A filter change leaves the camera alone so that what survives stays
/// under the same window; below this share nothing is left under the window to
/// hold still, and holding it would spend the whole view on empty canvas.
const READABLE_SHARE: f32 = 0.01;

/// Whether the `boxes` drawn in the layout-space `window` cover so little of it
/// that the camera has nothing left to hold onto.
///
/// Each box is measured whole, boxes nested in one another included, so the
/// share is an upper bound on what the drawing actually covers and the answer
/// errs toward leaving the camera where it stands. A window of no extent is one
/// no prepaint has sized yet, which the first fit answers for.
fn escapes(window: Rect, boxes: &[bviz_graph_layout::Box]) -> bool {
    let area = window.w * window.h;
    if area <= 0.0 {
        return false;
    }
    let covered: f64 = boxes.iter().map(|b| window.overlap(&Rect::of(b))).sum();
    covered / area < READABLE_SHARE as f64
}

/// What the frame request pending on `view` resolves to: what the panel draws
/// at the fitted camera, where the request still wants it. Framing is a camera
/// move, so every box stays where the layout put it.
fn frame_target(view: &GraphPanel) -> Target {
    let Some((frame, _)) = view.framing.frame else {
        return Target::Resolved(None);
    };
    let Some(drawn) = view.drawn() else {
        return Target::Pending;
    };
    let escaping = match frame {
        Frame::Drawing => false,
        Frame::Escaping => true,
        Frame::Node(id) => return node_target(view, &drawn, id),
    };
    let Some(laid_out) = view.view_zoom(&drawn).and_then(|zoom| drawn.view(zoom)) else {
        return Target::Pending;
    };
    let wanted = !escaping
        || escapes(
            visible_rect(view.framing.viewport(), view.framing.camera()),
            &laid_out.boxes,
        );
    Target::Resolved(wanted.then(|| drawn_rect(laid_out)).flatten())
}

/// Where `drawn` draws the node `id`: where the map places it, which every
/// level drawing it shares, or where the layered drawing puts it once any
/// derivation in flight, which may be opening what holds it, has landed.
fn node_target(view: &GraphPanel, drawn: &Drawn, id: Id) -> Target {
    match drawn {
        Drawn::Map { map, .. } => Target::Resolved(map.placed(id).map(Rect::placed)),
        Drawn::Layered { .. } if view.derived.deriving() => Target::Pending,
        Drawn::Layered { view: laid_out, .. } => {
            Target::Resolved(laid_out.boxes.iter().find(|b| b.id == id).map(Rect::of))
        }
    }
}

/// Steers the view's framing from the bounds the canvas was laid out at. Runs at
/// canvas prepaint because the fit needs the viewport size, known only once laid
/// out.
pub(crate) fn build_prepaint(
    parent: Entity<GraphPanel>,
    content: (f32, f32),
    recenter_target: Target,
) -> impl FnOnce(Bounds<Pixels>, &mut Window, &mut App) {
    move |bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App| {
        let _span = pan_bench::span(Phase::CanvasPrepaint);
        let (redraw, moved) = parent.update(cx, |view, _| {
            let before = view.camera();
            let stale = view.framing.fit_or_rescale(bounds, content);
            let target = frame_target(view);
            view.framing.arm_frame(content, target);
            view.framing.arm_settle(content, recenter_target);
            let redraw = stale | view.framing.step_settle();
            (redraw, view.camera() != before)
        });
        if redraw {
            window.request_animation_frame();
        }
        if moved {
            // A notify made while the frame is drawn draws no frame of its own.
            cx.defer(move |cx| parent.update(cx, GraphPanel::drawing_moved));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bviz_time::Instant;
    use gpui::{Size, point, px, size};

    // The whole graph, large enough that fit_zoom's floor stays well below 1.0 and
    // never bites in these cases.
    const CONTENT: (f32, f32) = (4000.0, 3000.0);

    fn vp(w: f32, h: f32) -> Size<Pixels> {
        size(px(w), px(h))
    }

    fn canvas_bounds(w: f32, h: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(0.), px(0.)), vp(w, h))
    }

    /// A framing already fitted to `bounds`, at the given camera and at rest.
    fn framed(zoom: f32, pan: (f32, f32), bounds: Bounds<Pixels>) -> Framing {
        Framing {
            camera: Camera { zoom, pan },
            chosen_zoom: zoom,
            viewport: bounds,
            fitted: true,
            fits: 1,
            recenter: false,
            frame: None,
            anim: None,
        }
    }

    /// A held view is eased back to under the fit it was held at, and left
    /// alone once the framing has been fitted to the window afresh.
    #[test]
    fn a_held_view_is_returned_to_only_under_the_fit_it_was_held_at() {
        let bounds = canvas_bounds(800., 600.);
        let away = Camera {
            zoom: 2.0,
            pan: (40.0, 30.0),
        };
        let mut f = framed(1.0, (0.0, 0.0), bounds);
        let held = f.hold().expect("a fitted framing holds its view");
        f.camera = away;
        f.return_to(held);
        let settle = f.anim.as_ref().expect("an ease back");
        assert_eq!((settle.to_zoom, settle.to_pan), (1.0, (0.0, 0.0)));

        f.anim = None;
        f.refit();
        f.fit_or_rescale(bounds, (400.0, 300.0));
        f.camera = away;
        f.return_to(held);
        assert!(f.anim.is_none());
    }

    /// The camera the settle tests ease away from: off the identity zoom and
    /// asymmetric across the axes, so a step that swaps the zoom, x and y
    /// components for one another lands outside the expected values.
    const EASE_FROM_ZOOM: f32 = 0.25;
    const EASE_FROM_PAN: (f32, f32) = (-30.0, 70.0);

    fn settle_to(zoom: f32, pan: (f32, f32), start: Instant) -> Settle {
        Settle {
            from_zoom: EASE_FROM_ZOOM,
            from_pan: EASE_FROM_PAN,
            to_zoom: zoom,
            to_pan: pan,
            start,
        }
    }

    fn lbox(x: f64, y: f64, w: f64, h: f64) -> bviz_graph_layout::Box {
        bviz_graph_layout::Box {
            id: bviz_graph_layout::Id(0),
            label: String::new(),
            kind: bviz_graph_layout::Kind::Source,
            x,
            y,
            width: w,
            height: h,
            compound: false,
            group: false,
        }
    }

    /// The window a view fitted to the whole graph shows, in layout units.
    const WINDOW: Rect = Rect {
        x: 0.0,
        y: 0.0,
        w: CONTENT.0 as f64,
        h: CONTENT.1 as f64,
    };

    /// A filter leaving boxes that fill the window leaves the camera where the
    /// reader put it; one leaving a speck in a window the whole graph's extent
    /// still spans leaves nothing to hold still.
    #[test]
    fn a_drawing_escapes_the_view_only_once_it_covers_next_to_none_of_it() {
        assert!(!escapes(WINDOW, &[lbox(0.0, 0.0, 2000.0, 1500.0)]));
        assert!(escapes(WINDOW, &[lbox(1000.0, 1000.0, 40.0, 30.0)]));
        assert!(escapes(WINDOW, &[]));
        // A box off the window covers none of it however large it is.
        assert!(escapes(WINDOW, &[lbox(90_000.0, 0.0, 4000.0, 3000.0)]));
    }

    /// A window no prepaint has sized is one the first fit answers for, so it
    /// asks for no move of its own.
    #[test]
    fn a_window_of_no_extent_moves_nothing() {
        let unsized_yet = Rect {
            x: 0.0,
            y: 0.0,
            w: 0.0,
            h: 0.0,
        };
        assert!(!escapes(unsized_yet, &[]));
    }

    #[test]
    fn first_prepaint_fits_and_latches() {
        let mut f = Framing::new();
        f.fit_or_rescale(canvas_bounds(800., 600.), CONTENT);
        let (z, p) = fit_view(vp(800., 600.), CONTENT);
        assert_eq!((f.camera.zoom, f.camera.pan), (z, p));
        assert!(f.fitted);
        assert_eq!(f.viewport.size, vp(800., 600.));
    }

    /// The fit a measured canvas is due is the one its first prepaint makes, so
    /// what that frame draws can be made ready before it.
    #[test]
    fn a_measured_canvas_foresees_its_fit() {
        let bounds = canvas_bounds(800., 600.);
        let mut f = Framing::new();
        assert_eq!(f.pending_fit(CONTENT), None, "nothing measured yet");

        f.measure(bounds);
        let foreseen = f.pending_fit(CONTENT);
        f.fit_or_rescale(bounds, CONTENT);

        assert_eq!(foreseen, Some(f.camera().zoom));
        assert_eq!(f.pending_fit(CONTENT), None, "the camera is fitted");
    }

    #[test]
    fn a_fit_that_moves_nothing_still_asks_for_a_frame() {
        let bounds = canvas_bounds(800., 600.);
        let (zoom, pan) = fit_view(bounds.size, CONTENT);
        let mut f = framed(zoom, pan, bounds);
        f.refit();
        assert!(f.fit_or_rescale(bounds, CONTENT));
        assert!(!f.fit_or_rescale(bounds, CONTENT));
    }

    #[test]
    fn a_second_prepaint_at_the_same_size_holds_the_view() {
        // A hand-panned view, redrawn at the same bounds: no refit, no reframe.
        let bounds = canvas_bounds(800., 600.);
        let mut f = framed(0.4, (-123.0, 45.0), bounds);
        f.anim = Some(settle_to(0.5, (10.0, 20.0), bviz_time::now()));
        f.fit_or_rescale(bounds, CONTENT);
        assert_eq!((f.camera.zoom, f.camera.pan), (0.4, (-123.0, 45.0)));
        assert!(f.anim.is_some(), "an unrelated repaint dropped the settle");
    }

    #[test]
    fn a_resize_rescales_rather_than_refitting_and_drops_a_settle() {
        // Centered on the content at 3x the 800x600 fit; the viewport then doubles
        // on both axes, so the fit — and with it the zoom — doubles too, and the
        // content stays centered.
        let mut f = framed(0.4, (-400.0, -300.0), canvas_bounds(800., 600.));
        f.anim = Some(settle_to(0.5, (10.0, 20.0), bviz_time::now()));
        f.fit_or_rescale(canvas_bounds(1600., 1200.), CONTENT);
        assert!((f.camera.zoom - 0.8).abs() < 1e-6, "zoom {}", f.camera.zoom);
        assert!(
            f.camera.zoom > fit_zoom(vp(1600., 1200.), CONTENT),
            "refitted instead of rescaling: {}",
            f.camera.zoom
        );
        assert!(
            (f.camera.pan.0 + 800.0).abs() < 1e-3,
            "pan {:?}",
            f.camera.pan
        );
        assert!(
            (f.camera.pan.1 + 600.0).abs() < 1e-3,
            "pan {:?}",
            f.camera.pan
        );
        assert!(
            f.anim.is_none(),
            "a settle aimed at the old size survived the resize"
        );
    }

    #[test]
    fn arming_below_the_settle_steps_installs_no_ease() {
        // The target already sits comfortably framed, so the move is a no-op.
        let target = Rect {
            x: 375.0,
            y: 280.0,
            w: 50.0,
            h: 40.0,
        };
        let mut f = framed(1.0, (0.0, 0.0), canvas_bounds(800., 600.));
        f.recenter = true;
        f.arm_settle(CONTENT, Target::Resolved(Some(target)));
        assert!(f.anim.is_none());
        assert!(
            !f.recenter,
            "the request outlived the prepaint that answered it"
        );
    }

    #[test]
    fn arming_measures_from_the_readers_zoom_not_from_where_a_settle_left_the_camera() {
        // A camera an earlier settle relaxed to 0.25 to take in content a toggle
        // had spread apart, under a reader's zoom of 1.0 that the settle left
        // alone. A target that fits again brings the view back up to it rather
        // than stranding it at the relaxed zoom.
        let target = Rect {
            x: 375.0,
            y: 280.0,
            w: 50.0,
            h: 40.0,
        };
        let mut f = framed(1.0, (0.0, 0.0), canvas_bounds(800., 600.));
        f.camera.zoom = 0.25;
        f.recenter = true;
        f.arm_settle(CONTENT, Target::Resolved(Some(target)));
        let a = f.anim.expect("no settle armed");
        assert!(
            (a.to_zoom - 1.0).abs() < 1e-4,
            "settled to {}, not back to the reader's 1.0",
            a.to_zoom
        );
        assert_eq!(f.chosen_zoom, 1.0, "the settle wrote the reader's zoom");
    }

    #[test]
    fn arming_on_an_offscreen_target_eases_onto_it() {
        let target = Rect {
            x: 1000.0,
            y: 100.0,
            w: 50.0,
            h: 40.0,
        };
        let mut f = framed(1.0, (0.0, 0.0), canvas_bounds(800., 600.));
        f.recenter = true;
        f.arm_settle(CONTENT, Target::Resolved(Some(target)));
        let (tz, tp) = settle_view(target, vp(800., 600.), CONTENT, 1.0, (0.0, 0.0), 1.0);
        let a = f.anim.expect("no settle armed");
        assert_eq!((a.from_zoom, a.from_pan), (1.0, (0.0, 0.0)));
        assert_eq!((a.to_zoom, a.to_pan), (tz, tp));
        // Arming alone leaves the camera put; the ease moves it.
        assert_eq!((f.camera.zoom, f.camera.pan), (1.0, (0.0, 0.0)));
        assert!(!f.recenter);
    }

    /// The rect a frame request names, far off the fitted view and small enough
    /// that framing it has to zoom well in.
    const NAMED: Rect = Rect {
        x: 3000.0,
        y: 2400.0,
        w: 120.0,
        h: 90.0,
    };

    #[test]
    fn a_frame_request_eases_onto_what_it_named_and_leaves_settles_coming_back_there() {
        let mut f = framed(1.0, (0.0, 0.0), canvas_bounds(800., 600.));
        f.request_frame(Frame::Drawing);
        f.arm_frame(CONTENT, Target::Resolved(Some(NAMED)));

        let a = f.anim.expect("the frame request armed no ease");
        assert_eq!(
            (a.to_zoom, a.to_pan),
            frame_view(NAMED, vp(800., 600.), px(0.), CONTENT)
        );
        assert_eq!(
            f.chosen_zoom, a.to_zoom,
            "a settle would pull the view off what was named"
        );
        assert!(f.frame.is_none());
    }

    /// A frame requested before there is a drawing to resolve it at, such as a
    /// restriction whose map the document has yet to place, waits for one.
    #[test]
    fn a_frame_request_waits_for_a_drawing_to_frame() {
        let mut f = framed(1.0, (0.0, 0.0), canvas_bounds(800., 600.));
        f.request_frame(Frame::Drawing);

        f.arm_frame(CONTENT, Target::Pending);
        assert!(f.anim.is_none());
        assert_eq!(f.frame, Some((Frame::Drawing, px(0.))));

        f.arm_frame(CONTENT, Target::Resolved(Some(NAMED)));
        let a = f.anim.expect("the frame request armed no ease");
        assert_eq!(
            (a.to_zoom, a.to_pan),
            frame_view(NAMED, vp(800., 600.), px(0.), CONTENT)
        );
        assert!(f.frame.is_none());
    }

    /// A filter change that names what to look at also lands a layered drawing,
    /// whose settle would otherwise carry the view onto whatever that relayout
    /// emphasized.
    #[test]
    fn a_frame_request_supersedes_a_recenter_pending_with_it() {
        let mut f = framed(1.0, (0.0, 0.0), canvas_bounds(800., 600.));
        f.request_recenter();
        f.request_frame(Frame::Drawing);

        f.arm_frame(CONTENT, Target::Resolved(Some(NAMED)));
        let framed_at = f.anim.expect("the frame request armed no ease");
        f.arm_settle(
            CONTENT,
            Target::Resolved(Some(Rect {
                x: 0.0,
                y: 0.0,
                w: 50.0,
                h: 40.0,
            })),
        );

        let still = f.anim.expect("the settle dropped the frame's ease");
        assert_eq!(
            (still.to_zoom, still.to_pan),
            (framed_at.to_zoom, framed_at.to_pan)
        );
    }

    /// A recenter requested while no view is drawn, such as a map whose level
    /// is yet to be derived, is answered by the first one that is.
    #[test]
    fn a_recenter_waits_for_a_view_to_resolve_it_at() {
        let target = Rect {
            x: 1000.0,
            y: 100.0,
            w: 50.0,
            h: 40.0,
        };
        let mut f = framed(1.0, (0.0, 0.0), canvas_bounds(800., 600.));
        f.request_recenter();

        f.arm_settle(CONTENT, Target::Pending);
        assert!(f.anim.is_none());
        assert!(f.recenter);

        f.arm_settle(CONTENT, Target::Resolved(Some(target)));
        assert!(f.anim.is_some());
        assert!(!f.recenter);
    }

    #[test]
    fn arming_without_a_target_consumes_the_request() {
        let mut f = framed(1.0, (0.0, 0.0), canvas_bounds(800., 600.));
        f.recenter = true;
        f.arm_settle(CONTENT, Target::Resolved(None));
        assert!(f.anim.is_none());
        assert!(!f.recenter);
    }

    #[test]
    fn a_finished_settle_lands_on_the_target_and_asks_for_the_frame_that_shows_it() {
        let mut f = framed(1.0, (0.0, 0.0), canvas_bounds(800., 600.));
        f.anim = Some(settle_to(
            2.0,
            (100.0, -50.0),
            bviz_time::now() - SETTLE_DURATION,
        ));
        assert!(
            f.step_settle(),
            "the step that moved the camera asked for no frame"
        );
        assert_eq!((f.camera.zoom, f.camera.pan), (2.0, (100.0, -50.0)));
        assert!(f.anim.is_none());
        // Landed and drawn: the ease is over and asks for nothing further.
        assert!(!f.step_settle());
    }

    #[test]
    fn a_settle_landing_where_the_camera_already_stands_asks_for_no_frame() {
        let (zoom, pan) = (EASE_FROM_ZOOM, EASE_FROM_PAN);
        let mut f = framed(zoom, pan, canvas_bounds(800., 600.));
        f.anim = Some(settle_to(zoom, pan, bviz_time::now() - SETTLE_DURATION));
        assert!(
            !f.step_settle(),
            "a landing that moved nothing asked for a frame"
        );
        assert!(f.anim.is_none());
    }

    #[test]
    fn a_settle_mid_ease_asks_for_another_frame() {
        let (to_zoom, to_pan) = (2.0, (100.0, -50.0));
        let mut f = framed(1.0, (0.0, 0.0), canvas_bounds(800., 600.));
        f.anim = Some(settle_to(
            to_zoom,
            to_pan,
            bviz_time::now() - SETTLE_DURATION / 2,
        ));
        assert!(f.step_settle());
        assert!(f.anim.is_some());
        // Halfway through in time, so every component sits where the ease curve
        // puts it over 0.4..=0.6 — a band loose enough to absorb the scheduling
        // jitter around that half, tight enough to exclude any other curve.
        let (lo, hi) = (ease_out_cubic(0.4), ease_out_cubic(0.6));
        let eased = |from: f32, to: f32| {
            let (a, b) = (from + (to - from) * lo, from + (to - from) * hi);
            a.min(b)..=a.max(b)
        };
        let (z, (panx, pany)) = (f.camera.zoom, f.camera.pan);
        assert!(
            eased(EASE_FROM_ZOOM, to_zoom).contains(&z),
            "zoom off the ease: {z}"
        );
        assert!(
            eased(EASE_FROM_PAN.0, to_pan.0).contains(&panx),
            "pan x off the ease: {panx}"
        );
        assert!(
            eased(EASE_FROM_PAN.1, to_pan.1).contains(&pany),
            "pan y off the ease: {pany}"
        );
    }

    #[test]
    fn stepping_a_framing_at_rest_asks_for_no_frame() {
        let mut f = framed(0.7, (12.0, -34.0), canvas_bounds(800., 600.));
        assert!(!f.step_settle());
        assert_eq!((f.camera.zoom, f.camera.pan), (0.7, (12.0, -34.0)));
    }

    #[test]
    fn a_prepaint_pass_records_each_step_in_the_framing() {
        // A resize with a recenter pending, driving all three steps: the pass
        // rescales the camera, discards the ease aimed at the old size, arms one
        // onto the target and starts it, so the framing left behind pins what
        // each step wrote.
        let bounds = canvas_bounds(800., 600.);
        let target = Rect {
            x: 2000.0,
            y: 1500.0,
            w: 200.0,
            h: 150.0,
        };
        let mut f = framed(0.6, (-100.0, -200.0), canvas_bounds(400., 300.));
        f.recenter = true;
        f.anim = Some(settle_to(4.0, (1.0, 2.0), bviz_time::now()));

        f.fit_or_rescale(bounds, CONTENT);
        f.arm_settle(CONTENT, Target::Resolved(Some(target)));
        assert!(
            f.step_settle(),
            "the ease landed on the frame that armed it"
        );

        assert_eq!(f.viewport, bounds);
        assert!(f.fitted);
        assert!(
            !f.recenter,
            "the request outlived the prepaint that answered it"
        );
        let a = f.anim.expect("the recenter armed no ease");
        // The viewport doubles on both axes, so the fit and with it the zoom
        // double, and the point under the old center stays under the new one.
        assert!(
            (a.from_zoom - 1.2).abs() < 1e-5,
            "ease origin zoom {}",
            a.from_zoom
        );
        assert!(
            (a.from_pan.0 + 200.0).abs() < 1e-3,
            "ease origin pan {:?}",
            a.from_pan
        );
        assert!(
            (a.from_pan.1 + 400.0).abs() < 1e-3,
            "ease origin pan {:?}",
            a.from_pan
        );
        assert_eq!(
            (a.to_zoom, a.to_pan),
            settle_view(
                target,
                bounds.size,
                CONTENT,
                a.from_zoom,
                a.from_pan,
                f.chosen_zoom
            )
        );
        let travelled = |v: f32, from: f32, to: f32| (from.min(to)..=from.max(to)).contains(&v);
        assert!(
            travelled(f.camera.pan.0, a.from_pan.0, a.to_pan.0),
            "pan {:?}",
            f.camera.pan
        );
        assert!(
            travelled(f.camera.pan.1, a.from_pan.1, a.to_pan.1),
            "pan {:?}",
            f.camera.pan
        );
    }
}
