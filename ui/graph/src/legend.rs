use crate::Palette;
use crate::colour::Colouring;
use crate::critical::CriticalNodes;
use crate::derive::{Drawn, ViewKey};
use crate::duration::lasted;
use crate::node_table::{Extent, Magnitude};
use crate::paint::{BADGE_FLOOR, Fills, STATUS_FLOOR, paint_diamond, paint_status};
use crate::playback::{NodeState, NodeStates, States};
use crate::spawn_bytes::NodeBytes;
use crate::status::{Status, Statuses};
use crate::timing::{NodeTimes, Ran};
use bviz_graph_layout::transition::Transition;
use bviz_graph_layout::{EdgeKind, Id, Kind, LaidOut};
use bviz_invocation::spawn::{ByteMeasure, Bytes};
use bviz_invocation::{Revision, Runner};
use bviz_ui_components::{LegendEntry, LegendState, Swatch, amount};
use gpui::{Entity, Rgba};
use std::rc::Rc;
use std::time::Duration;

/// Steps the time ramp is shown in, each a fill a box can wear.
const RAMP_STEPS: usize = 12;

/// The badge of `status`, as a box wears it at its smallest.
pub(crate) fn status(status: Status, palette: &Palette) -> Swatch {
    let palette = *palette;
    Swatch::Painted(Rc::new(move |bounds, window, _| {
        paint_status(bounds.center(), STATUS_FLOOR, status, 1.0, &palette, window)
    }))
}

/// The fill a box wears whose actions ran as `ran` says.
pub(crate) fn ran(ran: Ran, palette: &Palette) -> Swatch {
    fill(palette.ran_fill(ran), palette)
}

/// A box's fill, edged as a box is.
fn fill(fill: Rgba, palette: &Palette) -> Swatch {
    Swatch::Outlined {
        fill: fill.into(),
        edge: palette.border.into(),
    }
}

/// The line a route of `kind` is drawn as: solid for a dependency the graph
/// states, dashed for a bridge standing in for a path through boxes the
/// drawing does not hold.
fn edge(kind: EdgeKind, palette: &Palette) -> Swatch {
    let color = palette.edge_color(kind, false).into();
    match kind {
        EdgeKind::Transitive => Swatch::DashedLine(color),
        _ => Swatch::Line(color),
    }
}

/// The badge marking `transition`, as a route wears it at its smallest.
fn transition(transition: Transition, palette: &Palette) -> Swatch {
    let color = palette.transition_color(transition);
    Swatch::Painted(Rc::new(move |bounds, window, _| {
        paint_diamond(bounds.center(), BADGE_FLOOR, color, window)
    }))
}

/// A box wearing the critical path's halo.
fn halo(palette: &Palette) -> Swatch {
    Swatch::Ring {
        fill: palette.container.into(),
        ring: palette.label.into(),
    }
}

/// The fills a box's time is read off, from the least its kind holds to the
/// most.
fn ramp(palette: &Palette) -> Swatch {
    let last = (RAMP_STEPS - 1) as f32;
    let steps = (0..RAMP_STEPS).map(|step| palette.ramp_fill(step as f32 / last).into());
    Swatch::Ramp(steps.collect())
}

/// How the legend names the boxes of `kind`.
fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Package => "packages",
        Kind::Target => "targets",
        Kind::ConfiguredTarget => "configured targets",
        Kind::Action => "actions",
        Kind::Artifact => "generated files",
        Kind::Source => "source files",
        Kind::TargetGroup => "package and environment groups",
        Kind::MergeGroup => "merged file groups",
    }
}

/// How a view's boxes are filled, and what they are filled from. `timed`,
/// `logged` and `attempted` are the revisions of the session's timings,
/// logged spawns and attempts `times`, `bytes` and `states` were read from.
#[derive(Clone, Copy)]
pub(crate) struct Coloured<'a> {
    pub(crate) colouring: Colouring,
    pub(crate) times: Option<&'a NodeTimes>,
    pub(crate) timed: Revision,
    pub(crate) bytes: Option<&'a NodeBytes>,
    pub(crate) logged: Revision,
    pub(crate) states: Option<&'a NodeStates>,
    pub(crate) attempted: Revision,
}

/// Where a view's boxes wear the critical path's halo: the nodes its steps
/// land on, resolved from a session's report at `traced`, or `None` where the
/// halo is turned off.
#[derive(Clone, Copy)]
pub(crate) struct Haloed<'a> {
    pub(crate) nodes: Option<&'a CriticalNodes>,
    pub(crate) traced: Revision,
}

