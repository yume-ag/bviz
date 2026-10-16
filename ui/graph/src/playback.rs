//! Where the execution attempts a profile recorded stood at each instant,
//! placed on the nodes of a graph.

use crate::node_table::Slot;
use crate::status::Landings;
use bviz_graph_layout::{Graph, Id, Kind};
use bviz_invocation::{Attempt, Attempts, Done, EpochMicros, Phase, Venue};

/// Where a node stands at one instant, of the execution attempts landing on
/// it and on everything it contains.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum NodeState {
    /// None has started.
    Pending,
    /// Some have started and some have yet to end; this is the furthest stage
    /// any of them has entered.
    In(Stage),
    /// All have ended; cached where a cache answered every one.
    Done(Done),
}

impl NodeState {
    const FIRST_IN: usize = 1;
    const FIRST_DONE: usize = Self::FIRST_IN + Stage::COUNT;

    /// Every state, at its [`index`](Self::index).
    pub(crate) const ALL: [NodeState; Self::FIRST_DONE + 2] = [
        NodeState::Pending,
        NodeState::In(Stage::Preparing),
        NodeState::In(Stage::CheckingCache),
        NodeState::In(Stage::Scheduling),
        NodeState::In(Stage::Running(Running::Container)),
        NodeState::In(Stage::Running(Running::Action(Venue::Local))),
        NodeState::In(Stage::Running(Running::Action(Venue::Remote))),
        NodeState::Done(Done::Executed),
        NodeState::Done(Done::Cached),
    ];

    const fn index(self) -> usize {
        match self {
            NodeState::Pending => 0,
            NodeState::In(stage) => Self::FIRST_IN + stage.index(),
            NodeState::Done(Done::Executed) => Self::FIRST_DONE,
            NodeState::Done(Done::Cached) => Self::FIRST_DONE + 1,
        }
    }

    /// The name a reader is shown the state by.
    pub(crate) fn label(self) -> &'static str {
        match self {
            NodeState::Pending => "pending",
            NodeState::In(Stage::Preparing) => "preparing",
            NodeState::In(Stage::CheckingCache) => "checking cache",
            NodeState::In(Stage::Scheduling) => "scheduling",
            NodeState::In(Stage::Running(Running::Container)) => "running",
            NodeState::In(Stage::Running(Running::Action(Venue::Local))) => "running locally",
            NodeState::In(Stage::Running(Running::Action(Venue::Remote))) => "running remotely",
            NodeState::Done(Done::Executed) => "done, executed",
            NodeState::Done(Done::Cached) => "done, cached",
        }
    }
}

const _: () = {
    let mut index = 0;
    while index < NodeState::ALL.len() {
        assert!(NodeState::ALL[index].index() == index);
        index += 1;
    }
};

/// A set of [`NodeState`]s, a bit per [`NodeState::index`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct States(u16);

const _: () = assert!(NodeState::ALL.len() <= u16::BITS as usize);

impl States {
    fn with(self, state: NodeState) -> Self {
        States(self.0 | 1 << state.index())
    }

    pub(crate) fn union(self, other: States) -> Self {
        States(self.0 | other.0)
    }

    pub(crate) fn contains(self, state: NodeState) -> bool {
        self.0 & 1 << state.index() != 0
    }
}

/// The [`Phase`] a node's attempts have come to, running read as the node's
/// box runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Stage {
    Preparing,
    CheckingCache,
    Scheduling,
    Running(Running),
}

impl Stage {
    const COUNT: usize = 6;

    const fn index(self) -> usize {
        match self {
            Stage::Preparing => 0,
            Stage::CheckingCache => 1,
            Stage::Scheduling => 2,
            Stage::Running(Running::Container) => 3,
            Stage::Running(Running::Action(Venue::Local)) => 4,
            Stage::Running(Running::Action(Venue::Remote)) => 5,
        }
    }
}

/// How a node's box runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Running {
    /// An action's own box, on the venue its attempts spent longer running
    /// on, remote where they spent as long on each, for the whole profile so
    /// that it never moves from one to the other and back.
    Action(Venue),
    /// A box holding other nodes, whose actions each run on a venue of their
    /// own.
    Container,
}

