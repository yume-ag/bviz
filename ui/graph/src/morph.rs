use crate::camera::{Rect, ease_out_cubic, smoothstep};
use crate::{GraphPanel, SETTLE_DURATION};
use bviz_graph_layout::{Box, Id, LaidOut};
use bviz_time::Instant;
use gpui::Context;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// How long the new drawing's connectors take to fade in. It runs well longer
/// than the travel: a connector is a thin line against the background, so it
/// reads as present over a short stretch of its alpha and a brisk fade lands as
/// an appearance out of nothing rather than as an arrival.
const CONNECTOR_FADE: Duration = Duration::from_millis(520);

/// How far before the boxes land the connectors begin arriving. A route is
/// computed for one drawing's box positions, so it meets its endpoints only once
/// they have stopped; over this last stretch of a travel easing to a halt they
/// have all but arrived, and the fade holds the routes too faint to read
/// besides, so what little they are adrift by cannot be seen. Reaching
/// visibility on the boxes' heels rather than from nothing afterwards is what
/// keeps the drawing from pausing bare between the two.
const CONNECTOR_FADE_LEAD: Duration = Duration::from_millis(60);

/// When the connectors start arriving, measured from the start of the travel.
const CONNECTOR_FADE_START: Duration = SETTLE_DURATION.saturating_sub(CONNECTOR_FADE_LEAD);

/// The whole transition: the boxes travel, and the connectors arrive over the
/// close of that travel and the stretch after it.
const MORPH_SPAN: Duration = CONNECTOR_FADE_START.saturating_add(CONNECTOR_FADE);

/// Repaint interval while a morph runs. Boxes are in motion, which reads as
/// continuous only close to the display's own rate.
const MORPH_TICK: Duration = Duration::from_millis(16);

/// Layout-unit travel below which a box counts as having stayed put, so a
/// relayout that redraws the same picture animates nothing.
const MIN_TRAVEL: f64 = 0.5;

/// Where a box of the new drawing travels from, and the strength it starts at.
struct Start {
    from: Rect,
    /// Alpha at the start of the travel, reaching 1 as it ends: a box the
    /// previous drawing did not hold fades in from nothing, one caught part way
    /// through such a fade takes up again at the strength it had reached, and
    /// any other box is painted throughout.
    alpha: f32,
}

impl Start {
    /// The strength the box paints at, `travel` of the way through.
    fn alpha_at(&self, travel: f32) -> f32 {
        self.alpha + (1.0 - self.alpha) * travel
    }
}

/// A box only the previous drawing held, on its way out.
struct Leaving {
    /// Its place in the retained drawing's `boxes`.
    index: usize,
    from: Rect,
    to: Rect,
}

/// One drawing on its way to the drawing that replaced it. Sampling it at an
/// instant yields a [`Frame`], and every geometry the paint needs follows from
/// that alone.
pub(crate) struct Transition {
    /// The drawing being left, retained so the boxes it alone holds can still be
    /// painted while they fade.
    previous: Arc<LaidOut>,
    /// Keyed by box id of the new drawing. A box absent from it has neither
    /// moved, nor entered, nor a fade left to finish, so it paints where the new
    /// drawing puts it at full strength.
    starts: HashMap<Id, Start>,
    leaving: Vec<Leaving>,
    start: Instant,
}

