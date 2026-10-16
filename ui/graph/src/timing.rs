//! How long the actions a profile recorded took, placed on the nodes of a
//! graph.

use crate::node_table::{Extent, Landed, NodeTable, Tally, extent, extents};
use crate::status::Landings;
use bviz_graph_layout::{Graph, Id, Kind, Names};
use bviz_invocation::{Runner, Share, Spent, Timings};
use std::time::Duration;

mod words;

/// Why a graph's nodes carry no action time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Untimed {
    /// No profile of the graph's invocation is open.
    NoProfile,
    /// The profile records no actions.
    NoActions,
    /// The profile names its actions by neither primary output nor owner.
    Unnamed,
    /// None of the actions the profile names is a node of the graph or
    /// owned by one.
    Unplaced,
}

/// Of the actions a profile recorded, how many land on a node of a graph.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Coverage {
    pub(crate) placed: u32,
    pub(crate) profiled: u32,
}

/// An action node, by how long its action took.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Slowest {
    pub(crate) action: Id,
    pub(crate) wall: Duration,
}

/// The time spent on one node.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Timed {
    /// The actions landing on the node itself: an action node's own, and on
    /// a target those of its actions the graph holds no node for.
    pub(crate) own: Spent,
    /// `own` and the actions landing on every node the node contains.
    pub(crate) held: Spent,
    /// The slowest action node among the node and those it contains; `None`
    /// where every action held lands on a target.
    pub(crate) slowest: Option<Slowest>,
}

/// The share of a box's action time one runner has to hold for the box to
/// count as that runner's: well over half, so a box shown as one runner's
/// spent most of its time there.
const LEADING_SHARE: f64 = 0.6;

/// Where the actions of one node ran, taken together.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ran {
    /// One runner holds at least [`LEADING_SHARE`] of their time.
    On(Runner),
    /// No runner holds that much.
    Mixed,
}

impl Ran {
    /// Where the actions `spent` counts ran, by time. Actions that took no
    /// time at all are weighed by their count instead.
    pub(crate) fn of(spent: &Spent) -> Self {
        let total = spent.total();
        let weight = |share: Share| match total.wall.is_zero() {
            true => f64::from(share.actions),
            false => share.wall.as_secs_f64(),
        };
        let whole = weight(total);
        // A tie goes to the runner `runners` lists first, which `min_by`
        // keeps.
        let leading = spent
            .runners()
            .min_by(|(_, a), (_, b)| weight(*b).total_cmp(&weight(*a)));
        match leading {
            Some((runner, share)) if weight(share) >= LEADING_SHARE * whole => Ran::On(runner),
            _ => Ran::Mixed,
        }
    }
}

/// The time spent on every node of one graph that an action lands on or
/// that contains one.
#[derive(Debug, PartialEq)]
pub(crate) struct NodeTimes {
    timed: NodeTable<Timed>,
    /// By node kind, the extent of the time its nodes hold.
    extents: Vec<(Kind, Extent<Duration>)>,
    coverage: Coverage,
}

/// Where in `graph`, which `names` names, the actions `timings` recorded
/// land, sized for each owner among them to be looked up: one keying actions
/// known by no primary output, or one owning an action the graph holds no
/// node for.
pub(crate) fn landings<'a>(graph: &Graph, names: &'a Names, timings: &Timings) -> Landings<'a> {
    let owners = timings.actions().len() + timings.by_owner().count();
    Landings::new(graph, names, owners)
}

impl NodeTimes {
    /// Places what `timings` recorded on the nodes of `graph` that `landings`
    /// lands reports on.
    pub(crate) fn resolve<'a>(
        landings: &Landings<'a>,
        graph: &Graph,
        timings: &'a Timings,
    ) -> Result<Self, Untimed> {
        if timings.is_empty() {
            return Err(Untimed::NoActions);
        }
        if !timings.names_primary_outputs() && !timings.names_owners() {
            return Err(Untimed::Unnamed);
        }
        let landing = timings.actions().len() + timings.by_owner().count();
        let mut landed = Landed::<Timed>::with_capacity(landings.ids(), landing);
        let mut coverage = Coverage {
            placed: 0,
            profiled: timings.spent().total().actions,
        };
        for (primary_output, owner, timing) in timings.actions() {
            if let Some(node) = landings.action(primary_output, owner) {
                landed.on(node).own.add_action(timing);
                coverage.placed += 1;
            }
        }
        for (owner, spent) in timings.by_owner() {
            if let Some(node) = landings.landing(owner).node() {
                landed.on(node).own.add(spent);
                coverage.placed += spent.total().actions;
            }
        }
        if coverage.placed == 0 {
            return Err(Untimed::Unplaced);
        }
        let timed = landed.roll_up(graph);
        let extents = extents(&timed, graph, |timed| timed.held.total().wall);
        Ok(NodeTimes {
            timed,
            extents,
            coverage,
        })
    }

    /// The extent of the time the nodes of `kind` hold, absent where none of
    /// them holds any.
    pub(crate) fn extent(&self, kind: Kind) -> Option<Extent<Duration>> {
        extent(&self.extents, kind)
    }

    /// The time spent on the node `id` names, by [`NodeTable::get`].
    pub(crate) fn get(&self, id: Id) -> Option<&Timed> {
        self.timed.get(id)
    }

    pub(crate) fn coverage(&self) -> Coverage {
        self.coverage
    }
}