/// What the fills of a view's boxes say beyond each box's kind.
#[derive(Clone, Debug, PartialEq)]
enum Reading {
    /// Nothing: they are filled by kind.
    Kinds,
    /// The time of each box, each kind of box placed on a scale of its own.
    Times {
        /// The scale of each kind some box of the view holds time of, in
        /// enum order.
        scales: Vec<(Kind, Extent<Duration>)>,
        /// Whether some box of the view wears the untimed fill.
        untimed: bool,
    },
    /// The bytes in `measure` of each box's spawns, each kind of box placed
    /// on a scale of its own.
    Bytes {
        measure: ByteMeasure,
        /// The scale of each kind some box of the view holds bytes of, in
        /// enum order.
        scales: Vec<(Kind, Extent<Bytes>)>,
        /// Whether some box of the view wears the fill of a box holding none.
        unlogged: bool,
    },
    /// Where each box's actions ran.
    Runners {
        /// Each reading of runners some box of the view wears, in the order of
        /// [`Runner::ALL`] and the mix last.
        ran: Vec<Ran>,
        /// Whether some box of the view wears the untimed fill.
        untimed: bool,
    },
    /// Where the execution attempts landing on each box stand at the
    /// playback's time.
    States {
        /// Each state some box of the view may stand at over the playback, in
        /// the order of [`NodeState::ALL`].
        reached: Vec<NodeState>,
        /// Whether some box of the view wears the untimed fill.
        untimed: bool,
    },
}

impl Reading {
    /// What the [fills](Fills::of) `coloured` gives the boxes of `g` say.
    fn of(g: &LaidOut, coloured: Coloured) -> Self {
        let untimed_leaf = |times: &NodeTimes| {
            g.boxes
                .iter()
                .any(|b| !b.compound && times.get(b.id).is_none())
        };
        let fills = Fills::of(
            coloured.colouring,
            coloured.times,
            coloured.bytes,
            coloured.states,
        );
        match fills {
            Fills::Kind => Reading::Kinds,
            Fills::Playback(states) => {
                let reached = g.boxes.iter().fold(States::default(), |reached, b| {
                    reached.union(states.reached(b.id))
                });
                Reading::States {
                    reached: NodeState::ALL
                        .into_iter()
                        .filter(|&state| reached.contains(state))
                        .collect(),
                    untimed: g
                        .boxes
                        .iter()
                        .any(|b| !b.compound && states.reached(b.id) == States::default()),
                }
            }
            Fills::Time(times) => {
                let scales = kinds_holding(g, |id| times.get(id).is_some())
                    .filter_map(|kind| Some((kind, times.extent(kind)?)))
                    .collect();
                Reading::Times {
                    scales,
                    untimed: untimed_leaf(times),
                }
            }
            Fills::Bytes(bytes, measure) => {
                let scales = kinds_holding(g, |id| bytes.get(id).is_some())
                    .filter_map(|kind| Some((kind, bytes.extent(measure, kind)?)))
                    .collect();
                Reading::Bytes {
                    measure,
                    scales,
                    unlogged: g
                        .boxes
                        .iter()
                        .any(|b| !b.compound && bytes.get(b.id).is_none()),
                }
            }
            Fills::Runner(times) => {
                let worn: Vec<Ran> = g
                    .boxes
                    .iter()
                    .filter_map(|b| times.get(b.id))
                    .map(|timed| Ran::of(&timed.held))
                    .collect();
                let ran = Runner::ALL
                    .map(Ran::On)
                    .into_iter()
                    .chain([Ran::Mixed])
                    .filter(|ran| worn.contains(ran))
                    .collect();
                Reading::Runners {
                    ran,
                    untimed: untimed_leaf(times),
                }
            }
        }
    }
}

/// Each kind of which some box of `g` `holds` a figure, in enum order.
fn kinds_holding(g: &LaidOut, holds: impl Fn(Id) -> bool) -> impl Iterator<Item = Kind> {
    let mut kinds: Vec<Kind> = Vec::with_capacity(g.boxes.len());
    kinds.extend(g.boxes.iter().filter(|b| holds(b.id)).map(|b| b.kind));
    kinds.sort_unstable();
    kinds.dedup();
    kinds.into_iter()
}

/// The key to what a view draws: the edge kinds, transitions and statuses
/// present in it and what its fills say, found once per view so that a frame
/// drawn over an unchanged view does not scan it again.
#[derive(Default)]
pub(crate) struct Legend {
    found: Option<Found>,
    drawn_for: Option<ViewKey>,
}

/// What is present in the view `view` identifies, each in enum order.
struct Found {
    view: ViewKey,
    /// Which of a session's outcomes the statuses found were resolved from.
    resolved: Revision,
    /// Which of a session's timings the reading was found from.
    timed: Revision,
    /// Which of a session's logged spawns the reading was found from.
    logged: Revision,
    /// Which of a session's attempts the reading was found from.
    attempted: Revision,
    colouring: Colouring,
    /// Which report of the critical path, and whether its halo is drawn.
    haloed: (Revision, bool),
    kinds: Vec<EdgeKind>,
    transitions: Vec<Transition>,
    standing: Vec<Status>,
    reading: Reading,
    /// Whether some box wears the critical path's halo.
    on_path: bool,
}

