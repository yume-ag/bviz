use bviz_graph_layout::graph::Graph;
use bviz_graph_layout::streamed_proto::ReadError;
use bviz_graph_layout::transition::Transitions;
use bviz_graph_layout::{
    Anchor, Id, InputSet, Joint, LaidOut, Names, layout, quotient, streamed_proto,
};
use bviz_ui_graph_args::{GraphSource, Input, InputKind};
use rustc_hash::FxHashSet;
use std::collections::{BTreeMap, HashMap, HashSet};

/// Reads every input's streamed_proto file and merges them into one joint graph,
/// before any group quotienting, returned with its transitive reduction. Cached
/// so toggling a group needn't re-read or re-parse.
pub(crate) fn build_graph(inputs: &[Input]) -> Result<(Joint, Graph), String> {
    let mut aq = None;
    let mut cq = None;
    let mut q = None;
    for input in inputs {
        let path = input.path.display();
        let opened = bviz_input::Input::open(&input.path).map_err(|e| format!("{path}: {e}"))?;
        let size = opened.size();
        let stream = opened.into_reader();
        let unreadable = |e| match e {
            ReadError::Io(e) => format!("{path}: {e}"),
            e => undecodable(input, e),
        };
        match input.kind {
            InputKind::Aquery => {
                aq = Some(
                    streamed_proto::read_action_graph_containers(stream, size)
                        .map_err(unreadable)?,
                );
            }
            InputKind::Cquery => {
                cq = Some(streamed_proto::read_cquery_results(stream, size).map_err(unreadable)?);
            }
            InputKind::Query => {
                q = Some(streamed_proto::read_query_targets(stream, size).map_err(unreadable)?);
            }
        }
    }
    let joint = bviz_graph_layout::build_joint(aq.as_deref(), cq.as_deref(), q.as_deref(), false);
    // Transitively reduce the whole base once: a collapsed box then draws the
    // reduction of its hidden subtree's edges, so its edge count is stable under
    // expansion rather than re-derived (differently) from each view.
    let reduced = bviz_graph_layout::reduce::transitive_reduction(&joint.graph);
    Ok((joint, reduced))
}

/// Why `input` does not decode, worded for a reader who most often handed its
/// flag some other file: another command's output, a format other than
/// streamed_proto, or another tool's file altogether.
fn undecodable(input: &Input, e: impl std::fmt::Display) -> String {
    format!(
        "{} does not decode as the output of bazel {} --output=streamed_proto, the file --{} reads ({e})",
        input.path.display(),
        input.kind.command(),
        input.kind.flag(),
    )
}

/// The inputs `source` reads.
pub(crate) fn loaded_inputs(source: &GraphSource) -> InputSet {
    let (first, rest) = source.first_and_rest();
    rest.iter().fold(input_of(first.kind).into(), |set, input| {
        set.with(input_of(input.kind))
    })
}

fn input_of(kind: InputKind) -> bviz_graph_layout::Input {
    match kind {
        InputKind::Aquery => bviz_graph_layout::Input::Aquery,
        InputKind::Cquery => bviz_graph_layout::Input::Cquery,
        InputKind::Query => bviz_graph_layout::Input::Query,
    }
}

/// The transitive reduction `view` draws. `reduced_base` collapses to the same
/// boxes as the base it reduces, so `view` keeps only the edges that projection
/// leaves (a quotiented group's members map through `remap`, every other node
/// keeps its id). The projected box graph is reduced once more because folding a
/// subtree into one box makes edges redundant that are not redundant between
/// nodes: a box A->B edge is implied by a drawn A->M->B path through other
/// visible boxes.
fn reduce_view(
    mut view: Graph,
    reduced_base: &Graph,
    expanded: &HashSet<Id>,
    remap: &BTreeMap<Id, Id>,
) -> Graph {
    let skeleton = bviz_graph_layout::lod::collapse(reduced_base, expanded);
    let box_of = |id: Id| remap.get(&id).copied().unwrap_or(id);
    let kept: FxHashSet<(Id, Id)> = skeleton
        .edges
        .iter()
        .map(|e| (box_of(e.from), box_of(e.to)))
        .filter(|(a, b)| a != b)
        .collect();
    view.edges.retain(|e| kept.contains(&(e.from, e.to)));
    bviz_graph_layout::reduce::transitive_reduction(&view)
}

/// The most connectors a layered layout is attempted with. ELK's layered layout
/// grows about quadratically in the connectors it routes, so well past this it
/// runs for minutes and outgrows a desktop machine's memory.
pub(crate) const MAX_LAID_OUT_CONNECTORS: usize = 20_000;

/// A view [`relayout`] leaves unlaid, past [`MAX_LAID_OUT_CONNECTORS`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TooDense {
    pub(crate) connectors: usize,
}

