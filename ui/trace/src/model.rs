use crate::groups::{
    ChartLanes, GroupChrome, GroupLayoutState, STACKED_GROUPS, build_grouped_layout,
    default_collapsed_groups,
};
use crate::rebuild::Rebuild;
use crate::track_label::shared_names;
use crate::track_order::TrackSort;
use crate::{METRICS, Source};
use bviz_critical_path::{CriticalPath, NearlyCritical, Step, Work};
use bviz_invocation::spawn::LoggedSpawns;
use bviz_invocation::{
    ActionGraph, Chosen, ConfiguredTarget, CriticalStep, Recorded, Revision, Session,
};
use bviz_label::names::RepoNames;
use bviz_trace_layout::{
    FlameMode, LaneLayout, ScopeAggregate, TimeSelection, TrackFlamer, WorkingLanes, lay_out_lanes,
};
use bviz_trace_profile::{LaneClass, Profile, SliceAt, Str, TimeSpan};
use bviz_ui_components::{Pending, load_in_background};
use bviz_ui_playback::{Alignment, Clock, OnClock, Span};
use bviz_ui_session::Reports;
use gpui::{App, Context, Entity, EventEmitter, Subscription, Task};
use std::cell::RefCell;
use std::collections::HashSet;
use std::ops::{Deref, DerefMut};
use std::rc::Rc;
use std::sync::Arc;

/// A background aggregate build started for the current selection. Carried as an
/// event rather than a notification because nothing the model exposes has
/// changed yet — the previously built aggregate stays the shown one — so it
/// concerns only a panel that signals the rebuild in progress. A notification
/// would instead reach every observer, whose follow-up is written for a *replaced*
/// aggregate: dropping memoized layouts and re-sorting for a graph that has not
/// moved.
pub(crate) struct AggregateBuildStarted;

/// A different document is now loaded. Carried as an event because the state it
/// calls for dropping is view-local — a zoom held over the old graph, a sort
/// order over its rows — and a panel must drop it exactly once, on the swap,
/// rather than on every notification the model sends.
pub(crate) struct SourceReplaced;

/// The loaded profile's working lanes are derived. Carried as an event because
/// only the timeline's cached scene draws them, which a notification replays
/// rather than rebuilds.
pub(crate) struct WorkingLanesDerived;

/// The loaded profile's critical path is derived. Carried as an event because
/// it concerns only the panel showing the path.
pub(crate) struct CriticalPathDerived;

/// Something was chosen of the critical path, or what was chosen let go of.
/// Carried as an event because the timeline moves its view to what is chosen
/// once, on the choice, and not on every notification after it, and redraws
/// its lanes, which the chosen action's keeps in its group's sample.
pub(crate) struct ChoiceChanged;

/// Where the action span that ran `step` stands, where the path joined one.
fn step_span(step: &Step) -> Option<SliceAt> {
    match step.work() {
        Work::Spanned(split) => Some(split.at),
        Work::Unspanned(_) | Work::ChangePruned => None,
    }
}

/// The slices standing for `step` in the profile: the action span that ran
/// it, where the path joined one, then its own on the critical-path lane.
pub(crate) fn step_slices(step: &Step) -> impl Iterator<Item = SliceAt> {
    step_span(step).into_iter().chain([step.at()])
}

/// What was chosen of the critical path, as the profile records it.
#[derive(Clone, Copy)]
pub(crate) enum ChosenItem<'p> {
    Step(&'p Step),
    NearlyCritical(&'p NearlyCritical),
}

impl ChosenItem<'_> {
    /// The slices standing for it in the profile, the action span that ran
    /// it first where there is one.
    pub(crate) fn slices(self) -> Vec<SliceAt> {
        match self {
            ChosenItem::Step(step) => step_slices(step).collect(),
            ChosenItem::NearlyCritical(action) => vec![action.at],
        }
    }

    /// From the first start to the last end of its slices.
    pub(crate) fn time(self, profile: &Profile) -> TimeSpan {
        let (own, span) = match self {
            ChosenItem::Step(step) => (step.at(), step_span(step)),
            ChosenItem::NearlyCritical(action) => (action.at, None),
        };
        let own = profile.slice(own);
        span.map(|at| profile.slice(at)).into_iter().fold(
            TimeSpan {
                start_us: own.start_us,
                end_us: own.end_us(),
            },
            |time, slice| TimeSpan {
                start_us: time.start_us.min(slice.start_us),
                end_us: time.end_us.max(slice.end_us()),
            },
        )
    }
}

/// The actions nearly critical as the session of their invocation holds them.
fn nearly_critical_actions(actions: &[NearlyCritical]) -> Vec<CriticalStep> {
    actions
        .iter()
        .map(|action| {
            let owner = action
                .target
                .as_deref()
                .map(|label| ConfiguredTarget::new(label, action.configuration.as_deref()));
            CriticalStep::new(Some(&action.primary_output), owner)
        })
        .collect()
}

/// `steps` as the session of their invocation holds them.
fn critical_steps(steps: &[Step]) -> Vec<CriticalStep> {
    steps
        .iter()
        .map(|step| {
            let owner = step
                .target()
                .map(|label| ConfiguredTarget::new(label, step.configuration()));
            CriticalStep::new(step.primary_output(), owner)
        })
        .collect()
}

/// What ties a trace model to the document it is a file of.
struct Ties {
    /// Tells what the other outputs of the profile's invocation know of it,
    /// read to name what the profile holds only an id of, and holds what is
    /// chosen of the critical path, for the views of every output to show.
    reports: Reports,
    on_clock: OnClock,
    /// Joins the critical path to the spawns and the action graph the session
    /// learns of.
    _session_changed: Subscription,
}

/// The names the repositories the labels `profile` records name are shown
/// by, told apart among those alone.
fn repo_names(profile: &Profile) -> RepoNames {
    let mut targets: HashSet<Str> = HashSet::new();
    for track in profile.threads() {
        let named = profile
            .details(track)
            .iter()
            .filter_map(|detail| detail.target.get());
        targets.extend(named);
    }
    RepoNames::of_labels(targets.into_iter().map(|target| profile.str(target)))
}

/// How a profile is shown from its first paint on, derived with its read off
/// the UI thread.
struct Presentation {
    /// The names the repositories the profile's labels name are shown by.
    repos: RepoNames,
    /// The full thread/counter lane layout, which the chart restacks into
    /// groups.
    lanes: LaneLayout,
}

impl Presentation {
    fn of(profile: &Profile) -> Presentation {
        let t = bviz_time::now();
        let lanes = lay_out_lanes(profile, &METRICS);
        if bviz_switches::is_set(bviz_trace_profile::TIMING_SWITCH) {
            bviz_switches::report(format_args!(
                "[timing] lay_out_lanes: {:.3}s",
                t.elapsed().as_secs_f64()
            ));
        }
        Presentation {
            repos: repo_names(profile),
            lanes,
        }
    }
}

/// A loaded profile with what is derived from it once for every reader of it.
pub(crate) struct LoadedProfile {
    flamer: Arc<TrackFlamer>,
    presentation: Presentation,
    working_lanes: Derived<WorkingLanes>,
    critical_path: TracedPath,
    /// Derives the profile's action timings and attempts and reports them to
    /// its session, `None` until its document is joined; held with the
    /// profile, so that a profile replaced before they are ready reports none.
    reporting_timings: Option<Task<()>>,
}

/// What is derived from a profile off the UI thread.
pub(crate) enum Derived<T> {
    Deriving {
        /// Held with the profile it reads, so that it ends once another is
        /// loaded.
        _derivation: Task<()>,
    },
    Derived(T),
}

/// A loaded profile's critical path, as far as it is traced.
pub(crate) enum TracedPath {
    Tracing(Tracing),
    Traced {
        /// `None` for a profile with no critical-path lane.
        path: Option<Box<CriticalPath>>,
        /// What of its session `path` is joined to.
        joined: Joined,
        /// Traces the path again, joined to spawns its session learned of
        /// since; `path` stands until it lands.
        rejoining: Option<Tracing>,
    },
}

/// A tracing of a critical path under way, held with the profile it reads so
/// that dropping it, once another profile is loaded or other spawns are
/// logged, cancels it.
pub(crate) struct Tracing {
    /// What of its session the path is traced with.
    joined: Joined,
    _task: Task<()>,
}