impl Legend {
    /// Whether the key is drawn for a view other than `shown`, taking `shown`
    /// as the view it is drawn for from then on. The key is drawn in the
    /// panel's chrome, which a camera move or a fit leaves alone, so the
    /// caller redraws that chrome where the view changed.
    pub(crate) fn retarget(&mut self, shown: Option<ViewKey>) -> bool {
        let lags = self.drawn_for != shown;
        self.drawn_for = shown;
        lags
    }

    /// Keys, open as `state` says, what the fills of `drawn` at `zoom` say as
    /// `coloured` fills them, the statuses `statuses` gives its boxes, the edge
    /// kinds it draws and the transitions its connectors cross, so it explains
    /// only what is present in this view; the caller places it. `None` when nothing is
    /// drawn, or the view is yet to be derived.
    ///
    /// `resolved` says which of a session's outcomes `statuses` were resolved
    /// from. Outcomes, timings and the critical path land without the view
    /// changing, so what is found stands for as long as the view, `resolved`,
    /// `coloured` and `haloed` do.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn strip(
        &mut self,
        drawn: &Drawn,
        zoom: f32,
        statuses: &Statuses,
        resolved: Revision,
        coloured: Coloured,
        haloed: Haloed,
        state: &Entity<LegendState>,
        palette: &Palette,
    ) -> Option<bviz_ui_components::Legend> {
        let found = self.found(drawn, zoom, statuses, resolved, coloured, haloed)?;
        strip(found, coloured, state, palette)
    }

    fn found(
        &mut self,
        drawn: &Drawn,
        zoom: f32,
        statuses: &Statuses,
        resolved: Revision,
        coloured: Coloured,
        haloed: Haloed,
    ) -> Option<&Found> {
        let shown = drawn.shown(zoom)?;
        let view = shown.key();
        let halo_key = (haloed.traced, haloed.nodes.is_some());
        let found = match self.found.take() {
            Some(found)
                if found.view == view
                    && found.resolved == resolved
                    && found.timed == coloured.timed
                    && found.logged == coloured.logged
                    && found.attempted == coloured.attempted
                    && found.colouring == coloured.colouring
                    && found.haloed == halo_key =>
            {
                found
            }
            _ => {
                let g = shown.view();
                Found {
                    view,
                    resolved,
                    timed: coloured.timed,
                    logged: coloured.logged,
                    attempted: coloured.attempted,
                    colouring: coloured.colouring,
                    haloed: halo_key,
                    kinds: present(g),
                    transitions: crossed(g),
                    standing: standing(g, statuses),
                    reading: Reading::of(g, coloured),
                    on_path: haloed.nodes.is_some_and(|nodes| {
                        g.boxes.iter().any(|b| nodes.marks(b.id, !b.compound))
                    }),
                }
            }
        };
        Some(self.found.insert(found))
    }
}

/// The statuses some box of `g` stands at, whether or not a frame finds the
/// ground to paint that box's badge on.
fn standing(g: &LaidOut, statuses: &Statuses) -> Vec<Status> {
    Status::ALL
        .into_iter()
        .filter(|&status| g.boxes.iter().any(|b| statuses.get(b.id) == Some(status)))
        .collect()
}

fn present(g: &LaidOut) -> Vec<EdgeKind> {
    [
        EdgeKind::Query,
        EdgeKind::Cquery,
        EdgeKind::Aquery,
        EdgeKind::Transitive,
    ]
    .into_iter()
    .filter(|k| g.connectors.iter().any(|c| c.kinds.contains(k)))
    .collect()
}

/// The transitions some connector of `g` crosses.
fn crossed(g: &LaidOut) -> Vec<Transition> {
    Transition::ALL
        .into_iter()
        .filter(|&kind| g.connectors.iter().any(|c| c.transition == Some(kind)))
        .collect()
}

