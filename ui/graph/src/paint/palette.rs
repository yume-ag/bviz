use crate::Palette;
use crate::colour::Colouring;
use crate::playback::{NodeState, NodeStates, Running, Stage};
use crate::spawn_bytes::NodeBytes;
use crate::status::Status;
use crate::timing::{NodeTimes, Ran};
use bviz_graph_layout::transition::Transition;
use bviz_graph_layout::{EdgeKind, Kind};
use bviz_invocation::spawn::ByteMeasure;
use bviz_invocation::{Done, EpochMicros, Runner, Venue};
use gpui::{Hsla, Rgba};

/// How far a bridged route's ink is carried toward the canvas in perceptual
/// lightness. A bridge stands for a path through boxes the drawing does not
/// hold, which is a weaker claim than a dependency the graph states, and the
/// zoom that floors the dashes goes on to close them up into a solid line — so
/// the distinction is carried in lightness as well, which no zoom takes away.
/// Bounded above by the dimmed line, thinned by the taper, holding
/// [`bviz_ui_theme::MIN_DISTINCT_COLOR`] off every ground it crosses and every
/// ink it runs beside, which is the distance every route is held to.
const BRIDGE_DIM: f32 = 0.035;

/// How far a mark carries a connector's color in perceptual lightness,
/// away from the canvas. Bounded below by a marked line standing
/// [`bviz_ui_theme::MIN_DISTINCT_COLOR`] off the unmarked line of its own query,
/// and above by the three provenances holding that distance from one another
/// however each of a pair is marked, and from the inks a route is drawn beside.
/// Set where the nearest of those bounds is furthest off.
const MARK_LIFT: f32 = 0.072;

/// How far a split's badge is carried from the purple slot toward the red one:
/// halfway, to a magenta no slot holds and neither neighbour's stroke wears.
const SPLIT_BLEND: f32 = 0.5;

/// How far a kind accent is carried toward the canvas to make a leaf fill.
/// Bounded below by the fills staying clear of the connector colors drawn over
/// and between them, which are taken from the same slots, and above by the kinds
/// staying apart from one another and off the canvas.
const KIND_BLEND: f32 = 0.6;

/// How far the canvas is carried toward the yellow slot to fill the box
/// holding the least of a figure its kind holds.
const RAMP_LEAST: f32 = 0.225;

/// How far the canvas is carried toward the blue slot to fill the box holding
/// the most of a figure its kind holds.
const RAMP_MOST: f32 = 0.45;

/// How far an open container's fill is tinted toward the reading it wears
/// closed.
const OPEN_TINT: f32 = 0.5;

/// How far a runner's fill is carried toward the canvas in perceptual
/// lightness, past the blend toward it, which parts the warm slots that
/// otherwise crowd one another.
const RUNNER_DIM: f32 = 0.08;

const REMOTE_EXECUTION_BLEND: f32 = 0.575;
const WORKER_BLEND: f32 = 0.425;
const SANDBOXED_BLEND: f32 = 0.65;
const LOCAL_BLEND: f32 = 0.45;
const REMOTE_CACHE_HIT_BLEND: f32 = 0.375;
const DISK_CACHE_HIT_BLEND: f32 = 0.575;
const NO_SPAWN_RECORDED_BLEND: f32 = 0.525;

/// How a playback state's fill is made from its slot: carried `blend` toward
/// the canvas, then `lift` away from it in perceptual lightness, toward it
/// where negative. Each state's pair is as far as sets it apart from the other
/// states within the bounds every fill is held to.
#[derive(Clone, Copy)]
struct StateTint {
    blend: f32,
    lift: f32,
}

const PENDING_TINT: StateTint = StateTint {
    blend: 0.445,
    lift: 0.015,
};
const PREPARING_TINT: StateTint = StateTint {
    blend: 0.65,
    lift: -0.06,
};
const CHECKING_CACHE_TINT: StateTint = StateTint {
    blend: 0.633,
    lift: -0.018,
};
const SCHEDULING_TINT: StateTint = StateTint {
    blend: 0.59,
    lift: 0.008,
};
const RUNNING_LOCALLY_TINT: StateTint = StateTint {
    blend: 0.2625,
    lift: -0.18,
};
const RUNNING_TINT: StateTint = StateTint {
    blend: 0.0125,
    lift: -0.28,
};
const RUNNING_REMOTELY_TINT: StateTint = StateTint {
    blend: 0.2625,
    lift: -0.2,
};
const EXECUTED_TINT: StateTint = StateTint {
    blend: 0.632,
    lift: 0.0,
};
const CACHED_TINT: StateTint = StateTint {
    blend: 0.441,
    lift: -0.146,
};

/// How far a playback state's slot is carried from the first of the two slots
/// it lies between toward the second.
const PREPARING_HUE: f32 = 0.88;
const RUNNING_HUE: f32 = 0.4;
const RUNNING_REMOTELY_HUE: f32 = 0.05;
const CACHED_HUE: f32 = 0.26;

/// How far the neutral slot is carried toward the canvas to fill a box whose
/// actions ran on a mix of runners.
const MIXED_BLEND: f32 = 0.4;

/// How far the mixed fill is then carried away from the canvas in perceptual
/// lightness.
const MIXED_LIFT: f32 = 0.04;

/// How far a mark standing for a mix of runners is carried from the neutral
/// slot toward the label ink, away from the canvas.
const MIXED_MARK_TINT: f32 = 0.5;