/// What of its session a critical path is traced with, by the revisions the
/// session held it at.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Joined {
    logged: Revision,
    graphed: Revision,
}

impl Joined {
    fn of(session: &Session) -> Joined {
        Joined {
            logged: session.logged_spawns_revision(),
            graphed: session.action_graph_revision(),
        }
    }
}

/// What a critical path is joined to from its session: the spawns its
/// execution log records and its action graph.
struct JoinedTo {
    spawns: Option<Arc<LoggedSpawns>>,
    graph: Option<Arc<ActionGraph>>,
}

impl TracedPath {
    /// What the path was last asked to be joined to.
    fn asked(&self) -> Joined {
        match self {
            TracedPath::Tracing(tracing)
            | TracedPath::Traced {
                rejoining: Some(tracing),
                ..
            } => tracing.joined,
            TracedPath::Traced { joined, .. } => *joined,
        }
    }

    /// Has `tracing` take the place of any tracing under way, keeping a path
    /// already traced until it lands.
    fn retrace(&mut self, tracing: Tracing) {
        match self {
            TracedPath::Tracing(under_way) => *under_way = tracing,
            TracedPath::Traced { rejoining, .. } => *rejoining = Some(tracing),
        }
    }

    /// Takes `path` as traced if it was traced joined to `joined` by the
    /// tracing under way, and says whether it did.
    fn adopt(&mut self, joined: Joined, path: Option<CriticalPath>) -> bool {
        let under_way = match self {
            TracedPath::Tracing(tracing)
            | TracedPath::Traced {
                rejoining: Some(tracing),
                ..
            } => Some(tracing.joined),
            TracedPath::Traced { .. } => None,
        };
        let own = under_way == Some(joined);
        if own {
            *self = TracedPath::Traced {
                path: path.map(Box::new),
                joined,
                rejoining: None,
            };
        }
        own
    }
}

/// Traces the critical path of `profile` off the UI thread, joined `to`
/// what its session holds, for the model of `cx` to adopt as `joined`.
fn trace(
    profile: Arc<Profile>,
    to: JoinedTo,
    joined: Joined,
    cx: &mut Context<TraceModel>,
) -> Tracing {
    let task = cx.spawn(async move |model, cx| {
        let derive = async move {
            let path = CriticalPath::of(&profile, to.spawns.as_deref(), to.graph.as_deref());
            (profile, path)
        };
        let (profile, path) = cx.background_executor().spawn(derive).await;
        model
            .update(cx, |model, cx| {
                if model.adopt_critical_path(&profile, joined, path) {
                    model.report_critical_path(cx);
                    cx.emit(CriticalPathDerived);
                }
            })
            .ok();
    });
    Tracing {
        joined,
        _task: task,
    }
}

/// What `session` holds for a critical path to be joined to, and as of when.
fn joined_to(session: &Entity<Session>, cx: &Context<TraceModel>) -> (JoinedTo, Joined) {
    let session = session.read(cx);
    let to = JoinedTo {
        spawns: session.logged_spawns().cloned(),
        graph: session.action_graph().cloned(),
    };
    (to, Joined::of(session))
}

/// The tasks deriving what a [`LoadedProfile`] derives off the UI thread.
struct Derivations {
    working_lanes: Task<()>,
    critical_path: Tracing,
}

impl LoadedProfile {
    /// `profile` as loaded, its working lanes and critical path on their way
    /// to the model of `cx`, its critical path joined to what `session` holds.
    fn new(
        profile: Profile,
        presentation: Presentation,
        session: &Entity<Session>,
        cx: &mut Context<TraceModel>,
    ) -> LoadedProfile {
        let flamer = Arc::new(TrackFlamer::new(&Arc::new(profile)));
        let derived_from = flamer.clone();
        let deriving = cx.spawn(async move |model, cx| {
            let derive = async move {
                let lanes = derived_from.working_lanes();
                (derived_from, lanes)
            };
            let (derived_from, lanes) = cx.background_executor().spawn(derive).await;
            model
                .update(cx, |model, cx| {
                    if model.adopt_working_lanes(&derived_from, lanes) {
                        cx.emit(WorkingLanesDerived);
                    }
                })
                .ok();
        });
        let (to, joined) = joined_to(session, cx);
        let tracing = trace(flamer.profile().clone(), to, joined, cx);
        LoadedProfile::deriving(
            flamer,
            Derivations {
                working_lanes: deriving,
                critical_path: tracing,
            },
            presentation,
        )
    }

    fn deriving(
        flamer: Arc<TrackFlamer>,
        derivations: Derivations,
        presentation: Presentation,
    ) -> LoadedProfile {
        LoadedProfile {
            flamer,
            presentation,
            working_lanes: Derived::Deriving {
                _derivation: derivations.working_lanes,
            },
            critical_path: TracedPath::Tracing(derivations.critical_path),
            reporting_timings: None,
        }
    }

    pub(crate) fn profile(&self) -> &Arc<Profile> {
        self.flamer.profile()
    }

    /// Takes `lanes` as its own if they were derived from its own `flamer`,
    /// and says whether it did.
    fn adopt_working_lanes(&mut self, flamer: &Arc<TrackFlamer>, lanes: WorkingLanes) -> bool {
        let own = Arc::ptr_eq(&self.flamer, flamer);
        if own {
            self.working_lanes = Derived::Derived(lanes);
        }
        own
    }

    /// Takes `path` as its own if it was traced from its own profile, joined
    /// to what it last asked for, and says whether it did.
    fn adopt_critical_path(
        &mut self,
        profile: &Arc<Profile>,
        joined: Joined,
        path: Option<CriticalPath>,
    ) -> bool {
        Arc::ptr_eq(self.profile(), profile) && self.critical_path.adopt(joined, path)
    }
}

/// How a click on a track's label changes which tracks are selected.
#[derive(Clone, Copy)]
pub(crate) enum TrackPick {
    /// The track alone.
    Only,
    /// The track toggled in the selection.
    Flip,
    /// The visible lanes from anchor to track.
    Run,
    /// That run set to the anchor's own state, extending a just-added run or
    /// clearing a just-removed one.
    SetRun,
}

/// The trace document and the selection and visibility state derived from it,
/// plus the layouts and aggregates cached from those.
pub(crate) struct TraceState {
    source: Source,
    /// Which tracks the flame chart draws, indexed by `profile.threads()` order.
    visible_tracks: Vec<bool>,
    /// [`shared_names`] over the loaded profile, which counting the names of
    /// every track derives.
    shared_track_names: Vec<bool>,
    /// Which tracks the flame graph and stats aggregate over, indexed likewise.
    selected_tracks: Vec<bool>,
    /// The profile time the flame graph and stats aggregate over, intersected
    /// with the selected tracks.
    time_selection: TimeSelection,
    /// Which lane-class groups are collapsed in the chart, indexed by
    /// [`STACKED_GROUPS`]. Loading starts collapsed — it holds one lane per external
    /// repository, hundreds on a large build.
    collapsed_groups: [bool; STACKED_GROUPS.len()],
    /// Which groups draw every lane rather than a sample, indexed by
    /// [`STACKED_GROUPS`].
    group_show_all: [bool; STACKED_GROUPS.len()],
    /// The track the chosen action's slices are shown on, which its group's
    /// sample keeps for as long as it stays chosen.
    kept_in_sample: Option<usize>,
    /// The order every group stands its tracks in.
    track_sort: TrackSort,
    flame_mode: FlameMode,
    /// Bumped whenever an input the aggregate is built from changes — the track
    /// selection, the time selection, or the flame mode — keying the build so a
    /// stale build's result is discarded once the inputs have moved on.
    aggregate_key: u64,
    /// The track a shift-click range-selection extends from, if any.
    select_anchor: Option<usize>,
    /// The lane layout restricted to the visible tracks and restacked into
    /// lane-class groups, derived from the loaded profile's full layout and
    /// cleared whenever the [`GroupLayoutState`] it was built under moves.
    visible_lanes: RefCell<Option<Rc<ChartLanes>>>,
    /// The group headers and show-more rows over `visible_lanes`, built and
    /// cleared alongside it.
    group_chrome: RefCell<Option<Rc<GroupChrome>>>,
    /// The selected tracks' aggregate — its flame graph and stats rows, held
    /// together so the rows' and laid-out boxes' name handles resolve against
    /// the graph that produced them. Built on demand off the UI thread (walking
    /// every slice would freeze a paint) and swapped in when ready; the previous
    /// value stays shown while a rebuild runs, so a selection change doesn't
    /// blank the view.
    aggregates: Option<Arc<ScopeAggregate>>,
    /// Background-build bookkeeping for `aggregates`, keyed on
    /// `aggregate_key`.
    agg: Rebuild<u64>,
}

