//! The bytes the spawns an execution log records read and wrote, placed on
//! the nodes of a graph.

use crate::node_table::{Extent, Landed, NodeTable, Tally, extent, extents};
use crate::status::Landings;
use bviz_graph_layout::{Graph, Id, Kind, Names};
use bviz_invocation::ConfiguredTarget;
use bviz_invocation::spawn::{ByteMeasure, Bytes, LoggedSpawn, LoggedSpawns, SpawnBytes};
use std::ops::AddAssign;

/// Why a graph's nodes carry no spawn bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Unsized {
    /// No execution log of the graph's invocation is open.
    NoExecLog,
    /// The execution log records no spawns.
    NoSpawns,
    /// None of the spawns the log records writes an action of the graph or
    /// belongs to one of its targets.
    Unplaced,
}

/// Of the spawns an execution log recorded, how many land on a node of a
/// graph, and how.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Coverage {
    /// Those landing on the action writing their primary output.
    pub(crate) on_actions: u32,
    /// Those landing on their target, the graph holding no action writing
    /// their primary output.
    pub(crate) on_targets: u32,
    pub(crate) logged: u32,
}

impl Coverage {
    /// How many spawns are placed, and how many of them on their target
    /// where any are.
    pub(crate) fn lines(self) -> impl Iterator<Item = String> {
        let placed = self.on_actions + self.on_targets;
        let on_targets = match self.on_targets {
            0 => None,
            1 => Some("1 of them on its target, its action not in the graph".to_owned()),
            many => Some(format!(
                "{many} of them on their targets, their actions not in the graph"
            )),
        };
        let placed = format!(
            "{placed} of {} logged spawns placed on this graph",
            self.logged
        );
        std::iter::once(placed).chain(on_targets)
    }
}

/// Some spawns, counted, and the bytes they read and wrote in sum.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Spawned {
    pub(crate) spawns: u32,
    pub(crate) bytes: SpawnBytes,
}

impl Spawned {
    fn of(spawn: &LoggedSpawn<'_>) -> Self {
        Spawned {
            spawns: 1,
            bytes: SpawnBytes::of(spawn),
        }
    }
}

impl AddAssign for Spawned {
    fn add_assign(&mut self, other: Spawned) {
        self.spawns += other.spawns;
        self.bytes += other.bytes;
    }
}

/// The spawns landing on one node.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Sized {
    /// Those landing on the node itself: an action's own, and on a target
    /// those whose primary output the graph holds no action for.
    pub(crate) own: Spawned,
    /// `own` and those landing on every node the node contains.
    pub(crate) held: Spawned,
}

impl Tally for Sized {
    type Carried = Spawned;

    fn carried(&self, _: Id, _: Kind) -> Spawned {
        self.own
    }

    fn hold(&mut self, own: &Spawned) {
        self.held += *own;
    }
}

/// The spawn bytes on every node of one graph that a logged spawn lands on
/// or that contains one.
#[derive(Debug, PartialEq)]
pub(crate) struct NodeBytes {
    sized: NodeTable<Sized>,
    /// By [`ByteMeasure::index`], then by node kind, the extent of the bytes
    /// its nodes hold; none for a measure not yet measured.
    extents: [Vec<(Kind, Extent<Bytes>)>; ByteMeasure::ALL.len()],
    coverage: Coverage,
    /// Whether the bytes the spawns read are measured.
    inputs_measured: bool,
}

/// Where in `graph`, which `names` names, the spawns `logged` recorded land,
/// sized for each target they name to be looked up.
pub(crate) fn landings<'a>(graph: &Graph, names: &'a Names, logged: &LoggedSpawns) -> Landings<'a> {
    Landings::new(graph, names, logged.targets().len())
}

