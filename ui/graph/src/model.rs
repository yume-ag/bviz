use crate::GraphPanel;
use crate::actions::action_graph;
use crate::build::{build_graph, loaded_inputs};
use crate::chips::Completions;
use crate::critical::CriticalNodes;
use crate::derive::derive_levels;
use crate::filter::{Applied, DocumentFilter, Judgement};
use crate::fuzzy::{self, Matching};
use crate::jump::Candidates;
use crate::playback::NodeStates;
use crate::point::ConfigNames;
use crate::report::Report;
use crate::spawn_bytes::{self, NodeBytes, Unsized};
use crate::stand_in::Writers;
use crate::status::{Landings, Nodes, Statuses};
use crate::timecard::Timecard;
use crate::timing::{NodeTimes, Untimed, landings};
use bviz_graph_layout::dominators::{Dominators, Roots, ScopeGraph};
use bviz_graph_layout::graph::Graph;
use bviz_graph_layout::lod::{DEFAULT_MAX_EDGES, DEFAULT_MAX_NODES, Overflow};
use bviz_graph_layout::metrics::{Measurement, Scope, measure};
use bviz_graph_layout::query_lang::{
    Evaluation, Exclusion, Outcome, Prefixes, Readable, Stack, Universe,
};
use bviz_graph_layout::select;
use bviz_graph_layout::select::labels::LabelIndex;
use bviz_graph_layout::target_pattern::TargetIndex;
use bviz_graph_layout::{Capabilities, Facets, Id, InputSet, Joint, MapLayout, Names, Provenance};
use bviz_invocation::{ActionGraph, Chosen, Configurations, Revision, Session};
use bviz_ui_components::{Pending, load_in_background};
use bviz_ui_graph_args::GraphSource;
use bviz_ui_playback::Clock;
use bviz_ui_session::{Reports, Sharing};
use gpui::{App, Context, Entity, EntityId, EventEmitter, SharedString, Subscription, Task};
use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::marker::PhantomData;
use std::ops::Deref;
use std::sync::{Arc, LazyLock, OnceLock};

/// The graph a document holds, as it was read: whole, unfiltered, and shared by
/// every panel drawing it. Cheap to clone into a panel's background derivation,
/// which restricts it to what that panel's filter draws.
#[derive(Clone)]
pub(crate) struct Shared {
    /// The built (pre-quotient) graph, cached so toggling a group re-lays-out
    /// without re-reading or re-parsing the proto.
    pub(crate) base: Arc<Graph>,
    /// The base with its transitively-redundant edges removed, cached so the
    /// reduced view's edges are a stable projection of one fixed skeleton.
    pub(crate) reduced_base: Arc<Graph>,
    /// The placement every view of the whole graph is drawn in.
    pub(crate) map: Placement,
    /// The interned node-id table the base graph keys on, of which a panel's
    /// drawing takes a copy to extend with the group ids its quotient pass
    /// mints.
    pub(crate) names: Arc<Names>,
    /// The facets of the base graph's nodes, keyed by the same ids, which a
    /// panel's filter selects on.
    pub(crate) facets: Arc<Facets>,
    /// The canonical labels those nodes belong to, gathered once for every
    /// index over them.
    pub(crate) labels: Arc<LabelIndex>,
    /// The multi-child containers of the base graph, which a layered view offers
    /// to open.
    pub(crate) containers: Arc<HashSet<Id>>,
    /// Which filter axes the inputs this graph was read from support, by which
    /// the toolbar tells a control the inputs cannot answer from one they can.
    pub(crate) capabilities: Capabilities,
    /// The inputs that built each node of the base graph.
    pub(crate) provenance: Arc<Provenance>,
    /// The inputs the graph was read from.
    pub(crate) inputs: InputSet,
    /// How the configurations the facets name are named apart.
    pub(crate) config_names: Arc<ConfigNames>,
}

/// A graph's map, landing after the rest of the read. Every clone shares the
/// one placement, so whatever holds one draws the map as soon as it lands.
#[derive(Clone)]
pub(crate) struct Placement(Arc<OnceLock<Arc<MapLayout>>>);

/// The one handle that lands a [`Placement`]'s map, consumed by landing it, so a
/// placement lands at most once.
pub(crate) struct Landing(Placement);

impl Placement {
    /// A placement whose map has yet to land, and the handle that lands it.
    pub(crate) fn pending() -> (Self, Landing) {
        let placement = Placement(Arc::default());
        (placement.clone(), Landing(placement))
    }

    /// The map, once it has landed.
    pub(crate) fn placed(&self) -> Option<&Arc<MapLayout>> {
        self.0.get()
    }
}

impl Landing {
    pub(crate) fn land(self, map: Arc<MapLayout>) {
        self.0.0.get_or_init(|| map);
    }
}

/// The view a panel opens a freshly read graph at.
#[derive(Clone)]
pub(crate) struct Opening {
    /// Container ids to expand, sized so the first drawing stays legible.
    pub(crate) expanded: HashSet<Id>,
    /// The size of that view where it is past the caps that bound it, absent
    /// where it fits.
    pub(crate) over_budget: Option<Overflow>,
}

/// A graph that finished loading, in the form every panel's derivation starts
/// from.
pub(crate) struct Loaded {
    pub(crate) shared: Shared,
    pub(crate) opening: Opening,
    /// What the graph's inputs state of the data some query functions read.
    pub(crate) readable: Readable,
    measures: Passes<Scope, Measurement>,
    /// What the build of the graph's invocation reports of its nodes.
    statuses: Statuses,
    /// What the build of the graph's invocation reported, as the card over
    /// the graph lists it.
    reported: Report,
    /// Which of its session's outcomes `statuses` and `reported` are read
    /// from: those of a session no build has reported to, until a report is
    /// taken up.
    taken_up: Revision,
    /// How long the actions the invocation's profile recorded took, placed on
    /// the graph's nodes.
    times: Result<NodeTimes, Untimed>,
    /// Where the actions the invocation's profile recorded spent their time,
    /// as the card over the graph lists it.
    timecard: Timecard,
    timed: Revision,
    /// What the spawns the invocation's execution log recorded read and
    /// wrote, placed on the graph's nodes.
    bytes: Result<NodeBytes, Unsized>,
    logged: Revision,
    /// Where the steps of the invocation's critical path land.
    critical: CriticalNodes,
    traced: Revision,
    /// Which of its session's choices of what to show of the critical path
    /// was last shown.
    chosen: Revision,
    dominators: Passes<TreeKey, Dominators>,
    scope_graphs: HashMap<Scope, Prepared<ScopeGraph>>,
    /// The graph's targets by repository, package and name.
    targets: Prepared<Arc<TargetIndex>>,
    /// The graph's nodes as the jump palette offers them.
    candidates: Prepared<Candidates>,
    /// What the query field offers to complete a word with.
    completions: Prepared<Completions>,
    /// The graph as a query reads it.
    universe: Prepared<Universe>,
    /// The actions writing each file, which a view of actions lands a file
    /// on.
    writers: Prepared<Arc<Writers>>,
    /// Where the execution attempts the invocation's profile recorded stood
    /// at each instant, placed on the graph's nodes; `None` where none lands.
    states: Option<NodeStates>,
    attempted: Revision,
}

impl Loaded {
    pub(crate) fn new(shared: Shared, opening: Opening) -> Self {
        let targets = Prepared::new(&shared, |shared| {
            let labels = Arc::clone(&shared.labels);
            let index = TargetIndex::of(labels, &shared.base, &shared.facets, &shared.capabilities);
            Arc::new(index)
        });
        let candidates = Prepared::new(&shared, Candidates::of);
        let offered = candidates.clone();
        let completions = Prepared::new(&shared, move |_| Completions::of(offered));
        let indexed = targets.clone();
        let universe = Prepared::new(&shared, move |shared| {
            let targets = Arc::clone(indexed.wait());
            Universe::with_targets(&shared.base, targets, &shared.facets, &shared.capabilities)
        });
        let writers = Prepared::new(&shared, |shared| Arc::new(Writers::of(&shared.base)));
        Self {
            targets,
            candidates,
            completions,
            universe,
            writers,
            readable: Readable::of(&shared.base, &shared.capabilities),
            shared,
            opening,
            measures: Passes::keeping_all(),
            statuses: Statuses::default(),
            reported: Report::default(),
            taken_up: Revision::default(),
            times: Err(Untimed::NoProfile),
            timecard: Timecard::default(),
            timed: Revision::default(),
            bytes: Err(Unsized::NoExecLog),
            logged: Revision::default(),
            critical: CriticalNodes::default(),
            traced: Revision::default(),
            chosen: Revision::default(),
            dominators: Passes::keeping_recent(RECENT_TREES),
            scope_graphs: HashMap::new(),
            states: None,
            attempted: Revision::default(),
        }
    }