/// A profile's [`TraceState`], tied to its document. Shared as an entity by
/// every view of the profile, so each reads one source of truth, which it
/// reaches through the model's dereference; a view's own state stays on its
/// panel.
pub(crate) struct TraceModel {
    state: TraceState,
    ties: Ties,
    /// The read that fills the state's source, held so that it is not
    /// cancelled.
    _load: Task<()>,
}

impl Deref for TraceModel {
    type Target = TraceState;

    fn deref(&self) -> &TraceState {
        &self.state
    }
}

impl DerefMut for TraceModel {
    fn deref_mut(&mut self) -> &mut TraceState {
        &mut self.state
    }
}

impl EventEmitter<AggregateBuildStarted> for TraceModel {}
impl EventEmitter<SourceReplaced> for TraceModel {}
impl EventEmitter<WorkingLanesDerived> for TraceModel {}
impl EventEmitter<CriticalPathDerived> for TraceModel {}
impl EventEmitter<ChoiceChanged> for TraceModel {}

impl TraceModel {
    /// The document `profile` is being read into, loaded and playing on `clock`
    /// once the read finishes and failed where it does not.
    pub(crate) fn reading(
        profile: Pending<Recorded<Profile>>,
        session: Entity<Session>,
        clock: Entity<Clock>,
        cx: &mut Context<Self>,
    ) -> Self {
        let ties = Ties {
            _session_changed: cx.observe(&session, |model, _, cx| model.rejoin(cx)),
            reports: Reports::new(session, cx),
            on_clock: OnClock::new(clock, cx, Self::place_on_clock),
        };
        let load = load_in_background(
            cx,
            move || {
                let profile = profile.join()?;
                let presentation = Presentation::of(profile.get());
                Ok((profile, presentation))
            },
            |model, result: Result<(Recorded<Profile>, Presentation), String>, cx| {
                let source =
                    match result {
                        Ok((profile, presentation)) => {
                            let session = model.session().clone();
                            Source::Loaded(profile.map(|profile| {
                                LoadedProfile::new(profile, presentation, &session, cx)
                            }))
                        }
                        Err(err) => Source::Failed(err),
                    };
                model.set_source(source, cx);
                model.place_on_clock(cx);
            },
        );
        Self {
            state: TraceState::new(),
            ties,
            _load: load,
        }
    }

    /// Traces the loaded profile's critical path again where its session
    /// holds other spawns or another action graph than it was traced with.
    fn rejoin(&mut self, cx: &mut Context<Self>) {
        let (to, joined) = joined_to(self.session(), cx);
        let Some(loaded) = self.loaded_mut() else {
            return;
        };
        if loaded.critical_path.asked() == joined {
            return;
        }
        let tracing = trace(loaded.profile().clone(), to, joined, cx);
        loaded.critical_path.retrace(tracing);
    }

    pub(crate) fn session(&self) -> &Entity<Session> {
        self.ties.reports.session()
    }

    /// Registers the profile with a joined clock as what it plays, or
    /// withdraws it where no profile with a wall-clock origin is loaded, and
    /// has the session of a joined document learn the profile's action
    /// timings. A profile alone plays nothing and derives no timings: its
    /// document plays back, and other files read what it reports, once
    /// another file joins it.
    fn place_on_clock(&mut self, cx: &mut Context<Self>) {
        let joined = self.joined(cx);
        let played = self
            .profile()
            .filter(|_| joined)
            .and_then(|profile| clock_source(profile));
        self.ties.on_clock.place(played, cx);
        if joined {
            self.report_timings(cx);
        }
    }

    /// Has the loaded profile's action timings and attempts derived and
    /// reported to the session, unless they are already on their way.
    fn report_timings(&mut self, cx: &mut Context<Self>) {
        let Some(loaded) = self.loaded_mut() else {
            return;
        };
        if loaded.reporting_timings.is_some() {
            return;
        }
        let timed = loaded.profile().clone();
        loaded.reporting_timings = Some(cx.spawn(async move |model, cx| {
            let (timings, attempts) = cx
                .background_executor()
                .spawn(async move { (timed.timings(), timed.attempts()) })
                .await;
            model
                .update(cx, |model, cx| {
                    model.ties.reports.report(cx, |session, by, cx| {
                        session.report_timings(by, timings);
                        session.report_attempts(by, attempts.unwrap_or_default());
                        cx.notify();
                    });
                })
                .ok();
        }));
    }

    /// Whether the profile's document joins it to other files.
    fn joined(&self, cx: &App) -> bool {
        self.clock().read(cx).alignment() == Alignment::Joined
    }

    /// The clock the views follow.
    pub(crate) fn clock(&self) -> &Entity<Clock> {
        self.ties.on_clock.clock()
    }

    /// The profile instant the clock's time stands at, `None` while the loaded
    /// profile plays on no clock.
    pub(crate) fn playhead_us(&self, cx: &App) -> Option<i64> {
        let origin = self.profile()?.metadata().epoch_origin?;
        let now = self.clock().read(cx).now()?;
        Some(now.micros() - origin.micros())
    }

    /// Whether the loaded profile plays on no clock its document has, recording
    /// no wall-clock time to stand beside the other files' times.
    pub(crate) fn unaligned(&self, cx: &App) -> bool {
        self.joined(cx)
            && self
                .profile()
                .is_some_and(|profile| profile.metadata().epoch_origin.is_none())
    }

    /// Tells the session the steps of the traced path and the actions nearly
    /// critical, which tracing the path again with other spawns mostly
    /// leaves as they were, and lets the lane of what the session let go of
    /// with them fall back to its group's sample.
    fn report_critical_path(&mut self, cx: &mut Context<Self>) {
        let steps = critical_steps(self.steps());
        let nearly_critical = nearly_critical_actions(self.nearly_critical());
        self.ties.reports.report(cx, |session, by, cx| {
            if session.report_critical_path(by, steps, nearly_critical) {
                cx.notify();
            }
        });
        if self.held_choice(cx).is_none() && self.kept_in_sample.is_some() {
            self.release_chosen_lane();
            cx.emit(ChoiceChanged);
        }
    }

    /// What the session holds chosen of the critical path.
    fn held_choice(&self, cx: &App) -> Option<Chosen> {
        self.session().read(cx).chosen()
    }

    /// Has the session hold `chosen` chosen of the critical path, for the
    /// views of every output of the invocation to show.
    fn hold_choice(&self, chosen: Option<Chosen>, cx: &mut App) {
        self.session().update(cx, |session, cx| {
            if session.choose(chosen) {
                cx.notify();
            }
        });
    }

    /// What is chosen of the critical path, and the profile's record of it.
    pub(crate) fn chosen(&self, cx: &App) -> Option<(Chosen, ChosenItem<'_>)> {
        let chosen = self.held_choice(cx)?;
        Some((chosen, self.item_of(chosen)?))
    }

    /// Chooses `chosen` of the critical path and has the timeline and the
    /// views of the invocation's other outputs show it.
    pub(crate) fn choose(&mut self, chosen: Chosen, cx: &mut Context<Self>) {
        if self.adopt_chosen(chosen) {
            self.hold_choice(Some(chosen), cx);
            cx.emit(ChoiceChanged);
            cx.notify();
        }
    }

    /// Lets go of what is chosen of the critical path. Says whether anything
    /// was.
    pub(crate) fn unchoose(&mut self, cx: &mut Context<Self>) -> bool {
        if self.chosen(cx).is_none() {
            return false;
        }
        self.hold_choice(None, cx);
        self.release_chosen_lane();
        cx.emit(ChoiceChanged);
        cx.notify();
        true
    }