/// How far a group's fill is tinted toward the accent of the files it holds: far
/// enough to be told from a plain container fill, and not so far that the group
/// passes for one of the files.
const GROUP_TINT: f32 = 0.17;

/// Opacity of the wash a marked box's fill is painted over. Bounded below by the
/// wash carrying the fill a [`bviz_ui_theme::MIN_DISTINCT_COLOR`] off the one the
/// box wears unmarked, so the mark is seen at all, and above by the box's name —
/// inked from the unwashed fill — holding [`bviz_ui_theme::MIN_TEXT_CONTRAST`] on
/// the ground the wash makes of it.
const HIGHLIGHT_WASH: f32 = 0.155;

impl Palette {
    /// Accent for a leaf box, keyed by its node kind. Actions, artifacts and
    /// source files each take a slot of their own; every other kind shares the
    /// catch-all.
    pub(crate) fn kind_accent(&self, kind: Kind) -> Rgba {
        match kind {
            Kind::Action => self.data.orange,
            Kind::Artifact => self.data.green,
            Kind::Source => self.data.purple,
            _ => self.data.blue,
        }
        .into()
    }

    /// Fill for a leaf box, its kind's accent carried toward the canvas so the
    /// box reads as ground a name is set on rather than as a mark.
    fn leaf_fill(&self, kind: Kind) -> Rgba {
        bviz_ui_theme::mix(self.kind_accent(kind), self.background, KIND_BLEND)
    }

    /// Fill for a container box, by which nesting reads package > target >
    /// configured-target, and a group of files carries the accent of the files
    /// it holds.
    fn container_fill(&self, kind: Kind) -> Rgba {
        match kind {
            Kind::Package => self.package,
            Kind::MergeGroup => {
                bviz_ui_theme::mix(self.container, self.kind_accent(Kind::Artifact), GROUP_TINT)
            }
            _ => self.container,
        }
    }

    /// Fill for a box by the kind of node it stands for.
    fn kind_fill(&self, kind: Kind, compound: bool) -> Rgba {
        match compound {
            true => self.container_fill(kind),
            false => self.leaf_fill(kind),
        }
    }

    /// Fill for a box holding a figure, `place` being where that figure stands
    /// from 0 for the least its kind holds to 1 for the most: a ramp from a
    /// pale yellow to a deeper blue, which stands further off the canvas the
    /// more it says. Within the bounds every fill is held to — a name legible
    /// on it, the connectors and the marks drawn over it clear of it — and the
    /// least box apart from an [unread](Self::unread_fill) one, its ends stand
    /// as far apart as the bounds let them.
    pub(crate) fn ramp_fill(&self, place: f32) -> Rgba {
        let least = bviz_ui_theme::mix(self.background, self.data.yellow, RAMP_LEAST);
        let most = bviz_ui_theme::mix(self.background, self.data.blue, RAMP_MOST);
        bviz_ui_theme::mix(least, most, place)
    }

    /// `fill` as a box wears it: whole on a box drawn closed, and on an open
    /// container only as a tint of the container's own fill, far enough that
    /// its reading shows and short of the whole: an open container is the
    /// ground the boxes it holds are drawn on, and one of those wearing the
    /// same reading in full must still stand off it.
    fn held_fill(&self, kind: Kind, compound: bool, fill: Rgba) -> Rgba {
        match compound {
            true => bviz_ui_theme::mix(self.container_fill(kind), fill, OPEN_TINT),
            false => fill,
        }
    }

    /// Fill for a box whose actions ran as `ran` says: its
    /// [slot](Self::ran_slot) carried toward the canvas, and for a mix of
    /// runners then away from it, so the mix reads as a fill of its own beside
    /// an untimed box and beside every runner.
    pub(crate) fn ran_fill(&self, ran: Ran) -> Rgba {
        let Ran::On(runner) = ran else {
            let mixed = bviz_ui_theme::mix(self.ran_slot(Some(ran)), self.background, MIXED_BLEND);
            return bviz_ui_theme::lighten(mixed, MIXED_LIFT.copysign(self.mark_lift()));
        };
        let (slot, blend) = self.runner_accent(runner);
        let carried = bviz_ui_theme::mix(slot, self.background, blend);
        bviz_ui_theme::lighten(carried, -RUNNER_DIM.copysign(self.mark_lift()))
    }

    /// The colour a point whose actions ran as `ran`, `None` where no time
    /// lands on it, is drawn in where it is a mark rather than ground: its
    /// slot, and for a mix of runners that slot carried away from the canvas,
    /// as a mixed box's fill is, so it stands apart from an untimed point.
    pub(crate) fn ran_mark(&self, ran: Option<Ran>) -> Rgba {
        let slot = self.ran_slot(ran);
        match ran {
            Some(Ran::Mixed) => bviz_ui_theme::mix(slot, self.label, MIXED_MARK_TINT),
            Some(Ran::On(_)) | None => slot.into(),
        }
    }

    /// The slot the actions of a box that ran as `ran`, `None` where no time
    /// lands on it, are coloured from: [each runner's](Self::runner_accent),
    /// and for a mix of runners or no time at all, which no runner stands for,
    /// the neutral slot.
    fn ran_slot(&self, ran: Option<Ran>) -> Hsla {
        match ran {
            Some(Ran::On(runner)) => self.runner_accent(runner).0,
            Some(Ran::Mixed) | None => self.data.neutral,
        }
    }