    pub(crate) fn statuses(&self) -> &Statuses {
        &self.statuses
    }

    pub(crate) fn reported(&self) -> &Report {
        &self.reported
    }

    /// Which of its session's outcomes the [statuses](Self::statuses) say
    /// that of, so that a reader keeping it can tell whether they are still
    /// those it read.
    pub(crate) fn taken_up(&self) -> Revision {
        self.taken_up
    }

    /// Which of its session's timings the [times](Self::times) and the
    /// [timecard](Self::timecard) are read from, as
    /// [`taken_up`](Self::taken_up) is for outcomes.
    pub(crate) fn timed(&self) -> Revision {
        self.timed
    }

    pub(crate) fn times(&self) -> Result<&NodeTimes, Untimed> {
        self.times.as_ref().map_err(|untimed| *untimed)
    }

    pub(crate) fn timecard(&self) -> &Timecard {
        &self.timecard
    }

    /// Which of its session's logged spawns the [bytes](Self::bytes) are
    /// read from, as [`taken_up`](Self::taken_up) is for outcomes.
    pub(crate) fn logged(&self) -> Revision {
        self.logged
    }

    pub(crate) fn bytes(&self) -> Result<&NodeBytes, Unsized> {
        self.bytes.as_ref().map_err(|why| *why)
    }

    /// Which of its session's attempts the [states](Self::states) are read
    /// from, as [`taken_up`](Self::taken_up) is for outcomes.
    pub(crate) fn attempted(&self) -> Revision {
        self.attempted
    }

    pub(crate) fn states(&self) -> Option<&NodeStates> {
        self.states.as_ref()
    }

    pub(crate) fn critical(&self) -> &CriticalNodes {
        &self.critical
    }

    /// Which of a session's reports of the critical path
    /// [`critical`](Self::critical) was resolved from.
    pub(crate) fn traced(&self) -> Revision {
        self.traced
    }

    /// Takes up what the build, the profile and the execution log of
    /// `session` report of the graph's nodes, and says whether that changed
    /// anything a view of the graph shows. Each is read again only once it is
    /// reported anew.
    fn report(&mut self, session: &Session) -> bool {
        let outcomes = self.take_up_outcomes(session);
        let timings = self.take_up_timings(session);
        let attempts = self.take_up_attempts(session);
        let critical = self.take_up_critical_path(session);
        let bytes = self.take_up_logged_spawns(session);
        outcomes || timings || attempts || critical || bytes
    }

    fn take_up_logged_spawns(&mut self, session: &Session) -> bool {
        let revision = session.logged_spawns_revision();
        if self.logged == revision {
            return false;
        }
        self.logged = revision;
        let base = &self.shared.base;
        let bytes = match session.logged_spawns() {
            Some(logged) => {
                let landings = spawn_bytes::landings(base, &self.shared.names, logged);
                NodeBytes::resolve(&landings, base, logged)
            }
            None => Err(Unsized::NoExecLog),
        };
        let changed = self.bytes != bytes;
        self.bytes = bytes;
        changed
    }

    fn take_up_attempts(&mut self, session: &Session) -> bool {
        let revision = session.attempts_revision();
        if self.attempted == revision {
            return false;
        }
        self.attempted = revision;
        let attempts = session.attempts();
        let owners = attempts.actions().len() + attempts.by_owner().count();
        let base = &self.shared.base;
        let landings = Landings::new(base, &self.shared.names, owners);
        let states = NodeStates::resolve(&landings, base, attempts);
        let changed = self.states != states;
        self.states = states;
        changed
    }

    /// Takes the choice of the critical path `session` holds as one shown:
    /// one made before the graph landed is no choice to show it.
    fn pass_over_choice(&mut self, session: &Session) {
        self.chosen = session.chosen_revision();
    }

    /// The node of what `session` holds chosen of the critical path, where it
    /// was chosen since this last asked and lands on the graph.
    fn take_up_choice(&mut self, session: &Session) -> Option<Id> {
        let revision = session.chosen_revision();
        if self.chosen == revision {
            return None;
        }
        self.chosen = revision;
        match session.chosen()? {
            Chosen::Step(index) => self.critical.step(index),
            Chosen::NearlyCritical(index) => self.critical.nearly_critical(index),
        }
    }

    fn take_up_critical_path(&mut self, session: &Session) -> bool {
        let revision = session.critical_path_revision();
        if self.traced == revision {
            return false;
        }
        self.traced = revision;
        let shared = &self.shared;
        let critical = CriticalNodes::resolve(
            &shared.base,
            &shared.names,
            session.critical_path(),
            session.nearly_critical(),
        );
        let changed = self.critical != critical;
        self.critical = critical;
        changed
    }

    fn take_up_timings(&mut self, session: &Session) -> bool {
        let revision = session.timings_revision();
        if self.timed == revision {
            return false;
        }
        self.timed = revision;
        let timings = session.timings();
        let base = &self.shared.base;
        let landings = landings(base, &self.shared.names, timings);
        let times = NodeTimes::resolve(&landings, base, timings);
        let timecard = Timecard::of(timings, &landings);
        let changed = self.times != times || self.timecard != timecard;
        self.times = times;
        self.timecard = timecard;
        changed
    }

    fn take_up_outcomes(&mut self, session: &Session) -> bool {
        let revision = session.outcomes_revision();
        if self.taken_up == revision {
            return false;
        }
        self.taken_up = revision;
        let outcomes = session.outcomes();
        let (statuses, reported) = match outcomes.is_empty() {
            true => Default::default(),
            false => {
                let base = &self.shared.base;
                let nodes = Nodes::new(base, &self.shared.names, outcomes);
                (Statuses::resolve(&nodes, base), Report::of(&nodes, base))
            }
        };
        let changed = self.statuses != statuses || self.reported != reported;
        self.statuses = statuses;
        self.reported = reported;
        changed
    }

    /// The points of `scope` and the dependencies between them.
    fn scope_graph(&mut self, scope: Scope) -> Prepared<ScopeGraph> {
        let prepare = move |shared: &Shared| {
            let (base, names, facets) = (&shared.base, &shared.names, &shared.facets);
            ScopeGraph::new(base, names, facets, &shared.provenance, scope)
        };
        (self.scope_graphs.entry(scope))
            .or_insert_with(|| Prepared::new(&self.shared, prepare))
            .clone()
    }
}

/// The mnemonic `facets` gives each configuration, by its checksum. A
/// configuration the graph's inputs state no mnemonic for is left out.
fn configurations(facets: &Facets) -> Configurations {
    let strings = facets.strings();
    let configs = facets.configs();
    let mut described = Configurations::with_capacity(configs.size_hint().0);
    for (config, entry) in configs {
        if let Some(mnemonic) = entry.mnemonic {
            described.insert(
                strings.resolve(config.sym()).to_owned(),
                strings.resolve(mnemonic).to_owned(),
            );
        }
    }
    described
}

/// How many dominator trees no view wants are kept for a view coming back to
/// them, beside those wanted.
const RECENT_TREES: usize = 4;

/// What a reader takes from a loaded graph, prepared the first time it is
/// read and shared by every reader after.
pub(crate) struct Prepared<T>(Arc<Preparing<T>>);

struct Preparing<T> {
    shared: Shared,
    prepared: LazyLock<T, Box<dyn FnOnce() -> T + Send>>,
}

impl<T> Prepared<T> {
    /// What `prepare` takes from `shared`.
    fn new(shared: &Shared, prepare: impl FnOnce(&Shared) -> T + Send + 'static) -> Self {
        let from = shared.clone();
        Self(Arc::new(Preparing {
            shared: shared.clone(),
            prepared: LazyLock::new(Box::new(move || prepare(&from))),
        }))
    }