    /// Restricts the aggregate to `selection`, reporting whether that differs
    /// from the standing one. Keys a rebuild only for a set that actually
    /// changed, so re-committing the same regions costs nothing.
    pub(crate) fn set_time_selection(
        &mut self,
        selection: TimeSelection,
        cx: &mut Context<Self>,
    ) -> bool {
        if selection == self.time_selection {
            return false;
        }
        self.time_selection = selection;
        self.aggregate_key += 1;
        // The regions are drawn on the timeline and named by the panels the
        // aggregate covers, each in a pane of its own, so the new window has to
        // reach every observer rather than only the view the gesture landed in.
        cx.notify();
        self.ensure_aggregates(cx);
        true
    }

    /// Replaces the loaded document, dropping the lane layouts (which depend only
    /// on the profile), the aggregate built over the old document, and the time
    /// selection (whose instants address the old profile's timeline) and, for a
    /// loaded profile, seeding the default track selection.
    pub(crate) fn set_source(&mut self, source: Source, cx: &mut Context<Self>) {
        let session = self.session().clone();
        session.update(cx, |session, cx| {
            if self.state.replace_source(source, session) {
                cx.notify();
            }
        });
        cx.emit(SourceReplaced);
        self.ensure_aggregates(cx);
    }

    /// Selects `track` for the flame graph and stats as `pick` asks. Picking
    /// the one track moves the anchor to it; picking a run leaves the anchor,
    /// so the run can be re-adjusted. Keys a rebuild only when the set
    /// actually changes, so a repeat click is free.
    pub(crate) fn select_track(&mut self, track: usize, pick: TrackPick, cx: &mut Context<Self>) {
        let n = self.selected_tracks.len();
        if track >= n {
            return;
        }
        // The anchor run and the anchor's own selected state, read before any
        // mutation so the range extends from the existing anchor.
        let run = self.visible_run_to(track);
        let anchor_state = self
            .select_anchor
            .and_then(|a| self.selected_tracks.get(a).copied());
        let sel = match (pick, run) {
            (TrackPick::Only, _) => {
                let mut sel = vec![false; n];
                sel[track] = true;
                self.select_anchor = Some(track);
                sel
            }
            (TrackPick::Flip, _) => {
                let mut sel = self.selected_tracks.clone();
                sel[track] = !sel[track];
                self.select_anchor = Some(track);
                sel
            }
            (TrackPick::Run, Some(run)) => {
                let mut sel = vec![false; n];
                for t in run {
                    sel[t] = true;
                }
                sel
            }
            (TrackPick::SetRun, Some(run)) => {
                let state = anchor_state.unwrap_or(true);
                let mut sel = self.selected_tracks.clone();
                for t in run {
                    sel[t] = state;
                }
                sel
            }
            // A run with no anchor (or an anchor no longer visible) falls back
            // to selecting the one track.
            (TrackPick::Run | TrackPick::SetRun, None) => {
                let mut sel = vec![false; n];
                sel[track] = true;
                self.select_anchor = Some(track);
                sel
            }
        };
        if sel != self.selected_tracks {
            self.selected_tracks = sel;
            self.aggregate_key += 1;
        }
        self.ensure_aggregates(cx);
    }

    /// Puts the flame graph under `mode` and kicks off the rebuild it needs.
    /// Reports what [`Self::adopt_flame_mode`] did, so a caller can skip the work
    /// that only a mode change calls for.
    pub(crate) fn set_flame_mode(&mut self, mode: FlameMode, cx: &mut Context<Self>) -> bool {
        if !self.adopt_flame_mode(mode) {
            return false;
        }
        // The mode controls render from the mode itself, so their selected state
        // must not wait for the rebuild to land.
        cx.notify();
        self.ensure_aggregates(cx);
        true
    }

    /// Sets the selection to every visible track (the select-all gesture),
    /// returning whether the set changed.
    pub(crate) fn select_all_visible(&mut self, cx: &mut Context<Self>) -> bool {
        let changed = if self.selected_tracks != self.visible_tracks {
            self.selected_tracks = self.visible_tracks.clone();
            self.aggregate_key += 1;
            true
        } else {
            false
        };
        self.ensure_aggregates(cx);
        changed
    }

    /// Makes the current selection's aggregate the shown one: built on demand
    /// off the UI thread (walking every slice is far too costly for a paint) and
    /// swapped in on completion, the previously built graph staying shown until
    /// then. An in-flight build whose selection the inputs have moved past is
    /// abandoned. Idempotent — a no-op once the right value is ready or being
    /// computed. Emits [`AggregateBuildStarted`] when it spawns a build and
    /// notifies when one lands, so a panel showing the rebuild in progress and a
    /// panel holding state derived from the aggregate each hear only their own
    /// step, exactly once per build.
    pub(crate) fn ensure_aggregates(&mut self, cx: &mut Context<Self>) {
        let Some(loaded) = self.loaded() else {
            return;
        };
        let scan = loaded.flamer.clone();
        let key = self.aggregate_key;
        if self.agg.is_ready(key) {
            // The shown value is already current; cancel a build for a key the
            // inputs have since left so its late result can't clobber it.
            self.agg.cancel_inflight();
            return;
        }
        if self.agg.is_inflight(key) {
            return;
        }
        let generation = self.agg.start(key);
        cx.emit(AggregateBuildStarted);
        let selected = self.selected_tracks.clone();
        let time_window = self.time_selection.clone();
        let mode = self.flame_mode;
        let task = cx.spawn(async move |model, cx| {
            let aggregate = cx
                .background_executor()
                .spawn(async move {
                    let t = bviz_time::now();
                    let aggregate =
                        ScopeAggregate::build_for_tracks_reusing(&scan, mode, &time_window, |i| {
                            selected.get(i).copied().unwrap_or(false)
                        });
                    if bviz_switches::is_set(bviz_trace_profile::TIMING_SWITCH) {
                        bviz_switches::report(format_args!(
                            "[timing] track_aggregate: {:.3}s",
                            t.elapsed().as_secs_f64()
                        ));
                    }
                    aggregate
                })
                .await;
            model
                .update(cx, |m, cx| {
                    // Discard a result the inputs have moved past since this spawn.
                    if !m.agg.accept(generation, key) {
                        return;
                    }
                    m.aggregates = Some(Arc::new(aggregate));
                    cx.notify();
                })
                .ok();
        });
        self.agg.set_task(task);
    }
}

impl TraceState {
    /// The state of a document whose read has not finished.
    fn new() -> Self {
        Self {
            source: Source::Loading,
            visible_tracks: Vec::new(),
            shared_track_names: Vec::new(),
            selected_tracks: Vec::new(),
            time_selection: TimeSelection::default(),
            collapsed_groups: default_collapsed_groups(),
            group_show_all: [false; STACKED_GROUPS.len()],
            kept_in_sample: None,
            track_sort: TrackSort::default(),
            flame_mode: FlameMode::default(),
            aggregate_key: 0,
            select_anchor: None,
            visible_lanes: RefCell::new(None),
            group_chrome: RefCell::new(None),
            aggregates: None,
            agg: Rebuild::new(),
        }
    }

    pub(crate) fn source(&self) -> &Source {
        &self.source
    }

    fn loaded(&self) -> Option<&LoadedProfile> {
        match &self.source {
            Source::Loaded(loaded) => Some(loaded.get()),
            _ => None,
        }
    }

    fn loaded_mut(&mut self) -> Option<&mut LoadedProfile> {
        match &mut self.source {
            Source::Loaded(loaded) => Some(loaded.get_mut()),
            _ => None,
        }
    }

    pub(crate) fn cut_off(&self) -> bool {
        self.source.is_cut_off()
    }

    pub(crate) fn profile(&self) -> Option<&Arc<Profile>> {
        self.loaded().map(LoadedProfile::profile)
    }

    /// The names the loaded profile's repositories are shown by.
    pub(crate) fn repos(&self) -> Option<&RepoNames> {
        match &self.source {
            Source::Loaded(loaded) => Some(&loaded.get().presentation.repos),
            _ => None,
        }
    }

    /// The loaded profile's working lanes, once derived.
    pub(crate) fn working_lanes(&self) -> Option<&WorkingLanes> {
        match self.loaded()? {
            LoadedProfile {
                working_lanes: Derived::Derived(lanes),
                ..
            } => Some(lanes),
            _ => None,
        }
    }

    fn adopt_working_lanes(&mut self, flamer: &Arc<TrackFlamer>, lanes: WorkingLanes) -> bool {
        self.loaded_mut()
            .is_some_and(|loaded| loaded.adopt_working_lanes(flamer, lanes))
    }

