//! Layout-agnostic compound graph model.
//!
//! Nodes form a containment forest through `parent`: a node with no parent is a
//! root, and a node that is some other node's parent is a container. Edges run
//! between node ids and may cross containment levels; the layout engine places
//! each edge in the lowest common ancestor of its endpoints.

use crate::intern::Id;

/// A coarse node category, used to pick a fill when rendering and to decide
/// which leaves may merge. `Group` is a Bazel `package_group`/`environment_group`
/// target and also the kind of the container an expanded merge-group renders as.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Kind {
    Package,
    Target,
    ConfiguredTarget,
    Action,
    Artifact,
    Source,
    Group,
}

impl Kind {
    /// Every kind, in declaration order, for a UI that lets a user choose among
    /// them (e.g. a kind filter).
    pub const ALL: [Kind; 7] = [
        Kind::Package,
        Kind::Target,
        Kind::ConfiguredTarget,
        Kind::Action,
        Kind::Artifact,
        Kind::Source,
        Kind::Group,
    ];

    /// The name shown for this kind in such a UI.
    pub fn label(self) -> &'static str {
        match self {
            Kind::Package => "package",
            Kind::Target => "target",
            Kind::ConfiguredTarget => "configured target",
            Kind::Action => "action",
            Kind::Artifact => "artifact",
            Kind::Source => "source",
            Kind::Group => "group",
        }
    }
}

/// A box in the graph. A node is a container if any other node names it as
/// parent, and a leaf otherwise.
#[derive(Clone)]
pub struct Node {
    pub id: Id,
    pub label: String,
    pub kind: Kind,
    /// A synthetic node a quotient pass folds interchangeable `Source`/`Artifact`
    /// leaves into. A collapsed merge-group keeps its members' kind (so it fills
    /// like them), so this — not the kind — marks the boxes that can expand or
    /// collapse a group.
    pub group: bool,
    pub parent: Option<Id>,
}

/// Which Bazel query an edge came from, so the renderer can style the static
/// (query), configured (cquery), and execution-phase (aquery) dependencies
/// distinctly when several are drawn in one joint graph. `Transitive` marks an
/// edge the `filter` pass synthesizes to stand in for a path the original graph
/// only had through a node the filter dropped, so it carries no single query's
/// provenance.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum EdgeKind {
    Query,
    Cquery,
    Aquery,
    Transitive,
}

impl EdgeKind {
    /// The provenance name shown in the renderer's legend.
    pub fn label(self) -> &'static str {
        match self {
            EdgeKind::Query => "query",
            EdgeKind::Cquery => "cquery",
            EdgeKind::Aquery => "aquery",
            EdgeKind::Transitive => "transitive",
        }
    }
}

/// A directed dependency from one node to another, both by id, tagged with the
/// query it came from.
#[derive(Clone)]
pub struct Edge {
    pub from: Id,
    pub to: Id,
    pub kind: EdgeKind,
}

#[derive(Clone, Default)]
pub struct Graph {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}

impl Graph {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_node(&mut self, id: Id, label: impl Into<String>, kind: Kind, parent: Option<Id>) {
        self.nodes.push(Node {
            id,
            label: label.into(),
            kind,
            group: false,
            parent,
        });
    }

    pub fn add_edge(&mut self, from: Id, to: Id, kind: EdgeKind) {
        self.edges.push(Edge { from, to, kind });
    }
}

/// A graph's containment forest, indexed by node id: every id up to the largest
/// the graph mentions carries a parent and a depth, an id the graph never names
/// standing as a root. Containment must be acyclic, since every query walks
/// towards the roots.
pub struct Containment {
    parent: Vec<Option<Id>>,
    depth: Vec<u32>,
}