    /// What was prepared, prepared here where no reader has prepared it yet,
    /// and waited for where another is preparing it; so read only off the UI
    /// thread.
    pub(crate) fn wait(&self) -> &T {
        &self.0.prepared
    }

    /// Waits as [`Self::wait`] does, then yields a handle that reads the value
    /// without waiting; call it only off the UI thread.
    pub(crate) fn ready(self) -> Ready<T> {
        self.wait();
        Ready(self.0)
    }

    /// The graph it was taken from.
    pub(crate) fn shared(&self) -> &Shared {
        &self.0.shared
    }
}

impl<T> Clone for Prepared<T> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

/// What a [`Prepared`] holds, once prepared, which reading never waits on.
pub(crate) struct Ready<T>(Arc<Preparing<T>>);

impl<T> Ready<T> {
    /// The graph it was taken from.
    pub(crate) fn shared(&self) -> &Shared {
        &self.0.shared
    }
}

impl<T> Deref for Ready<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0.prepared
    }
}

impl<T> Clone for Ready<T> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

/// What a graph keeps each of its dominator trees by: all that a tree is a
/// function of.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub(crate) struct TreeKey {
    pub(crate) scope: Scope,
    pub(crate) roots: Roots,
}

/// Whether a graph's layered view starts out drawing the transitive reduction of
/// its edges rather than every edge. The opening expansion is sized against the
/// edges that view draws, so the two are read from one place.
pub(crate) const OPENS_REDUCED: bool = true;

/// A read that has landed but for its map, which is still being placed.
pub(crate) struct Read {
    loaded: Loaded,
    placing: Pending<Arc<MapLayout>>,
    /// Lands what `placing` places in the read's [`Placement`].
    landing: Landing,
}

/// Reads `source` and builds the graph, and starts placing its map.
pub(crate) fn build(source: GraphSource) -> Result<Read, String> {
    let inputs = loaded_inputs(&source);
    let (
        Joint {
            graph,
            names,
            facets,
            capabilities,
            provenance,
        },
        reduced_base,
    ) = build_graph(&source.inputs())?;
    // The map derives each of its views against these, so it holds a share in
    // them rather than a copy.
    let (base, reduced_base) = (Arc::new(graph), Arc::new(reduced_base));
    let (map, landing) = Placement::pending();
    let placing = {
        let (base, reduced_base) = (Arc::clone(&base), Arc::clone(&reduced_base));
        Pending::start(move || Ok(Arc::new(bviz_graph_layout::map_layout(base, reduced_base))))
    };
    // A graph no expansion of which fits still has to open at something, and the
    // unexpanded view is the smallest drawing of it there is.
    let (expanded, over_budget) = match bviz_graph_layout::lod::default_expansion(
        &base,
        OPENS_REDUCED.then_some(&reduced_base),
        DEFAULT_MAX_NODES,
        DEFAULT_MAX_EDGES,
    ) {
        Ok(expanded) => (expanded, None),
        Err(overflow) => (HashSet::new(), Some(overflow)),
    };
    let containers = Arc::new(bviz_graph_layout::lod::expandable_containers(&base));
    let labels = Arc::new(LabelIndex::of(&base, &names, &facets));
    let config_names = Arc::new(ConfigNames::of(&facets));
    Ok(Read {
        loaded: Loaded::new(
            Shared {
                base,
                reduced_base,
                map,
                names: Arc::new(names),
                facets: Arc::new(facets),
                labels,
                containers,
                capabilities,
                provenance: Arc::new(provenance),
                inputs,
                config_names,
            },
            Opening {
                expanded,
                over_budget,
            },
        ),
        placing,
        landing,
    })
}

pub(crate) type Source = bviz_ui_components::Source<Loaded>;

/// The read has landed, loaded or failed, leaving nothing selected. Carried
/// as an event because what it calls for turns on each view's own state.
pub(crate) struct SourceReplaced;

/// The loaded graph's map has landed in its [`Placement`].
pub(crate) struct MapPlaced;

/// A view of the graph named a node for the graph panel it defers to to show.
pub(crate) struct RevealRequested {
    pub(crate) panel: EntityId,
    pub(crate) node: Id,
    pub(crate) reveal: Reveal,
}

/// How a node is revealed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Reveal {
    /// The node's own box is drawn, opening what holds it, and the panel is
    /// brought forward with its pane made the active one.
    Activate,
    /// The node is shown as [`crate::reveal::Ask::InPlace`] asks, and the panel
    /// keeps its place among its pane's.
    InPlace,
    /// The paths that put the node in the build are drawn, and the panel is
    /// brought forward as for [`Self::Activate`].
    AskWhy,
}

/// The graph panel the graph's other views defer to.
struct Deferred {
    panel: EntityId,
    /// Forgets the panel once it is released.
    _released: Subscription,
}

/// The loaded graph's pass yielding a `T` for `key` has landed.
pub(crate) struct Landed<K, T> {
    pub(crate) key: K,
    _pass: PhantomData<fn() -> T>,
}

pub(crate) type MetricsLanded = Landed<Scope, Measurement>;
pub(crate) type DominatorsLanded = Landed<TreeKey, Dominators>;

/// The graph's views have another [`GraphModel::filter`] to draw through.
pub(crate) struct FilterChanged {
    /// Whether the change named the part of the graph to look at, which a
    /// view drawing it moves onto.
    pub(crate) frame: bool,
}

/// The verdicts [`GraphModel::verdicts`] was asked for have landed.
pub(crate) struct VerdictsLanded;

/// Another node, or none, is [`GraphModel::selected`]. A view marks what it
/// selects itself as it publishes it, so it takes up only a change some other
/// view made.
pub(crate) struct SelectionChanged {
    /// The view that made the selection.
    pub(crate) by: EntityId,
}

/// What leaves a node out of the applied query has been named, as
/// [`GraphModel::exclusion`] reads.
pub(crate) struct ExclusionNamed;

/// The chips staged in [`GraphModel::query`] changed.
pub(crate) struct QueryChanged;

/// The chips staged have been counted, as [`GraphModel::counted`] reads.
pub(crate) struct QueryCounted;

/// A query asked to be applied was not, for the reason it carries.
pub(crate) struct QueryRefused {
    /// The query bar that asked for the apply.
    pub(crate) by: EntityId,
    pub(crate) why: SharedString,
}

/// The query bar `by` asked for has been applied, whether or not that changed
/// the filter.
pub(crate) struct QueryApplied {
    pub(crate) by: EntityId,
}

/// A pass over the whole of a graph, which is only taken once a view drawing
/// what it yields asks for it.
enum Pass<T> {
    Running {
        /// Dropping it gives the pass up: one not yet started never runs, and
        /// one running lands nothing.
        _work: Task<()>,
    },
    Done {
        yielded: Arc<T>,
        /// When it was last wanted or landed, on its [`Passes`]' clock.
        used: u64,
    },
}

/// A graph's passes of one kind, by what each passes over. Goes with the
/// graph they pass over, so a pass still running when the graph is replaced
/// never lands.
struct Passes<K, T> {
    by_key: HashMap<K, Pass<T>>,
    /// How many passes done that are no longer wanted are kept, those used
    /// last.
    kept: usize,
    /// Counts each time the passes are wanted or one lands.
    clock: u64,
}

impl<K: Clone + Eq + Hash, T> Passes<K, T> {
    fn keeping_all() -> Self {
        Self::keeping_recent(usize::MAX)
    }

    fn keeping_recent(kept: usize) -> Self {
        Self {
            by_key: HashMap::new(),
            kept,
            clock: 0,
        }
    }

    fn done(&self, key: &K) -> Option<&Arc<T>> {
        match self.by_key.get(key)? {
            Pass::Done { yielded, .. } => Some(yielded),
            Pass::Running { .. } => None,
        }
    }

    fn land(&mut self, key: K, yielded: Arc<T>) {
        self.clock += 1;
        let used = self.clock;
        self.by_key.insert(key, Pass::Done { yielded, used });
    }

