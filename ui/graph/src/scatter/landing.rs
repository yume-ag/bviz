//! Where a node of the graph stands among the points a scatter plot draws,
//! which a jump to it lands on.

use super::plot::Plotted;
use crate::chips::Unmarked;
use crate::hidden::HiddenBy;
use crate::jump::Stands;
use crate::stand_in::{self, StandIns, Writers};
use bviz_graph_layout::graph::Graph;
use bviz_graph_layout::metrics::{Measurement, Metrics, Unit};
use bviz_graph_layout::select::Verdicts;
use bviz_graph_layout::{Facets, Id};
use rustc_hash::{FxHashMap, FxHashSet};
use std::sync::Arc;

/// The points a plot measures in its unit, and the marks drawing them, by
/// node.
pub(crate) struct PointIndex {
    unit: Unit,
    /// Every point the plot takes, drawn or not.
    points: FxHashSet<Id>,
    /// The mark drawing each point drawn, by its index among the marks.
    marks: FxHashMap<Id, usize>,
    /// The points the marks draw, which a list over the plot ranks first.
    drawn: Arc<FxHashSet<Id>>,
    /// What each node holding points holds of them.
    holders: FxHashMap<Id, Holding>,
    writers: Arc<Writers>,
}

/// What a node holds of a plot's points.
#[derive(Clone, Copy, Default)]
struct Holding {
    /// Whether a mark draws any of them.
    drawn: bool,
    /// The first of them no mark draws.
    undrawn: Option<Id>,
}

/// What a jump to a node lands on among a plot's points.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Landing {
    /// The marks standing for it, by their indices among the plotted marks.
    Marks(Vec<usize>),
    /// The point standing for it, which the filter hides by what this names.
    Hidden(Id, HiddenBy),
    Absent(Absent),
}

/// Why a node stands for no point a plot draws, the filter aside.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Absent {
    /// Nothing the plot measures in its unit stands for it.
    NotAPoint,
    /// The points standing for it have no measure on an axis.
    Unplaced,
}

impl Landing {
    /// Why a plot landing a jump here marks no point.
    pub(crate) fn unmarked(&self) -> Option<Unmarked> {
        match self {
            Landing::Marks(_) => None,
            Landing::Hidden(..) => Some(Unmarked::Hidden),
            Landing::Absent(_) => Some(Unmarked::Absent),
        }
    }
}

impl Absent {
    pub(crate) fn describe(self) -> &'static str {
        match self {
            Absent::NotAPoint => "not a point of this plot",
            Absent::Unplaced => "not placed: no measure on an axis",
        }
    }
}

impl PointIndex {
    /// The index of the points of `measured` that `plots` takes, drawn as
    /// `plotted`, over `graph`, whose files `writers` lands on actions.
    pub(crate) fn of(
        measured: &Measurement,
        plotted: &Plotted,
        plots: impl Fn(&Metrics) -> bool,
        graph: &Graph,
        writers: Arc<Writers>,
    ) -> Self {
        let mut points = FxHashSet::default();
        points.reserve(measured.points.len());
        points.extend(measured.points.iter().filter(|p| plots(p)).map(|p| p.id));
        let mut marks = FxHashMap::default();
        marks.reserve(plotted.marks.len());
        marks.extend(
            (plotted.marks.iter().enumerate()).map(|(index, mark)| (mark.target.id, index)),
        );
        let drawn = Arc::new(marks.keys().copied().collect());
        let mut holders: FxHashMap<Id, Holding> = FxHashMap::default();
        for &point in &points {
            let drawn = marks.contains_key(&point);
            for holder in stand_in::ancestors(graph, point).skip(1) {
                let holding = holders.entry(holder).or_default();
                holding.drawn |= drawn;
                if !drawn && holding.undrawn.is_none() {
                    holding.undrawn = Some(point);
                }
            }
        }
        PointIndex {
            unit: measured.scope.unit(),
            points,
            marks,
            drawn,
            holders,
            writers,
        }
    }

    /// The points the marks draw.
    pub(crate) fn drawn(&self) -> &Arc<FxHashSet<Id>> {
        &self.drawn
    }

    /// Where `id` stands among the points, as a palette row badges it: a node
    /// holding points stands among them where a mark draws any of them, and
    /// is otherwise hidden or absent as the first of them is.
    pub(crate) fn stands(
        &self,
        graph: &Graph,
        facets: &Facets,
        verdicts: &Verdicts,
        id: Id,
    ) -> Stands {
        let points = match self.stand_ins(graph, facets, id) {
            StandIns::Points(points) => points,
            StandIns::Holding => match self.holders.get(&id).copied().unwrap_or_default() {
                Holding { drawn: true, .. } => return Stands::Here,
                Holding { undrawn, .. } => undrawn.into_iter().collect(),
            },
        };
        match self.landing_on(&points, verdicts) {
            Landing::Marks(_) => Stands::Here,
            Landing::Hidden(_, by) => Stands::Hidden(by),
            Landing::Absent(_) => Stands::Absent,
        }
    }

