//! What a report places on some nodes of a graph, each node holding what
//! lands on it and on every node it contains.

use bviz_graph_layout::{Graph, Id, Kind};
use bviz_invocation::spawn::Bytes;
use std::num::NonZeroU32;
use std::time::Duration;

/// A node's place in a table holding what only some nodes of a graph carry,
/// such as a [`NodeTable`]'s entries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Slot(NonZeroU32);

impl Slot {
    /// The slot of `table[index]`. Only nodes take a slot, so `index` is
    /// below the node ids, which fit a `u32`.
    pub(crate) fn at(index: usize) -> Self {
        Slot(NonZeroU32::MIN.saturating_add(index as u32))
    }

    pub(crate) fn index(self) -> usize {
        self.0.get() as usize - 1
    }
}

/// What a node carries of a report, landing on it and rolled up from the
/// nodes it contains.
pub(crate) trait Tally: Default {
    /// What the figure landing on a node itself adds to that node and to
    /// every node containing it.
    type Carried;

    /// What `self`, landed on `node` of `kind` and on nothing it contains,
    /// carries up.
    fn carried(&self, node: Id, kind: Kind) -> Self::Carried;

    /// Adds `carried`, from the node itself or one it contains.
    fn hold(&mut self, carried: &Self::Carried);
}

/// The entries of the nodes of one graph a report lands on and of those
/// containing them, held apart from the dense table finding them since only
/// some nodes carry one.
#[derive(Debug, PartialEq)]
pub(crate) struct NodeTable<T> {
    /// By node id, where in `entries` the node's entry is.
    slots: Vec<Option<Slot>>,
    entries: Vec<T>,
}

impl<T> NodeTable<T> {
    /// The entry of the node `id` names, absent where nothing lands on it or
    /// on anything it contains. An id past the graph's own, as a merge
    /// group's is, reads as a node carrying none.
    pub(crate) fn get(&self, id: Id) -> Option<&T> {
        let slot = (*self.slots.get(id.0 as usize)?)?;
        Some(&self.entries[slot.index()])
    }
}

/// What lands on the nodes of a graph, each node carrying only what lands on
/// it itself until [rolled up](Self::roll_up).
pub(crate) struct Landed<T> {
    table: NodeTable<T>,
}

impl<T: Tally> Landed<T> {
    /// Room for a graph of `ids` node ids, `landing` of them landed on.
    pub(crate) fn with_capacity(ids: usize, landing: usize) -> Self {
        Landed {
            table: NodeTable {
                slots: vec![None; ids],
                entries: Vec::with_capacity(landing),
            },
        }
    }

    /// The entry of `node`, taken from now on if it carried none.
    pub(crate) fn on(&mut self, node: Id) -> &mut T {
        let table = &mut self.table;
        let slot = *table.slots[node.0 as usize].get_or_insert_with(|| {
            table.entries.push(T::default());
            Slot::at(table.entries.len() - 1)
        });
        &mut table.entries[slot.index()]
    }

    /// Has each node of `graph` landed on hold what it carries up, and every
    /// node containing it, which takes an entry where it had none. Owners
    /// carry up in id order, so a tally keeping the first of equals keeps the
    /// one of least id whatever order `graph` lists its nodes in.
    pub(crate) fn roll_up(mut self, graph: &Graph) -> NodeTable<T> {
        let ids = self.table.slots.len();
        let mut parent = vec![None; ids];
        let mut kind = vec![None; ids];
        for node in &graph.nodes {
            parent[node.id.0 as usize] = node.parent;
            kind[node.id.0 as usize] = Some(node.kind);
        }
        let mut owners: Vec<(Id, Kind)> = Vec::with_capacity(self.table.entries.len());
        for (id, (slot, kind)) in self.table.slots.iter().zip(&kind).enumerate() {
            if let (Some(_), Some(kind)) = (slot, kind) {
                owners.push((Id(id as u32), *kind));
            }
        }
        let unheld = unheld_containers(&owners, &parent, &self.table.slots);
        self.table.entries.reserve(unheld);
        for (owner, kind) in owners {
            let carried = self.on(owner).carried(owner, kind);
            let mut at = Some(owner);
            while let Some(node) = at {
                self.on(node).hold(&carried);
                at = parent[node.0 as usize];
            }
        }
        self.table
    }
}