/// Builds the display graph with the given ids expanded and lays it out: fold
/// every unexpanded container away, merge interchangeable leaves, then lay out
/// only the resulting visible view, transitively reduced when `reduced`. The
/// result's `full_edges` is the visible view's unreduced connectivity, for
/// hover. Returned beside it is the remap [`quotient::quotient`] returns.
/// A view with more connectors to route than [`MAX_LAID_OUT_CONNECTORS`] is
/// [`TooDense`] and is not laid out.
///
/// Each connector carries the transition of `transitions`, found over `base`,
/// that the edges folded into it cross.
///
/// `pinned` is the configuration of the drawing this one replaces, which the
/// layout is held to so every box the two drawings share keeps its reading
/// order; the first layered layout of a session has no drawing to replace. A
/// configuration that anchors nothing holds no frame, so a drawing that numbered
/// no node leaves the view as free as no configuration at all.
pub(crate) fn relayout(
    base: &Graph,
    reduced_base: &Graph,
    expanded: &HashSet<Id>,
    names: &mut Names,
    reduced: bool,
    transitions: &Transitions,
    pinned: Option<&HashMap<Id, Anchor>>,
) -> Result<(LaidOut, BTreeMap<Id, Id>), TooDense> {
    let (visible, folding) = bviz_graph_layout::lod::collapse_folded(base, expanded);
    let (mut view, remap) = quotient::quotient(&visible, expanded, names);
    let full_edges: Vec<(Id, Id)> = view
        .edges
        .iter()
        .map(|e| (e.from, e.to))
        .collect::<FxHashSet<_>>()
        .into_iter()
        .collect();
    if reduced {
        view = reduce_view(view, reduced_base, expanded, &remap);
    }
    let connectors = view
        .edges
        .iter()
        .map(|e| (e.from, e.to))
        .collect::<FxHashSet<_>>()
        .len();
    if connectors > MAX_LAID_OUT_CONNECTORS {
        return Err(TooDense { connectors });
    }
    let mut laid = match pinned {
        Some(anchors) => layout::layout_pinned(&view, anchors),
        None => layout::layout(&view),
    };
    laid.full_edges = full_edges;
    let box_of = |id: Id| remap.get(&id).copied().unwrap_or(id);
    transitions.mark(&mut laid.connectors, |from, to| {
        let (from, to) = folding.pair(from, to)?;
        Some((box_of(from), box_of(to)))
    });
    Ok((laid, remap))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bviz_graph_layout::graph::Node;
    use bviz_graph_layout::{EdgeKind, Facets, Kind};
    use std::cmp::Ordering;

    /// A file other than the streamed_proto output its flag reads is reported
    /// with the Bazel command that writes what the flag expects.
    #[test]
    fn a_file_of_another_format_names_the_output_its_flag_reads() {
        let dir = std::env::var("TEST_TMPDIR")
            .unwrap_or_else(|_| std::env::temp_dir().display().to_string());
        let path = std::path::Path::new(&dir).join("profile.json");
        std::fs::write(&path, r#"{"otherData":{},"traceEvents":[]}"#).expect("write the file");
        for kind in [InputKind::Aquery, InputKind::Cquery, InputKind::Query] {
            let input = Input {
                kind,
                path: path.clone(),
            };
            let Err(why) = build_graph(&[input]) else {
                panic!("a profile reads as no {kind:?} output");
            };
            let command = format!("bazel {} --output=streamed_proto", kind.command());
            assert!(why.contains(&command), "{why}");
            assert!(why.contains(&format!("--{}", kind.flag())), "{why}");
        }
    }

    /// A target whose two configured targets are built as tools, one of which
    /// the configured target `app` depends on: the connector into `app` carries
    /// the exec transition whether the tools are drawn or folded into their
    /// target, and the edge between the tools none.
    #[test]
    fn a_connector_carries_the_transitions_folded_into_it() {
        use bviz_graph_layout::facets::Tri;
        use bviz_graph_layout::transition::Transition;

        let mut names = Names::default();
        let mut base = Graph::new();
        let mut facets = Facets::default();
        let exec = facets.intern_config("ck1", Some("k8-opt-exec"), Tri::Yes);
        let target = facets.intern_config("ck2", Some("k8-fastbuild"), Tri::No);
        let tool = names.intern("tool");
        base.add_node(Node::new(tool, "tool", Kind::Target, None));
        let mut add = |name: &str, parent: Option<Id>, config| {
            let id = names.intern(name);
            base.add_node(Node::new(id, name, Kind::ConfiguredTarget, parent));
            facets.set_config(id, Some(config));
            id
        };
        let (built, other) = (
            add("built", Some(tool), exec),
            add("other", Some(tool), exec),
        );
        let app = add("app", None, target);
        base.add_edge(built, app, EdgeKind::Cquery);
        base.add_edge(other, built, EdgeKind::Cquery);
        let transitions = Transitions::of(&base, &facets);
        let reduced_base = bviz_graph_layout::reduce::transitive_reduction(&base);

        for (expanded, from) in [(HashSet::new(), tool), ([tool].into(), built)] {
            let (laid, _) = relayout(
                &base,
                &reduced_base,
                &expanded,
                &mut names,
                false,
                &transitions,
                None,
            )
            .expect("a view this sparse lays out");
            let crossed: Vec<(Id, Id, Option<Transition>)> = laid
                .connectors
                .iter()
                .map(|c| (c.from, c.to, c.transition))
                .collect();
            assert!(
                crossed.contains(&(from, app, Some(Transition::Exec))),
                "{crossed:?}"
            );
            assert!(
                crossed
                    .iter()
                    .all(|&(_, to, crossed)| to == app || crossed.is_none()),
                "{crossed:?}"
            );
        }
    }

    /// A graph and the tables a relayout of it needs, so a test draws it at an
    /// expansion of its choosing.
    struct Fixture {
        base: Graph,
        reduced_base: Graph,
        names: Names,
    }

    impl Fixture {
        /// Two packages wired so the order the layout reads `srcs`'s targets in
        /// depends on whether `build` is open: `source` feeds the step `build`
        /// runs first and `plugin` the step it runs second, a distinction a
        /// collapsed `build` hides by taking both as one box.
        fn new() -> Self {
            let mut names = Names::default();
            let mut base = Graph::new();
            let mut add = |base: &mut Graph, name: &str, kind: Kind, parent: Option<Id>| {
                let id = names.intern(name);
                base.add_node(Node::new(id, name, kind, parent));
                id
            };

            let build = add(&mut base, "build", Kind::Package, None);
            let srcs = add(&mut base, "srcs", Kind::Package, None);
            let compile = add(&mut base, "compile", Kind::Action, Some(build));
            let link = add(&mut base, "link", Kind::Action, Some(build));
            let plugin = add(&mut base, "plugin", Kind::Target, Some(srcs));
            let source = add(&mut base, "source", Kind::Target, Some(srcs));

            for (from, to) in [(compile, link), (source, compile), (plugin, link)] {
                base.add_edge(from, to, EdgeKind::Query);
            }

            let reduced_base = bviz_graph_layout::reduce::transitive_reduction(&base);
            Self {
                base,
                reduced_base,
                names,
            }
        }

        fn id(&self, label: &str) -> Id {
            self.names
                .get(label)
                .unwrap_or_else(|| panic!("the fixture has no node labeled {label}"))
        }

        fn expansion(&self, labels: &[&str]) -> HashSet<Id> {
            labels.iter().map(|label| self.id(label)).collect()
        }

        /// Draws the fixture the way the view draws it, reducing as the view
        /// does by default.
        fn draw(
            &mut self,
            expanded: &HashSet<Id>,
            pinned: Option<&HashMap<Id, Anchor>>,
        ) -> LaidOut {
            relayout(
                &self.base,
                &self.reduced_base,
                expanded,
                &mut self.names,
                true,
                &Transitions::default(),
                pinned,
            )
            .expect("a view this sparse lays out")
            .0
        }

        fn label_of(&self, id: Id) -> &str {
            self.names.resolve(id)
        }
    }

    /// How many pairs of boxes of one frame both drawings drew, and which of
    /// them `after` reads in the opposite order: swapped layer order, or swapped
    /// slot order within a layer they still share. Two boxes that shared a layer
    /// and no longer do have not swapped: a revealed node the layout must place
    /// above one of them pushes it past the other without either crossing.
    fn reversed_pairs(
        fixture: &Fixture,
        before: &LaidOut,
        after: &LaidOut,
    ) -> (usize, Vec<String>) {
        let parent_of: HashMap<Id, Option<Id>> = fixture
            .base
            .nodes
            .iter()
            .map(|node| (node.id, node.parent))
            .collect();
        let (mut compared, mut reversed) = (0, Vec::new());
        for (&first, &was_first) in &before.anchors {
            for (&second, &was_second) in &before.anchors {
                if first == second || parent_of[&first] != parent_of[&second] {
                    continue;
                }
                let (Some(&now_first), Some(&now_second)) =
                    (after.anchors.get(&first), after.anchors.get(&second))
                else {
                    continue;
                };
                compared += 1;
                let swapped = match (
                    was_first.layer.cmp(&was_second.layer),
                    now_first.layer.cmp(&now_second.layer),
                ) {
                    (Ordering::Less, Ordering::Greater) | (Ordering::Greater, Ordering::Less) => {
                        true
                    }
                    (Ordering::Equal, Ordering::Equal) => matches!(
                        (
                            was_first.slot.cmp(&was_second.slot),
                            now_first.slot.cmp(&now_second.slot),
                        ),
                        (Ordering::Less, Ordering::Greater) | (Ordering::Greater, Ordering::Less)
                    ),
                    _ => false,
                };
                if swapped {
                    reversed.push(format!(
                        "{} {:?}->{:?} and {} {:?}->{:?}",
                        fixture.label_of(first),
                        was_first,
                        now_first,
                        fixture.label_of(second),
                        was_second,
                        now_second,
                    ));
                }
            }
        }
        (compared, reversed)
    }

    /// A configuration keyed by node label and ordered, so a mismatch reports
    /// which nodes moved.
    fn by_label(fixture: &Fixture, anchors: &HashMap<Id, Anchor>) -> BTreeMap<String, Anchor> {
        anchors
            .iter()
            .map(|(&id, &at)| (fixture.label_of(id).to_string(), at))
            .collect()
    }

    /// Opening a container leaves every box the drawing already held reading
    /// where it did. The same expansion laid out with nothing to follow reorders
    /// boxes it kept, so it is the pinning that holds them and not the graph.
    #[test]
    fn opening_a_container_holds_the_boxes_around_it_in_their_order() {
        let mut fixture = Fixture::new();
        let closed = fixture.expansion(&["srcs"]);
        let open = fixture.expansion(&["srcs", "build"]);

        let before = fixture.draw(&closed, None);
        let held = fixture.draw(&open, Some(&before.anchors));
        assert!(
            held.boxes.len() > before.boxes.len(),
            "opening build revealed nothing: {} boxes before, {} after",
            before.boxes.len(),
            held.boxes.len(),
        );

        let (compared, reversed) = reversed_pairs(&fixture, &before, &held);
        assert!(
            reversed.is_empty(),
            "boxes swapped over an expansion: {reversed:#?}"
        );
        assert!(compared > 0, "no frame holds two boxes both drawings drew");

        let free = fixture.draw(&open, None);
        let (_, unheld) = reversed_pairs(&fixture, &before, &free);
        assert!(
            !unheld.is_empty(),
            "the fixture's expansion reorders nothing on its own, so holding it to an \
             order proves nothing",
        );
    }

    /// A relayout with no drawing to replace orders the view itself: the same
    /// view held to a configuration that swaps a pair comes back swapped, so the
    /// order the unheld drawings settle on is one a configuration could have
    /// overridden and none did.
    #[test]
    fn a_relayout_with_no_drawing_to_replace_orders_the_view_itself() {
        let mut fixture = Fixture::new();
        let open = fixture.expansion(&["srcs", "build"]);
        let free = fixture.draw(&open, None);

        let (one, other) = (fixture.id("plugin"), fixture.id("source"));
        let (one_at, other_at) = (free.anchors[&one], free.anchors[&other]);
        assert_eq!(
            one_at.layer, other_at.layer,
            "the fixture holds these two in one layer"
        );
        let mut swapped = free.anchors.clone();
        swapped.insert(one, other_at);
        swapped.insert(other, one_at);

        let held = fixture.draw(&open, Some(&swapped));
        assert_eq!(
            (held.anchors[&one], held.anchors[&other]),
            (other_at, one_at)
        );

        let free_again = fixture.draw(&open, None);
        assert_eq!(
            by_label(&fixture, &free_again.anchors),
            by_label(&fixture, &free.anchors)
        );
    }

    /// Two rows of leaves with every one of the first depending on every one of
    /// the second, which no reduction thins, are past the connectors a layout
    /// is attempted with and come back unlaid.
    #[test]
    fn a_view_with_more_connectors_than_a_layout_takes_is_not_laid_out() {
        let side = MAX_LAID_OUT_CONNECTORS.isqrt() + 1;
        let mut names = Names::default();
        let mut base = Graph::new();
        let mut row = |row: &str| -> Vec<Id> {
            (0..side)
                .map(|i| {
                    let id = names.intern(&format!("{row}{i}"));
                    base.add_node(Node::new(id, "leaf", Kind::Action, None));
                    id
                })
                .collect()
        };
        let (dependents, dependencies) = (row("a"), row("b"));
        for &from in &dependents {
            for &to in &dependencies {
                base.add_edge(from, to, EdgeKind::Aquery);
            }
        }
        let reduced_base = bviz_graph_layout::reduce::transitive_reduction(&base);

        let laid = relayout(
            &base,
            &reduced_base,
            &HashSet::new(),
            &mut names,
            true,
            &Transitions::default(),
            None,
        );
        assert_eq!(
            laid.err(),
            Some(TooDense {
                connectors: side * side
            })
        );
    }
}