    /// The loaded profile's critical path, as far as it is traced; `None`
    /// until a profile is loaded.
    pub(crate) fn critical_path(&self) -> Option<&TracedPath> {
        self.loaded().map(|loaded| &loaded.critical_path)
    }

    fn adopt_critical_path(
        &mut self,
        profile: &Arc<Profile>,
        joined: Joined,
        path: Option<CriticalPath>,
    ) -> bool {
        self.loaded_mut()
            .is_some_and(|loaded| loaded.adopt_critical_path(profile, joined, path))
    }

    /// The traced path's steps; none while it is traced or where the profile
    /// records no path.
    fn steps(&self) -> &[Step] {
        match self.critical_path() {
            Some(TracedPath::Traced {
                path: Some(path), ..
            }) => path.steps(),
            _ => &[],
        }
    }

    /// The actions nearly critical of the traced path; none while it is
    /// traced or where no modeled build holds it.
    fn nearly_critical(&self) -> &[NearlyCritical] {
        match self.critical_path() {
            Some(TracedPath::Traced {
                path: Some(path), ..
            }) => path
                .modeled()
                .map_or(&[], |modeled| modeled.nearly_critical()),
            _ => &[],
        }
    }

    /// The profile's record of `chosen`, where the path holds it.
    fn item_of(&self, chosen: Chosen) -> Option<ChosenItem<'_>> {
        match chosen {
            Chosen::Step(index) => self.steps().get(index).map(ChosenItem::Step),
            Chosen::NearlyCritical(index) => self
                .nearly_critical()
                .get(index)
                .map(ChosenItem::NearlyCritical),
        }
    }

    /// Opens the lane the timeline shows `chosen` on. Says whether the path
    /// holds it.
    fn adopt_chosen(&mut self, chosen: Chosen) -> bool {
        let Some(profile) = self.profile().cloned() else {
            return false;
        };
        let Some(item) = self.item_of(chosen) else {
            return false;
        };
        let shown_on = item.slices().first().map(|at| at.track());
        if let Some(track) = shown_on {
            self.open_lane_of(&profile, track);
        }
        true
    }

    /// Has the chart draw the lane of `track`: kept in its group's sample,
    /// and the group expanded where it is collapsed. A track the reader hid
    /// stays hidden.
    fn open_lane_of(&mut self, profile: &Profile, track: usize) {
        self.keep_in_sample(Some(track));
        let drawn = self
            .chart_lanes()
            .is_some_and(|chart| chart.lanes().any(|lane| lane.track == track));
        if drawn || !self.visible_tracks.get(track).copied().unwrap_or(false) {
            return;
        }
        let class = profile.threads()[track].lane_class;
        if let Some(group) = STACKED_GROUPS.iter().position(|&g| g == class)
            && self.collapsed_groups[group]
        {
            self.collapsed_groups[group] = false;
            self.invalidate_visible_lanes();
        }
    }

    fn keep_in_sample(&mut self, track: Option<usize>) {
        if self.kept_in_sample != track {
            self.kept_in_sample = track;
            self.invalidate_visible_lanes();
        }
    }

    /// Lets the lane what was let go of was shown on fall back to its
    /// group's sample.
    fn release_chosen_lane(&mut self) {
        self.keep_in_sample(None);
    }

    pub(crate) fn visible_tracks(&self) -> &[bool] {
        &self.visible_tracks
    }

    pub(crate) fn shared_track_names(&self) -> &[bool] {
        &self.shared_track_names
    }

    pub(crate) fn selected_tracks(&self) -> &[bool] {
        &self.selected_tracks
    }

    pub(crate) fn time_selection(&self) -> &TimeSelection {
        &self.time_selection
    }

    /// Takes `source` in place of the document, dropping what was derived
    /// from the one it replaces and what was chosen in it, which `session`
    /// holds, and for a loaded profile standing its tracks in their defaults:
    /// every one visible, every one but the critical-path lane's selected.
    /// The critical-path lane holds redirected copies of worker time, so
    /// leaving it out of the default selection keeps the aggregate from
    /// double-counting that time. Says whether `session` let go of a choice.
    fn replace_source(&mut self, source: Source, session: &mut Session) -> bool {
        let Self {
            source: replaced,
            visible_tracks,
            shared_track_names,
            selected_tracks,
            time_selection,
            collapsed_groups,
            group_show_all,
            kept_in_sample,
            track_sort,
            flame_mode: _,
            aggregate_key,
            select_anchor,
            visible_lanes,
            group_chrome,
            aggregates,
            agg,
        } = self;
        visible_lanes.get_mut().take();
        group_chrome.get_mut().take();
        *aggregates = None;
        agg.invalidate();
        time_selection.clear();
        *kept_in_sample = None;
        *replaced = source;
        if let Source::Loaded(loaded) = replaced {
            let profile = loaded.get().profile();
            *visible_tracks = vec![true; profile.threads().len()];
            *shared_track_names = shared_names(profile);
            *collapsed_groups = default_collapsed_groups();
            *group_show_all = [false; STACKED_GROUPS.len()];
            *track_sort = TrackSort::default();
            *selected_tracks = profile
                .threads()
                .iter()
                .map(|t| t.lane_class != LaneClass::CriticalPath)
                .collect();
            *select_anchor = None;
            *aggregate_key += 1;
        }
        session.chosen().is_some() && session.choose(None)
    }

    /// The lanes the flame chart draws: the full layout restricted to the
    /// visible tracks and restacked into lane-class groups, cached until the
    /// state it is built under moves. Every paint and hit-test
    /// works from this, so a lane hidden by its group is neither drawn nor
    /// selectable.
    pub(crate) fn chart_lanes(&self) -> Option<Rc<ChartLanes>> {
        self.ensure_grouped();
        self.visible_lanes.borrow().clone()
    }

    /// The group headers and show-more rows over [`Self::chart_lanes`]'
    /// scrolled lanes.
    pub(crate) fn group_chrome(&self) -> Option<Rc<GroupChrome>> {
        self.ensure_grouped();
        self.group_chrome.borrow().clone()
    }

    /// Builds and caches the grouped lane layout and its chrome if stale.
    ///
    /// It fills their cells through `&self` because every hit-test and paint
    /// asks for a layout through a shared `&TraceModel`, several of them from
    /// inside a `read` of a panel that holds the model, where no `&mut` to the
    /// model can be taken.
    fn ensure_grouped(&self) {
        if self.visible_lanes.borrow().is_some() {
            return;
        }
        let Some(loaded) = self.loaded() else {
            return;
        };
        let (layout, chrome) = build_grouped_layout(
            loaded.profile(),
            &loaded.presentation.lanes,
            &GroupLayoutState {
                visible: &self.visible_tracks,
                collapsed: &self.collapsed_groups,
                show_all: &self.group_show_all,
                kept: self.kept_in_sample,
                sort: self.track_sort,
            },
        );
        *self.visible_lanes.borrow_mut() = Some(Rc::new(layout));
        *self.group_chrome.borrow_mut() = Some(Rc::new(chrome));
    }

    /// Drops the cached visible layout and chrome so they rebuild and restack.
    fn invalidate_visible_lanes(&mut self) {
        self.visible_lanes.borrow_mut().take();
        self.group_chrome.borrow_mut().take();
    }

    pub(crate) fn track_sort(&self) -> TrackSort {
        self.track_sort
    }

    /// Stands every group's tracks in `sort` and restacks the chart under it, or
    /// reports false when it is already the standing order, so re-choosing one
    /// costs nothing.
    pub(crate) fn set_track_sort(&mut self, sort: TrackSort) -> bool {
        if self.track_sort == sort {
            return false;
        }
        self.track_sort = sort;
        self.invalidate_visible_lanes();
        true
    }

    /// Toggles a lane-class group's collapse and rebuilds the grouped layout.
    pub(crate) fn toggle_group_collapsed(&mut self, group: usize) {
        if let Some(c) = self.collapsed_groups.get_mut(group) {
            *c = !*c;
            self.invalidate_visible_lanes();
        }
    }

    /// Toggles whether a group shows every lane or only its leading ones.
    pub(crate) fn toggle_group_show_all(&mut self, group: usize) {
        if let Some(s) = self.group_show_all.get_mut(group) {
            *s = !*s;
            self.invalidate_visible_lanes();
        }
    }

    /// The number of visible tracks and the total.
    pub(crate) fn visible_count(&self) -> (usize, usize) {
        let visible = self.visible_tracks.iter().filter(|v| **v).count();
        (visible, self.visible_tracks.len())
    }

    /// Toggles whether track `i` is drawn in the flame chart, dropping the cached
    /// visible layout so it restacks.
    pub(crate) fn toggle_track(&mut self, i: usize) {
        if let Some(v) = self.visible_tracks.get_mut(i) {
            *v = !*v;
            self.invalidate_visible_lanes();
        }
    }

    /// Shows or hides every track at once.
    pub(crate) fn set_all_visible(&mut self, visible: bool) {
        for v in &mut self.visible_tracks {
            *v = visible;
        }
        self.invalidate_visible_lanes();
    }

    /// Shows or hides every track in one lane-class group at once.
    pub(crate) fn set_group_visible(&mut self, class: LaneClass, visible: bool) {
        let targets: Vec<usize> = {
            let Some(profile) = self.profile() else {
                return;
            };
            profile
                .threads()
                .iter()
                .enumerate()
                .filter(|(_, t)| t.lane_class == class)
                .map(|(i, _)| i)
                .collect()
        };
        for i in targets {
            if let Some(v) = self.visible_tracks.get_mut(i) {
                *v = visible;
            }
        }
        self.invalidate_visible_lanes();
    }

    pub(crate) fn flame_mode(&self) -> FlameMode {
        self.flame_mode
    }

    /// Adopts `mode` and keys a rebuild for it, or reports false when it is
    /// already the standing mode, so re-choosing one costs nothing.
    fn adopt_flame_mode(&mut self, mode: FlameMode) -> bool {
        if self.flame_mode == mode {
            return false;
        }
        self.flame_mode = mode;
        self.aggregate_key += 1;
        true
    }

    /// The aggregated flame graph and stats for the current track selection, or
    /// `None` until the first background build for the loaded profile completes.
    /// Reflects the previous selection while a rebuild is still running;
    /// [`Self::ensure_aggregates`] owns the (re)build.
    pub(crate) fn aggregates(&self) -> Option<Arc<ScopeAggregate>> {
        self.aggregates.clone()
    }

    /// The `aggregate_key` the ready aggregate reflects, if any; the stats
    /// panel keys its sort order on it.
    pub(crate) fn aggregate_version(&self) -> Option<u64> {
        self.agg.ready()
    }

    /// Whether a background aggregate build is currently running.
    pub(crate) fn aggregate_building(&self) -> bool {
        self.agg.is_building()
    }

    /// The visible-lane tracks from the selection anchor to `track` inclusive, in
    /// display order; `None` with no anchor or one no longer a visible lane.
    fn visible_run_to(&self, track: usize) -> Option<Vec<usize>> {
        let anchor = self.select_anchor?;
        let order: Vec<usize> = self.chart_lanes()?.lanes().map(|l| l.track).collect();
        let ai = order.iter().position(|&t| t == anchor)?;
        let ti = order.iter().position(|&t| t == track)?;
        Some(order[ai.min(ti)..=ai.max(ti)].to_vec())
    }
}