/// How many nodes containing one of `owners`, by `parent`, have no slot.
fn unheld_containers(
    owners: &[(Id, Kind)],
    parent: &[Option<Id>],
    slots: &[Option<Slot>],
) -> usize {
    let mut counted = vec![false; slots.len()];
    let mut unheld = 0;
    for (owner, _) in owners {
        let mut at = parent[owner.0 as usize];
        while let Some(node) = at {
            let index = node.0 as usize;
            // A container with a slot is an owner, and counts its own.
            if slots[index].is_some() || counted[index] {
                break;
            }
            counted[index] = true;
            unheld += 1;
            at = parent[index];
        }
    }
    unheld
}

/// A figure placed on a log scale, which reaches down no further than its
/// [`LEAST_PLACED`](Self::LEAST_PLACED): a scale reaching toward nothing at
/// all would crowd every other figure against its top.
pub(crate) trait Magnitude: Copy + Ord {
    /// The figure every smaller one is placed as.
    const LEAST_PLACED: Self;

    fn ln(self) -> f64;
}

/// An action that short is all but instant.
impl Magnitude for Duration {
    const LEAST_PLACED: Duration = Duration::from_millis(1);

    fn ln(self) -> f64 {
        self.as_secs_f64().ln()
    }
}

/// Files that small are all but empty.
impl Magnitude for Bytes {
    const LEAST_PLACED: Bytes = Bytes::new(1 << 10);

    fn ln(self) -> f64 {
        (self.get() as f64).ln()
    }
}

/// The least and the most of a figure the nodes of one kind hold, of those
/// that hold any.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Extent<M> {
    least: M,
    most: M,
}

impl<M: Magnitude> Extent<M> {
    pub(crate) fn of(figure: M) -> Self {
        Extent {
            least: figure,
            most: figure,
        }
    }

    pub(crate) fn widen(&mut self, figure: M) {
        self.least = self.least.min(figure);
        self.most = self.most.max(figure);
    }

    pub(crate) fn least(self) -> M {
        self.least
    }

    pub(crate) fn most(self) -> M {
        self.most
    }

    /// Where `figure` stands in the extent on a log scale, from 0 at its
    /// least to 1 at its most, clamped to that range, with every figure
    /// below [`Magnitude::LEAST_PLACED`] placed as that. An extent of one
    /// figure places it at its most.
    pub(crate) fn place(self, figure: M) -> f32 {
        let log = |figure: M| figure.max(M::LEAST_PLACED).ln();
        let span = log(self.most) - log(self.least);
        if span <= 0.0 {
            return 1.0;
        }
        ((log(figure) - log(self.least)) / span).clamp(0.0, 1.0) as f32
    }
}

/// By node kind, the extent of the figure `figure` reads of each entry of
/// `table`, over the nodes of `graph`.
pub(crate) fn extents<T, M: Magnitude>(
    table: &NodeTable<T>,
    graph: &Graph,
    figure: impl Fn(&T) -> M,
) -> Vec<(Kind, Extent<M>)> {
    let mut extents: Vec<(Kind, Extent<M>)> = Vec::new();
    for node in &graph.nodes {
        let Some(entry) = table.get(node.id) else {
            continue;
        };
        let figure = figure(entry);
        match extents.iter_mut().find(|(kind, _)| *kind == node.kind) {
            Some((_, extent)) => extent.widen(figure),
            None => extents.push((node.kind, Extent::of(figure))),
        }
    }
    extents
}