impl Tally for Timed {
    /// The time landing on the node itself, and the node where it is an
    /// action.
    type Carried = (Spent, Option<Slowest>);

    fn carried(&self, node: Id, kind: Kind) -> Self::Carried {
        let slowest = (kind == Kind::Action).then(|| Slowest {
            action: node,
            wall: self.own.total().wall,
        });
        (self.own.clone(), slowest)
    }

    fn hold(&mut self, (own, slowest): &Self::Carried) {
        self.held.add(own);
        if let Some(slowest) = *slowest {
            let slower = self.slowest.is_none_or(|held| held.wall < slowest.wall);
            if slower {
                self.slowest = Some(slowest);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bviz_graph_layout::graph::Node;
    use bviz_graph_layout::{Kind, Names};
    use bviz_invocation::{ConfiguredTarget, Profiled, Spawns, Timing};
    use std::time::Duration;

    const CONFIG: &str = "a7a71f";

    #[derive(Default)]
    struct Built {
        graph: Graph,
        names: Names,
    }

    impl Built {
        fn node(&mut self, name: &str, kind: Kind, parent: Option<Id>) -> Id {
            let id = self.names.intern(name);
            self.graph.add_node(Node::new(id, name, kind, parent));
            id
        }

        /// The package `//{package}` with the target `name` in it, configured
        /// under [`CONFIG`]: the package, the target and the configured target.
        fn target(&mut self, package: &str, name: &str) -> (Id, Id, Id) {
            let label = format!("//{package}:{name}");
            let package = match self.names.get(&format!("pkg:{package}")) {
                Some(package) => package,
                None => self.node(&format!("pkg:{package}"), Kind::Package, None),
            };
            let target = self.node(&format!("target:{label}"), Kind::Target, Some(package));
            let configured = self.node(
                &format!("ct:{label}#{CONFIG}"),
                Kind::ConfiguredTarget,
                Some(target),
            );
            (package, target, configured)
        }

        fn action(&mut self, primary_output: &str, owner: Id) -> Id {
            self.node(
                &format!("action:{primary_output}"),
                Kind::Action,
                Some(owner),
            )
        }

        fn resolve(&self, timings: &Timings) -> Result<NodeTimes, Untimed> {
            NodeTimes::resolve(
                &landings(&self.graph, &self.names, timings),
                &self.graph,
                timings,
            )
        }
    }

    fn timing(millis: u64, runner: Runner) -> Timing {
        Timing {
            wall: Duration::from_millis(millis),
            runner,
        }
    }

    fn owner(label: &str) -> Option<ConfiguredTarget<&str>> {
        Some(ConfiguredTarget::new(label, Some(CONFIG)))
    }

    /// An action as a profile records it: by primary output, owner and time.
    type Recorded<'a> = (Option<&'a str>, Option<ConfiguredTarget<&'a str>>, Timing);

    fn timings(actions: &[Recorded]) -> Timings {
        let mut timings = Timings::with_capacity(actions.len());
        for &(primary_output, owner, timing) in actions {
            timings.record(Profiled {
                primary_output,
                owner,
                wall: timing.wall,
                spawns: Spawns::on(timing.runner, timing.wall),
            });
        }
        timings
    }

    fn share(actions: u32, millis: u64) -> Share {
        Share {
            actions,
            wall: Duration::from_millis(millis),
        }
    }

    fn held(times: &NodeTimes, id: Id) -> Option<Share> {
        times.get(id).map(|timed| timed.held.total())
    }

    #[test]
    fn a_profile_naming_no_action_or_none_of_the_graph_times_no_node() {
        let mut built = Built::default();
        let (_, _, bin) = built.target("app", "bin");
        built.action("bazel-out/bin/app/bin", bin);
        let run = timing(5, Runner::Local);

        assert_eq!(built.resolve(&Timings::default()), Err(Untimed::NoActions));
        assert_eq!(
            built.resolve(&timings(&[(None, None, run)])),
            Err(Untimed::Unnamed)
        );
        assert_eq!(
            built.resolve(&timings(&[
                (Some("bazel-out/bin/lib/lib.a"), None, run),
                (None, owner("//lib:lib"), run),
            ])),
            Err(Untimed::Unplaced)
        );
    }

    #[test]
    fn an_action_lands_on_its_node_and_its_time_on_every_container_above_it() {
        let mut built = Built::default();
        let (package, target, configured) = built.target("app", "bin");
        let compile = built.action("bazel-out/bin/app/bin.o", configured);
        let link = built.action("bazel-out/bin/app/bin", configured);
        let (_, _, idle) = built.target("app", "idle");
        let recorded = timings(&[
            (
                Some("bazel-out/bin/app/bin.o"),
                None,
                timing(30, Runner::RemoteExecution),
            ),
            (
                Some("bazel-out/bin/app/bin"),
                None,
                timing(12, Runner::Sandboxed),
            ),
        ]);

        let times = built.resolve(&recorded).expect("timed");

        assert_eq!(held(&times, compile), Some(share(1, 30)));
        assert_eq!(held(&times, link), Some(share(1, 12)));
        for container in [configured, target, package] {
            assert_eq!(held(&times, container), Some(share(2, 42)));
            assert_eq!(times.get(container).unwrap().own, Spent::default());
        }
        let spent = &times.get(package).unwrap().held;
        assert_eq!(spent.of(Runner::RemoteExecution), share(1, 30));
        let slowest = |id| times.get(id).and_then(|timed| timed.slowest);
        let compiling = Some(Slowest {
            action: compile,
            wall: Duration::from_millis(30),
        });
        for holder in [compile, configured, target, package] {
            assert_eq!(slowest(holder), compiling);
        }
        assert_eq!(slowest(link).map(|slowest| slowest.action), Some(link));
        assert_eq!(spent.of(Runner::Sandboxed), share(1, 12));
        assert_eq!(times.get(idle), None);
    }

    #[test]
    fn an_action_named_only_by_its_owner_lands_on_the_owner() {
        let mut built = Built::default();
        let (package, target, configured) = built.target("app", "bin");
        let other_package = built.node("pkg:lib", Kind::Package, None);
        let unconfigured = built.node("target://lib:lib", Kind::Target, Some(other_package));
        let recorded = timings(&[
            (None, owner("//app:bin"), timing(8, Runner::Sandboxed)),
            (None, owner("//app:bin"), timing(2, Runner::NoSpawnRecorded)),
            (None, owner("//lib:lib"), timing(5, Runner::Worker)),
            (None, owner("//gone:gone"), timing(100, Runner::Local)),
            (None, None, timing(1, Runner::Local)),
        ]);

        let times = built.resolve(&recorded).expect("timed");

        assert_eq!(
            times.get(configured).unwrap().own.total(),
            share(2, 10),
            "lands on the configured target it names"
        );
        for container in [target, package] {
            assert_eq!(held(&times, container), Some(share(2, 10)));
        }
        assert_eq!(held(&times, unconfigured), Some(share(1, 5)));
        assert_eq!(
            times.get(package).unwrap().slowest,
            None,
            "no action node is held"
        );
        assert_eq!(held(&times, other_package), Some(share(1, 5)));
    }

    #[test]
    fn an_action_the_graph_holds_no_node_for_lands_on_its_owner() {
        let mut built = Built::default();
        let (package, _, configured) = built.target("app", "bin");
        let recorded = timings(&[
            (
                Some("bazel-out/bin/app/bin"),
                owner("//app:bin"),
                timing(9, Runner::Sandboxed),
            ),
            (
                Some("bazel-out/bin/gone/gone"),
                owner("//gone:gone"),
                timing(1, Runner::Local),
            ),
        ]);

        let times = built.resolve(&recorded).expect("timed");

        assert_eq!(times.get(configured).unwrap().own.total(), share(1, 9));
        assert_eq!(held(&times, package), Some(share(1, 9)));
    }

    #[test]
    fn coverage_counts_the_profiled_actions_landing_on_a_node() {
        let mut built = Built::default();
        let (_, _, bin) = built.target("app", "bin");
        built.action("bazel-out/bin/app/bin", bin);
        let run = timing(1, Runner::Local);
        let recorded = timings(&[
            (Some("bazel-out/bin/app/bin"), None, run),
            (Some("bazel-out/bin/app/gen"), owner("//app:bin"), run),
            (Some("bazel-out/bin/gone/gone"), owner("//gone:gone"), run),
            (None, owner("//app:bin"), run),
            (None, owner("//app:bin"), run),
            (None, None, run),
        ]);

        let times = built.resolve(&recorded).expect("timed");

        assert_eq!(
            times.coverage(),
            Coverage {
                placed: 4,
                profiled: 6
            }
        );
    }

    #[test]
    fn each_kind_spans_the_least_to_the_most_time_its_nodes_hold() {
        let mut built = Built::default();
        let (app, _, bin) = built.target("app", "bin");
        let fast = built.action("bazel-out/fast", bin);
        let slow = built.action("bazel-out/slow", bin);
        let (lib_package, _, lib) = built.target("lib", "lib");
        let middling = built.action("bazel-out/middling", lib);
        let recorded = timings(&[
            (Some("bazel-out/fast"), None, timing(10, Runner::Local)),
            (Some("bazel-out/slow"), None, timing(1_000, Runner::Local)),
            (Some("bazel-out/middling"), None, timing(100, Runner::Local)),
        ]);

        let times = built.resolve(&recorded).expect("timed");
        let place = |kind, id| {
            let extent = times.extent(kind).expect("timed kind");
            extent.place(times.get(id).unwrap().held.total().wall)
        };

        assert_eq!(place(Kind::Action, fast), 0.0);
        assert_eq!(place(Kind::Action, slow), 1.0);
        assert!((place(Kind::Action, middling) - 0.5).abs() < 1e-6);
        assert_eq!(place(Kind::ConfiguredTarget, bin), 1.0);
        assert_eq!(place(Kind::ConfiguredTarget, lib), 0.0);
        assert_eq!(place(Kind::Package, app), 1.0);
        assert_eq!(place(Kind::Package, lib_package), 0.0);
        assert_eq!(times.extent(Kind::Source), None);
        let once = Duration::from_millis(3);
        assert_eq!(
            Extent::of(once).place(once),
            1.0,
            "one time places at the most"
        );
        let mut instant = Extent::of(Duration::ZERO);
        instant.widen(Duration::from_secs(1));
        assert!(
            (instant.place(Duration::from_millis(31)) - 0.5).abs() < 0.01,
            "a time of none is placed as the shortest placed"
        );
    }

    #[test]
    fn a_node_ran_on_the_runner_holding_most_of_its_time_or_on_a_mix() {
        let spent = |actions: &[Timing]| {
            let mut spent = Spent::default();
            for action in actions {
                spent.add_action(*action);
            }
            spent
        };
        let remote = timing(70, Runner::RemoteExecution);
        let cached = timing(30, Runner::RemoteCacheHit);
        assert_eq!(Ran::of(&spent(&[remote])), Ran::On(Runner::RemoteExecution));
        assert_eq!(
            Ran::of(&spent(&[remote, cached])),
            Ran::On(Runner::RemoteExecution)
        );
        assert_eq!(
            Ran::of(&spent(&[remote, timing(70, Runner::Local)])),
            Ran::Mixed
        );
        let instant = |runner| timing(0, runner);
        assert_eq!(
            Ran::of(&spent(&[
                instant(Runner::NoSpawnRecorded),
                instant(Runner::NoSpawnRecorded),
                instant(Runner::Local)
            ])),
            Ran::On(Runner::NoSpawnRecorded),
            "weighed by count where no time was taken"
        );
        assert_eq!(Ran::of(&Spent::default()), Ran::Mixed);
    }

    /// The ids are handed out so that one container precedes what it holds
    /// and another follows it, and a target holds time of its own beside an
    /// action of its own.
    #[test]
    fn a_container_holds_its_own_time_and_all_it_contains_whatever_the_id_order() {
        let mut built = Built::default();
        let action = built.node("action:bazel-out/t.log", Kind::Action, None);
        let (package, target, configured) = built.target("tests", "check");
        built.graph.nodes[action.0 as usize].parent = Some(configured);
        let recorded = timings(&[
            (Some("bazel-out/t.log"), None, timing(20, Runner::Local)),
            (
                None,
                owner("//tests:check"),
                timing(4, Runner::DiskCacheHit),
            ),
        ]);

        let times = built.resolve(&recorded).expect("timed");

        assert_eq!(held(&times, action), Some(share(1, 20)));
        assert_eq!(times.get(configured).unwrap().own.total(), share(1, 4));
        for container in [configured, target, package] {
            assert_eq!(held(&times, container), Some(share(2, 24)));
        }
        let group = Id(built.names.len() as u32);
        assert_eq!(times.get(group), None);
    }
}