/// What `profile` gives a clock: the wall-clock span its events cover, `None`
/// where it records no wall-clock origin or covers no time.
fn clock_source(profile: &Profile) -> Option<bviz_ui_playback::Source> {
    let origin = profile.metadata().epoch_origin?;
    let span = profile.span();
    let covered = Span::new(
        origin.plus_micros(span.start_us),
        origin.plus_micros(span.end_us),
    )?;
    Some(bviz_ui_playback::Source::new(covered))
}

#[cfg(test)]
mod tests {
    use super::{
        Chosen, Derivations, Joined, LoadedProfile, Presentation, Recorded, TraceState, TracedPath,
        Tracing, clock_source, critical_steps, nearly_critical_actions, step_slices,
    };
    use crate::groups::STACKED_GROUPS;
    use crate::track_order::TrackSort;
    use crate::{Source, TOP_N_PER_GROUP};
    use bviz_critical_path::CriticalPath;
    use bviz_invocation::spawn::LoggedSpawns;
    use bviz_invocation::{ActionGraph, ConfiguredTarget, Session};
    use bviz_trace_layout::{
        FlameDirection, FlameLabels, FlameMode, FlameWeight, TimeSelection, TrackFlamer,
        span_between,
    };
    use bviz_trace_profile::{Profile, TimeSpan};
    use gpui::Task;
    use std::sync::Arc;

    /// A document whose read has not finished, for exercising the state a panel
    /// drives it through.
    fn model() -> TraceState {
        TraceState::new()
    }