    /// Drops each pass running for a key `wanted` leaves out, and of those
    /// done for such a key, all but the [`Self::kept`] used last.
    fn forget_unwanted(&mut self, wanted: &[K]) {
        self.clock += 1;
        for key in wanted {
            if let Some(Pass::Done { used, .. }) = self.by_key.get_mut(key) {
                *used = self.clock;
            }
        }
        let mut idle: Vec<(u64, K)> = Vec::with_capacity(self.by_key.len());
        self.by_key.retain(|key, pass| match pass {
            _ if wanted.contains(key) => true,
            Pass::Running { .. } => false,
            Pass::Done { used, .. } => {
                idle.push((*used, key.clone()));
                true
            }
        });
        idle.sort_unstable_by_key(|&(used, _)| std::cmp::Reverse(used));
        for (_, key) in idle.into_iter().skip(self.kept) {
            self.by_key.remove(&key);
        }
    }
}

/// A graph read for viewing, held apart from the panels drawing it so a view of
/// it can close and be shown again without the graph being read or laid out
/// afresh.
pub(crate) struct GraphModel {
    source: Source,
    /// The read in flight, held so that it is not cancelled.
    _work: Task<()>,
    /// The placement of the map in flight once the read has landed, held so
    /// that it is not cancelled.
    _placing: Task<()>,
    /// The graph panel opened or shown last, for as long as it is open.
    deferred: Option<Deferred>,
    /// What every view of the graph shows its share of it through.
    filter: DocumentFilter,
    /// `filter` as it stands over the loaded graph, evaluated once a view asks
    /// for its verdicts; absent while the graph is still being read.
    judgement: Option<Arc<Judgement>>,
    /// Announces the verdicts of `judgement` as they land, absent until a view
    /// asks for them. Replacing it announces nothing of the judgement it
    /// served, whose evaluation, once running, still runs to its end.
    judging: Option<Task<()>>,
    /// The chips the query bars stage, which applying makes the filter's
    /// scope.
    query: Stack,
    /// The values of the query's prefixes over the loaded graph, which only
    /// that graph's ids index.
    prefixes: Arc<Prefixes>,
    /// Applies the query committed last; replacing it gives up an apply still
    /// under way.
    _applying: Task<()>,
    /// The chips staged last counted over the loaded graph, and the stack
    /// they were as it was counted.
    counted: Option<(Stack, Arc<Evaluation>)>,
    /// Counts the chips staged, and the stack being counted; absent while
    /// none is.
    counting: Option<(Stack, Task<()>)>,
    /// What leaves each of the nodes last asked about out of the applied
    /// query, named or being named; at most [`NAMED_EXCLUSIONS`] of them,
    /// the oldest first.
    exclusions: Vec<ExclusionNaming>,
    selected: Selected,
    /// The matcher pool the lists over the graph — each panel's palette and
    /// each query bar's completion — take up in turn.
    matchers: fuzzy::Pool,
    in_session: InSession,
    /// The playback clock of the document the graph is opened in.
    clock: Entity<Clock>,
}

/// Takes up what `session` reports of the graph `source` loaded, saying whether
/// that changed anything a view of it shows, and the reveal of what was
/// chosen of the critical path since, which the panel the graph's other
/// views defer to, `deferred`, shows in place.
fn take_up_from(
    source: &mut Source,
    deferred: Option<&Deferred>,
    session: &Session,
) -> (bool, Option<RevealRequested>) {
    let Source::Loaded(loaded) = source else {
        return (false, None);
    };
    let changed = loaded.report(session);
    let reveal = loaded
        .take_up_choice(session)
        .zip(deferred)
        .map(|(node, deferred)| RevealRequested {
            panel: deferred.panel,
            node,
            reveal: Reveal::InPlace,
        });
    (changed, reveal)
}

/// Lands the map `placed` through `landing`, or fails `source` where the map
/// could not be placed.
fn land(
    source: &mut Source,
    placed: Result<Arc<MapLayout>, String>,
    landing: Landing,
) -> Option<Arc<MapLayout>> {
    match placed {
        Ok(map) => {
            landing.land(Arc::clone(&map));
            Some(map)
        }
        Err(err) => {
            *source = Source::Failed(err);
            None
        }
    }
}

/// How many nodes the document keeps named what leaves them out of the
/// applied query: one for each view's card, with room for a few more asked
/// about since.
const NAMED_EXCLUSIONS: usize = 8;

/// What leaves `node` out of the scope `stack` names, being named off the
/// UI thread until `named` lands.
struct ExclusionNaming {
    node: Id,
    stack: Stack,
    named: Option<Option<Exclusion>>,
    _work: Task<()>,
}

/// The graph's part in its document's session.
struct InSession {
    /// Held for as long as the graph is: a session nothing holds is released,
    /// and tells its observers nothing further.
    reports: Reports,
    /// Takes up what the build reports whenever the session changes.
    _changed: Subscription,
}

impl EventEmitter<SourceReplaced> for GraphModel {}
impl EventEmitter<MapPlaced> for GraphModel {}
impl EventEmitter<RevealRequested> for GraphModel {}
impl<K: 'static, T: 'static> EventEmitter<Landed<K, T>> for GraphModel {}
impl EventEmitter<FilterChanged> for GraphModel {}
impl EventEmitter<VerdictsLanded> for GraphModel {}
impl EventEmitter<QueryChanged> for GraphModel {}
impl EventEmitter<ExclusionNamed> for GraphModel {}
impl EventEmitter<QueryRefused> for GraphModel {}
impl EventEmitter<QueryApplied> for GraphModel {}
impl EventEmitter<QueryCounted> for GraphModel {}
impl EventEmitter<SelectionChanged> for GraphModel {}

/// The node of the loaded graph every view of it marks as the one selected.
#[derive(Default)]
struct Selected(Option<Id>);

impl Selected {
    /// Selects `node`, or nothing for `None`, reporting whether that changed
    /// what is selected.
    fn set(&mut self, node: Option<Id>) -> bool {
        std::mem::replace(&mut self.0, node) != node
    }
}

impl GraphModel {
    /// The graph `load` is reading, viewable once the read finishes and failed
    /// where it does not.
    pub(crate) fn reading(
        load: Pending<Read>,
        session: Entity<Session>,
        clock: Entity<Clock>,
        sharing: Sharing,
        cx: &mut Context<Self>,
    ) -> Self {
        // The build's outcomes can land on either side of the graph, so they
        // are taken up as the graph lands and again whenever the session
        // changes.
        let in_session = InSession {
            _changed: cx.observe(&session, |model, _, cx| model.take_up(cx)),
            reports: Reports::new(session, cx),
        };
        let shares = sharing == Sharing::Invocation;
        let work = load_in_background(
            cx,
            move || {
                let read = load.join()?;
                let shared = &read.loaded.shared;
                let described = configurations(&shared.facets);
                let actions = shares
                    .then(|| action_graph(&shared.base, &shared.names))
                    .flatten();
                Ok((read, described, actions))
            },
            move |model,
                  result: Result<(Read, Configurations, Option<ActionGraph>), String>,
                  cx| {
                model.source = match result {
                    Ok((mut read, described, actions)) => {
                        let reports = &model.in_session.reports;
                        read.loaded.pass_over_choice(reports.session().read(cx));
                        reports.report(cx, |session, by, cx| {
                            session.add_configurations(described);
                            if let Some(actions) = actions {
                                session.report_action_graph(by, actions);
                            }
                            cx.notify();
                        });
                        model._placing = Self::placing(read.placing, read.landing, cx);
                        Source::Loaded(read.loaded)
                    }
                    Err(err) => Source::Failed(err),
                };
                model.take_up(cx);
                model.judge();
                model.counted = None;
                model.counting = None;
                model.exclusions.clear();
                model.replaced(cx);
            },
        );
        Self::new(work, in_session, clock)
    }

    /// Takes up what the session's build reports of the loaded graph.
    fn take_up(&mut self, cx: &mut Context<Self>) {
        let session = self.in_session.reports.session().read(cx);
        let (changed, reveal) = take_up_from(&mut self.source, self.deferred.as_ref(), session);
        if changed {
            cx.notify();
        }
        if let Some(reveal) = reveal {
            cx.emit(reveal);
        }
    }

    /// Announces that the graph has been replaced, whose nodes no selection
    /// names any longer.
    fn replaced(&mut self, cx: &mut Context<Self>) {
        self.selected = Selected::default();
        cx.emit(SourceReplaced);
    }