impl Running {
    /// How a node of `kind`, whose attempts add up to `life`, runs.
    fn of(kind: Option<Kind>, life: &Life) -> Self {
        let spent = |venue| life.running[venue_slot(venue)];
        match kind {
            Some(Kind::Action) => {
                Running::Action(match spent(Venue::Local) > spent(Venue::Remote) {
                    true => Venue::Local,
                    false => Venue::Remote,
                })
            }
            _ => Running::Container,
        }
    }
}

/// The slot of `venue` in a table by venue.
const fn venue_slot(venue: Venue) -> usize {
    match venue {
        Venue::Local => 0,
        Venue::Remote => 1,
    }
}

/// What some attempts add up to over the whole profile.
#[derive(Clone, Copy, Debug)]
struct Life {
    start: EpochMicros,
    end: EpochMicros,
    /// By [`Phase::index`], when the first of them entered the phase.
    entered: [Option<EpochMicros>; Phase::ALL.len()],
    /// By [`venue_slot`], the microseconds they spent running there.
    running: [i64; Venue::ALL.len()],
    cached: bool,
}

impl Life {
    fn of(attempt: &Attempt) -> Self {
        let mut runs =
            Venue::ALL.map(|venue| Some((attempt.entered(Phase::Running(venue))?, venue)));
        // Of venues entered at one instant the attempt runs on the one the
        // venues' order puts last, as `Attempt::state_at` reads it.
        runs.sort_unstable();
        let mut runs = runs.into_iter().flatten().peekable();
        let mut running = [0; Venue::ALL.len()];
        while let Some((from, venue)) = runs.next() {
            let until = runs.peek().map_or(attempt.end(), |&(next, _)| next);
            running[venue_slot(venue)] += until.micros() - from.micros();
        }
        Life {
            start: attempt.start(),
            end: attempt.end(),
            entered: Phase::ALL.map(|phase| attempt.entered(phase)),
            running,
            cached: attempt.done() == Done::Cached,
        }
    }

    fn absorb(&mut self, other: &Life) {
        self.start = self.start.min(other.start);
        self.end = self.end.max(other.end);
        for (earliest, then) in self.entered.iter_mut().zip(other.entered) {
            *earliest = match (*earliest, then) {
                (Some(earlier), Some(then)) => Some(earlier.min(then)),
                (earlier, then) => earlier.or(then),
            };
        }
        for (spent, more) in self.running.iter_mut().zip(other.running) {
            *spent += more;
        }
        self.cached &= other.cached;
    }
}

/// When the first of a node's attempts began to run, and how its box runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Runs {
    from: EpochMicros,
    running: Running,
}

/// When the attempts one node holds first entered each phase and when the last
/// of them ended, which is all its state at any instant is read from.
#[derive(Debug, PartialEq)]
struct Held {
    start: EpochMicros,
    end: EpochMicros,
    checking_cache: Option<EpochMicros>,
    scheduling: Option<EpochMicros>,
    runs: Option<Runs>,
    done: Done,
}

impl Held {
    /// What `life` adds up to on a node of `kind`.
    fn of(kind: Option<Kind>, life: Life) -> Self {
        let entered = |phase: Phase| life.entered[phase.index()];
        let runs = Venue::ALL
            .into_iter()
            .filter_map(|venue| entered(Phase::Running(venue)))
            .min()
            .map(|from| Runs {
                from,
                running: Running::of(kind, &life),
            });
        Held {
            start: life.start,
            end: life.end,
            checking_cache: entered(Phase::CheckingCache),
            scheduling: entered(Phase::Scheduling),
            runs,
            done: match life.cached {
                true => Done::Cached,
                false => Done::Executed,
            },
        }
    }

    /// Each stage the node enters, with when it does, furthest last.
    fn stages(&self) -> impl DoubleEndedIterator<Item = (EpochMicros, Stage)> {
        [
            Some((self.start, Stage::Preparing)),
            self.checking_cache.map(|at| (at, Stage::CheckingCache)),
            self.scheduling.map(|at| (at, Stage::Scheduling)),
            self.runs
                .map(|runs| (runs.from, Stage::Running(runs.running))),
        ]
        .into_iter()
        .flatten()
    }