impl NodeBytes {
    /// Places what the spawns `logged` recorded read and wrote on the nodes
    /// of `graph` that `landings` lands them on: the action writing a
    /// spawn's primary output, or where the graph holds none, the spawn's
    /// target. The log names no configuration, so a spawn landing by target
    /// lands on the target, not on one configuration of it.
    pub(crate) fn resolve<'a>(
        landings: &Landings<'a>,
        graph: &Graph,
        logged: &'a LoggedSpawns,
    ) -> Result<Self, Unsized> {
        if logged.is_empty() {
            return Err(Unsized::NoSpawns);
        }
        let mut landed = Landed::<Sized>::with_capacity(landings.ids(), logged.len());
        let mut coverage = Coverage {
            logged: u32::try_from(logged.len()).unwrap_or(u32::MAX),
            ..Coverage::default()
        };
        for spawn in logged.iter() {
            let on_action = spawn
                .primary_output()
                .and_then(|output| landings.action(output, None));
            let node = match on_action {
                Some(action) => {
                    coverage.on_actions += 1;
                    action
                }
                None => {
                    let target = spawn.target_label().and_then(|label| {
                        landings.landing(ConfiguredTarget::new(label, None)).node()
                    });
                    let Some(target) = target else {
                        continue;
                    };
                    coverage.on_targets += 1;
                    target
                }
            };
            landed.on(node).own += Spawned::of(&spawn);
        }
        if coverage.on_actions + coverage.on_targets == 0 {
            return Err(Unsized::Unplaced);
        }
        let sized = landed.roll_up(graph);
        let extents = ByteMeasure::ALL
            .map(|measure| extents(&sized, graph, |sized| sized.held.bytes.measure(measure)));
        Ok(NodeBytes {
            sized,
            extents,
            coverage,
            inputs_measured: logged.inputs_measured(),
        })
    }

    /// The spawns landing on the node `id` names, by [`NodeTable::get`].
    pub(crate) fn get(&self, id: Id) -> Option<&Sized> {
        self.sized.get(id)
    }

    /// The extent of the bytes in `measure` the nodes of `kind` hold, absent
    /// where none of them holds any.
    pub(crate) fn extent(&self, measure: ByteMeasure, kind: Kind) -> Option<Extent<Bytes>> {
        extent(&self.extents[measure.index()], kind)
    }

    pub(crate) fn coverage(&self) -> Coverage {
        self.coverage
    }

    pub(crate) fn inputs_measured(&self) -> bool {
        self.inputs_measured
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::timing::tests::Built;
    use bviz_invocation::spawn::{Builder, InputBytes, Logged, MeasuredInputs, PhaseTiming};
    use std::sync::Arc;

    /// A spawn as an execution log records it: by primary output, target
    /// label, and the bytes it wrote, read and read of its tools.
    pub(crate) type Recorded<'a> = (Option<&'a str>, &'a str, u64, u64, u64);

    /// The spawns `spawns` records, their inputs not yet measured.
    pub(crate) fn unmeasured(spawns: &[Recorded]) -> LoggedSpawns {
        let mut builder = Builder::with_capacity(spawns.len(), spawns.len(), 0);
        for &(primary_output, target_label, outputs, _, _) in spawns {
            if let Some(output) = primary_output {
                builder.output(output, Bytes::new(outputs));
            }
            builder.push(Logged {
                runner: "remote",
                mnemonic: "Compile",
                target_label,
                cache_hit: false,
                timing: PhaseTiming::default(),
            });
        }
        builder.finish()
    }

    pub(crate) fn logged(spawns: &[Recorded]) -> LoggedSpawns {
        let read = spawns
            .iter()
            .map(|&(_, _, _, others, tools)| InputBytes {
                tools: Bytes::new(tools),
                others: Bytes::new(others),
            })
            .collect();
        let read = Arc::new(MeasuredInputs::new(read, Bytes::ZERO));
        unmeasured(spawns)
            .measured(read)
            .expect("one measure per spawn")
    }

    fn resolve(built: &Built, logged: &LoggedSpawns) -> Result<NodeBytes, Unsized> {
        let landings = landings(&built.graph, &built.names, logged);
        NodeBytes::resolve(&landings, &built.graph, logged)
    }

    fn held(bytes: &NodeBytes, id: Id) -> Option<(u32, u64, u64, u64)> {
        bytes.get(id).map(|sized| {
            let Spawned { spawns, bytes } = sized.held;
            let [outputs, read, tools] =
                ByteMeasure::ALL.map(|measure| bytes.measure(measure).expect("measured").get());
            (spawns, outputs, read, tools)
        })
    }

    #[test]
    fn a_log_of_no_spawns_or_none_of_the_graph_sizes_no_node() {
        let mut built = Built::default();
        let (_, _, bin) = built.target("app", "bin");
        built.action("bazel-out/bin/app/bin", bin);

        assert_eq!(resolve(&built, &logged(&[])), Err(Unsized::NoSpawns));
        assert_eq!(
            resolve(
                &built,
                &logged(&[
                    (Some("bazel-out/bin/gone/gone"), "//gone:gone", 1, 2, 3),
                    (None, "", 1, 2, 3),
                ])
            ),
            Err(Unsized::Unplaced)
        );
    }

    #[test]
    fn a_spawn_lands_on_the_action_writing_its_primary_output_and_rolls_up() {
        let mut built = Built::default();
        let (package, target, configured) = built.target("app", "bin");
        let compile = built.action("bazel-out/bin/app/bin.o", configured);
        let link = built.action("bazel-out/bin/app/bin", configured);
        let (_, _, idle) = built.target("app", "idle");
        let recorded = logged(&[
            (Some("bazel-out/bin/app/bin.o"), "//app:bin", 100, 40, 1000),
            (Some("bazel-out/bin/app/bin"), "//app:bin", 500, 100, 0),
        ]);

        let bytes = resolve(&built, &recorded).expect("sized");

        assert_eq!(held(&bytes, compile), Some((1, 100, 1040, 1000)));
        assert_eq!(held(&bytes, link), Some((1, 500, 100, 0)));
        for container in [configured, target, package] {
            assert_eq!(held(&bytes, container), Some((2, 600, 1140, 1000)));
            assert_eq!(bytes.get(container).unwrap().own, Spawned::default());
        }
        assert_eq!(bytes.get(idle), None);
        assert_eq!(
            bytes.coverage(),
            Coverage {
                on_actions: 2,
                on_targets: 0,
                logged: 2
            }
        );
    }

    /// The log names no configuration, so the target is where a spawn the
    /// graph holds no action for lands, and its configured target holds
    /// none of it.
    #[test]
    fn a_spawn_the_graph_holds_no_action_for_lands_on_its_target() {
        let mut built = Built::default();
        let (package, target, configured) = built.target("app", "bin");
        let link = built.action("bazel-out/bin/app/bin", configured);
        let recorded = logged(&[
            (Some("bazel-out/bin/app/bin"), "//app:bin", 500, 100, 0),
            (Some("bazel-out/bin/app/bin.lint"), "//app:bin", 7, 30, 200),
            (None, "//app:bin", 0, 3, 0),
            (Some("bazel-out/bin/gone/gone"), "//gone:gone", 1, 1, 1),
        ]);

        let bytes = resolve(&built, &recorded).expect("sized");

        assert_eq!(bytes.get(target).unwrap().own.spawns, 2);
        assert_eq!(held(&bytes, target), Some((3, 507, 333, 200)));
        assert_eq!(held(&bytes, package), Some((3, 507, 333, 200)));
        assert_eq!(held(&bytes, configured), Some((1, 500, 100, 0)));
        assert_eq!(held(&bytes, link), Some((1, 500, 100, 0)));
        assert_eq!(
            bytes.coverage(),
            Coverage {
                on_actions: 1,
                on_targets: 2,
                logged: 4
            }
        );
    }

    #[test]
    fn coverage_names_the_spawns_placed_on_their_target_only_where_any_are() {
        let lines = |on_actions, on_targets| -> Vec<String> {
            let coverage = Coverage {
                on_actions,
                on_targets,
                logged: 9,
            };
            coverage.lines().collect()
        };
        assert_eq!(lines(4, 0), ["4 of 9 logged spawns placed on this graph"]);
        assert_eq!(
            lines(4, 1),
            [
                "5 of 9 logged spawns placed on this graph",
                "1 of them on its target, its action not in the graph"
            ]
        );
        assert_eq!(
            lines(0, 3)[1],
            "3 of them on their targets, their actions not in the graph"
        );
    }

    #[test]
    fn each_measure_spans_the_least_to_the_most_bytes_a_kind_holds() {
        let mut built = Built::default();
        let (_, _, bin) = built.target("app", "bin");
        built.action("bazel-out/small", bin);
        built.action("bazel-out/large", bin);
        let recorded = logged(&[
            (Some("bazel-out/small"), "//app:bin", 4 << 10, 5, 9),
            (
                Some("bazel-out/large"),
                "//app:bin",
                4 << 20,
                (1 << 20) - 9,
                9,
            ),
        ]);

        let bytes = resolve(&built, &recorded).expect("sized");
        let span = |measure, kind| {
            let extent = bytes.extent(measure, kind).expect("sized kind");
            (extent.least().get(), extent.most().get())
        };

        assert_eq!(span(ByteMeasure::Outputs, Kind::Action), (4 << 10, 4 << 20));
        assert_eq!(span(ByteMeasure::Inputs, Kind::Action), (14, 1 << 20));
        assert_eq!(span(ByteMeasure::Tools, Kind::Action), (9, 9));
        assert_eq!(
            span(ByteMeasure::Outputs, Kind::ConfiguredTarget),
            ((4 << 20) + (4 << 10), (4 << 20) + (4 << 10))
        );
        let inputs = bytes.extent(ByteMeasure::Inputs, Kind::Action).unwrap();
        assert_eq!(
            inputs.place(Bytes::new(5)),
            inputs.place(Bytes::new(1 << 10)),
            "a size under the least placed is placed as that"
        );
        assert!((inputs.place(Bytes::new(32 << 10)) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn spawns_whose_inputs_are_unmeasured_place_what_they_wrote_alone() {
        let mut built = Built::default();
        let (_, _, bin) = built.target("app", "bin");
        let link = built.action("bazel-out/bin/app/bin", bin);
        let recorded = [(Some("bazel-out/bin/app/bin"), "//app:bin", 500, 100, 7)];

        let bytes = resolve(&built, &unmeasured(&recorded)).expect("sized");

        assert!(!bytes.inputs_measured());
        let held = bytes.get(link).expect("landed").held.bytes;
        assert_eq!(held.measure(ByteMeasure::Outputs), Some(Bytes::new(500)));
        assert_eq!(held.measure(ByteMeasure::Inputs), None);
        assert!(bytes.extent(ByteMeasure::Outputs, Kind::Action).is_some());
        assert!(bytes.extent(ByteMeasure::Inputs, Kind::Action).is_none());
        assert!(
            resolve(&built, &logged(&recorded))
                .expect("sized")
                .inputs_measured()
        );
    }
}