    /// Waits on `placing` and lands the map it places through `landing`.
    fn placing(
        placing: Pending<Arc<MapLayout>>,
        landing: Landing,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        load_in_background(
            cx,
            move || placing.join(),
            |model, placed, cx| match land(&mut model.source, placed, landing) {
                Some(map) => {
                    derive_levels(map, cx.background_executor());
                    cx.emit(MapPlaced);
                }
                None => model.replaced(cx),
            },
        )
    }

    /// Lands what placing the loaded graph's map came to: the map, through
    /// `landing`, or a failed read where placing failed. Returns the map where
    /// it landed.
    fn new(work: Task<()>, in_session: InSession, clock: Entity<Clock>) -> Self {
        Self {
            source: Source::Loading,
            _work: work,
            _placing: Task::ready(()),
            deferred: None,
            filter: DocumentFilter::default(),
            judgement: None,
            judging: None,
            query: Stack::default(),
            prefixes: Arc::default(),
            _applying: Task::ready(()),
            counted: None,
            counting: None,
            exclusions: Vec::with_capacity(NAMED_EXCLUSIONS),
            selected: Selected::default(),
            matchers: fuzzy::Pool::default(),
            in_session,
            clock,
        }
    }

    /// Takes `panel` as the graph panel the graph's other views defer to.
    pub(crate) fn defer_to(&mut self, panel: &Entity<GraphPanel>, cx: &mut Context<Self>) {
        let id = panel.entity_id();
        if self
            .deferred
            .as_ref()
            .is_some_and(|deferred| deferred.panel == id)
        {
            return;
        }
        let released = cx.observe_release(panel, |model, _, cx| {
            model.deferred = None;
            cx.notify();
        });
        self.deferred = Some(Deferred {
            panel: id,
            _released: released,
        });
        cx.notify();
    }

    /// Whether a graph panel is open to show a node another view names.
    pub(crate) fn reveals(&self) -> bool {
        self.deferred.is_some()
    }

    /// What every view of the graph shows its share of it through.
    pub(crate) fn filter(&self) -> &DocumentFilter {
        &self.filter
    }

    /// `filter` as it stands over the loaded graph, whose verdicts are
    /// evaluated once for every view drawing through it.
    pub(crate) fn judgement(&self) -> Option<&Arc<Judgement>> {
        self.judgement.as_ref()
    }

    /// The verdicts of the filter as it stands, where they have landed; asks
    /// for them otherwise, emitting [`VerdictsLanded`] once they land. What
    /// they change is each view's own to redraw, so their landing notifies
    /// none.
    pub(crate) fn verdicts(&mut self, cx: &mut Context<Self>) -> Option<Arc<select::Verdicts>> {
        let judgement = self.judgement.as_ref()?;
        if let Some(verdicts) = judgement.evaluated() {
            return Some(Arc::clone(verdicts));
        }
        if self.judging.is_none() {
            let judgement = Arc::clone(judgement);
            self.judging = Some(cx.spawn(async move |model, cx| {
                let evaluating = async move {
                    judgement.verdicts();
                };
                cx.background_executor().spawn(evaluating).await;
                model.update(cx, |_, cx| cx.emit(VerdictsLanded)).ok();
            }));
        }
        None
    }

    /// Pins the nodes `pins` names, and no other.
    pub(crate) fn set_pins(&mut self, pins: HashMap<Id, select::Pin>, cx: &mut Context<Self>) {
        if self.filter.set_pins(pins) {
            self.filter_changed(false, cx);
        }
    }

    /// Takes up a change to the filter, `frame` saying whether it named the
    /// part of the graph to look at.
    fn filter_changed(&mut self, frame: bool, cx: &mut Context<Self>) {
        self.judge();
        cx.emit(FilterChanged { frame });
    }

    /// Makes the judgement of the filter as it stands over the loaded graph,
    /// where the one held judges another.
    fn judge(&mut self) {
        let Some(loaded) = self.loaded() else {
            return;
        };
        let (filter, base) = (self.filter.filter(), &loaded.shared.base);
        if (self.judgement.as_ref()).is_some_and(|judgement| judgement.judges(&filter, base)) {
            return;
        }
        self.judgement = Some(Arc::new(Judgement::new(filter, Arc::clone(base))));
        self.judging = None;
    }

    /// The chips staged, which the filter takes up once they are applied.
    pub(crate) fn query(&self) -> &Stack {
        &self.query
    }

    /// Runs `edit` on the chips staged, announcing what it changed.
    pub(crate) fn edit_query<R>(
        &mut self,
        edit: impl FnOnce(&mut Stack) -> R,
        cx: &mut Context<Self>,
    ) -> R {
        let before = self.query.clone();
        let edited = edit(&mut self.query);
        if self.query != before {
            cx.emit(QueryChanged);
        }
        edited
    }

    /// Stages the chips applied last again, taking back every edit made
    /// since; with none applied, none.
    pub(crate) fn revert_query(&mut self, cx: &mut Context<Self>) {
        let applied = self.filter.applied().map(|applied| applied.stack().clone());
        self.edit_query(|query| *query = applied.unwrap_or_default(), cx);
    }

    /// Counts what the chips staged name off the UI thread, once for every
    /// view showing them, emitting [`QueryCounted`] as the count lands; does
    /// nothing where they are counted or being counted as they stand.
    pub(crate) fn count_query(&mut self, cx: &mut Context<Self>) {
        let stack = &self.query;
        let held = |held: &Stack| held == stack;
        if self
            .counted
            .as_ref()
            .is_some_and(|(counted, _)| held(counted))
            || self
                .counting
                .as_ref()
                .is_some_and(|(counting, _)| held(counting))
        {
            return;
        }
        let stack = stack.clone();
        // An empty stack names nothing, which needs no universe to know: a
        // graph nobody queries never prepares one.
        if stack.is_empty() {
            let nothing = Evaluation {
                chips: Vec::new(),
                value: None,
            };
            self.counting = None;
            self.counted = Some((stack, Arc::new(nothing)));
            cx.emit(QueryCounted);
            return;
        }
        let Some(universe) = self.universe() else {
            return;
        };
        let prefixes = Arc::clone(&self.prefixes);
        let counted = stack.clone();
        let task = load_in_background(
            cx,
            move || stack.evaluate(universe.wait(), &prefixes),
            move |model, evaluation, cx| {
                model.counting = None;
                model.counted = Some((counted, Arc::new(evaluation)));
                cx.emit(QueryCounted);
            },
        );
        self.counting = Some((self.query.clone(), task));
    }

    /// What the chips staged last counted name; of a stack since edited
    /// where the count of the stack as it stands has yet to land.
    pub(crate) fn counted(&self) -> Option<&Evaluation> {
        self.counted.as_ref().map(|(_, evaluation)| &**evaluation)
    }

    /// Names off the UI thread what leaves `node` out of the applied query,
    /// emitting [`ExclusionNamed`] once it is named; does nothing where it is
    /// named or being named, or where no query is applied.
    pub(crate) fn name_exclusion(&mut self, node: Id, cx: &mut Context<Self>) {
        let Some(applied) = self.filter.applied() else {
            return;
        };
        let stack = applied.stack();
        self.exclusions.retain(|held| held.stack == *stack);
        if self.exclusions.iter().any(|held| held.node == node) {
            return;
        }
        let Some(universe) = self.universe() else {
            return;
        };
        let (prefixes, named) = (self.prefixes(), stack.clone());
        let work = cx.spawn(async move |model, cx| {
            let naming = async move { named.excluded_by(universe.wait(), &prefixes, node) };
            let exclusion = cx.background_executor().spawn(naming).await;
            let _ = model.update(cx, |model, cx| {
                let named = model.exclusions.iter_mut().find(|held| held.node == node);
                if let Some(held) = named {
                    held.named = Some(exclusion);
                    cx.emit(ExclusionNamed);
                }
            });
        });
        if self.exclusions.len() == NAMED_EXCLUSIONS {
            self.exclusions.remove(0);
        }
        self.exclusions.push(ExclusionNaming {
            node,
            stack: stack.clone(),
            named: None,
            _work: work,
        });
    }