    /// The slot each runner is coloured from, every runner its own, and how
    /// far that slot is carried toward the canvas to fill a box. The slots
    /// crowd one another unevenly around the wheel, so each is carried as far
    /// as sets it apart from its neighbours within the bounds every fill is
    /// held to.
    fn runner_accent(&self, runner: Runner) -> (Hsla, f32) {
        match runner {
            Runner::RemoteExecution => (self.data.blue, REMOTE_EXECUTION_BLEND),
            Runner::Worker => (self.data.purple, WORKER_BLEND),
            Runner::Sandboxed => (self.data.orange, SANDBOXED_BLEND),
            Runner::Local => (self.data.red, LOCAL_BLEND),
            Runner::RemoteCacheHit => (self.data.cyan, REMOTE_CACHE_HIT_BLEND),
            Runner::DiskCacheHit => (self.data.green, DISK_CACHE_HIT_BLEND),
            Runner::NoSpawnRecorded => (self.data.yellow, NO_SPAWN_RECORDED_BLEND),
        }
    }

    /// Fill for a box standing at `state`. A box yet to start keeps to the
    /// neutral slot; one on its way to running deepens through blues, one
    /// running warms to the amber of a construction site (an action locally
    /// toward orange and remotely toward yellow, a container between them),
    /// and one done settles to a green low in chroma, a cache hit nearer the
    /// canvas and toward cyan.
    pub(crate) fn state_fill(&self, state: NodeState) -> Rgba {
        let d = &self.data;
        let between = bviz_ui_theme::mix;
        let (slot, tint) = match state {
            NodeState::Pending => (d.neutral.into(), PENDING_TINT),
            NodeState::In(Stage::Preparing) => {
                (between(d.cyan, d.blue, PREPARING_HUE), PREPARING_TINT)
            }
            NodeState::In(Stage::CheckingCache) => (d.blue.into(), CHECKING_CACHE_TINT),
            NodeState::In(Stage::Scheduling) => (d.blue.into(), SCHEDULING_TINT),
            NodeState::In(Stage::Running(Running::Container)) => {
                (between(d.orange, d.yellow, RUNNING_HUE), RUNNING_TINT)
            }
            NodeState::In(Stage::Running(Running::Action(Venue::Local))) => {
                (d.orange.into(), RUNNING_LOCALLY_TINT)
            }
            NodeState::In(Stage::Running(Running::Action(Venue::Remote))) => (
                between(d.yellow, d.orange, RUNNING_REMOTELY_HUE),
                RUNNING_REMOTELY_TINT,
            ),
            NodeState::Done(Done::Executed) => (d.green.into(), EXECUTED_TINT),
            NodeState::Done(Done::Cached) => (between(d.green, d.cyan, CACHED_HUE), CACHED_TINT),
        };
        let carried = bviz_ui_theme::mix(slot, self.background, tint.blend);
        bviz_ui_theme::lighten(carried, tint.lift * self.mark_lift().signum())
    }

    /// Fill for a box on which nothing its colouring reads lands: a container
    /// keeps the fill it wears by kind, and a leaf takes the
    /// [slot](Self::ran_slot) of no time at all.
    pub(crate) fn unread_fill(&self, kind: Kind, compound: bool) -> Rgba {
        match compound {
            true => self.container_fill(kind),
            false => bviz_ui_theme::mix(self.ran_slot(None), self.background, KIND_BLEND),
        }
    }

    /// Connector color, keyed by the query an edge came from, so static (query),
    /// configured (cquery), and execution-phase (aquery) dependencies read
    /// distinctly; a filter's synthetic transitive edges take a fourth hue of
    /// their own. `marked` carries the color by [`MARK_LIFT`], which leaves
    /// untouched the hue that names the kind.
    pub(crate) fn edge_color(&self, kind: EdgeKind, marked: bool) -> Rgba {
        let slot = match kind {
            EdgeKind::Query => self.data.blue,
            EdgeKind::Cquery => self.data.cyan,
            EdgeKind::Aquery => self.data.orange,
            EdgeKind::Transitive => self.data.red,
        };
        let color = match marked {
            false => slot.into(),
            true => bviz_ui_theme::lighten(slot, self.mark_lift()),
        };
        match kind {
            EdgeKind::Transitive => bviz_ui_theme::lighten(color, self.bridge_dim()),
            _ => color,
        }
    }

    /// The fill of a transition badge: each kind its own, and no badge sharing
    /// a hue with an edge kind's stroke, so a badge never reads as a piece of
    /// route. The slots no edge kind takes are too few for every kind, so one
    /// kind takes a hue between two slots, and an unknown change, which says
    /// least of what changed, wears the label ink.
    pub(crate) fn transition_color(&self, transition: Transition) -> Rgba {
        match transition {
            Transition::Exec => self.data.yellow.into(),
            Transition::Platform => self.data.purple.into(),
            Transition::Flag => self.data.green.into(),
            Transition::Split => bviz_ui_theme::mix(self.data.purple, self.data.red, SPLIT_BLEND),
            Transition::Changed => self.label,
        }
    }

    /// The ink of a status badge's glyph.
    pub(super) fn status_color(&self, status: Status) -> Rgba {
        match status {
            Status::Succeeded => self.colors.success,
            Status::Skipped | Status::Unfinished => self.colors.text_muted,
            Status::Flaky => self.colors.warning,
            Status::TestFailed | Status::Failed | Status::RootCause => self.colors.error,
        }
        .into()
    }