/// The entries explaining what the fills read as `reading` says, and the notes
/// on how to read them and how much of the profile or the execution log
/// `coloured` reads from places; none for fills by kind.
fn reading(
    reading: &Reading,
    coloured: Coloured,
    palette: &Palette,
) -> (Vec<LegendEntry>, Vec<String>) {
    let untimed = |said: &'static str| {
        LegendEntry::new(
            fill(palette.unread_fill(Kind::Action, false), palette),
            said,
        )
    };
    let mut notes = Vec::with_capacity(3);
    let times_placed = coloured.times.map(|times| times.coverage().line());
    let (entries, placed) = match reading {
        Reading::Kinds => return (Vec::new(), notes),
        Reading::Times {
            scales,
            untimed: any,
        } => {
            notes.push("action time, on a log scale per kind".to_owned());
            let entries = ramps(scales, lasted, palette)
                .chain(any.then(|| untimed("no action time")))
                .collect();
            (entries, times_placed)
        }
        Reading::Bytes {
            measure,
            scales,
            unlogged: any,
        } => {
            let read = match measure {
                ByteMeasure::Outputs => "bytes their spawns wrote",
                ByteMeasure::Inputs => "bytes their spawns read",
                ByteMeasure::Tools => "bytes of their spawns' tools",
                ByteMeasure::ReadAlone => "bytes only one of their spawns read",
            };
            notes.push(format!("{read}, on a log scale per kind"));
            let entries = ramps(scales, amount::bytes, palette)
                .chain(any.then(|| untimed("no logged spawn")))
                .collect();
            notes.extend(
                coloured
                    .bytes
                    .into_iter()
                    .flat_map(|b| b.coverage().lines()),
            );
            (entries, None)
        }
        Reading::Runners { ran, untimed: any } => {
            let entries = ran
                .iter()
                .map(|&ran| LegendEntry::new(self::ran(ran, palette), ran.label()))
                .chain(any.then(|| untimed("no action time")))
                .collect();
            (entries, times_placed)
        }
        Reading::States {
            reached,
            untimed: any,
        } => {
            let entries = reached
                .iter()
                .map(|&state| {
                    LegendEntry::new(fill(palette.state_fill(state), palette), state.label())
                })
                .chain(any.then(|| untimed("no record")))
                .collect();
            (entries, times_placed)
        }
    };
    notes.extend(placed);
    (entries, notes)
}

/// The ramp each kind's `scales` is read off, its ends as `read` says them.
fn ramps<'s, M: Magnitude>(
    scales: &'s [(Kind, Extent<M>)],
    read: fn(M) -> String,
    palette: &'s Palette,
) -> impl Iterator<Item = LegendEntry> + 's {
    scales.iter().map(move |&(kind, extent)| {
        let (least, most) = (read(extent.least()), read(extent.most()));
        LegendEntry::new(
            ramp(palette),
            format!("{}: {least} to {most}", kind_name(kind)),
        )
    })
}

/// The key's id, the same whether it keys a drawing or stands blank.
const ID: &str = "graph-legend";

/// The key to a view drawing nothing it keys, which holds the toggle's place
/// in the toolbar while a layout runs.
pub(crate) fn blank(state: &Entity<LegendState>) -> bviz_ui_components::Legend {
    bviz_ui_components::Legend::new(ID, state)
}