/// The extent `extents` gives the nodes of `kind`, absent where none of them
/// holds any.
pub(crate) fn extent<M: Copy>(extents: &[(Kind, Extent<M>)], kind: Kind) -> Option<Extent<M>> {
    extents
        .iter()
        .find(|(of, _)| *of == kind)
        .map(|(_, extent)| *extent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bviz_graph_layout::graph::Node;

    /// The owners carried up into a node, in the order they arrive.
    #[derive(Default, Debug, PartialEq)]
    struct Arrivals(Vec<Id>);

    impl Tally for Arrivals {
        type Carried = Id;

        fn carried(&self, node: Id, _: Kind) -> Id {
            node
        }

        fn hold(&mut self, carried: &Id) {
            self.0.push(*carried);
        }
    }

    const PACKAGE: Id = Id(0);
    const TARGET: Id = Id(1);
    const FIRST: Id = Id(2);
    const SECOND: Id = Id(3);
    const ELSEWHERE: Id = Id(4);

    /// Two actions of one target of one package, and a target outside it,
    /// listed from the last id to the first.
    fn graph() -> Graph {
        let mut graph = Graph::new();
        for (id, kind, parent) in [
            (ELSEWHERE, Kind::Target, None),
            (SECOND, Kind::Action, Some(TARGET)),
            (FIRST, Kind::Action, Some(TARGET)),
            (TARGET, Kind::Target, Some(PACKAGE)),
            (PACKAGE, Kind::Package, None),
        ] {
            graph.add_node(Node::new(id, format!("{id:?}"), kind, parent));
        }
        graph
    }

    fn held(table: &NodeTable<Arrivals>, id: Id) -> Option<&[Id]> {
        table.get(id).map(|arrivals| arrivals.0.as_slice())
    }

    #[test]
    fn owners_carry_up_in_id_order_whatever_order_the_graph_lists() {
        let graph = graph();
        let mut landed = Landed::<Arrivals>::with_capacity(graph.nodes.len(), 2);
        landed.on(SECOND);
        landed.on(FIRST);
        let table = landed.roll_up(&graph);
        assert_eq!(held(&table, FIRST), Some(&[FIRST][..]));
        assert_eq!(held(&table, TARGET), Some(&[FIRST, SECOND][..]));
        assert_eq!(held(&table, PACKAGE), Some(&[FIRST, SECOND][..]));
        assert_eq!(held(&table, ELSEWHERE), None);
        assert_eq!(held(&table, Id(99)), None);
    }

    #[test]
    fn a_container_landed_on_carries_its_own_and_what_it_holds() {
        let graph = graph();
        let mut landed = Landed::<Arrivals>::with_capacity(graph.nodes.len(), 2);
        landed.on(FIRST);
        landed.on(TARGET);
        let table = landed.roll_up(&graph);
        assert_eq!(held(&table, TARGET), Some(&[TARGET, FIRST][..]));
        assert_eq!(held(&table, PACKAGE), Some(&[TARGET, FIRST][..]));
    }

    #[test]
    fn each_container_without_a_slot_counts_once() {
        let graph = graph();
        let mut parent = vec![None; graph.nodes.len()];
        for node in &graph.nodes {
            parent[node.id.0 as usize] = node.parent;
        }
        let mut slots = vec![None; graph.nodes.len()];
        let owners = [(FIRST, Kind::Action), (SECOND, Kind::Action)];
        for (index, (owner, _)) in owners.iter().enumerate() {
            slots[owner.0 as usize] = Some(Slot::at(index));
        }
        assert_eq!(unheld_containers(&owners, &parent, &slots), 2);
        slots[TARGET.0 as usize] = Some(Slot::at(owners.len()));
        let with_target = [owners[0], owners[1], (TARGET, Kind::Target)];
        assert_eq!(unheld_containers(&with_target, &parent, &slots), 1);
    }

    #[test]
    fn a_figure_is_placed_on_a_log_scale_between_the_extents_ends() {
        let mut extent = Extent::of(Duration::from_millis(10));
        extent.widen(Duration::from_millis(1000));
        let place = |millis| extent.place(Duration::from_millis(millis));
        assert_eq!(place(10), 0.0);
        assert_eq!(place(1000), 1.0);
        assert!((place(100) - 0.5).abs() < 1e-6);
        assert_eq!(place(1), 0.0);
        assert_eq!(place(5000), 1.0);
    }

    #[test]
    fn figures_below_the_least_placed_stand_as_it() {
        let mut extent = Extent::of(Bytes::new(1));
        extent.widen(Bytes::new(1 << 20));
        assert_eq!(extent.place(Bytes::new(1)), 0.0);
        assert_eq!(extent.place(Bytes::LEAST_PLACED), 0.0);
        assert!((extent.place(Bytes::new(32 << 10)) - 0.5).abs() < 1e-6);
        let single = Extent::of(Bytes::new(7));
        assert_eq!(single.place(Bytes::new(7)), 1.0);
    }
}