    fn at(&self, at: EpochMicros) -> NodeState {
        if at < self.start {
            return NodeState::Pending;
        }
        if at >= self.end {
            return NodeState::Done(self.done);
        }
        let furthest = self
            .stages()
            .filter(|&(entered, _)| entered <= at)
            .fold(Stage::Preparing, |_, (_, stage)| stage);
        NodeState::In(furthest)
    }

    /// Every state the node stands at for some time: pending, done, and each
    /// phase it enters before both its end and a further phase.
    fn reached(&self) -> States {
        let mut reached = States::default()
            .with(NodeState::Pending)
            .with(NodeState::Done(self.done));
        let mut until = self.end;
        for (entered, stage) in self.stages().rev() {
            if entered < until {
                reached = reached.with(NodeState::In(stage));
            }
            until = until.min(entered);
        }
        reached
    }
}

/// Where the execution attempts landing on every node of one graph stood at
/// each instant. Only the nodes an attempt lands on and those containing them
/// hold any, so a node's attempts are held apart from the dense table finding
/// them.
#[derive(Debug, PartialEq)]
pub(crate) struct NodeStates {
    /// By node id, where in `held` the node's attempts are.
    slots: Vec<Option<Slot>>,
    held: Vec<Held>,
}

impl NodeStates {
    /// Places the attempts `attempts` recorded on the nodes of `graph` that
    /// `landings` lands them on; `None` where none lands.
    pub(crate) fn resolve<'a>(
        landings: &Landings<'a>,
        graph: &Graph,
        attempts: &'a Attempts,
    ) -> Option<Self> {
        let recorded = attempts
            .actions()
            .map(|(_, _, tried)| tried.len())
            .sum::<usize>()
            + attempts
                .by_owner()
                .map(|(_, tried)| tried.len())
                .sum::<usize>();
        let mut landed: Vec<(Id, Life)> = Vec::with_capacity(recorded);
        for (primary_output, owner, tried) in attempts.actions() {
            if let Some(node) = landings.action(primary_output, owner) {
                landed.extend(tried.iter().map(|attempt| (node, Life::of(attempt))));
            }
        }
        for (owner, tried) in attempts.by_owner() {
            if let Some(node) = landings.landing(owner).node() {
                landed.extend(tried.iter().map(|attempt| (node, Life::of(attempt))));
            }
        }
        if landed.is_empty() {
            return None;
        }
        let mut parent = vec![None; landings.ids()];
        let mut kind = vec![None; landings.ids()];
        for node in &graph.nodes {
            parent[node.id.0 as usize] = node.parent;
            kind[node.id.0 as usize] = Some(node.kind);
        }
        let holders = |node: Id| std::iter::successors(Some(node), |held| parent[held.0 as usize]);

        let mut holding = vec![false; parent.len()];
        let mut holders_count = 0;
        for (node, _) in &landed {
            // A node already counted had every node containing it counted
            // with it.
            for holder in holders(*node) {
                let holds = &mut holding[holder.0 as usize];
                if *holds {
                    break;
                }
                *holds = true;
                holders_count += 1;
            }
        }
        let mut slots: Vec<Option<Slot>> = vec![None; parent.len()];
        let mut lives: Vec<(Option<Kind>, Life)> = Vec::with_capacity(holders_count);
        for (node, life) in &landed {
            for holder in holders(*node) {
                match slots[holder.0 as usize] {
                    Some(slot) => lives[slot.index()].1.absorb(life),
                    None => {
                        slots[holder.0 as usize] = Some(Slot::at(lives.len()));
                        lives.push((kind[holder.0 as usize], *life));
                    }
                }
            }
        }
        Some(NodeStates {
            slots,
            held: lives
                .into_iter()
                .map(|(kind, life)| Held::of(kind, life))
                .collect(),
        })
    }

    fn held(&self, id: Id) -> Option<&Held> {
        let slot = (*self.slots.get(id.0 as usize)?)?;
        Some(&self.held[slot.index()])
    }

    /// Where the node `id` names stands at `at`; `None` where no attempt lands
    /// on it or on anything it contains. An id past the graph's own, as a merge
    /// group's is, reads as such a node.
    pub(crate) fn at(&self, id: Id, at: EpochMicros) -> Option<NodeState> {
        Some(self.held(id)?.at(at))
    }

    /// Every state the node `id` names may stand at, none where no attempt
    /// lands on it or on anything it contains.
    pub(crate) fn reached(&self, id: Id) -> States {
        self.held(id).map_or(States::default(), Held::reached)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use bviz_graph_layout::graph::Node;
    use bviz_graph_layout::{Kind, Names};

    const ORIGIN_MS: i64 = 1_781_439_245_231;
    const LOCAL: Phase = Phase::Running(Venue::Local);
    const REMOTE: Phase = Phase::Running(Venue::Remote);
    const RUNNING_LOCALLY: Stage = Stage::Running(Running::Action(Venue::Local));
    const RUNNING_REMOTELY: Stage = Stage::Running(Running::Action(Venue::Remote));
    const RUNNING: Stage = Stage::Running(Running::Container);

    /// The instant `micros` past a fixed origin.
    pub(crate) fn at(micros: i64) -> EpochMicros {
        EpochMicros::from_millis(ORIGIN_MS).plus_micros(micros)
    }

    /// An attempt from [`at`] `start` to `end`, entering each phase `seen` at
    /// [`at`] its instant.
    pub(crate) fn attempt(start: i64, end: i64, seen: &[(Phase, i64)]) -> Attempt {
        Attempt::new(
            at(start),
            at(end),
            seen.iter().map(|&(phase, micros)| (phase, at(micros))),
        )
    }

    /// How far along `state` is, every way of running alike.
    fn progress(state: NodeState) -> usize {
        match state {
            NodeState::Pending => 0,
            NodeState::In(Stage::Preparing) => 1,
            NodeState::In(Stage::CheckingCache) => 2,
            NodeState::In(Stage::Scheduling) => 3,
            NodeState::In(Stage::Running(_)) => 4,
            NodeState::Done(_) => 5,
        }
    }

    /// The stage an action's own box stands at while its attempt is in `phase`.
    fn action_stage(phase: Phase) -> Stage {
        match phase {
            Phase::Preparing => Stage::Preparing,
            Phase::CheckingCache => Stage::CheckingCache,
            Phase::Scheduling => Stage::Scheduling,
            Phase::Running(venue) => Stage::Running(Running::Action(venue)),
        }
    }

    /// A package holding one configured target, which owns the actions
    /// writing each of `outputs`, and the attempts of each.
    struct Built {
        graph: Graph,
        names: Names,
        package: Id,
        target: Id,
        actions: Vec<Id>,
        attempts: Attempts,
    }

    impl Built {
        fn new(actions: &[(&str, &[Attempt])]) -> Self {
            let mut names = Names::default();
            let mut graph = Graph::new();
            let package = names.intern("pkg:app");
            graph.add_node(Node::new(package, "app", Kind::Package, None));
            let target = names.intern("target://app:bin");
            graph.add_node(Node::new(target, "bin", Kind::Target, Some(package)));
            let mut attempts = Attempts::with_capacity(actions.len());
            let actions = actions
                .iter()
                .map(|(output, tried)| {
                    let id = names.intern(&format!("action:{output}"));
                    graph.add_node(Node::new(id, *output, Kind::Action, Some(target)));
                    for attempt in *tried {
                        attempts.record(Some(output), None, *attempt);
                    }
                    id
                })
                .collect();
            Built {
                graph,
                names,
                package,
                target,
                actions,
                attempts,
            }
        }

        fn states(&self) -> NodeStates {
            let landings = Landings::new(&self.graph, &self.names, 0);
            NodeStates::resolve(&landings, &self.graph, &self.attempts).expect("landed")
        }
    }

    #[test]
    fn an_action_stands_where_its_attempt_does() {
        let compile = attempt(
            100,
            200,
            &[
                (Phase::CheckingCache, 110),
                (Phase::Scheduling, 120),
                (REMOTE, 130),
            ],
        );
        let built = Built::new(&[("bazel-out/a.o", &[compile])]);
        let states = built.states();
        let action = built.actions[0];
        for t in [0, 99, 100, 110, 125, 130, 199, 200, 300] {
            let expected = match compile.state_at(at(t)) {
                bviz_invocation::State::Pending => NodeState::Pending,
                bviz_invocation::State::In(phase) => NodeState::In(action_stage(phase)),
                bviz_invocation::State::Done(done) => NodeState::Done(done),
            };
            assert_eq!(states.at(action, at(t)), Some(expected), "at {t}");
        }
    }

    #[test]
    fn a_container_stands_at_the_furthest_phase_any_attempt_has_entered() {
        let built = Built::new(&[
            (
                "bazel-out/a.o",
                &[attempt(0, 100, &[(Phase::Scheduling, 10)])],
            ),
            (
                "bazel-out/b.o",
                &[attempt(20, 60, &[(Phase::CheckingCache, 30)])],
            ),
        ]);
        let states = built.states();
        for container in [built.target, built.package] {
            let state = |t| states.at(container, at(t));
            assert_eq!(state(-1), Some(NodeState::Pending));
            assert_eq!(state(5), Some(NodeState::In(Stage::Preparing)));
            assert_eq!(state(35), Some(NodeState::In(Stage::Scheduling)));
            assert_eq!(state(99), Some(NodeState::In(Stage::Scheduling)));
            assert_eq!(
                state(100),
                Some(NodeState::Done(Done::Executed)),
                "cached only where every attempt was"
            );
            assert!(
                !states
                    .reached(container)
                    .contains(NodeState::In(Stage::CheckingCache)),
                "a phase entered after a further one is never stood in"
            );
        }
    }

    /// A container whose attempts leave gaps between them never steps back:
    /// not to pending or preparing in a gap, nor to an earlier phase a later
    /// attempt enters, and it is done only once the last attempt has ended.
    #[test]
    fn a_container_only_moves_forward_through_the_gaps_between_its_attempts() {
        let built = Built::new(&[
            ("bazel-out/a.o", &[attempt(0, 10, &[(LOCAL, 2)])]),
            (
                "bazel-out/b.o",
                &[attempt(20, 30, &[(Phase::CheckingCache, 21)])],
            ),
            (
                "bazel-out/c.o",
                &[attempt(40, 50, &[(Phase::Scheduling, 41), (REMOTE, 45)])],
            ),
        ]);
        let states = built.states();
        for container in [built.target, built.package] {
            let swept: Vec<NodeState> = (-5..=60)
                .map(|t| states.at(container, at(t)).expect("held"))
                .collect();
            for (t, pair) in (-5..).zip(swept.windows(2)) {
                assert!(
                    progress(pair[0]) <= progress(pair[1]),
                    "stepped back from {:?} to {:?} at {}",
                    pair[0],
                    pair[1],
                    t + 1
                );
            }
            let state = |t| states.at(container, at(t));
            assert_eq!(state(15), Some(NodeState::In(RUNNING)));
            assert_eq!(state(45), Some(NodeState::In(RUNNING)));
            assert_eq!(state(49), Some(NodeState::In(RUNNING)));
            assert_eq!(state(50), Some(NodeState::Done(Done::Executed)));
            let seen = swept
                .iter()
                .fold(States::default(), |seen, &state| seen.with(state));
            assert_eq!(states.reached(container), seen);
        }
    }

    #[test]
    fn a_container_is_done_cached_only_where_a_cache_answered_every_attempt() {
        let hit = [attempt(0, 10, &[(Phase::CheckingCache, 1)])];
        let built = Built::new(&[("bazel-out/a.o", &hit), ("bazel-out/b.o", &hit)]);
        let state = built.states().at(built.package, at(10));
        assert_eq!(state, Some(NodeState::Done(Done::Cached)));
    }

    /// A container's actions each run on a venue of their own, so the
    /// container runs on none, while each action's box says where it runs.
    #[test]
    fn a_container_runs_on_no_venue_and_its_actions_on_their_own() {
        let built = Built::new(&[
            ("bazel-out/a.o", &[attempt(0, 100, &[(LOCAL, 10)])]),
            ("bazel-out/b.o", &[attempt(0, 100, &[(REMOTE, 20)])]),
        ]);
        let states = built.states();
        for container in [built.target, built.package] {
            assert_eq!(states.at(container, at(50)), Some(NodeState::In(RUNNING)));
            let reached = states.reached(container);
            for venue in [RUNNING_LOCALLY, RUNNING_REMOTELY] {
                assert!(!reached.contains(NodeState::In(venue)), "{venue:?}");
            }
        }
        let [local, remote] = [built.actions[0], built.actions[1]];
        assert_eq!(
            states.at(local, at(50)),
            Some(NodeState::In(RUNNING_LOCALLY))
        );
        assert_eq!(
            states.at(remote, at(50)),
            Some(NodeState::In(RUNNING_REMOTELY))
        );
    }

    #[test]
    fn an_action_runs_on_the_venue_its_attempts_ran_on_longer_throughout() {
        let rewound = [
            attempt(0, 30, &[(LOCAL, 10)]),
            attempt(40, 100, &[(REMOTE, 50)]),
        ];
        let built = Built::new(&[("bazel-out/a.o", &rewound)]);
        let states = built.states();
        let action = built.actions[0];
        for t in [10, 25, 35, 50, 99] {
            assert_eq!(
                states.at(action, at(t)),
                Some(NodeState::In(RUNNING_REMOTELY)),
                "at {t}"
            );
        }
        let reached = states.reached(action);
        assert!(!reached.contains(NodeState::In(RUNNING_LOCALLY)));

        let tied = [
            attempt(0, 50, &[(LOCAL, 10)]),
            attempt(50, 100, &[(REMOTE, 60)]),
        ];
        let built = Built::new(&[("bazel-out/a.o", &tied)]);
        assert_eq!(
            built.states().at(built.actions[0], at(20)),
            Some(NodeState::In(RUNNING_REMOTELY)),
            "a tie runs remotely"
        );
    }

    #[test]
    fn an_attempt_falling_back_to_a_venue_counts_its_time_there() {
        let fallback = attempt(0, 100, &[(REMOTE, 10), (LOCAL, 20)]);
        let built = Built::new(&[("bazel-out/a.o", &[fallback])]);
        let states = built.states();
        let action = built.actions[0];
        assert_eq!(
            states.at(action, at(15)),
            Some(NodeState::In(RUNNING_LOCALLY))
        );
        assert_eq!(
            states.at(action, at(50)),
            Some(NodeState::In(RUNNING_LOCALLY))
        );
    }

    #[test]
    fn a_rewound_action_holds_its_furthest_phase_until_its_last_attempt_ends() {
        let first = attempt(0, 10, &[(Phase::CheckingCache, 1)]);
        let again = attempt(50, 60, &[(REMOTE, 55)]);
        let built = Built::new(&[("bazel-out/a.o", &[first, again])]);
        let states = built.states();
        let action = built.actions[0];
        let state = |t| states.at(action, at(t));
        assert_eq!(state(5), Some(NodeState::In(Stage::CheckingCache)));
        assert_eq!(state(10), Some(NodeState::In(Stage::CheckingCache)));
        assert_eq!(state(52), Some(NodeState::In(Stage::CheckingCache)));
        assert_eq!(state(56), Some(NodeState::In(RUNNING_REMOTELY)));
        assert_eq!(state(60), Some(NodeState::Done(Done::Executed)));
        let reached = states.reached(action);
        for state in NodeState::ALL {
            let expected = matches!(
                state,
                NodeState::Pending
                    | NodeState::In(Stage::Preparing | Stage::CheckingCache | RUNNING_REMOTELY)
                    | NodeState::Done(Done::Executed)
            );
            assert_eq!(reached.contains(state), expected, "{state:?}");
        }
    }

    #[test]
    fn a_node_no_attempt_lands_under_has_no_state() {
        let mut built = Built::new(&[("bazel-out/a.o", &[attempt(0, 10, &[])])]);
        let unattempted = built.names.intern("action:bazel-out/unattempted");
        built.graph.add_node(Node::new(
            unattempted,
            "unattempted",
            Kind::Action,
            Some(built.target),
        ));
        let states = built.states();
        assert_eq!(states.at(unattempted, at(5)), None);
        assert_eq!(states.reached(unattempted), States::default());
        let group = Id(built.names.len() as u32);
        assert_eq!(states.at(group, at(5)), None);
        assert!(states.at(built.package, at(5)).is_some());
    }

    #[test]
    fn attempts_landing_on_no_node_place_nothing() {
        let built = Built::new(&[]);
        let mut attempts = Attempts::default();
        attempts.record(Some("bazel-out/gone"), None, attempt(0, 10, &[]));
        let landings = Landings::new(&built.graph, &built.names, 0);
        assert_eq!(
            NodeStates::resolve(&landings, &built.graph, &attempts),
            None
        );
    }
}