    /// What a jump to `id` lands on.
    pub(crate) fn landing(
        &self,
        graph: &Graph,
        facets: &Facets,
        verdicts: &Verdicts,
        id: Id,
    ) -> Landing {
        let points = match self.stand_ins(graph, facets, id) {
            StandIns::Points(points) => points,
            StandIns::Holding => {
                let mut held: Vec<Id> =
                    stand_in::held(graph, self.points.iter().copied(), id).collect();
                held.sort_unstable();
                held
            }
        };
        self.landing_on(&points, verdicts)
    }

    /// What a jump onto `points` lands on: those drawn, and otherwise the
    /// first the filter hides, or why none is drawn.
    fn landing_on(&self, points: &[Id], verdicts: &Verdicts) -> Landing {
        let marks: Vec<usize> = points
            .iter()
            .filter_map(|id| self.marks.get(id).copied())
            .collect();
        if !marks.is_empty() {
            return Landing::Marks(marks);
        }
        let taken = points.iter().filter(|id| self.points.contains(id));
        let mut absent = Absent::NotAPoint;
        for &point in taken {
            match verdicts.hiding(point) {
                Some(hiding) => return Landing::Hidden(point, HiddenBy::of(hiding)),
                None => absent = Absent::Unplaced,
            }
        }
        Landing::Absent(absent)
    }