impl Transition {
    /// The transition from `previous` to `current`, or `None` when the two
    /// drawings differ by nothing there is to animate. `drawn` places the boxes
    /// of `previous`, so a relayout landing while a morph is still under way
    /// carries each box on from where it is drawn and at the strength it has
    /// reached.
    pub(crate) fn between(
        previous: Arc<LaidOut>,
        current: &LaidOut,
        drawn: Option<&Frame>,
    ) -> Option<Self> {
        let was: HashMap<Id, Rect> = previous
            .boxes
            .iter()
            .map(|b| (b.id, drawn.map_or(Rect::of(b), |frame| frame.place(b).0)))
            .collect();
        let now: HashMap<Id, Rect> = current.boxes.iter().map(|b| (b.id, Rect::of(b))).collect();
        let kept_now = kept_containers(&current.boxes, &now, &was);
        let kept_before = kept_containers(&previous.boxes, &was, &now);

        let mut starts = HashMap::with_capacity(current.boxes.len());
        for b in &current.boxes {
            let to = Rect::of(b);
            match was.get(&b.id) {
                Some(&from) => {
                    let alpha = drawn.map_or(1.0, |frame| frame.alpha_of(b.id));
                    if travels(from, to) || alpha < 1.0 {
                        starts.insert(b.id, Start { from, alpha });
                    }
                }
                None => {
                    starts.insert(
                        b.id,
                        Start {
                            from: across(to, &kept_now),
                            alpha: 0.0,
                        },
                    );
                }
            }
        }
        let leaving: Vec<Leaving> = previous
            .boxes
            .iter()
            .enumerate()
            .filter(|(_, b)| !now.contains_key(&b.id))
            .map(|(index, b)| {
                let from = was[&b.id];
                Leaving {
                    index,
                    from,
                    to: across(from, &kept_before),
                }
            })
            .collect();
        if starts.is_empty() && leaving.is_empty() {
            return None;
        }
        Some(Transition {
            previous,
            starts,
            leaving,
            start: bviz_time::now(),
        })
    }

    /// How far along `transition` stands at `now`, or `None` once it is over —
    /// past which the new drawing paints exactly as it does at rest.
    pub(crate) fn at(transition: &Arc<Self>, now: Instant) -> Option<Frame> {
        let elapsed = now.saturating_duration_since(transition.start);
        if elapsed >= MORPH_SPAN {
            return None;
        }
        let travelled = (elapsed.as_secs_f32() / SETTLE_DURATION.as_secs_f32()).clamp(0.0, 1.0);
        let connectors = (elapsed.saturating_sub(CONNECTOR_FADE_START).as_secs_f32()
            / CONNECTOR_FADE.as_secs_f32())
        .clamp(0.0, 1.0);
        Some(Frame {
            transition: Arc::clone(transition),
            travel: ease_out_cubic(travelled),
            connectors: smoothstep(connectors),
        })
    }
}

/// A transition sampled at one instant, which every box and connector of that
/// frame is placed and shaded from.
#[derive(Clone)]
pub(crate) struct Frame {
    transition: Arc<Transition>,
    /// Eased progress of the boxes' travel: 0 at the previous drawing, 1 at the
    /// new one.
    travel: f32,
    /// Alpha the new drawing's connectors paint at.
    pub(crate) connectors: f32,
}

impl Frame {
    /// Where a box of the new drawing sits this frame, and the alpha it paints
    /// at.
    pub(crate) fn place(&self, b: &Box) -> (Rect, f32) {
        let to = Rect::of(b);
        match self.transition.starts.get(&b.id) {
            Some(start) => (
                between(start.from, to, self.travel),
                start.alpha_at(self.travel),
            ),
            None => (to, 1.0),
        }
    }

    /// The strength the box `id` paints at this frame, whatever rect it is drawn
    /// in, so a transition armed over this one can take a fade up where it left
    /// off.
    fn alpha_of(&self, id: Id) -> f32 {
        self.transition
            .starts
            .get(&id)
            .map_or(1.0, |start| start.alpha_at(self.travel))
    }

    /// The boxes only the previous drawing held, each where it sits this frame
    /// and at the alpha it fades out through.
    pub(crate) fn leaving(&self) -> impl Iterator<Item = (&Box, Rect, f32)> {
        let (transition, travel) = (&self.transition, self.travel);
        transition.leaving.iter().map(move |l| {
            let b = &transition.previous.boxes[l.index];
            (b, between(l.from, l.to, travel), 1.0 - travel)
        })
    }
}

/// The containers of `boxes` that both drawings hold, each as the rect it spans
/// in `here` paired with the rect it spans in `there`.
fn kept_containers(
    boxes: &[Box],
    here: &HashMap<Id, Rect>,
    there: &HashMap<Id, Rect>,
) -> Vec<(Rect, Rect)> {
    boxes
        .iter()
        .filter(|b| b.compound)
        .filter_map(|b| Some((*here.get(&b.id)?, *there.get(&b.id)?)))
        .collect()
}