    /// What leaves `node` out of the applied query, with the applied stack
    /// naming it, where [`Self::name_exclusion`] has named it.
    pub(crate) fn exclusion(&self, node: Id) -> Option<(Exclusion, &Stack)> {
        let stack = self.filter.applied()?.stack();
        let held = self.exclusions.iter().find(|held| held.node == node)?;
        let named = held.named.flatten()?;
        (held.stack == *stack).then_some((named, stack))
    }

    /// A matching with no query, on the graph's matcher pool where no list
    /// holds it.
    pub(crate) fn take_matcher<S>(&mut self, cx: &App) -> Matching<S> {
        self.matchers.take(cx.background_executor())
    }

    /// Hands the pool of `matching` back for another list to take up.
    pub(crate) fn park_matcher<S>(&mut self, matching: Matching<S>) {
        self.matchers.park(matching);
    }

    /// The values of the query's prefixes over the loaded graph, shared by
    /// every evaluation of the query.
    pub(crate) fn prefixes(&self) -> Arc<Prefixes> {
        Arc::clone(&self.prefixes)
    }

    /// Scopes the filter to what the chips staged name, off the UI thread,
    /// giving up an apply asked for before; `by` is the query bar asking.
    pub(crate) fn apply_query(&mut self, by: EntityId, cx: &mut Context<Self>) {
        let stack = self.query.clone();
        if matches!(stack.compile(), Ok(None)) {
            self._applying = Task::ready(());
            self.scope_landed(stack, Ok(None), by, cx);
            return;
        }
        let Some(universe) = self.universe() else {
            return;
        };
        let prefixes = Arc::clone(&self.prefixes);
        self._applying = load_in_background(
            cx,
            move || {
                let scope = scope_of(&stack, universe.wait(), &prefixes);
                (stack, scope)
            },
            move |model, (stack, scope), cx| model.scope_landed(stack, scope, by, cx),
        );
    }

    fn scope_landed(
        &mut self,
        stack: Stack,
        scope: Result<Option<select::Scope>, SharedString>,
        by: EntityId,
        cx: &mut Context<Self>,
    ) {
        match scope {
            Ok(scope) => {
                cx.emit(QueryApplied { by });
                let applied = scope.map(|scope| Applied::new(stack, scope));
                if self.filter.set_applied(applied) {
                    self.filter_changed(true, cx);
                }
            }
            Err(why) => cx.emit(QueryRefused { by, why }),
        }
    }

    /// Has the loaded graph measured off the UI thread in each scope of
    /// `wanted`, as [`Self::want`] does.
    pub(crate) fn want_metrics(&mut self, wanted: &[Scope], cx: &mut Context<Self>) {
        let pass = |loaded: &mut Loaded, &scope: &Scope| {
            let shared = loaded.shared.clone();
            move || {
                let (base, names, facets) = (&shared.base, &shared.names, &shared.facets);
                measure(base, names, facets, &shared.provenance, scope)
            }
        };
        self.want(wanted, |loaded| &mut loaded.measures, pass, cx);
    }

    /// The loaded graph's metrics in `scope`, once measured.
    pub(crate) fn metrics(&self, scope: Scope) -> Option<&Arc<Measurement>> {
        self.loaded()?.measures.done(&scope)
    }

    /// Has the loaded graph's dominator tree computed off the UI thread for
    /// each key of `wanted`, as [`Self::want`] does.
    pub(crate) fn want_dominators(&mut self, wanted: &[TreeKey], cx: &mut Context<Self>) {
        let pass = |loaded: &mut Loaded, key: &TreeKey| {
            let (prepared, roots) = (loaded.scope_graph(key.scope), key.roots.clone());
            move || {
                let shared = prepared.shared();
                (prepared.wait()).dominators(&shared.names, &shared.facets, &roots)
            }
        };
        self.want(wanted, |loaded| &mut loaded.dominators, pass, cx);
    }

    /// The loaded graph's dominator tree for `key`, once computed.
    pub(crate) fn dominators(&self, key: &TreeKey) -> Option<&Arc<Dominators>> {
        self.loaded()?.dominators.done(key)
    }

    /// The points of `scope` of the loaded graph and the dependencies between
    /// them, which the graph's dominator trees in `scope` share; absent while
    /// the graph is still being read.
    pub(crate) fn scope_graph(&mut self, scope: Scope) -> Option<Prepared<ScopeGraph>> {
        match &mut self.source {
            Source::Loaded(loaded) => Some(loaded.scope_graph(scope)),
            _ => None,
        }
    }

    /// The targets of the loaded graph, indexed for resolving a target set;
    /// absent while the graph is still being read.
    pub(crate) fn targets(&self) -> Option<Prepared<Arc<TargetIndex>>> {
        Some(self.loaded()?.targets.clone())
    }

    pub(crate) fn writers(&self) -> Option<Prepared<Arc<Writers>>> {
        Some(self.loaded()?.writers.clone())
    }

    pub(crate) fn candidates(&self) -> Option<Prepared<Candidates>> {
        Some(self.loaded()?.candidates.clone())
    }

    pub(crate) fn completions(&self) -> Option<Prepared<Completions>> {
        Some(self.loaded()?.completions.clone())
    }

    /// The loaded graph as a query reads it; absent while the graph is still
    /// being read.
    pub(crate) fn universe(&self) -> Option<Prepared<Universe>> {
        Some(self.loaded()?.universe.clone())
    }

    /// Runs the work `pass` makes of the loaded graph off the UI thread for
    /// each key of `wanted` not yet asked for, keeping what it yields among
    /// the passes `passes` picks out and emitting [`Landed`] as each lands,
    /// and gives up any other key's pass still running, which would only hold
    /// the workers from those wanted; of what other keys' passes yielded, the
    /// passes keep as many as they keep. Does nothing while the graph is still
    /// being read.
    fn want<K, T, W>(
        &mut self,
        wanted: &[K],
        passes: fn(&mut Loaded) -> &mut Passes<K, T>,
        pass: impl Fn(&mut Loaded, &K) -> W,
        cx: &mut Context<Self>,
    ) where
        K: Clone + Eq + Hash + 'static,
        T: Send + Sync + 'static,
        W: FnOnce() -> T + Send + 'static,
    {
        let Source::Loaded(loaded) = &mut self.source else {
            return;
        };
        passes(loaded).forget_unwanted(wanted);
        for key in wanted {
            if passes(loaded).by_key.contains_key(key) {
                continue;
            }
            let landing = key.clone();
            let work = load_in_background(cx, pass(loaded, key), move |model, done, cx| {
                if let Source::Loaded(loaded) = &mut model.source {
                    passes(loaded).land(landing.clone(), Arc::new(done));
                    cx.emit(Landed::<K, T> {
                        key: landing,
                        _pass: PhantomData,
                    });
                }
            });
            (passes(loaded).by_key).insert(key.clone(), Pass::Running { _work: work });
        }
    }

    /// The node every view of the graph marks as the one selected.
    pub(crate) fn selected(&self) -> Option<Id> {
        self.selected.0
    }

    /// Selects `node`, or nothing for `None`, for every view of the graph;
    /// `by` is the view selecting it. Selecting what is selected already
    /// announces nothing.
    pub(crate) fn select(&mut self, node: Option<Id>, by: EntityId, cx: &mut Context<Self>) {
        if self.selected.set(node) {
            cx.emit(SelectionChanged { by });
        }
    }

    /// Asks the graph panel the graph's other views defer to to show the node
    /// `id`, where one is open.
    pub(crate) fn reveal(&mut self, id: Id, cx: &mut Context<Self>) {
        self.request(id, Reveal::Activate, cx);
    }

    /// Asks the graph panel the graph's other views defer to why the node
    /// `id` is in the build, where one is open.
    pub(crate) fn ask_why(&mut self, id: Id, cx: &mut Context<Self>) {
        self.request(id, Reveal::AskWhy, cx);
    }

    fn request(&mut self, node: Id, reveal: Reveal, cx: &mut Context<Self>) {
        if let Some(deferred) = &self.deferred {
            cx.emit(RevealRequested {
                panel: deferred.panel,
                node,
                reveal,
            });
        }
    }

    pub(crate) fn clock(&self) -> &Entity<Clock> {
        &self.clock
    }

    pub(crate) fn source(&self) -> &Source {
        &self.source
    }