    /// [`MARK_LIFT`] signed to point away from the canvas: up on a theme dark
    /// enough to take white text, down on one that takes black.
    fn mark_lift(&self) -> f32 {
        match bviz_ui_theme::label_on(self.background).l > 0.5 {
            true => MARK_LIFT,
            false => -MARK_LIFT,
        }
    }

    /// [`BRIDGE_DIM`] signed to point toward the canvas, which is the way a mark
    /// does not carry a connector.
    fn bridge_dim(&self) -> f32 {
        -BRIDGE_DIM.copysign(self.mark_lift())
    }

    /// The wash a marked box's fill is painted over, at `strength` of the full
    /// mark.
    pub(super) fn highlight_wash(&self, strength: f32) -> Rgba {
        Rgba {
            a: HIGHLIGHT_WASH * strength,
            ..self.highlight
        }
    }
}

/// What a frame fills its boxes by: the colouring chosen, with what that
/// colouring reads.
pub(crate) enum Fills<'a> {
    Kind,
    Time(&'a NodeTimes),
    Runner(&'a NodeTimes),
    Bytes(&'a NodeBytes, ByteMeasure),
    Playback(&'a NodeStates),
}

impl<'a> Fills<'a> {
    /// The fills `colouring` gives the boxes of a graph whose nodes carry
    /// `times`, `bytes` and `states`. A colouring by what the graph does not
    /// carry fills by kind.
    pub(crate) fn of(
        colouring: Colouring,
        times: Option<&'a NodeTimes>,
        bytes: Option<&'a NodeBytes>,
        states: Option<&'a NodeStates>,
    ) -> Self {
        match (colouring, times, states) {
            (Colouring::Time, Some(times), _) => Fills::Time(times),
            (Colouring::Runner, Some(times), _) => Fills::Runner(times),
            (Colouring::Playback, _, Some(states)) => Fills::Playback(states),
            _ => match colouring.measure().zip(bytes) {
                Some((measure, bytes)) => Fills::Bytes(bytes, measure),
                None => Fills::Kind,
            },
        }
    }

    /// The fill of `b`, where a playback stands at `at`.
    pub(super) fn fill(
        &self,
        b: &bviz_graph_layout::Box,
        at: Option<EpochMicros>,
        palette: &Palette,
    ) -> Rgba {
        match self {
            Fills::Kind => palette.kind_fill(b.kind, b.compound),
            Fills::Time(times) => match times.get(b.id).zip(times.extent(b.kind)) {
                Some((timed, extent)) => {
                    let fill = palette.ramp_fill(extent.place(timed.held.total().wall));
                    palette.held_fill(b.kind, b.compound, fill)
                }
                None => palette.unread_fill(b.kind, b.compound),
            },
            Fills::Runner(times) => match times.get(b.id) {
                Some(timed) => {
                    let fill = palette.ran_fill(Ran::of(&timed.held));
                    palette.held_fill(b.kind, b.compound, fill)
                }
                None => palette.unread_fill(b.kind, b.compound),
            },
            Fills::Bytes(bytes, measure) => {
                let place = bytes
                    .get(b.id)
                    .zip(bytes.extent(*measure, b.kind))
                    .and_then(|(sized, extent)| {
                        Some(extent.place(sized.held.bytes.measure(*measure)?))
                    });
                match place {
                    Some(place) => palette.held_fill(b.kind, b.compound, palette.ramp_fill(place)),
                    None => palette.unread_fill(b.kind, b.compound),
                }
            }
            Fills::Playback(states) => match at.and_then(|at| states.at(b.id, at)) {
                Some(state) => palette.held_fill(b.kind, b.compound, palette.state_fill(state)),
                None => palette.unread_fill(b.kind, b.compound),
            },
        }
    }
}

#[cfg(test)]
mod colors {
    use super::*;
    use crate::paint::connector::TAPER_FLOOR;
    use crate::paint::faded;
    use bviz_ui_theme::{
        MIN_DISTINCT_COLOR, MIN_EDGED_STEP, MIN_SECONDARY_CONTRAST, MIN_TEXT_CONTRAST,
        MIN_VISIBLE_STEP, built_in_themes, contrast_ratio, distance, label_on, level_step,
        luminance, oklch, over,
    };
    use gpui::SharedString;

    /// One node kind per accent slot: the three that carry a slot of their own,
    /// and one standing for every kind that takes the catch-all.
    const ACCENTED_KINDS: [Kind; 4] = [Kind::Action, Kind::Artifact, Kind::Source, Kind::Target];

    /// One node kind per container fill: the two that carry a fill of their own,
    /// and one standing for every kind that takes the plain one.
    const CONTAINER_KINDS: [Kind; 3] = [Kind::Package, Kind::MergeGroup, Kind::Target];

    /// One node kind per container fill a container holding actions wears.
    const TIMED_CONTAINER_KINDS: [Kind; 2] = [Kind::Package, Kind::Target];

    /// Every provenance a connector is drawn in.
    const EDGE_KINDS: [EdgeKind; 4] = [
        EdgeKind::Query,
        EdgeKind::Cquery,
        EdgeKind::Aquery,
        EdgeKind::Transitive,
    ];

    /// Every theme's palette, under the name it answers to, for a rule that has
    /// to hold whichever one a window paints with.
    fn palettes() -> Vec<(SharedString, Palette)> {
        built_in_themes()
            .iter()
            .map(|theme| (theme.name.clone(), Palette::from_theme(theme)))
            .collect()
    }

    /// Places along the time ramp standing for every fill it gives: its two
    /// ends and evenly between.
    const RAMP_PLACES: [f32; 5] = [0.0, 0.25, 0.5, 0.75, 1.0];

    /// Every fill a box is painted in, each once.
    fn box_fills(p: &Palette) -> Vec<Rgba> {
        ACCENTED_KINDS
            .iter()
            .map(|kind| p.leaf_fill(*kind))
            .chain(CONTAINER_KINDS.iter().map(|kind| p.container_fill(*kind)))
            .chain([p.unread_fill(Kind::Action, false)])
            .chain(readings(p).into_iter().flat_map(|fill| {
                TIMED_CONTAINER_KINDS
                    .map(|kind| p.held_fill(kind, true, fill))
                    .into_iter()
                    .chain([fill])
            }))
            .collect()
    }

    /// Every fill a box coloured by runner wears.
    fn ran_fills(p: &Palette) -> Vec<Rgba> {
        Runner::ALL
            .map(Ran::On)
            .into_iter()
            .chain([Ran::Mixed])
            .map(|ran| p.ran_fill(ran))
            .collect()
    }

    /// Every fill a closed box coloured by playback wears.
    fn state_fills(p: &Palette) -> Vec<Rgba> {
        NodeState::ALL.map(|state| p.state_fill(state)).into()
    }

    /// Every fill a closed box coloured on the ramp, by runner or by
    /// playback wears.
    fn readings(p: &Palette) -> Vec<Rgba> {
        RAMP_PLACES
            .map(|place| p.ramp_fill(place))
            .into_iter()
            .chain(ran_fills(p))
            .chain(state_fills(p))
            .collect()
    }

    /// Every ground a connector is drawn over, each once. A marked connector runs
    /// between marked boxes, and the boxes are painted before the connectors, so
    /// each fill counts both bare and under the hover mark's wash.
    fn edge_grounds(p: &Palette) -> Vec<Rgba> {
        box_fills(p)
            .into_iter()
            .flat_map(|fill| [fill, over(p.highlight_wash(1.0), fill)])
            .chain([p.background])
            .collect()
    }

    /// Every ink a route is drawn beside, each once: the text the controls
    /// framing the canvas are set in, and the name a box carries over the routes
    /// crossing it.
    fn edge_inks(p: &Palette) -> Vec<Rgba> {
        let mut inks = vec![p.label];
        for fill in box_fills(p) {
            let ink = label_on(fill).into();
            if !inks.contains(&ink) {
                inks.push(ink);
            }
        }
        inks
    }

    /// A badge's kind is read off its fill, so no two kinds may be taken for
    /// each other, for the stroke of a route, or for the canvas rim around it.
    #[test]
    fn every_transition_badge_is_told_from_the_others_and_from_every_route() {
        for (theme, p) in palettes() {
            for (i, kind) in Transition::ALL.iter().enumerate() {
                let fill = p.transition_color(*kind);
                let rim = distance(fill, p.background);
                assert!(rim >= MIN_DISTINCT_COLOR, "{theme} {kind:?}: {rim}");
                for edge in EDGE_KINDS {
                    let apart = distance(fill, p.edge_color(edge, false));
                    assert!(
                        apart >= MIN_DISTINCT_COLOR,
                        "{theme} {kind:?}/{edge:?}: {apart}"
                    );
                }
                for other in &Transition::ALL[i + 1..] {
                    let apart = distance(fill, p.transition_color(*other));
                    assert!(
                        apart >= MIN_DISTINCT_COLOR,
                        "{theme} {kind:?}/{other:?}: {apart}"
                    );
                }
            }
        }
    }

    /// An untimed box must not pass for one holding the least time of its
    /// kind, and the ramp must stand further off the canvas at every step
    /// along it.
    #[test]
    fn the_time_ramp_is_told_from_an_untimed_box_and_steps_off_the_canvas() {
        for (theme, p) in palettes() {
            let least = p.ramp_fill(0.0);
            for untimed in [
                p.unread_fill(Kind::Action, false),
                p.unread_fill(Kind::Target, true),
                p.unread_fill(Kind::Package, true),
            ] {
                let apart = distance(least, untimed);
                assert!(apart >= MIN_DISTINCT_COLOR, "{theme}: {apart}");
            }
            let canvas = luminance(p.background);
            let off_canvas: Vec<f32> = RAMP_PLACES
                .iter()
                .map(|place| (luminance(p.ramp_fill(*place)) - canvas).abs())
                .collect();
            assert!(off_canvas.is_sorted(), "{theme}: {off_canvas:?}");
        }
    }

    /// A box held in an open container that reads the same as the container
    /// must still stand off it, or the container swallows it.
    #[test]
    fn a_box_stands_off_the_open_container_reading_the_same() {
        for (theme, p) in palettes() {
            for fill in readings(&p) {
                for kind in TIMED_CONTAINER_KINDS {
                    let open = p.held_fill(kind, true, fill);
                    let apart = distance(open, fill);
                    assert!(apart >= MIN_VISIBLE_STEP, "{theme} {kind:?}: {apart}");
                    for ground in [open, over(p.highlight_wash(1.0), open)] {
                        let held = contrast_ratio(label_on(open), ground);
                        assert!(held >= MIN_TEXT_CONTRAST, "{theme} {kind:?}: {held}");
                    }
                }
            }
        }
    }

    /// A box's runner is read off its fill, so no two runners, nor a mix of
    /// them, may be taken for each other or for a box no action lands on.
    #[test]
    fn every_runner_is_told_from_the_others_and_from_an_untimed_box() {
        for (theme, p) in palettes() {
            let fills = ran_fills(&p);
            for (i, fill) in fills.iter().enumerate() {
                for untimed in [
                    p.unread_fill(Kind::Action, false),
                    p.unread_fill(Kind::Target, true),
                    p.unread_fill(Kind::Package, true),
                    p.background,
                ] {
                    let apart = distance(*fill, untimed);
                    assert!(apart >= MIN_DISTINCT_COLOR, "{theme} {i}: {apart}");
                }
                for (j, other) in fills.iter().enumerate().skip(i + 1) {
                    let apart = distance(*fill, *other);
                    assert!(apart >= MIN_DISTINCT_COLOR, "{theme} {i}/{j}: {apart}");
                }
            }
        }
    }

    /// A box further along reads as further along: each stage its attempts
    /// enter is deeper in chroma than the one before it, so running stands
    /// out most, and an ended attempt is not as deep as one waiting on its
    /// turn to run.
    #[test]
    fn an_attempt_deepens_in_chroma_as_it_moves_on_and_settles_once_ended() {
        let running = [
            Running::Container,
            Running::Action(Venue::Local),
            Running::Action(Venue::Remote),
        ]
        .map(Stage::Running);
        for (theme, p) in palettes() {
            let chroma = |state| oklch(p.state_fill(state)).chroma;
            let under_way = [Stage::Preparing, Stage::CheckingCache, Stage::Scheduling];
            let mut before = chroma(NodeState::Pending);
            for stage in under_way {
                let deeper = chroma(NodeState::In(stage));
                assert!(
                    deeper > before,
                    "{theme} {stage:?}: {deeper} after {before}"
                );
                before = deeper;
            }
            for stage in running {
                let deeper = chroma(NodeState::In(stage));
                assert!(
                    deeper > before,
                    "{theme} {stage:?}: {deeper} after {before}"
                );
            }
            let scheduling = chroma(NodeState::In(Stage::Scheduling));
            for settled in [
                NodeState::Done(Done::Executed),
                NodeState::Done(Done::Cached),
            ] {
                let settles = chroma(settled);
                assert!(
                    settles < scheduling,
                    "{theme} {settled:?}: {settles} against {scheduling}"
                );
            }
        }
    }

    /// A box's state is read off its fill, so no two states may be taken for
    /// each other or for a box no attempt lands on.
    #[test]
    fn every_state_is_told_from_the_others_and_from_a_box_no_attempt_lands_on() {
        for (theme, p) in palettes() {
            let fills = state_fills(&p);
            for (state, fill) in NodeState::ALL.iter().zip(&fills) {
                for untimed in [
                    p.unread_fill(Kind::Action, false),
                    p.unread_fill(Kind::Target, true),
                    p.unread_fill(Kind::Package, true),
                ] {
                    let apart = distance(*fill, untimed);
                    assert!(apart >= MIN_DISTINCT_COLOR, "{theme} {state:?}: {apart}");
                }
            }
            for (i, (state, fill)) in NodeState::ALL.iter().zip(&fills).enumerate() {
                for (other, other_fill) in NodeState::ALL.iter().zip(&fills).skip(i + 1) {
                    let apart = distance(*fill, *other_fill);
                    assert!(
                        apart >= MIN_DISTINCT_COLOR,
                        "{theme} {state:?}/{other:?}: {apart}"
                    );
                }
            }
        }
    }

    /// A point's runner is read off its mark, so neither a mix of runners nor
    /// no time at all may be taken for each other or for any runner.
    #[test]
    fn a_mixed_and_an_untimed_mark_are_told_from_each_other_and_every_runner() {
        for (theme, p) in palettes() {
            let runners = Runner::ALL.map(|runner| p.ran_mark(Some(Ran::On(runner))));
            let (mixed, untimed) = (p.ran_mark(Some(Ran::Mixed)), p.ran_mark(None));
            let apart = distance(mixed, untimed);
            assert!(apart >= MIN_DISTINCT_COLOR, "{theme}: {apart}");
            for (runner, mark) in Runner::ALL.iter().zip(runners) {
                for other in [mixed, untimed] {
                    let apart = distance(mark, other);
                    assert!(apart >= MIN_DISTINCT_COLOR, "{theme} {runner:?}: {apart}");
                }
            }
        }
    }

    #[test]
    fn every_status_ink_is_seen_on_its_rim() {
        for (theme, p) in palettes() {
            for status in Status::ALL {
                let held = contrast_ratio(p.status_color(status), p.background);
                assert!(
                    held >= MIN_SECONDARY_CONTRAST,
                    "{theme} {status:?}: {held}:1"
                );
            }
        }
    }

    /// The rim is a few pixels wide, so what lies past it counts too: on every
    /// fill a badge is set on, bare or under the hover mark's wash, either the
    /// ink holds its contrast against the fill itself or the rim stands off
    /// the fill as a line of its own around the glyph.
    #[test]
    fn every_status_badge_is_seen_on_the_box_it_marks() {
        for (theme, p) in palettes() {
            for fill in box_fills(&p) {
                for ground in [fill, over(p.highlight_wash(1.0), fill)] {
                    let rimmed = distance(p.background, ground) >= MIN_DISTINCT_COLOR;
                    for status in Status::ALL {
                        let held = contrast_ratio(p.status_color(status), ground);
                        assert!(
                            rimmed || held >= MIN_SECONDARY_CONTRAST,
                            "{theme} {status:?}: {held}:1 on a fill the rim does not show on"
                        );
                    }
                }
            }
        }
    }

    /// Statuses drawn in one ink are told apart by their glyphs; the inks
    /// themselves say which kind of ending a status is, so no two of them may
    /// be taken for each other.
    #[test]
    fn the_status_inks_are_told_apart() {
        for (theme, p) in palettes() {
            let mut inks: Vec<Rgba> = Vec::with_capacity(Status::ALL.len());
            for status in Status::ALL {
                let ink = p.status_color(status);
                if !inks.contains(&ink) {
                    inks.push(ink);
                }
            }
            assert!(inks.len() > 1, "{theme}: every status wears one ink");
            for (i, ink) in inks.iter().enumerate() {
                for other in &inks[i + 1..] {
                    let apart = distance(*ink, *other);
                    assert!(apart >= MIN_DISTINCT_COLOR, "{theme}: {apart}");
                }
            }
        }
    }

    /// A leaf's kind is read off the color of the box, so no two kinds may be
    /// taken for each other and none may sink into the canvas they are drawn on.
    #[test]
    fn every_node_kind_is_told_from_the_others_and_from_the_canvas() {
        for (theme, p) in palettes() {
            for (i, kind) in ACCENTED_KINDS.iter().enumerate() {
                let fill = p.leaf_fill(*kind);
                let over_canvas = distance(fill, p.background);
                assert!(
                    over_canvas >= MIN_DISTINCT_COLOR,
                    "{theme} {kind:?}: {over_canvas}"
                );
                for other in &ACCENTED_KINDS[i + 1..] {
                    let apart = distance(fill, p.leaf_fill(*other));
                    assert!(
                        apart >= MIN_DISTINCT_COLOR,
                        "{theme} {kind:?}/{other:?}: {apart}"
                    );
                }
            }
        }
    }

    /// A connector is followed across the canvas and over the boxes it runs
    /// between, so its provenance has to be told from every ground it crosses,
    /// wherever the hover leaves it.
    #[test]
    fn every_edge_provenance_is_told_from_what_it_crosses() {
        for (theme, p) in palettes() {
            let grounds = edge_grounds(&p);
            for kind in EDGE_KINDS {
                for marked in [false, true] {
                    let color = p.edge_color(kind, marked);
                    for ground in &grounds {
                        let apart = distance(color, *ground);
                        assert!(
                            apart >= MIN_DISTINCT_COLOR,
                            "{theme} {kind:?} marked {marked} on a ground: {apart}"
                        );
                    }
                }
            }
        }
    }

    /// A route is followed past writing as well as over fills — a box's own name
    /// over the fill it crosses, the legend and toolbar text beyond the canvas.
    /// A line that meets one of those has to go on reading as a line rather than
    /// as more of the writing, wherever the hover leaves it.
    #[test]
    fn every_edge_provenance_is_told_from_the_inks_it_runs_beside() {
        for (theme, p) in palettes() {
            let inks = edge_inks(&p);
            for kind in EDGE_KINDS {
                for marked in [false, true] {
                    let color = p.edge_color(kind, marked);
                    for ink in &inks {
                        let apart = distance(color, *ink);
                        assert!(
                            apart >= MIN_DISTINCT_COLOR,
                            "{theme} {kind:?} marked {marked} beside an ink: {apart}"
                        );
                    }
                }
            }
        }
    }

    /// The color of a connector names the query it came from, and a lane runs
    /// beside lanes of every other provenance, each of the pair left as the
    /// hover found it or carried by [`MARK_LIFT`]. No two of those meet.
    #[test]
    fn no_two_provenances_meet_however_the_hover_marks_them() {
        for (theme, p) in palettes() {
            for (i, kind) in EDGE_KINDS.iter().enumerate() {
                for other in &EDGE_KINDS[i + 1..] {
                    for marked in [false, true] {
                        for other_marked in [false, true] {
                            let apart = distance(
                                p.edge_color(*kind, marked),
                                p.edge_color(*other, other_marked),
                            );
                            assert!(
                                apart >= MIN_DISTINCT_COLOR,
                                "{theme} {kind:?} marked {marked} / {other:?} marked \
                                 {other_marked}: {apart}"
                            );
                        }
                    }
                }
            }
        }
    }

    /// A marked lane runs beside unmarked lanes of its own query, so the mark
    /// has to carry the line clear of the one it is drawn from.
    #[test]
    fn a_marked_connector_is_told_from_the_unmarked_line_of_its_own_query() {
        for (theme, p) in palettes() {
            for kind in EDGE_KINDS {
                let seen = distance(p.edge_color(kind, true), p.edge_color(kind, false));
                assert!(seen >= MIN_DISTINCT_COLOR, "{theme} {kind:?}: {seen}");
            }
        }
    }

    /// Slack allowed on a hue compared across the lift, as a turn of the wheel,
    /// covering the rounding of the f32 arithmetic through it.
    const HUE_SLACK: f32 = 1e-3;

    /// A connector's hue is what names the query it came from, so a mark leaves
    /// the connector saying what it said and only turns the light up on it.
    #[test]
    fn a_mark_holds_the_hue_that_names_the_query() {
        for (theme, p) in palettes() {
            for kind in EDGE_KINDS {
                let bare = oklch(p.edge_color(kind, false)).hue;
                let marked = oklch(p.edge_color(kind, true)).hue;
                let turned = (bare - marked).abs();
                assert!(
                    turned.min(1.0 - turned) < HUE_SLACK,
                    "{theme} {kind:?}: hue {bare} -> {marked}"
                );
            }
        }
    }

    /// Which way a mark carries a connector is the theme's to say: a marked line
    /// stands further off the canvas in lightness than the line it marks,
    /// whether the canvas is the dark end of the range or the light one.
    #[test]
    fn a_mark_carries_a_connector_away_from_the_canvas() {
        for (theme, p) in palettes() {
            let canvas = luminance(p.background);
            for kind in EDGE_KINDS {
                let bare = luminance(p.edge_color(kind, false)) - canvas;
                let marked = luminance(p.edge_color(kind, true)) - canvas;
                assert!(
                    marked.abs() > bare.abs() && marked.signum() == bare.signum(),
                    "{theme} {kind:?}: {bare} -> {marked} off the canvas"
                );
            }
        }
    }

    /// The accent border is what draws the eye back to the box just toggled, so
    /// it has to stand off the border it replaces, the fill it is drawn around,
    /// the canvas beyond it and the connectors meeting it.
    #[test]
    fn the_highlight_is_told_from_everything_it_is_drawn_against() {
        for (theme, p) in palettes() {
            let against = box_fills(&p)
                .into_iter()
                .chain([p.background, p.border])
                .chain(
                    EDGE_KINDS
                        .iter()
                        .flat_map(|kind| [p.edge_color(*kind, false), p.edge_color(*kind, true)]),
                );
            for color in against {
                let apart = distance(p.highlight, color);
                assert!(apart >= MIN_DISTINCT_COLOR, "{theme}: {apart}");
            }
        }
    }

    /// A box carries its name on its own fill, whichever kind picked that fill,
    /// and on the ground the hover mark's wash makes of it — the one wash a fill
    /// ever wears. The ink is picked from the fill alone, so that one choice has
    /// to hold on both.
    #[test]
    fn a_name_is_legible_on_the_box_it_names() {
        for (theme, p) in palettes() {
            for fill in box_fills(&p) {
                let ink = label_on(fill);
                for ground in [fill, over(p.highlight_wash(1.0), fill)] {
                    let held = contrast_ratio(ink, ground);
                    assert!(held >= MIN_TEXT_CONTRAST, "{theme}: {held}");
                }
            }
        }
    }

    /// Nesting is read off the fills: a container stands off the canvas it is
    /// laid on, and the package holding it stands off the container in turn.
    /// Each pair meets at a drawn border.
    #[test]
    fn a_container_and_the_package_holding_it_step_off_the_canvas_in_turn() {
        for (theme, p) in palettes() {
            let plain = p.container_fill(Kind::Target);
            let off_canvas = level_step(plain, p.background);
            assert!(off_canvas >= MIN_EDGED_STEP, "{theme}: {off_canvas} levels");
            let off_container = level_step(p.container_fill(Kind::Package), plain);
            assert!(
                off_container >= MIN_EDGED_STEP,
                "{theme}: {off_container} levels"
            );
        }
    }

    /// The thinning is what a route drawn on a map is seen through, so a line
    /// held at the floor has to stand off every ground it crosses just as the
    /// unthinned line does.
    #[test]
    fn a_connector_held_at_the_taper_floor_is_told_from_what_it_crosses() {
        for (theme, p) in palettes() {
            let grounds = edge_grounds(&p);
            for kind in EDGE_KINDS {
                for marked in [false, true] {
                    let color = faded(p.edge_color(kind, marked), TAPER_FLOOR);
                    for ground in &grounds {
                        let apart = distance(over(color, *ground), *ground);
                        assert!(
                            apart >= MIN_DISTINCT_COLOR,
                            "{theme} {kind:?} marked {marked} on a ground: {apart}"
                        );
                    }
                }
            }
        }
    }

    /// A group is a container of files, which its fill says by carrying their
    /// accent — far enough from a plain container to be seen carrying it, and
    /// not so far that the group passes for one of the files inside it.
    #[test]
    fn a_group_reads_as_a_container_of_the_files_it_holds() {
        for (theme, p) in palettes() {
            let group = p.container_fill(Kind::MergeGroup);
            let off_plain = distance(group, p.container_fill(Kind::Target));
            assert!(off_plain >= MIN_DISTINCT_COLOR, "{theme}: {off_plain}");
            let off_file = distance(group, p.leaf_fill(Kind::Artifact));
            assert!(off_file >= MIN_DISTINCT_COLOR, "{theme}: {off_file}");
        }
    }

    /// The wash marks the box the hover picked out: whatever fill that box wears,
    /// the wash carries it far enough off the one the box wears unmarked to be
    /// seen beside the boxes still wearing it.
    #[test]
    fn a_marked_box_is_seen_whatever_fill_it_wears() {
        for (theme, p) in palettes() {
            for fill in box_fills(&p) {
                let seen = distance(over(p.highlight_wash(1.0), fill), fill);
                assert!(seen >= MIN_DISTINCT_COLOR, "{theme}: {seen}");
            }
        }
    }
}