/// Where `r` comes from, or goes to, in the other drawing: carried across by the
/// smallest kept container enclosing it, which holds it inside that container for
/// the whole travel however far the container itself moves or resizes. A box no
/// kept container encloses fades where it stands.
fn across(r: Rect, kept: &[(Rect, Rect)]) -> Rect {
    kept.iter()
        .filter(|(here, _)| here.encloses(&r))
        .min_by(|(a, _), (b, _)| (a.w * a.h).total_cmp(&(b.w * b.h)))
        .map_or(r, |&(here, there)| remap(r, here, there))
}

/// The rect `r` occupies once `from` is carried onto `to`, keeping the same
/// proportion of the container it sits in.
fn remap(r: Rect, from: Rect, to: Rect) -> Rect {
    let axis = |v: f64, len: f64, f0: f64, f_len: f64, t0: f64, t_len: f64| {
        let scale = if f_len > 0.0 { t_len / f_len } else { 1.0 };
        (t0 + (v - f0) * scale, len * scale)
    };
    let (x, w) = axis(r.x, r.w, from.x, from.w, to.x, to.w);
    let (y, h) = axis(r.y, r.h, from.y, from.h, to.y, to.h);
    Rect { x, y, w, h }
}

/// The rect `e` of the way from `from` to `to`.
fn between(from: Rect, to: Rect, e: f32) -> Rect {
    let mix = |a: f64, b: f64| a + (b - a) * e as f64;
    Rect {
        x: mix(from.x, to.x),
        y: mix(from.y, to.y),
        w: mix(from.w, to.w),
        h: mix(from.h, to.h),
    }
}

/// Whether a box crossing from `from` to `to` moves far enough to be worth
/// animating.
fn travels(from: Rect, to: Rect) -> bool {
    (from.x - to.x).abs() >= MIN_TRAVEL
        || (from.y - to.y).abs() >= MIN_TRAVEL
        || (from.w - to.w).abs() >= MIN_TRAVEL
        || (from.h - to.h).abs() >= MIN_TRAVEL
}

/// The transition between the drawing a relayout replaced and the one that
/// replaced it, and the pump that carries it through.
pub(crate) struct Morph {
    current: Option<Arc<Transition>>,
    /// Identifies the transition a timer drives, so a timer whose generation has
    /// been superseded retires instead of ending a fresher one.
    generation: u64,
}

/// What a morph timer's tick found.
enum Step {
    /// A later relayout owns the morph, leaving this timer nothing to drive.
    Superseded,
    Travelling,
    Landed,
}

impl Morph {
    pub(crate) fn none() -> Self {
        Self {
            current: None,
            generation: 0,
        }
    }

    /// How far the transition in flight stands at `now`; `None` at rest.
    pub(crate) fn at(&self, now: Instant) -> Option<Frame> {
        Transition::at(self.current.as_ref()?, now)
    }

    /// Drops the morph, retiring any timer driving it.
    pub(crate) fn clear(&mut self) {
        self.current = None;
        self.generation = self.generation.wrapping_add(1);
    }

    /// Starts easing `previous` onto `current`, yielding the generation its timer
    /// quotes back to [`Morph::tick`]; `None` when there is nothing to animate,
    /// which leaves whatever is in flight to finish on its own.
    fn arm(&mut self, previous: Arc<LaidOut>, current: &LaidOut) -> Option<u64> {
        let drawn = self.at(bviz_time::now());
        let transition = Transition::between(previous, current, drawn.as_ref())?;
        self.current = Some(Arc::new(transition));
        self.generation = self.generation.wrapping_add(1);
        Some(self.generation)
    }

    fn tick(&mut self, generation: u64) -> Step {
        if self.generation != generation {
            return Step::Superseded;
        }
        match &self.current {
            Some(t) if t.start.elapsed() < MORPH_SPAN => Step::Travelling,
            _ => {
                self.current = None;
                Step::Landed
            }
        }
    }
}