    /// A profile of one slice over `[0, 100)` whose writer started at
    /// `start_ms` past the epoch, where it says when.
    fn profile_started_at(start_ms: Option<i64>) -> Profile {
        let other_data = start_ms.map_or(String::new(), |ms| {
            format!(r#""otherData":{{"profile_start_ts":{ms}}},"#)
        });
        let json = format!(
            r#"{{{other_data}"traceEvents":[{{"cat":"general information","name":"a","ph":"X","ts":0,"dur":100,"tid":1}}]}}"#
        );
        Profile::from_bytes(json.as_bytes())
            .expect("parse test profile")
            .into_inner()
    }

    #[test]
    fn a_profile_saying_when_it_started_plays_on_a_clock() {
        assert!(clock_source(&profile_started_at(Some(1_000))).is_some());
    }

    #[test]
    fn a_profile_not_saying_when_it_started_gives_a_clock_nothing_to_play() {
        assert!(clock_source(&profile_started_at(None)).is_none());
    }

    /// Derivations that have finished.
    fn derivations() -> Derivations {
        Derivations {
            working_lanes: Task::ready(()),
            critical_path: tracing(Joined::default()),
        }
    }

    /// A tracing joined to `joined` that has finished.
    fn tracing(joined: Joined) -> Tracing {
        Tracing {
            joined,
            _task: Task::ready(()),
        }
    }

    /// The flamer of a profile of one lane working over `[0, 100)`.
    fn flamer() -> Arc<TrackFlamer> {
        let json = r#"{"traceEvents":[
            {"name":"thread_name","ph":"M","tid":1,"args":{"name":"skyframe-evaluator-execution-1"}},
            {"cat":"general information","name":"a","ph":"X","ts":0,"dur":100,"tid":1}
        ]}"#;
        let profile = Profile::from_bytes(json.as_bytes())
            .expect("parse test profile")
            .into_inner();
        Arc::new(TrackFlamer::new(&Arc::new(profile)))
    }

    /// The repositories a profile's labels name are shown as a graph of the
    /// same build shows them, told apart among those the profile names.
    #[test]
    fn a_profile_s_labels_are_shown_by_the_repositories_it_names() {
        let json = r#"{"traceEvents":[
            {"name":"thread_name","ph":"M","tid":1,"args":{"name":"skyframe-evaluator-execution-1"}},
            {"cat":"action processing","name":"Compiling a","ph":"X","ts":0,"dur":10,"tid":1,"args":{"target":"@@llvm++llvm+llvm-project//llvm:Support"}},
            {"cat":"action processing","name":"Compiling b","ph":"X","ts":20,"dur":10,"tid":1,"args":{"target":"//b:b"}}
        ]}"#;
        let profile = Profile::from_bytes(json.as_bytes())
            .expect("parse test profile")
            .into_inner();
        let repos = super::repo_names(&profile);
        let shown = |label| repos.shown_label(label);
        assert_eq!(
            shown("@@llvm++llvm+llvm-project//llvm:Support"),
            "@llvm-project//llvm:Support"
        );
        assert_eq!(shown("//b:b"), "//b");
    }

    #[test]
    fn working_lanes_are_adopted_only_by_the_loaded_profile_they_were_derived_from() {
        let mut model = model();
        let replaced = flamer();
        assert!(!model.adopt_working_lanes(&replaced, replaced.working_lanes()));

        let loaded = flamer();
        model.source = Source::Loaded(Recorded::Complete(LoadedProfile::deriving(
            loaded.clone(),
            derivations(),
            Presentation::of(loaded.profile()),
        )));
        assert!(!model.adopt_working_lanes(&replaced, replaced.working_lanes()));
        assert!(model.working_lanes().is_none());

        assert!(model.adopt_working_lanes(&loaded, loaded.working_lanes()));
        let adopted = model.working_lanes().expect("the loaded profile's lanes");
        assert_eq!(*adopted, loaded.working_lanes());
    }

    #[test]
    fn a_critical_path_is_adopted_only_by_the_loaded_profile_it_was_derived_from() {
        let unlogged = Joined::default();
        let mut model = model();
        let replaced = flamer();
        assert!(!model.adopt_critical_path(replaced.profile(), unlogged, None));

        let loaded = flamer();
        model.source = Source::Loaded(Recorded::Complete(LoadedProfile::deriving(
            loaded.clone(),
            derivations(),
            Presentation::of(loaded.profile()),
        )));
        assert!(!model.adopt_critical_path(replaced.profile(), unlogged, None));
        assert!(matches!(
            model.critical_path(),
            Some(TracedPath::Tracing { .. })
        ));

        assert!(model.adopt_critical_path(loaded.profile(), unlogged, None));
        assert!(matches!(
            model.critical_path(),
            Some(TracedPath::Traced { path: None, .. })
        ));
    }

    #[test]
    fn a_path_traced_before_spawns_were_logged_stands_until_it_is_traced_again_with_them() {
        let mut session = Session::default();
        let by = session.reporter();
        let unlogged = Joined::of(&session);
        let mut model = model();
        let loaded = flamer();
        model.source = Source::Loaded(Recorded::Complete(LoadedProfile::deriving(
            loaded.clone(),
            derivations(),
            Presentation::of(loaded.profile()),
        )));
        assert!(model.adopt_critical_path(loaded.profile(), unlogged, None));

        session.report_logged_spawns(by, LoggedSpawns::default());
        let logged = Joined::of(&session);
        let Some(loaded_profile) = model.loaded_mut() else {
            panic!("a loaded profile");
        };
        loaded_profile.critical_path.retrace(tracing(logged));
        assert!(matches!(
            model.critical_path(),
            Some(TracedPath::Traced { path: None, .. })
        ));
        assert!(!model.adopt_critical_path(loaded.profile(), unlogged, None));
        assert!(model.adopt_critical_path(loaded.profile(), logged, None));
    }

    /// A profile read up to where its file is cut off is shown whole as far
    /// as it goes, and said to end early.
    #[test]
    fn a_profile_cut_off_is_shown_as_far_as_it_goes() {
        let mut model = model();
        let loaded = flamer();
        model.source = Source::Loaded(Recorded::Complete(LoadedProfile::deriving(
            loaded.clone(),
            derivations(),
            Presentation::of(loaded.profile()),
        )));
        assert!(!model.cut_off());
        model.source = Source::Loaded(Recorded::CutOff(LoadedProfile::deriving(
            loaded.clone(),
            derivations(),
            Presentation::of(loaded.profile()),
        )));
        assert!(model.cut_off());
        assert!(
            model
                .profile()
                .is_some_and(|shown| Arc::ptr_eq(shown, loaded.profile()))
        );
    }

    #[test]
    fn a_path_traced_before_the_action_graph_was_reported_is_traced_again_with_it() {
        let mut session = Session::default();
        let by = session.reporter();
        let ungraphed = Joined::of(&session);
        let mut model = model();
        let loaded = flamer();
        model.source = Source::Loaded(Recorded::Complete(LoadedProfile::deriving(
            loaded.clone(),
            derivations(),
            Presentation::of(loaded.profile()),
        )));
        assert!(model.adopt_critical_path(loaded.profile(), ungraphed, None));

        session.report_action_graph(by, ActionGraph::default());
        let graphed = Joined::of(&session);
        assert_ne!(graphed, ungraphed);
        let Some(loaded_profile) = model.loaded_mut() else {
            panic!("a loaded profile");
        };
        assert_ne!(
            loaded_profile.critical_path.asked(),
            graphed,
            "traced again"
        );
        loaded_profile.critical_path.retrace(tracing(graphed));
        assert!(matches!(
            model.critical_path(),
            Some(TracedPath::Traced { path: None, .. })
        ));
        assert!(!model.adopt_critical_path(loaded.profile(), ungraphed, None));
        assert!(model.adopt_critical_path(loaded.profile(), graphed, None));
    }

    #[test]
    fn a_path_traced_after_the_action_graph_was_reported_is_joined_to_it_already() {
        let mut session = Session::default();
        let by = session.reporter();
        session.report_action_graph(by, ActionGraph::default());
        let graphed = Joined::of(&session);
        let mut model = model();
        let loaded = flamer();
        let derived = Derivations {
            critical_path: tracing(graphed),
            ..derivations()
        };
        model.source = Source::Loaded(Recorded::Complete(LoadedProfile::deriving(
            loaded.clone(),
            derived,
            Presentation::of(loaded.profile()),
        )));
        assert!(model.adopt_critical_path(loaded.profile(), graphed, None));
        let Some(loaded_profile) = model.loaded() else {
            panic!("a loaded profile");
        };
        assert_eq!(loaded_profile.critical_path.asked(), Joined::of(&session));
    }

    #[test]
    fn a_tracing_under_way_gives_way_to_one_joined_to_spawns_logged_since() {
        let mut session = Session::default();
        let by = session.reporter();
        let unlogged = Joined::of(&session);
        session.report_logged_spawns(by, LoggedSpawns::default());
        let logged = Joined::of(&session);
        let mut model = model();
        let loaded = flamer();
        let mut loaded_profile = LoadedProfile::deriving(
            loaded.clone(),
            derivations(),
            Presentation::of(loaded.profile()),
        );
        loaded_profile.critical_path.retrace(tracing(logged));
        model.source = Source::Loaded(Recorded::Complete(loaded_profile));
        assert!(matches!(
            model.critical_path(),
            Some(TracedPath::Tracing { .. })
        ));
        assert!(!model.adopt_critical_path(loaded.profile(), unlogged, None));
        assert!(model.adopt_critical_path(loaded.profile(), logged, None));
    }

    /// The action span running the first step of [`traced`]'s path, on an
    /// execution thread too idle for its group's sample.
    const IDLE_TID: usize = TOP_N_PER_GROUP + 1;

    /// A loaded profile whose critical path is traced: a link of
    /// `[100, 1100)` run on [`IDLE_TID`], then a step of no length, every
    /// track shown.
    fn traced() -> TraceState {
        traced_with(None)
    }

    /// [`traced`], and where `graph` models the build, one whose action
    /// writing `bazel-out/b.o` ran on [`IDLE_TID`] in `[0, 90)` off the path.
    fn traced_with(graph: Option<&ActionGraph>) -> TraceState {
        let mut events = vec![
            r#"{"name":"thread_name","ph":"M","tid":0,"args":{"name":"Critical Path"}}"#
                .to_owned(),
            r#"{"cat":"critical path component","name":"action 'Linking a'","ph":"X","ts":100,"dur":1000,"tid":0}"#.to_owned(),
            r#"{"cat":"critical path component","name":"action 'Linking a'","ph":"i","ts":2000,"tid":0}"#.to_owned(),
        ];
        for tid in 1..=IDLE_TID {
            events.push(format!(
                r#"{{"name":"thread_name","ph":"M","tid":{tid},"args":{{"name":"skyframe-evaluator-execution-{tid}"}}}}"#
            ));
        }
        for tid in 1..IDLE_TID {
            for ts in [0, 10, 20] {
                events.push(format!(
                    r#"{{"cat":"general information","name":"busy","ph":"X","ts":{ts},"dur":5,"tid":{tid}}}"#
                ));
            }
        }
        events.push(format!(
            r#"{{"cat":"action processing","name":"Linking a","ph":"X","ts":105,"dur":996,"tid":{IDLE_TID},"out":"bazel-out/a"}}"#
        ));
        events.push(format!(
            r#"{{"cat":"action processing","name":"Compiling b","ph":"X","ts":0,"dur":90,"tid":{IDLE_TID},"out":"bazel-out/b.o"}}"#
        ));
        let json = format!(r#"{{"traceEvents":[{}]}}"#, events.join(","));
        let profile = Profile::from_bytes(json.as_bytes())
            .expect("parse test profile")
            .into_inner();
        let path = CriticalPath::of(&profile, None, graph);
        let loaded = Arc::new(TrackFlamer::new(&Arc::new(profile)));
        let mut model = model();
        model.source = Source::Loaded(Recorded::Complete(LoadedProfile::deriving(
            loaded.clone(),
            derivations(),
            Presentation::of(loaded.profile()),
        )));
        assert!(model.adopt_critical_path(loaded.profile(), Joined::default(), path));
        let tracks = loaded.profile().threads().len();
        model.visible_tracks = vec![true; tracks];
        model.selected_tracks = vec![true; tracks];
        model
    }

    /// Whether the chart draws a lane of `track`.
    fn drawn(model: &TraceState, track: usize) -> bool {
        let chart = model.chart_lanes().expect("a laid-out chart");
        chart.lanes().any(|lane| lane.track == track)
    }

    #[test]
    fn a_chosen_step_opens_the_lane_its_action_ran_on_until_let_go_of() {
        let mut model = traced();
        let step = model.steps().first().expect("a step");
        let [span, own] = step_slices(step).collect::<Vec<_>>()[..] else {
            panic!("a step joined to its span")
        };
        assert_eq!(own, step.at());
        assert!(drawn(&model, own.track()));
        assert!(!drawn(&model, span.track()), "left out of the sample");

        assert!(model.adopt_chosen(Chosen::Step(0)));
        assert!(drawn(&model, span.track()));
        assert_eq!(model.group_show_all, [false; STACKED_GROUPS.len()]);

        model.release_chosen_lane();
        assert!(!drawn(&model, span.track()), "back out of the sample");
    }

    #[test]
    fn choosing_a_step_and_letting_go_of_it_leave_the_time_selection() {
        let mut model = traced();
        let mut selection = TimeSelection::default();
        selection.set(span_between(5, 50).expect("a region"));
        selection.insert(span_between(2000, 2100).expect("a region"));
        model.time_selection = selection.clone();
        assert!(model.adopt_chosen(Chosen::Step(0)));
        assert_eq!(model.time_selection, selection);
        model.release_chosen_lane();
        assert_eq!(model.time_selection, selection);
    }

    #[test]
    fn a_replaced_document_holds_no_chosen_step() {
        let mut model = traced();
        let mut session = Session::default();
        let by = session.reporter();
        let steps = critical_steps(model.steps());
        session.report_critical_path(by, steps, Vec::new());
        assert!(model.adopt_chosen(Chosen::Step(0)));
        assert!(session.choose(Some(Chosen::Step(0))));
        assert!(model.replace_source(Source::Loading, &mut session));
        assert_eq!(session.chosen(), None);
        assert_eq!(model.kept_in_sample, None);
    }

    #[test]
    fn a_replaced_profile_leaves_no_group_headers() {
        let mut model = traced();
        assert!(model.group_chrome().is_some());
        model.replace_source(Source::Loading, &mut Session::default());
        assert!(model.group_chrome().is_none());
    }

    #[test]
    fn a_step_is_reported_by_its_primary_output_and_its_configured_target() {
        const CONFIGURATION: &str =
            "4f8a2bd4c1e07b3e9a6d5f2c8b1e0a7d3c6f9b2e5a8d1c4f7b0e3a6d9c2f5b8e";
        let json = format!(
            r#"{{"traceEvents":[
            {{"name":"thread_name","ph":"M","tid":0,"args":{{"name":"Critical Path"}}}},
            {{"name":"thread_name","ph":"M","tid":1,"args":{{"name":"skyframe-evaluator-execution-1"}}}},
            {{"cat":"critical path component","name":"action 'Linking a'","ph":"X","ts":100,"dur":1000,"tid":0}},
            {{"cat":"action processing","name":"Linking a","ph":"X","ts":105,"dur":996,"tid":1,"out":"bazel-out/a","args":{{"target":"//a:a","configuration":"{CONFIGURATION}"}}}},
            {{"cat":"critical path component","name":"action 'Linking b'","ph":"X","ts":2000,"dur":10,"tid":0}}]}}"#
        );
        let profile = Profile::from_bytes(json.as_bytes())
            .expect("parse test profile")
            .into_inner();
        let path = CriticalPath::of(&profile, None, None).expect("a critical path");
        let steps = critical_steps(path.steps());
        assert_eq!(steps[0].primary_output(), Some("bazel-out/a"));
        assert_eq!(
            steps[0].owner(),
            Some(ConfiguredTarget::new("//a:a", Some(CONFIGURATION)))
        );
        assert_eq!((steps[1].primary_output(), steps[1].owner()), (None, None));
    }

    #[test]
    fn a_step_past_the_path_is_not_chosen() {
        let mut model = traced();
        assert!(model.adopt_chosen(Chosen::Step(1)));
        assert!(model.item_of(Chosen::Step(1)).is_some());
        assert!(!model.adopt_chosen(Chosen::Step(2)));
        assert!(model.item_of(Chosen::Step(2)).is_none());
        assert!(
            !model.adopt_chosen(Chosen::NearlyCritical(0)),
            "none modeled"
        );
    }

    #[test]
    fn a_chosen_nearly_critical_action_opens_the_lane_of_its_span() {
        let graph = ActionGraph::new(
            vec![
                Some("bazel-out/b.o".to_owned()),
                Some("bazel-out/a".to_owned()),
            ],
            vec![(1, 0)],
        );
        let mut model = traced_with(Some(&graph));
        let [action] = model.nearly_critical() else {
            panic!("one action nearly critical")
        };
        let at = action.at;
        assert_eq!(
            nearly_critical_actions(model.nearly_critical())[0].primary_output(),
            Some("bazel-out/b.o")
        );
        assert!(!drawn(&model, at.track()), "left out of the sample");

        assert!(model.adopt_chosen(Chosen::NearlyCritical(0)));
        assert!(drawn(&model, at.track()));
        let item = model.item_of(Chosen::NearlyCritical(0)).expect("an action");
        assert_eq!(item.slices(), [at]);
        let profile = model.profile().expect("a profile").clone();
        assert_eq!(
            item.time(&profile),
            TimeSpan {
                start_us: 0,
                end_us: 90
            }
        );
        assert!(!model.adopt_chosen(Chosen::NearlyCritical(1)));
    }

    #[test]
    fn re_choosing_the_standing_flame_mode_keys_no_rebuild() {
        let mut model = model();
        let key = model.aggregate_key;
        let standing = model.flame_mode();
        assert!(!model.adopt_flame_mode(standing));
        assert_eq!(model.aggregate_key, key);
    }

    #[test]
    fn a_weight_chosen_keys_a_rebuild() {
        let mut model = model();
        let key = model.aggregate_key;
        let standing = model.flame_mode();
        assert_eq!(standing.weight, FlameWeight::Duration);

        let asked = FlameMode {
            weight: FlameWeight::WallShare,
            ..standing
        };
        assert!(model.adopt_flame_mode(asked));
        assert_eq!(model.aggregate_key, key + 1);
        assert_eq!(model.flame_mode().weight, FlameWeight::WallShare);

        assert!(!model.adopt_flame_mode(asked));
        assert_eq!(model.aggregate_key, key + 1);
    }

    #[test]
    fn re_choosing_the_standing_track_order_restacks_nothing() {
        let mut model = model();
        assert!(!model.set_track_sort(model.track_sort()));
        assert!(model.set_track_sort(TrackSort::Weight));
        assert_eq!(model.track_sort(), TrackSort::Weight);
        assert!(!model.set_track_sort(TrackSort::Weight));
    }

    #[test]
    fn each_part_of_the_flame_mode_keys_a_rebuild_of_its_own() {
        let mut model = model();
        let key = model.aggregate_key;

        let inverted = FlameMode {
            direction: FlameDirection::BottomUp,
            ..model.flame_mode()
        };
        assert!(model.adopt_flame_mode(inverted));
        assert_eq!(model.flame_mode(), inverted);
        assert_eq!(model.aggregate_key, key + 1);

        // The labelling moves independently of the direction, which the new mode
        // carries over.
        let renamed = FlameMode {
            labels: FlameLabels::Name,
            ..inverted
        };
        assert!(model.adopt_flame_mode(renamed));
        assert_eq!(model.flame_mode(), renamed);
        assert_eq!(model.aggregate_key, key + 2);

        let reweighed = FlameMode {
            weight: FlameWeight::WallShare,
            ..renamed
        };
        assert!(model.adopt_flame_mode(reweighed));
        assert_eq!(model.flame_mode(), reweighed);
        assert_eq!(model.aggregate_key, key + 3);
    }
}