/// The key to `found`, open as `state` says: what the fills read first, then
/// the marks boxes wear, then the routes.
fn strip(
    found: &Found,
    coloured: Coloured,
    state: &Entity<LegendState>,
    palette: &Palette,
) -> Option<bviz_ui_components::Legend> {
    let (fills, notes) = reading(&found.reading, coloured, palette);
    let statuses = found
        .standing
        .iter()
        .map(|&status| LegendEntry::new(self::status(status, palette), status.label()));
    let on_path = found
        .on_path
        .then(|| LegendEntry::new(halo(palette), "critical path"));
    let kinds = found
        .kinds
        .iter()
        .map(|&kind| LegendEntry::new(edge(kind, palette), kind.label()));
    let transitions = found
        .transitions
        .iter()
        .map(|&crossed| LegendEntry::new(transition(crossed, palette), crossed.label()));
    let mut entries = fills;
    entries.extend(statuses.chain(on_path).chain(kinds).chain(transitions));
    if entries.is_empty() {
        return None;
    }
    let legend = bviz_ui_components::Legend::new(ID, state).entries(entries);
    Some(
        notes
            .into_iter()
            .fold(legend, bviz_ui_components::Legend::note),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::derive::Shown;
    use crate::playback::tests::attempt;
    use crate::playback::{Running, Stage};
    use crate::status::Nodes;
    use crate::timing::landings;
    use bviz_graph_layout::graph::Node;
    use bviz_graph_layout::reduce::transitive_reduction;
    use bviz_graph_layout::{Connector, Graph, Id, Kind, Names, map_layout};
    use bviz_invocation::spawn::LoggedSpawns;
    use bviz_invocation::{
        Attempts, BuildOutcome, ConfiguredTarget, Done, Outcomes, Phase, Profiled, Session, Spawns,
        Timings, Venue,
    };
    use std::sync::Arc;
    use std::time::Duration;

    /// Boxes filled by kind, over a graph no profile times.
    fn uncoloured() -> Coloured<'static> {
        Coloured {
            colouring: Colouring::Kind,
            times: None,
            timed: Revision::default(),
            bytes: None,
            logged: Revision::default(),
            states: None,
            attempted: Revision::default(),
        }
    }

    /// No critical path's halo drawn.
    fn unhaloed() -> Haloed<'static> {
        Haloed {
            nodes: None,
            traced: Revision::default(),
        }
    }

    /// A zoom far enough out that a map draws its coarsest level.
    const COARSEST: f32 = 0.0;
    /// A zoom far enough in that a map draws its finest level.
    const FINEST: f32 = f32::MAX;

    /// The edge kinds `legend` lists for `drawn` at `zoom`, no box of which the
    /// build reports on.
    fn kinds<'a>(legend: &'a mut Legend, drawn: &Drawn, zoom: f32) -> Option<&'a [EdgeKind]> {
        let unreported = Revision::default();
        Some(
            &legend
                .found(
                    drawn,
                    zoom,
                    &Statuses::default(),
                    unreported,
                    uncoloured(),
                    unhaloed(),
                )?
                .kinds,
        )
    }

    /// A layered drawing numbered `drawing` of the boxes `Id(0)` and `Id(1)`.
    fn boxes(drawing: u64) -> Drawn {
        let leaf = |id: u32| bviz_graph_layout::Box {
            id: Id(id),
            label: String::new(),
            kind: Kind::Target,
            x: 0.0,
            y: 0.0,
            width: 10.0,
            height: 10.0,
            compound: false,
            group: false,
        };
        Drawn::Layered {
            view: Arc::new(LaidOut {
                boxes: vec![leaf(0), leaf(1)],
                ..LaidOut::default()
            }),
            drawing,
        }
    }

    /// What a build reporting `first` of the node `Id(0)` and `second` of
    /// `Id(1)` gives the two, `Id(2)` being a node it also reports as built.
    fn reported(first: BuildOutcome, second: Option<BuildOutcome>) -> Statuses {
        let labels = ["//app:first", "//app:second", "//app:elsewhere"];
        let mut graph = Graph::new();
        let mut names = Names::default();
        for label in labels {
            let id = names.intern(&format!("target:{label}"));
            graph.add_node(Node::new(id, label, Kind::Target, None));
        }
        let mut outcomes = Outcomes::default();
        let outcome = [Some(first), second, Some(BuildOutcome::Built)];
        for (label, outcome) in labels.into_iter().zip(outcome) {
            if let Some(outcome) = outcome {
                let target = ConfiguredTarget::new(label.to_owned(), None);
                outcomes.report_build(target, outcome);
            }
        }
        let nodes = Nodes::new(&graph, &names, &outcomes);
        Statuses::resolve(&nodes, &graph)
    }

    fn standing_in(legend: &mut Legend, drawn: &Drawn, statuses: &Statuses) -> Vec<Status> {
        let found = legend.found(
            drawn,
            1.0,
            statuses,
            Revision::default(),
            uncoloured(),
            unhaloed(),
        );
        found.expect("a derived view").standing.clone()
    }

    #[test]
    fn the_statuses_the_boxes_of_a_view_stand_at_are_listed_from_the_least_severe() {
        let mut legend = Legend::default();
        let drawn = boxes(1);
        let failed = BuildOutcome::Failed { causes: Vec::new() };

        let statuses = reported(failed, Some(BuildOutcome::Skipped));
        assert_eq!(
            standing_in(&mut legend, &drawn, &statuses),
            [Status::Skipped, Status::RootCause],
            "a status only a box outside the view stands at is listed"
        );
        assert_eq!(
            standing_in(&mut legend, &boxes(2), &Statuses::default()),
            []
        );
    }

    /// A build's outcomes land without the view changing, so the view's key
    /// alone does not say the legend is still right.
    #[test]
    fn a_view_is_scanned_again_once_its_statuses_are_resolved_from_other_outcomes() {
        let mut session = Session::default();
        let by = session.reporter();
        let unreported = session.outcomes_revision();
        session.report_outcomes(by, Outcomes::default());
        let first = session.outcomes_revision();
        session.report_outcomes(by, Outcomes::default());
        let second = session.outcomes_revision();
        let mut legend = Legend::default();
        let drawn = boxes(1);
        let mut standing = |statuses: &Statuses, resolved| {
            let found = legend.found(&drawn, 1.0, statuses, resolved, uncoloured(), unhaloed());
            found.expect("a derived view").standing.clone()
        };
        assert_eq!(standing(&Statuses::default(), unreported), []);

        let landed = reported(BuildOutcome::NotBuilt, None);
        assert_eq!(standing(&landed, first), [Status::Unfinished]);
        let changed = reported(BuildOutcome::Built, None);
        assert_eq!(
            standing(&changed, first),
            [Status::Unfinished],
            "a view under an unchanged key and unchanged outcomes was scanned again"
        );
        assert_eq!(standing(&changed, second), [Status::Succeeded]);
    }

    /// A package holding a target that holds a fast remotely executed action
    /// and a slow remote cache hit, beside a target no action lands on, laid
    /// out each as a box of one layered drawing; with the times its profile
    /// places on them, the states their attempts stand at and the bytes their
    /// logged spawns wrote, one of them landing on the target by label.
    fn timed() -> (Drawn, NodeTimes, NodeStates, NodeBytes) {
        let mut graph = Graph::new();
        let mut names = Names::default();
        let mut add = |name: &str, kind, parent| {
            let id = names.intern(name);
            graph.add_node(Node::new(id, name, kind, parent));
            id
        };
        let package = add("pkg:app", Kind::Package, None);
        let target = add("target://app:bin", Kind::Target, Some(package));
        let fast = add("action:fast", Kind::Action, Some(target));
        let slow = add("action:slow", Kind::Action, Some(target));
        let idle = add("target://app:idle", Kind::Target, Some(package));
        let mut timings = Timings::default();
        for (primary_output, millis, runner) in [
            ("fast", 10, Runner::RemoteExecution),
            ("slow", 1_000, Runner::RemoteCacheHit),
        ] {
            timings.record(Profiled {
                primary_output: Some(primary_output),
                owner: None,
                wall: Duration::from_millis(millis),
                spawns: Spawns::on(runner, Duration::from_millis(millis)),
            });
        }
        let landings = landings(&graph, &names, &timings);
        let times = NodeTimes::resolve(&landings, &graph, &timings).expect("timed");
        let mut attempts = Attempts::default();
        for (primary_output, millis, phase) in [
            ("fast", 10, Phase::Running(Venue::Remote)),
            ("slow", 1_000, Phase::CheckingCache),
        ] {
            let tried = attempt(0, millis * 1_000, &[(phase, 1)]);
            attempts.record(Some(primary_output), None, tried);
        }
        let states = NodeStates::resolve(&landings, &graph, &attempts).expect("landed");
        let spawns = crate::spawn_bytes::tests::logged(&[
            (Some("fast"), "//app:bin", 4 << 10, 0, 0),
            (Some("slow"), "//app:bin", 4 << 20, 0, 0),
            (Some("fast.lint"), "//app:bin", 7, 0, 0),
        ]);
        let spawn_landings = crate::spawn_bytes::landings(&graph, &names, &spawns);
        let bytes = NodeBytes::resolve(&spawn_landings, &graph, &spawns).expect("sized");
        let laid = |id: Id, kind, compound| bviz_graph_layout::Box {
            id,
            label: String::new(),
            kind,
            x: 0.0,
            y: 0.0,
            width: 10.0,
            height: 10.0,
            compound,
            group: false,
        };
        let view = LaidOut {
            boxes: vec![
                laid(package, Kind::Package, true),
                laid(target, Kind::Target, true),
                laid(fast, Kind::Action, false),
                laid(slow, Kind::Action, false),
                laid(idle, Kind::Target, false),
            ],
            ..LaidOut::default()
        };
        let drawn = Drawn::Layered {
            view: Arc::new(view),
            drawing: 1,
        };
        (drawn, times, states, bytes)
    }

    fn reading_of(
        legend: &mut Legend,
        drawn: &Drawn,
        colouring: Colouring,
        times: &NodeTimes,
        timed: Revision,
    ) -> Reading {
        let coloured = Coloured {
            colouring,
            times: Some(times),
            timed,
            bytes: None,
            logged: Revision::default(),
            states: None,
            attempted: Revision::default(),
        };
        let found = legend.found(
            drawn,
            1.0,
            &Statuses::default(),
            Revision::default(),
            coloured,
            unhaloed(),
        );
        found.expect("a derived view").reading.clone()
    }

    #[test]
    fn the_fills_of_a_view_are_explained_as_its_colouring_reads_them() {
        let (drawn, times, _, _) = timed();
        let mut legend = Legend::default();
        let timed = Revision::default();
        let extent = |kind| times.extent(kind).expect("a timed kind");

        assert_eq!(
            reading_of(&mut legend, &drawn, Colouring::Time, &times, timed),
            Reading::Times {
                scales: vec![
                    (Kind::Package, extent(Kind::Package)),
                    (Kind::Target, extent(Kind::Target)),
                    (Kind::Action, extent(Kind::Action)),
                ],
                untimed: true,
            }
        );
        assert_eq!(
            extent(Kind::Action).least(),
            Duration::from_millis(10),
            "each kind is read on its own scale"
        );
        assert_eq!(
            reading_of(&mut legend, &drawn, Colouring::Runner, &times, timed),
            Reading::Runners {
                ran: vec![
                    Ran::On(Runner::RemoteExecution),
                    Ran::On(Runner::RemoteCacheHit)
                ],
                untimed: true,
            },
            "a box that ran mostly as a cache hit reads as one"
        );
        assert_eq!(
            reading_of(&mut legend, &drawn, Colouring::Kind, &times, timed),
            Reading::Kinds
        );
    }

    /// A profile's timings land without the view changing, so the view's key
    /// alone does not say the legend is still right.
    #[test]
    fn a_view_is_scanned_again_once_its_times_are_read_from_other_timings() {
        let (drawn, times, _, _) = timed();
        let mut session = Session::default();
        let by = session.reporter();
        let untimed = session.timings_revision();
        session.report_timings(by, Timings::default());
        let timed = session.timings_revision();
        let mut legend = Legend::default();
        let stale = Coloured {
            colouring: Colouring::Runner,
            times: None,
            timed: untimed,
            bytes: None,
            logged: Revision::default(),
            states: None,
            attempted: Revision::default(),
        };
        let found = legend.found(
            &drawn,
            1.0,
            &Statuses::default(),
            Revision::default(),
            stale,
            unhaloed(),
        );
        assert_eq!(found.map(|found| &found.reading), Some(&Reading::Kinds));

        let read = reading_of(&mut legend, &drawn, Colouring::Runner, &times, timed);
        assert!(matches!(read, Reading::Runners { .. }), "{read:?}");
    }

    /// Logged spawns land without the view changing, so the view's key alone
    /// does not say the legend is still right.
    #[test]
    fn a_view_coloured_by_bytes_reads_each_kinds_scale_from_the_spawns_logged() {
        let (drawn, _, _, bytes) = timed();
        let mut session = Session::default();
        let by = session.reporter();
        let unlogged = session.logged_spawns_revision();
        session.report_logged_spawns(by, LoggedSpawns::default());
        let logged = session.logged_spawns_revision();
        let mut legend = Legend::default();
        let mut read = |bytes, logged| {
            let coloured = Coloured {
                colouring: Colouring::Written,
                times: None,
                timed: Revision::default(),
                bytes,
                logged,
                states: None,
                attempted: Revision::default(),
            };
            let found = legend.found(
                &drawn,
                1.0,
                &Statuses::default(),
                Revision::default(),
                coloured,
                unhaloed(),
            );
            found.expect("a derived view").reading.clone()
        };
        assert_eq!(read(None, unlogged), Reading::Kinds);

        let extent = |kind| bytes.extent(ByteMeasure::Outputs, kind).expect("a kind");
        assert_eq!(
            read(Some(&bytes), logged),
            Reading::Bytes {
                measure: ByteMeasure::Outputs,
                scales: vec![
                    (Kind::Package, extent(Kind::Package)),
                    (Kind::Target, extent(Kind::Target)),
                    (Kind::Action, extent(Kind::Action)),
                ],
                unlogged: true,
            }
        );
        assert_eq!(extent(Kind::Action).least(), Bytes::new(4 << 10));
        assert_eq!(
            extent(Kind::Target).most(),
            Bytes::new((4 << 20) + (4 << 10) + 7)
        );
    }

    /// What the legend reads off a view coloured by playback, the states its
    /// nodes stand at read from the session's attempts at `attempted`.
    fn played(
        legend: &mut Legend,
        drawn: &Drawn,
        states: Option<&NodeStates>,
        attempted: Revision,
    ) -> Reading {
        let coloured = Coloured {
            colouring: Colouring::Playback,
            times: None,
            timed: Revision::default(),
            bytes: None,
            logged: Revision::default(),
            states,
            attempted,
        };
        let found = legend.found(
            drawn,
            1.0,
            &Statuses::default(),
            Revision::default(),
            coloured,
            unhaloed(),
        );
        found.expect("a derived view").reading.clone()
    }

    #[test]
    fn a_view_coloured_by_playback_lists_every_state_its_boxes_may_stand_at() {
        let (drawn, _, states, _) = timed();
        let mut legend = Legend::default();
        assert_eq!(
            played(&mut legend, &drawn, Some(&states), Revision::default()),
            Reading::States {
                reached: vec![
                    NodeState::Pending,
                    NodeState::In(Stage::Preparing),
                    NodeState::In(Stage::CheckingCache),
                    NodeState::In(Stage::Running(Running::Container)),
                    NodeState::In(Stage::Running(Running::Action(Venue::Remote))),
                    NodeState::Done(Done::Executed),
                    NodeState::Done(Done::Cached),
                ],
                untimed: true,
            }
        );
    }

    /// A profile's attempts land without the view changing, so the view's
    /// key alone does not say the legend is still right; the playback's time
    /// plays no part in it.
    #[test]
    fn a_view_coloured_by_playback_is_scanned_again_once_other_attempts_land() {
        let (drawn, _, states, _) = timed();
        let mut session = Session::default();
        let by = session.reporter();
        let unattempted = session.attempts_revision();
        session.report_attempts(by, Attempts::default());
        let attempted = session.attempts_revision();
        let mut legend = Legend::default();
        let mut reading = |states, attempted| played(&mut legend, &drawn, states, attempted);
        assert_eq!(reading(None, unattempted), Reading::Kinds);
        assert_eq!(
            reading(Some(&states), unattempted),
            Reading::Kinds,
            "a view under an unchanged key and unchanged attempts was scanned again"
        );
        let read = reading(Some(&states), attempted);
        assert!(matches!(read, Reading::States { .. }), "{read:?}");
    }

    /// A layered drawing numbered `drawing`, of one connector carrying `kinds`.
    fn layered(drawing: u64, kinds: Vec<EdgeKind>) -> Drawn {
        crossing(drawing, kinds, None)
    }

    /// [`layered`], its connector crossing `transition`.
    fn crossing(drawing: u64, kinds: Vec<EdgeKind>, transition: Option<Transition>) -> Drawn {
        Drawn::Layered {
            view: Arc::new(LaidOut {
                connectors: vec![Connector {
                    points: Vec::new(),
                    kinds,
                    from: Id(0),
                    to: Id(1),
                    transition,
                }],
                ..LaidOut::default()
            }),
            drawing,
        }
    }

    /// A map numbered `drawing` of two packages whose actions form one chain of
    /// `kind` edges, so it has levels to zoom between, each derived.
    fn map(drawing: u64, kind: EdgeKind) -> Drawn {
        let mut base = Graph::new();
        let mut ids = (0..).map(Id);
        let mut actions = Vec::new();
        for package in ["a", "b"] {
            let package_id = ids.next().expect("an id");
            base.add_node(Node::new(package_id, package, Kind::Package, None));
            let target = ids.next().expect("an id");
            base.add_node(Node::new(target, "t", Kind::Target, Some(package_id)));
            for action in ["compile", "link"] {
                let id = ids.next().expect("an id");
                base.add_node(Node::new(id, action, Kind::Action, Some(target)));
                actions.push(id);
            }
        }
        for pair in actions.windows(2) {
            base.add_edge(pair[0], pair[1], kind);
        }
        let reduced = transitive_reduction(&base);
        let map = map_layout(Arc::new(base), Arc::new(reduced));
        for level in 0..map.level_count() {
            map.level(level);
        }
        Drawn::Map {
            map: Arc::new(map),
            drawing,
        }
    }

    #[test]
    fn a_view_is_scanned_once_under_its_key() {
        let mut legend = Legend::default();
        assert_eq!(
            kinds(&mut legend, &layered(1, vec![EdgeKind::Query]), 1.0),
            Some(&[EdgeKind::Query][..])
        );
        assert_eq!(
            kinds(&mut legend, &layered(1, vec![EdgeKind::Aquery]), 1.0),
            Some(&[EdgeKind::Query][..]),
            "a view under an unchanged key was scanned again"
        );
        assert_eq!(
            kinds(&mut legend, &layered(2, vec![EdgeKind::Aquery]), 1.0),
            Some(&[EdgeKind::Aquery][..])
        );
    }

    #[test]
    fn the_transitions_a_view_crosses_are_listed() {
        let mut legend = Legend::default();
        let transitions = |legend: &mut Legend, drawn: &Drawn| {
            legend
                .found(
                    drawn,
                    1.0,
                    &Statuses::default(),
                    Revision::default(),
                    uncoloured(),
                    unhaloed(),
                )
                .map(|found| found.transitions.clone())
        };
        let exec = crossing(1, vec![EdgeKind::Cquery], Some(Transition::Exec));
        assert_eq!(
            transitions(&mut legend, &exec),
            Some(vec![Transition::Exec])
        );
        let none = layered(2, vec![EdgeKind::Cquery]);
        assert_eq!(transitions(&mut legend, &none), Some(vec![]));
    }

    #[test]
    fn a_map_is_scanned_again_at_another_level() {
        let query = map(1, EdgeKind::Query);
        let aquery = map(1, EdgeKind::Aquery);
        assert_ne!(
            aquery.shown(COARSEST).map(Shown::key),
            aquery.shown(FINEST).map(Shown::key)
        );
        let mut legend = Legend::default();
        let before = kinds(&mut legend, &query, COARSEST).map(<[_]>::to_vec);

        let after = kinds(&mut legend, &aquery, FINEST).map(<[_]>::to_vec);

        assert_ne!(before, after);
        assert_eq!(after, aquery.view(FINEST).map(present));
    }
}