/// Eases the boxes of the drawing a relayout replaced onto their places in the
/// new one, repainting as they travel and retiring at the end. Called as the
/// relayout lands.
pub(crate) fn begin(
    view: &mut GraphPanel,
    previous: Arc<LaidOut>,
    current: &LaidOut,
    cx: &mut Context<GraphPanel>,
) {
    let Some(generation) = view.morph.arm(previous, current) else {
        return;
    };
    cx.spawn(async move |this, cx| {
        loop {
            cx.background_executor().timer(MORPH_TICK).await;
            let done = this.update(cx, |this, cx| match this.morph.tick(generation) {
                Step::Superseded => true,
                Step::Travelling => {
                    this.invalidate_canvas_only(cx);
                    false
                }
                Step::Landed => {
                    this.invalidate_canvas_only(cx);
                    true
                }
            });
            if done.unwrap_or(true) {
                break;
            }
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;
    use bviz_graph_layout::Kind;

    /// Alpha under which a connector, a hairline against the background, does
    /// not read as drawn at all.
    const IMPERCEPTIBLE_CONNECTOR: f32 = 0.05;

    fn lbox(id: u32, r: Rect, compound: bool) -> Box {
        Box {
            id: Id(id),
            label: String::new(),
            kind: Kind::Source,
            x: r.x,
            y: r.y,
            width: r.w,
            height: r.h,
            compound,
            group: false,
        }
    }

    fn rect(x: f64, y: f64, w: f64, h: f64) -> Rect {
        Rect { x, y, w, h }
    }

    fn drawing(boxes: Vec<Box>) -> LaidOut {
        LaidOut {
            boxes,
            ..Default::default()
        }
    }

    /// The container as the collapsed drawing holds it, and as the open one does.
    const SHUT: Rect = Rect {
        x: 400.0,
        y: 0.0,
        w: 120.0,
        h: 30.0,
    };
    const OPEN: Rect = Rect {
        x: 300.0,
        y: 200.0,
        w: 600.0,
        h: 400.0,
    };

    /// A container beside a neighbour, collapsed: the container holds nothing —
    /// which is why it is not compound, as a layout marks only a box with drawn
    /// children — and the neighbour sits where the open drawing does not put it.
    fn collapsed() -> LaidOut {
        drawing(vec![
            lbox(0, SHUT, false),
            lbox(1, rect(0.0, 0.0, 200.0, 40.0), false),
        ])
    }

    /// The collapsed drawing with its neighbour nudged `dx` along, so a
    /// transition onto it moves one box by exactly that much and leaves the
    /// other where it stands.
    fn nudged(dx: f64) -> LaidOut {
        drawing(vec![
            lbox(0, SHUT, false),
            lbox(1, rect(dx, 0.0, 200.0, 40.0), false),
        ])
    }

    /// The same two boxes with the container open over two members, so a
    /// transition either way has a box travelling, boxes entering or leaving, and
    /// a container carrying them.
    fn opened() -> LaidOut {
        drawing(vec![
            lbox(0, OPEN, true),
            lbox(1, rect(0.0, 500.0, 200.0, 40.0), false),
            lbox(2, rect(340.0, 260.0, 200.0, 40.0), false),
            lbox(3, rect(340.0, 360.0, 200.0, 40.0), false),
        ])
    }

    fn transition(before: LaidOut, after: &LaidOut) -> Arc<Transition> {
        Arc::new(Transition::between(Arc::new(before), after, None).expect("nothing to animate"))
    }

    /// `t` sampled `fraction` of the way through the boxes' travel.
    fn at(t: &Arc<Transition>, fraction: f32) -> Frame {
        Transition::at(t, t.start + SETTLE_DURATION.mul_f32(fraction))
            .expect("the transition was already over")
    }

    /// Slack in layout units for a rect compared against a morph sampled at a
    /// slightly different instant, wide enough to cover the sliver of travel
    /// between two `bviz_time::now()` calls and far narrower than the hundreds of
    /// units the fixtures' boxes travel.
    const SAMPLING_SLACK: f64 = 10.0;

    fn assert_close(got: Rect, want: Rect, what: &str) {
        let near = |a: f64, b: f64| (a - b).abs() < 1e-6;
        assert!(
            near(got.x, want.x)
                && near(got.y, want.y)
                && near(got.w, want.w)
                && near(got.h, want.h),
            "{what}: {got:?} is not {want:?}"
        );
    }

    /// Whether `inner` sits wholly within `outer`, with a hair of slack for the
    /// arithmetic that placed it.
    fn within(inner: Rect, outer: Rect) -> bool {
        let eps = 1e-6;
        inner.x >= outer.x - eps
            && inner.y >= outer.y - eps
            && inner.x + inner.w <= outer.x + outer.w + eps
            && inner.y + inner.h <= outer.y + outer.h + eps
    }

    #[test]
    fn a_box_both_drawings_hold_travels_from_its_old_rect_to_its_new_one() {
        let after = opened();
        let neighbour = &after.boxes[1];
        let (was, is) = (rect(0.0, 0.0, 200.0, 40.0), Rect::of(neighbour));
        let t = transition(collapsed(), &after);

        let start = at(&t, 0.0);
        assert_close(start.place(neighbour).0, was, "at the start");
        assert_eq!(
            start.place(neighbour).1,
            1.0,
            "a box both drawings hold faded"
        );

        let end = at(&t, 1.0);
        assert_close(end.place(neighbour).0, is, "at the end");

        let (half, alpha) = at(&t, 0.5).place(neighbour);
        assert_eq!(alpha, 1.0, "a box both drawings hold faded");
        assert!(
            half.y > was.y && half.y < is.y,
            "halfway the box sits at {half:?}, not between {was:?} and {is:?}"
        );
    }

    #[test]
    fn a_box_the_relayout_added_fades_in_from_inside_where_its_container_stood() {
        let after = opened();
        let member = &after.boxes[2];
        let t = transition(collapsed(), &after);

        let (from, alpha) = at(&t, 0.0).place(member);
        assert_eq!(
            alpha, 0.0,
            "a box only the new drawing holds started painted"
        );
        assert!(
            from.w > 0.0 && from.h > 0.0,
            "the entering box starts at nothing: {from:?}"
        );
        assert!(
            within(from, SHUT),
            "the entering box starts at {from:?}, outside the container it appears in ({SHUT:?})",
        );

        let mid = at(&t, 0.5);
        let (half, half_alpha) = mid.place(member);
        assert!(
            half_alpha > 0.0 && half_alpha < 1.0,
            "alpha {half_alpha} is not mid-fade"
        );
        assert!(
            within(half, between(SHUT, OPEN, mid.travel)),
            "halfway the entering box at {half:?} has left its container",
        );

        let end = at(&t, 1.0);
        assert_close(end.place(member).0, Rect::of(member), "at the end");
        assert_eq!(
            end.place(member).1,
            1.0,
            "the entering box never reached full strength"
        );
    }

    #[test]
    fn a_box_the_relayout_dropped_fades_out_into_where_its_container_lands() {
        let before = opened();
        let member = Rect::of(&before.boxes[2]);
        let t = transition(before, &collapsed());

        let start = at(&t, 0.0);
        let (b, from, alpha) = start
            .leaving()
            .find(|(b, _, _)| b.id == Id(2))
            .expect("the dropped box is not leaving");
        assert_eq!(b.id, Id(2));
        assert_eq!(alpha, 1.0, "the leaving box started already faded");
        assert_close(from, member, "at the start");

        let end = at(&t, 1.0);
        let (_, to, gone) = end
            .leaving()
            .find(|(b, _, _)| b.id == Id(2))
            .expect("the dropped box is not leaving");
        assert_eq!(gone, 0.0, "the leaving box never faded out");
        assert!(
            within(to, SHUT),
            "the leaving box lands at {to:?}, outside the container it collapses into ({SHUT:?})",
        );
        assert!(
            to.w > 0.0 && to.h > 0.0,
            "the leaving box collapses to nothing: {to:?}"
        );
    }

    #[test]
    fn connectors_stay_invisible_until_the_boxes_have_landed() {
        let after = opened();
        let t = transition(collapsed(), &after);
        for fraction in [0.0, 0.25, 0.5, 0.75, 1.0] {
            let connectors = at(&t, fraction).connectors;
            assert!(
                connectors < IMPERCEPTIBLE_CONNECTOR,
                "connectors read at {connectors} {fraction} of the way through the travel",
            );
        }
        assert!(
            at(&t, 1.0).connectors > 0.0,
            "the connectors had not begun as the boxes landed"
        );
        let landing = Transition::at(&t, t.start + MORPH_SPAN - Duration::from_millis(1))
            .expect("over already");
        assert!(
            landing.connectors > 0.9,
            "connectors barely arrived: {}",
            landing.connectors
        );
        assert_eq!(
            landing.travel, 1.0,
            "the boxes were still moving as the connectors arrived"
        );
    }

    /// What keeps the overlap from showing: over the lead the fade is still
    /// imperceptible, and the travel it overlaps has all but finished.
    #[test]
    fn the_lead_the_connectors_start_on_is_one_nothing_can_be_seen_over() {
        let lead = smoothstep(CONNECTOR_FADE_LEAD.as_secs_f32() / CONNECTOR_FADE.as_secs_f32());
        assert!(
            lead < IMPERCEPTIBLE_CONNECTOR,
            "the connectors reach {lead} before the boxes land"
        );
        let travelled = at(
            &transition(collapsed(), &opened()),
            CONNECTOR_FADE_START.as_secs_f32() / SETTLE_DURATION.as_secs_f32(),
        )
        .travel;
        assert!(
            travelled > 0.95,
            "the boxes are only {travelled} along when the connectors start"
        );
    }

    /// A connector is a thin line, so it reads as present well below full
    /// strength: the fade has to still be faint a good while after it starts, or
    /// the connectors land as an appearance rather than an arrival.
    #[test]
    fn the_connectors_come_up_gradually_rather_than_switching_on() {
        let t = transition(collapsed(), &opened());
        let alpha = |into: Duration| {
            Transition::at(&t, t.start + CONNECTOR_FADE_START + into)
                .expect("the fade was over already")
                .connectors
        };
        assert!(
            CONNECTOR_FADE > SETTLE_DURATION,
            "the connectors arrive faster than the boxes"
        );
        let sixth = alpha(CONNECTOR_FADE / 6);
        assert!(
            sixth < 0.15,
            "a sixth of the way in the connectors already read at {sixth}"
        );
        assert!(
            alpha(CONNECTOR_FADE / 2) < 0.75,
            "half way in the connectors are all but there"
        );
        assert!(
            sixth > 0.0,
            "the fade had not begun a sixth of the way through itself"
        );
    }

    /// The strength a box of `opened` is drawn at `fraction` of the way into a
    /// transition onto it, paired with that frame, so a further transition can be
    /// armed over the same instant.
    fn caught_fading(fraction: f32) -> (Arc<LaidOut>, Frame, f32) {
        let opened = Arc::new(opened());
        let partway = at(&transition(collapsed(), &opened), fraction);
        let reached = partway.place(&opened.boxes[2]).1;
        assert!(
            reached > 0.0 && reached < 1.0,
            "the box is not mid-fade at {reached}"
        );
        (opened, partway, reached)
    }

    /// A relayout landing mid-fade carries the strength a box has reached, as it
    /// carries where the box is drawn — a box half materialized must not snap to
    /// full and fade no further.
    #[test]
    fn a_box_still_fading_in_takes_up_at_the_strength_it_had_reached() {
        let (previous, partway, reached) = caught_fading(0.1);

        // A drawing a box must travel to, so the transition has something to
        // animate whatever the fading box turns out to contribute.
        let mut next = opened();
        next.boxes[1] = lbox(1, rect(80.0, 500.0, 200.0, 40.0), false);

        let second = Arc::new(
            Transition::between(previous, &next, Some(&partway)).expect("nothing to animate"),
        );
        let resumed = at(&second, 0.0).place(&next.boxes[2]).1;
        assert!(
            (resumed - reached).abs() < 1e-6,
            "the box took up at {resumed} rather than the {reached} it had reached",
        );
    }

    /// The fade is carried even by a box the new drawing happens to put where it
    /// already stands, which travels no distance for the transition to notice.
    #[test]
    fn a_box_fading_where_it_already_stands_still_carries_its_fade() {
        let (previous, partway, reached) = caught_fading(0.1);

        let mut next = opened();
        next.boxes[1] = lbox(1, rect(80.0, 500.0, 200.0, 40.0), false);
        let standing = partway.place(&next.boxes[2]).0;
        next.boxes[2] = lbox(2, standing, false);

        let second = Arc::new(
            Transition::between(previous, &next, Some(&partway)).expect("nothing to animate"),
        );
        let resumed = at(&second, 0.0).place(&next.boxes[2]).1;
        assert!(
            (resumed - reached).abs() < 1e-6,
            "the box that stayed put took up at {resumed} rather than {reached}",
        );
    }

    #[test]
    fn a_transition_that_has_run_its_span_places_nothing() {
        let t = transition(collapsed(), &opened());
        assert!(Transition::at(&t, t.start + MORPH_SPAN).is_none());
    }

    #[test]
    fn a_box_the_relayout_leaves_put_paints_where_it_stands_at_full_strength() {
        let after = nudged(MIN_TRAVEL);
        let t = transition(collapsed(), &after);
        let still = &after.boxes[0];
        for fraction in [0.0, 0.5, 1.0] {
            let (rect, alpha) = at(&t, fraction).place(still);
            assert_close(rect, SHUT, "the box the relayout left where it stood");
            assert_eq!(
                alpha, 1.0,
                "the box the relayout left put faded at {fraction}"
            );
        }
    }

    #[test]
    fn a_move_under_the_travel_floor_is_not_worth_animating() {
        assert!(
            Transition::between(Arc::new(collapsed()), &nudged(MIN_TRAVEL / 2.0), None).is_none(),
            "a nudge under the travel floor animated the drawing",
        );
        let after = nudged(MIN_TRAVEL);
        let t = Arc::new(
            Transition::between(Arc::new(collapsed()), &after, None)
                .expect("a move at the travel floor animated nothing"),
        );
        assert_close(
            at(&t, 0.0).place(&after.boxes[1]).0,
            rect(0.0, 0.0, 200.0, 40.0),
            "at the start, the box that moved by the floor",
        );
    }

    #[test]
    fn a_gesture_mid_morph_lands_on_the_box_drawn_under_it() {
        let after = opened();
        let travelling = &after.boxes[1];
        let t = transition(collapsed(), &after);
        let frame = at(&t, 0.5);

        let drawn = frame.place(travelling).0;
        let settled = Rect::of(travelling);
        assert!(
            !drawn.covers(settled.x as f32 + 1.0, settled.y as f32 + 1.0),
            "the box is drawn over the ground it is laid out on, so a hit test \
             against either would answer the same",
        );
        let hit = |r: Rect| {
            crate::innermost(crate::boxes_over(
                &crate::spatial::Indexed::of(&after),
                Some(&frame),
                r.x as f32 + 1.0,
                r.y as f32 + 1.0,
            ))
            .map(|b| b.id)
        };
        assert_eq!(
            hit(drawn),
            Some(travelling.id),
            "the box the cursor is over resolves to another"
        );
        assert_ne!(
            hit(settled),
            Some(travelling.id),
            "the ground the travelling box has left still answers for it",
        );
    }

    #[test]
    fn a_relayout_landing_mid_morph_starts_each_box_from_where_it_is_drawn() {
        let mut morph = Morph::none();
        morph
            .arm(Arc::new(collapsed()), &opened())
            .expect("nothing to animate");
        // Wind the first morph back to the middle of its travel, as the second
        // relayout finds it.
        let travelling = Arc::get_mut(morph.current.as_mut().unwrap()).unwrap();
        travelling.start -= SETTLE_DURATION.mul_f32(0.5);

        let after = opened();
        let moved = &after.boxes[1];
        let drawn = morph
            .at(bviz_time::now())
            .expect("the first morph had landed")
            .place(moved)
            .0;
        assert!(
            (drawn.y - Rect::of(moved).y).abs() > SAMPLING_SLACK,
            "the first morph had all but landed the box, so a restart from where it \
             was laid out would be indistinguishable",
        );

        let back = collapsed();
        morph
            .arm(Arc::new(after), &back)
            .expect("nothing to animate");
        let second = morph
            .current
            .clone()
            .expect("the second relayout armed nothing");
        let from = Transition::at(&second, second.start)
            .expect("the second morph was over before it began")
            .place(&back.boxes[1])
            .0;
        assert!(
            (from.y - drawn.y).abs() < SAMPLING_SLACK,
            "the second morph starts the box at {from:?}, not where it was drawn ({drawn:?})",
        );
    }

    #[test]
    fn a_morph_superseded_by_a_second_relayout_leaves_the_first_pump_nothing_to_drive() {
        let mut morph = Morph::none();
        let first = morph
            .arm(Arc::new(collapsed()), &opened())
            .expect("nothing to animate");
        let second = morph
            .arm(Arc::new(opened()), &collapsed())
            .expect("nothing to animate");
        assert_ne!(first, second);

        assert!(matches!(morph.tick(first), Step::Superseded));
        assert!(
            morph.current.is_some(),
            "the superseded pump ended a fresher morph"
        );
        assert!(matches!(morph.tick(second), Step::Travelling));
    }

    /// A morph onto [`opened`] whose boxes have finished travelling while its
    /// connectors are still arriving, together with the generation of the pump
    /// carrying it. This is where a relayout onto the same picture finds every
    /// box already at rest, so there is nothing left to ease.
    fn all_but_landed() -> (Morph, u64) {
        let mut morph = Morph::none();
        let generation = morph
            .arm(Arc::new(collapsed()), &opened())
            .expect("nothing to animate");
        Arc::get_mut(morph.current.as_mut().unwrap()).unwrap().start -= SETTLE_DURATION;
        (morph, generation)
    }

    #[test]
    fn a_relayout_that_redraws_the_same_picture_arms_no_morph() {
        let mut morph = Morph::none();
        assert!(morph.arm(Arc::new(opened()), &opened()).is_none());
        assert!(
            morph.current.is_none(),
            "an unchanged drawing armed a morph"
        );

        // The same arm with a morph in flight returns before it can disturb it:
        // the transition stands and so does its pump.
        let (mut morph, running) = all_but_landed();
        assert!(morph.arm(Arc::new(opened()), &opened()).is_none());
        assert!(
            morph.current.is_some(),
            "an unchanged drawing dropped the morph in flight"
        );
        assert!(
            matches!(morph.tick(running), Step::Travelling),
            "an unchanged drawing retired the pump of the morph in flight",
        );
    }

    #[test]
    fn the_morph_an_unchanged_drawing_leaves_standing_still_places_that_drawing() {
        let (mut morph, _) = all_but_landed();
        let redrawn = opened();
        assert!(morph.arm(Arc::new(opened()), &redrawn).is_none());

        let standing = morph
            .current
            .clone()
            .expect("the morph in flight was dropped");
        let frame =
            Transition::at(&standing, bviz_time::now()).expect("the transition was already over");
        for b in &redrawn.boxes {
            let (rect, alpha) = frame.place(b);
            assert_close(rect, Rect::of(b), "the box the standing morph places");
            assert_eq!(alpha, 1.0, "the box the standing morph places is faded");
        }
    }

    #[test]
    fn a_cleared_morph_drops_its_transition_and_retires_its_pump() {
        let mut morph = Morph::none();
        let generation = morph
            .arm(Arc::new(collapsed()), &opened())
            .expect("nothing to animate");
        morph.clear();
        assert!(
            morph.current.is_none(),
            "a cleared morph still holds the drawing it was leaving"
        );
        assert!(
            matches!(morph.tick(generation), Step::Superseded),
            "the pump of a cleared morph kept asking for frames",
        );
    }

    #[test]
    fn a_morph_whose_span_has_run_out_drops_the_drawing_it_was_leaving() {
        let mut morph = Morph::none();
        let generation = morph
            .arm(Arc::new(collapsed()), &opened())
            .expect("nothing to animate");
        assert!(matches!(morph.tick(generation), Step::Travelling));

        // Wind the start back past the whole span, as the tick that lands on it
        // finds it.
        let landed = Arc::get_mut(morph.current.as_mut().unwrap()).unwrap();
        landed.start -= MORPH_SPAN;
        assert!(matches!(morph.tick(generation), Step::Landed));
        assert!(
            morph.current.is_none(),
            "a landed morph still holds the drawing it left"
        );
    }
}
