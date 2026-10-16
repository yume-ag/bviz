//! The bytes the spawns an execution log records read and wrote, placed on
//! the nodes of a graph.

use crate::node_table::{Landed, NodeTable, Tally};
use crate::status::Landings;
use bviz_graph_layout::{Graph, Id, Kind, Names};
use bviz_invocation::ConfiguredTarget;
use bviz_invocation::spawn::{LoggedSpawn, LoggedSpawns, SpawnBytes};
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
        let mut placed = false;
        for spawn in logged.iter() {
            let on_action = spawn
                .primary_output()
                .and_then(|output| landings.action(output, None));
            let node = on_action.or_else(|| {
                let target = ConfiguredTarget::new(spawn.target_label()?, None);
                landings.landing(target).node()
            });
            if let Some(node) = node {
                landed.on(node).own += Spawned::of(&spawn);
                placed = true;
            }
        }
        if !placed {
            return Err(Unsized::Unplaced);
        }
        Ok(NodeBytes {
            sized: landed.roll_up(graph),
        })
    }

    /// The spawns landing on the node `id` names, by [`NodeTable::get`].
    pub(crate) fn get(&self, id: Id) -> Option<&Sized> {
        self.sized.get(id)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::timing::tests::Built;
    use bviz_invocation::spawn::{
        Builder, ByteMeasure, Bytes, InputBytes, Logged, MeasuredInputs, PhaseTiming,
    };

    /// A spawn as an execution log records it: by primary output, target
    /// label, and the bytes it wrote, read and read of its tools.
    pub(crate) type Recorded<'a> = (Option<&'a str>, &'a str, u64, u64, u64);

    pub(crate) fn logged(spawns: &[Recorded]) -> LoggedSpawns {
        let mut builder = Builder::with_capacity(spawns.len(), spawns.len(), 0);
        let mut read = Vec::with_capacity(spawns.len());
        for &(primary_output, target_label, outputs, others, tools) in spawns {
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
            read.push(InputBytes {
                tools: Bytes::new(tools),
                others: Bytes::new(others),
            });
        }
        builder.finish(MeasuredInputs::new(read))
    }

    fn resolve(built: &Built, logged: &LoggedSpawns) -> Result<NodeBytes, Unsized> {
        let landings = landings(&built.graph, &built.names, logged);
        NodeBytes::resolve(&landings, &built.graph, logged)
    }

    fn held(bytes: &NodeBytes, id: Id) -> Option<(u32, u64, u64, u64)> {
        bytes.get(id).map(|sized| {
            let Spawned { spawns, bytes } = sized.held;
            let [outputs, read, tools] =
                ByteMeasure::ALL.map(|measure| bytes.measure(measure).get());
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
    }
}