    fn stand_ins(&self, graph: &Graph, facets: &Facets, id: Id) -> StandIns {
        let is_point = |id| self.points.contains(&id);
        stand_in::stand_ins(self.unit, graph, facets, &self.writers, is_point, id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hue::Hue;
    use crate::jump::fixture;
    use crate::scatter::axis::{Axes, Reported};
    use bviz_graph_layout::InputSet;
    use bviz_graph_layout::graph::{EdgeKind, Kind};
    use bviz_graph_layout::metrics::{Count, Scope, Sizing};
    use bviz_graph_layout::select::{Filter, Pin, Visibility};
    use std::collections::HashMap;

    #[test]
    fn a_selection_landing_on_no_mark_is_unmarked_as_it_lands() {
        assert_eq!(Landing::Marks(vec![0]).unmarked(), None);
        let hidden = Landing::Hidden(Id(1), HiddenBy::Query);
        assert_eq!(hidden.unmarked(), Some(Unmarked::Hidden));
        let absent = Landing::Absent(Absent::NotAPoint);
        assert_eq!(absent.unmarked(), Some(Unmarked::Absent));
    }

    /// Every node of `kind` in `graph`, measured alike.
    fn measured(graph: &Graph, unit: Unit, kinds: &[Kind]) -> Measurement {
        let points = graph
            .nodes
            .iter()
            .filter(|node| kinds.contains(&node.kind))
            .map(|node| Metrics {
                id: node.id,
                kind: node.kind,
                closure: Count::Exact(1),
                blast_radius: Count::Exact(1),
                fan_in: 0,
                fan_out: 0,
                actions: 0,
            })
            .collect();
        Measurement {
            scope: Scope::new(unit, InputSet::ALL).unwrap(),
            points,
            sizing: Sizing::FanIn,
        }
    }

    /// The landing of each named node in a plot of `unit` measuring `kinds`,
    /// with `hidden` pinned hidden.
    fn landings(
        graph: &Graph,
        facets: &Facets,
        unit: Unit,
        kinds: &[Kind],
        hidden: Option<Id>,
        ids: &[Id],
    ) -> Vec<Landing> {
        let measured = measured(graph, unit, kinds);
        let pins: HashMap<Id, Pin> = hidden
            .map(|id| (id, Pin::Set(Visibility::Hidden)))
            .into_iter()
            .collect();
        let filter = Filter {
            scope: None,
            rules: Vec::new(),
            pins,
        };
        let verdicts = filter.evaluate(graph);
        let plots = |point: &Metrics| unit != Unit::Action || point.kind == Kind::Action;
        let plotted = Plotted::of(
            &measured,
            &measured,
            Axes::OPENING,
            |_| Reported::default(),
            plots,
            |id| verdicts.get(id),
            |_| Hue::Other,
        );
        let writers = Arc::new(Writers::of(graph));
        let index = PointIndex::of(&measured, &plotted, plots, graph, writers);
        let marked = |landing: Landing| match landing {
            Landing::Marks(marks) => {
                let mut ids: Vec<usize> = (marks.iter())
                    .map(|&mark| plotted.marks[mark].target.id.0 as usize)
                    .collect();
                ids.sort_unstable();
                Landing::Marks(ids)
            }
            other => other,
        };
        (ids.iter())
            .map(|&id| marked(index.landing(graph, facets, &verdicts, id)))
            .collect()
    }

    fn marks(ids: &[Id]) -> Landing {
        Landing::Marks(ids.iter().map(|id| id.0 as usize).collect())
    }

    /// A node lands on the target it belongs to; a package on every target it
    /// holds.
    #[test]
    fn a_node_lands_on_its_target_and_a_package_on_those_it_holds() {
        let shared = fixture();
        let id = |name: &str| shared.names.get(name).unwrap();
        let (lib, bin, src) = (
            id("target://app:lib"),
            id("target://app:bin"),
            id("target://app:lib.cc"),
        );
        let landed = landings(
            &shared.base,
            &shared.facets,
            Unit::Target,
            &[Kind::Target, Kind::Source],
            None,
            &[id("action:bazel-out/app/liblib.a"), id("pkg:app"), lib],
        );
        assert_eq!(
            landed,
            [marks(&[lib]), marks(&[lib, bin, src]), marks(&[lib])]
        );
    }

    /// A target lands on each of its configured targets; a file on the one
    /// holding it.
    #[test]
    fn a_target_lands_on_its_configured_targets_and_a_file_on_its_holder() {
        let shared = fixture();
        let id = |name: &str| shared.names.get(name).unwrap();
        let lib_ct = id("ct://app:lib#3f2a01");
        let landed = landings(
            &shared.base,
            &shared.facets,
            Unit::ConfiguredTarget,
            &[Kind::ConfiguredTarget],
            None,
            &[id("target://app:lib"), id("artifact7")],
        );
        assert_eq!(landed, [marks(&[lib_ct]), marks(&[lib_ct])]);
    }

    /// A file lands on the action writing it, a source file on no point, and
    /// a point the filter hides is named with what hides it.
    #[test]
    fn a_file_lands_on_its_writer_and_a_hidden_point_is_named() {
        let mut shared = fixture();
        let id = |name: &str| shared.names.get(name).unwrap();
        let (out, compile) = (id("artifact7"), id("action:bazel-out/app/liblib.a"));
        let source = id("target://app:lib.cc");
        let mut graph = (*shared.base).clone();
        graph.add_edge(compile, out, EdgeKind::Aquery);
        shared.base = std::sync::Arc::new(graph);
        let kinds = [Kind::Action, Kind::Source];
        let landed = landings(
            &shared.base,
            &shared.facets,
            Unit::Action,
            &kinds,
            None,
            &[out, source],
        );
        assert_eq!(
            landed,
            [marks(&[compile]), Landing::Absent(Absent::NotAPoint)]
        );
        let hidden = landings(
            &shared.base,
            &shared.facets,
            Unit::Action,
            &kinds,
            Some(compile),
            &[out],
        );
        assert_eq!(hidden, [Landing::Hidden(compile, HiddenBy::Pin)]);
    }

    /// A holder whose every point the filter hides is badged hidden, as its
    /// first such point is; one holding a drawn point stands among them.
    #[test]
    fn a_holder_of_hidden_points_alone_is_hidden() {
        let shared = fixture();
        let id = |name: &str| shared.names.get(name).unwrap();
        let (graph, facets) = (&*shared.base, &*shared.facets);
        let unit = Unit::ConfiguredTarget;
        let measured = measured(graph, unit, &[Kind::ConfiguredTarget]);
        let lib_ct = id("ct://app:lib#3f2a01");
        let pins = HashMap::from([(lib_ct, Pin::Set(Visibility::Hidden))]);
        let filter = Filter {
            scope: None,
            rules: Vec::new(),
            pins,
        };
        let verdicts = filter.evaluate(graph);
        let plotted = Plotted::of(
            &measured,
            &measured,
            Axes::OPENING,
            |_| Reported::default(),
            |_| true,
            |id| verdicts.get(id),
            |_| Hue::Other,
        );
        let writers = Arc::new(Writers::of(graph));
        let index = PointIndex::of(&measured, &plotted, |_| true, graph, writers);
        let stands = |name: &str| index.stands(graph, facets, &verdicts, id(name));
        assert_eq!(stands("target://app:lib"), Stands::Hidden(HiddenBy::Pin));
        assert_eq!(stands("pkg:app"), Stands::Here);
        assert_eq!(stands("target://app:lib.cc"), Stands::Absent);
    }
}