    /// The configurations known of the invocation the graph is opened as part
    /// of, or of the graph alone.
    pub(crate) fn configurations<'a>(&self, cx: &'a App) -> &'a Configurations {
        self.in_session.reports.session().read(cx).configurations()
    }

    pub(crate) fn loaded(&self) -> Option<&Loaded> {
        match &self.source {
            Source::Loaded(loaded) => Some(loaded),
            _ => None,
        }
    }
}

/// The scope `stack` names over `universe`, `None` where it names none; or
/// why it cannot be applied.
fn scope_of(
    stack: &Stack,
    universe: &Universe,
    prefixes: &Prefixes,
) -> Result<Option<select::Scope>, SharedString> {
    let compiled = match stack.compile() {
        Ok(None) => return Ok(None),
        Ok(Some(compiled)) => compiled,
        Err(invalid) => return Err(format!("not applied: {}", invalid.error.message).into()),
    };
    let evaluation = stack.evaluate(universe, prefixes);
    let Some(value) = &evaluation.value else {
        let failed = evaluation
            .chips
            .iter()
            .find_map(|(_, outcome)| match outcome {
                Outcome::Failed(error) => Some(error.message.clone()),
                _ => None,
            });
        let why = failed.unwrap_or_else(|| "a chip does not evaluate".to_string());
        return Err(format!("not applied: {why}").into());
    };
    Ok(Some(universe.scope(compiled.expr.to_string(), value)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bviz_graph_layout::facets::Tri;
    use bviz_graph_layout::graph::Node;
    use bviz_graph_layout::metrics::{Sizing, Unit};
    use bviz_graph_layout::{Input, Kind};
    use bviz_invocation::{
        Attempts, BuildOutcome, Chosen, ConfiguredTarget, CriticalStep, Outcomes, Profiled, Runner,
        Spawns, Timings,
    };
    use std::time::Duration;

    #[test]
    fn selecting_what_is_selected_changes_nothing() {
        let mut selected = Selected::default();
        assert!(!selected.set(None));
        assert!(selected.set(Some(Id(3))));
        assert!(!selected.set(Some(Id(3))));
        assert!(selected.set(Some(Id(4))));
        assert!(selected.set(None));
        assert_eq!(selected.0, None);
    }

    #[test]
    fn a_graph_describes_its_configurations_by_checksum() {
        let mut facets = Facets::default();
        facets.intern_config("55817e2200e2", Some("k8-fastbuild"), Tri::No);
        facets.intern_config("0c3d9a7714be", Some("k8-opt-exec"), Tri::Yes);
        facets.intern_config("91e24984663c", None, Tri::No);

        let described = configurations(&facets);
        let name = |checksum| described.name(checksum);
        assert_eq!(name("55817e2200e2").as_deref(), Some("k8-fastbuild"));
        assert_eq!(name("0c3d9a7714be").as_deref(), Some("k8-opt-exec"));
        assert_eq!(name("91e24984663c"), None);
    }

    /// The graph of the one target `//app:bin` and the action writing it, as
    /// a read lands it.
    fn loaded() -> Loaded {
        let mut names = Names::default();
        let mut base = Graph::new();
        let bin = names.intern("target://app:bin");
        base.add_node(Node::new(bin, "bin", Kind::Target, None));
        let link = names.intern("action:bazel-out/bin/app/bin");
        base.add_node(Node::new(link, "link", Kind::Action, Some(bin)));
        let reduced_base = bviz_graph_layout::reduce::transitive_reduction(&base);
        let containers = Arc::new(bviz_graph_layout::lod::expandable_containers(&base));
        let (base, reduced_base) = (Arc::new(base), Arc::new(reduced_base));
        let facets = Arc::new(Facets::default());
        let labels = Arc::new(LabelIndex::of(&base, &names, &facets));
        Loaded::new(
            Shared {
                base,
                reduced_base,
                map: Placement::pending().0,
                names: Arc::new(names),
                facets: Arc::clone(&facets),
                labels,
                containers,
                capabilities: bviz_graph_layout::capability::CapabilitiesBuilder::default().seal(),
                provenance: Arc::default(),
                inputs: Input::Query.into(),
                config_names: Arc::new(ConfigNames::of(&facets)),
            },
            Opening {
                expanded: HashSet::new(),
                over_budget: None,
            },
        )
    }

    fn reporting(outcome: BuildOutcome) -> Outcomes {
        let mut outcomes = Outcomes::default();
        outcomes.report_build(ConfiguredTarget::new("//app:bin".to_owned(), None), outcome);
        outcomes
    }

    /// A session changes for more than its outcomes, and outcomes can be
    /// reported that say what the last ones did.
    #[test]
    fn a_graph_takes_up_the_outcomes_its_session_holds_once_per_report_of_them() {
        let mut loaded = loaded();
        let mut session = Session::default();
        let by = session.reporter();
        assert!(!loaded.report(&session));
        assert_eq!(loaded.statuses, Statuses::default());

        session.report_outcomes(by, reporting(BuildOutcome::NotBuilt));
        assert!(loaded.report(&session));
        assert_ne!(loaded.statuses, Statuses::default());
        assert!(!loaded.report(&session));

        session.add_configurations(Configurations::default());
        assert!(!loaded.report(&session));

        session.report_outcomes(by, reporting(BuildOutcome::NotBuilt));
        assert!(!loaded.report(&session));
        session.report_outcomes(by, reporting(BuildOutcome::Built));
        assert!(loaded.report(&session));
    }

    const SANDBOXED_WALL: Duration = Duration::from_millis(7);

    fn timing(primary_output: &str) -> Timings {
        let mut timings = Timings::default();
        timings.record(Profiled {
            primary_output: Some(primary_output),
            owner: None,
            wall: SANDBOXED_WALL,
            spawns: Spawns::on(Runner::Sandboxed, SANDBOXED_WALL),
        });
        timings
    }

    /// A report of timings is taken up apart from one of outcomes, so that
    /// either landing leaves what the other placed as it was.
    #[test]
    fn a_graph_takes_up_timings_and_outcomes_each_once_per_report_of_them() {
        let mut loaded = loaded();
        let mut session = Session::default();
        let by = session.reporter();
        assert_eq!(loaded.times, Err(Untimed::NoProfile));

        session.report_timings(by, timing("bazel-out/bin/app/bin"));
        assert!(loaded.report(&session));
        assert!(loaded.times.is_ok());
        assert!(!loaded.report(&session));

        session.report_outcomes(by, reporting(BuildOutcome::NotBuilt));
        let timed = loaded.timed;
        assert!(loaded.report(&session));
        assert_eq!(loaded.timed, timed);
        assert!(loaded.times.is_ok());

        let taken_up = loaded.taken_up;
        session.report_timings(by, timing("bazel-out/bin/app/other"));
        assert!(loaded.report(&session));
        assert_eq!(loaded.taken_up, taken_up);
        assert_eq!(loaded.times, Err(Untimed::Unplaced));
        assert_ne!(loaded.statuses, Statuses::default());
    }

    /// Logged spawns are taken up apart from timings, and once per report of
    /// them.
    #[test]
    fn a_graph_takes_up_the_logged_spawns_its_session_holds_once_per_report_of_them() {
        let mut loaded = loaded();
        let mut session = Session::default();
        let by = session.reporter();
        assert!(!loaded.report(&session));
        assert_eq!(loaded.bytes, Err(Unsized::NoExecLog));

        let spawn = (Some("bazel-out/bin/app/bin"), "//app:bin", 5, 6, 7);
        session.report_logged_spawns(by, crate::spawn_bytes::tests::logged(&[spawn]));
        assert!(loaded.report(&session));
        assert!(loaded.bytes.is_ok());
        assert!(!loaded.report(&session));

        session.report_timings(by, timing("bazel-out/bin/app/bin"));
        let logged = loaded.logged;
        assert!(loaded.report(&session));
        assert_eq!(loaded.logged, logged);

        session.report_logged_spawns(by, crate::spawn_bytes::tests::logged(&[]));
        assert!(loaded.report(&session));
        assert_eq!(loaded.bytes, Err(Unsized::NoSpawns));
    }

    /// Attempts are taken up apart from timings, and once per report of them.
    #[test]
    fn a_graph_takes_up_the_attempts_its_session_holds_once_per_report_of_them() {
        let mut loaded = loaded();
        let mut session = Session::default();
        let by = session.reporter();
        assert!(!loaded.report(&session));
        assert!(loaded.states().is_none());

        let mut attempts = Attempts::default();
        let attempt = crate::playback::tests::attempt(0, 10, &[]);
        attempts.record(Some("bazel-out/bin/app/bin"), None, attempt);
        session.report_attempts(by, attempts);
        assert!(loaded.report(&session));
        assert!(loaded.states().is_some());
        assert!(!loaded.report(&session));

        session.report_timings(by, timing("bazel-out/bin/app/bin"));
        assert!(loaded.report(&session));
        assert!(loaded.states().is_some());

        session.report_attempts(by, Attempts::default());
        assert!(loaded.report(&session));
        assert!(loaded.states().is_none());
    }

    /// A critical path is taken up once per report of it, and one landing on
    /// none of the graph's nodes marks nothing.
    #[test]
    fn a_graph_takes_up_the_critical_path_once_per_report_of_it() {
        let mut loaded = loaded();
        let mut session = Session::default();
        let by = session.reporter();
        let step = |primary_output: &str| CriticalStep::new(Some(primary_output), None);

        session.report_critical_path(by, vec![step("bazel-out/bin/app/bin")], Vec::new());
        assert!(loaded.report(&session));
        assert!(loaded.critical().any());
        assert!(!loaded.report(&session));

        session.report_critical_path(by, vec![step("bazel-out/bin/app/other")], Vec::new());
        assert!(loaded.report(&session));
        assert!(!loaded.critical().any());
    }

    /// Each choice of a step is shown once, the same step chosen again
    /// included, and a step landing off the graph is shown as none.
    #[test]
    fn a_graph_shows_each_chosen_critical_path_step_once() {
        let mut loaded = loaded();
        let mut session = Session::default();
        let by = session.reporter();
        let step = |primary_output: &str| CriticalStep::new(Some(primary_output), None);
        session.report_critical_path(
            by,
            vec![
                step("bazel-out/bin/app/bin"),
                step("bazel-out/bin/app/other"),
            ],
            Vec::new(),
        );
        loaded.report(&session);
        assert_eq!(loaded.take_up_choice(&session), None);

        assert!(session.choose(Some(Chosen::Step(0))));
        let shown = loaded.take_up_choice(&session);
        assert!(shown.is_some());
        assert_eq!(loaded.take_up_choice(&session), None);
        assert!(session.choose(Some(Chosen::Step(0))));
        assert_eq!(loaded.take_up_choice(&session), shown);

        assert!(session.choose(Some(Chosen::Step(1))));
        assert_eq!(loaded.take_up_choice(&session), None);
        assert!(session.choose(None));
        assert_eq!(loaded.take_up_choice(&session), None);
    }

    /// A chosen action nearly critical is shown on the node of its action,
    /// which marks no node as on the path.
    #[test]
    fn a_graph_shows_a_chosen_nearly_critical_action() {
        let mut loaded = loaded();
        let mut session = Session::default();
        let by = session.reporter();
        let action = |primary_output: &str| CriticalStep::new(Some(primary_output), None);
        session.report_critical_path(
            by,
            vec![action("bazel-out/bin/app/other")],
            vec![action("bazel-out/bin/app/bin")],
        );
        loaded.report(&session);
        assert!(!loaded.critical().any());

        assert!(session.choose(Some(Chosen::NearlyCritical(0))));
        let shown = loaded.take_up_choice(&session);
        assert!(shown.is_some());
        assert_eq!(shown, loaded.critical().nearly_critical(0));
    }

    /// A step chosen before the graph landed is not shown on its landing.
    #[test]
    fn a_graph_passes_over_a_step_chosen_before_it_landed() {
        let mut session = Session::default();
        let by = session.reporter();
        let step = CriticalStep::new(Some("bazel-out/bin/app/bin"), None);
        session.report_critical_path(by, vec![step], Vec::new());
        assert!(session.choose(Some(Chosen::Step(0))));
        let mut loaded = loaded();
        loaded.pass_over_choice(&session);
        loaded.report(&session);
        assert_eq!(loaded.take_up_choice(&session), None);

        assert!(session.choose(Some(Chosen::Step(0))));
        assert!(loaded.take_up_choice(&session).is_some());
    }

    /// A chosen step is revealed in place, in the panel the graph's other
    /// views defer to.
    #[test]
    fn a_chosen_step_is_revealed_in_place_in_the_deferred_to_panel() {
        let mut session = Session::default();
        let by = session.reporter();
        let step = CriticalStep::new(Some("bazel-out/bin/app/bin"), None);
        session.report_critical_path(by, vec![step], Vec::new());
        let mut source = Source::Loaded(loaded());
        let panel = EntityId::from(1);
        let deferred = Some(Deferred {
            panel,
            _released: Subscription::new(|| {}),
        });
        let mut take_up = |session: &Session| take_up_from(&mut source, deferred.as_ref(), session);
        assert!(take_up(&session).1.is_none());

        assert!(session.choose(Some(Chosen::Step(0))));
        let (_, reveal) = take_up(&session);
        let reveal = reveal.expect("a reveal");
        assert_eq!((reveal.panel, reveal.reveal), (panel, Reveal::InPlace));
        let Source::Loaded(loaded) = &source else {
            panic!("a loaded graph");
        };
        assert_eq!(Some(reveal.node), loaded.critical().step(0));
    }

    /// Outcomes reported before the graph is read are the first it takes up.
    #[test]
    fn a_graph_takes_up_outcomes_reported_before_it_landed() {
        let mut session = Session::default();
        let by = session.reporter();
        session.report_outcomes(by, reporting(BuildOutcome::Built));
        let mut loaded = loaded();
        assert!(loaded.report(&session));
        assert_ne!(loaded.statuses, Statuses::default());
    }

    /// Placing runs apart from the read it belongs to, so a panic there fails
    /// the read all the same, and lands no map.
    #[test]
    fn a_placement_that_panics_fails_the_read() {
        let (placement, landing) = Placement::pending();
        let mut source = Source::Loading;
        let placed = Pending::<Arc<MapLayout>>::start(|| panic!("placing the map")).join();

        assert!(land(&mut source, placed, landing).is_none());
        assert!(matches!(source, Source::Failed(_)));
        assert!(placement.placed().is_none());
    }

    fn running() -> Pass<Measurement> {
        Pass::Running {
            _work: Task::ready(()),
        }
    }

    fn measured(scope: Scope) -> Arc<Measurement> {
        Arc::new(Measurement {
            scope,
            points: Vec::new(),
            sizing: Sizing::FanIn,
        })
    }

    /// A scope no longer wanted stops being passed over, but what a pass
    /// yielded in it stays for when it is wanted again.
    #[test]
    fn an_unwanted_pass_is_dropped_and_a_landed_one_kept() {
        let scope = |input: Input| Scope::new(Unit::Target, input.into()).unwrap();
        let (landed, dropped, kept) = (
            scope(Input::Query),
            scope(Input::Cquery),
            scope(Input::Aquery),
        );
        let mut passes = Passes::keeping_all();
        passes.land(landed, measured(landed));
        passes.by_key.insert(dropped, running());
        passes.by_key.insert(kept, running());

        passes.forget_unwanted(&[kept]);

        assert!(passes.done(&landed).is_some());
        assert!(!passes.by_key.contains_key(&dropped));
        assert!(matches!(
            passes.by_key.get(&kept),
            Some(Pass::Running { .. })
        ));
    }

    /// Of the passes done that are no longer wanted, those wanted or landed
    /// last are kept, as many as the passes keep, and the rest dropped.
    #[test]
    fn of_the_passes_no_longer_wanted_only_the_last_used_are_kept() {
        let mut passes: Passes<u32, Measurement> = Passes::keeping_recent(2);
        let scope = Scope::new(Unit::Target, Input::Query.into()).unwrap();
        for key in 0..4 {
            passes.land(key, measured(scope));
        }
        passes.forget_unwanted(&[0]);
        passes.forget_unwanted(&[3]);

        let mut kept: Vec<u32> = passes.by_key.keys().copied().collect();
        kept.sort_unstable();
        assert_eq!(kept, [0, 2, 3]);
    }
}