impl Containment {
    pub fn new(graph: &Graph) -> Self {
        let mentioned = graph
            .nodes
            .iter()
            .flat_map(|n| [Some(n.id), n.parent])
            .flatten();
        let len = mentioned.map(|id| id.0 as usize + 1).max().unwrap_or(0);
        let mut parent: Vec<Option<Id>> = vec![None; len];
        for node in &graph.nodes {
            parent[node.id.0 as usize] = node.parent;
        }

        /// Stands in the depth table for an id whose distance to its root is not
        /// yet known.
        const UNWALKED: u32 = u32::MAX;

        let mut depth: Vec<u32> = vec![UNWALKED; len];
        let mut unwalked: Vec<Id> = Vec::new();
        for start in (0..len as u32).map(Id) {
            let mut at = Some(start);
            let mut steps = loop {
                let Some(id) = at else { break 0 };
                let walked = depth[id.0 as usize];
                if walked != UNWALKED {
                    break walked + 1;
                }
                unwalked.push(id);
                at = parent[id.0 as usize];
            };
            // `unwalked` holds the deepest id first, so filling it in reverse
            // starts at the shallowest, whose depth the walk leaves in `steps`.
            for &id in unwalked.iter().rev() {
                depth[id.0 as usize] = steps;
                steps += 1;
            }
            unwalked.clear();
        }
        Self { parent, depth }
    }

    pub fn parent(&self, id: Id) -> Option<Id> {
        self.parent[id.0 as usize]
    }

    /// `id`'s containment parent, or `id` itself where it is a root.
    fn up(&self, id: Id) -> Id {
        self.parent(id).unwrap_or(id)
    }

    /// Steps from `id` up to the root of its tree, a root measuring zero.
    pub fn depth(&self, id: Id) -> u32 {
        self.depth[id.0 as usize]
    }

    /// The frame that places `from` and `to` side by side — their nearest common
    /// ancestor, or `None` for the top frame when they are in different trees —
    /// and the child of it each of them descends from. `None` overall when one
    /// of the two contains the other, which no frame places side by side.
    pub fn sibling_pair(&self, from: Id, to: Id) -> Option<(Option<Id>, Id, Id)> {
        let (mut a, mut b) = (from, to);
        while self.depth(a) > self.depth(b) {
            a = self.up(a);
        }
        while self.depth(b) > self.depth(a) {
            b = self.up(b);
        }
        if a == b {
            return None;
        }
        // Both stand at one depth from here, so they reach the roots together
        // and meet at latest where both parents are `None`.
        while self.parent(a) != self.parent(b) {
            a = self.up(a);
            b = self.up(b);
        }
        Some((self.parent(a), a, b))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ids `0..=6`: `0` roots a tree holding `2` and `1`, which holds `3`; `4`
    /// roots a second holding `5`; and `6` names the absent id `9` as its parent.
    fn forest() -> Graph {
        let mut graph = Graph::new();
        let parents = [None, Some(0), Some(0), Some(1), None, Some(4), Some(9)];
        for (id, parent) in parents.into_iter().enumerate() {
            graph.add_node(Id(id as u32), "n", Kind::Target, parent.map(Id));
        }
        graph
    }

    #[test]
    fn depth_counts_the_steps_to_a_root() {
        let containment = Containment::new(&forest());
        let depths: Vec<u32> = (0..=6).map(|id| containment.depth(Id(id))).collect();
        assert_eq!(depths, [0, 1, 1, 2, 0, 1, 1]);
        assert_eq!(containment.depth(Id(9)), 0);
    }

    #[test]
    fn an_empty_graph_mentions_no_ids() {
        let containment = Containment::new(&Graph::new());
        assert!(containment.depth.is_empty());
    }

    #[test]
    fn sibling_pair_lifts_both_ends_into_one_frame() {
        let containment = Containment::new(&forest());
        assert_eq!(
            containment.sibling_pair(Id(3), Id(2)),
            Some((Some(Id(0)), Id(1), Id(2)))
        );
        assert_eq!(
            containment.sibling_pair(Id(2), Id(3)),
            Some((Some(Id(0)), Id(2), Id(1)))
        );
    }

    #[test]
    fn sibling_pair_puts_separate_trees_in_the_top_frame() {
        let containment = Containment::new(&forest());
        assert_eq!(
            containment.sibling_pair(Id(3), Id(5)),
            Some((None, Id(0), Id(4)))
        );
        assert_eq!(
            containment.sibling_pair(Id(1), Id(4)),
            Some((None, Id(0), Id(4)))
        );
    }

    #[test]
    fn no_frame_holds_a_node_and_its_own_descendant() {
        let containment = Containment::new(&forest());
        assert_eq!(containment.sibling_pair(Id(0), Id(3)), None);
        assert_eq!(containment.sibling_pair(Id(3), Id(0)), None);
        assert_eq!(containment.sibling_pair(Id(2), Id(2)), None);
    }
}
