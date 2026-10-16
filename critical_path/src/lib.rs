//! Where the wall time of a Bazel build went along its critical path, as the
//! build's `--profile` records it, and as its execution log
//! (`--execution_log_compact_file`) times the spawns of the path's actions.
//!
//! Bazel's critical path is a chain of actions, each waiting on the one
//! before. The profile's critical-path lane holds one span per step, timed
//! from when the action won its execution slot to when its outputs were
//! checked.

mod logged;
mod modeled;
mod schedule;
mod spans;

use bviz_invocation::ActionGraph;
use bviz_invocation::spawn::{LoggedSpawns, SpawnBytes};
pub use bviz_invocation::spawn::{Phase, SpawnRunner};
use bviz_trace_profile::{LaneClass, OptStr, Profile, SliceAt, TimeSpan};
pub use modeled::{Modeled, NEARLY_CRITICAL_KEPT, NearlyCritical, StepModel};
pub use schedule::{Ceilings, Finish, Schedule};
use spans::Spans;
use std::borrow::Cow;
use std::time::Duration;

/// What Bazel wraps an action's description in to name its step.
const ACTION_NAME_PREFIX: &str = "action '";
const ACTION_NAME_SUFFIX: &str = "'";
/// What Bazel puts before a target's label to name the step building the
/// target's runfiles tree.
const RUNFILES_NAME_PREFIX: &str = "runfiles for ";
/// The mnemonic of the action building a runfiles tree, which also opens the
/// action's description.
const RUNFILES_MNEMONIC: &str = "RunfilesTree";

/// The profile's wall time, partitioned along its critical path: the
/// lead-in, then each step's wait and own time, then the tail add up to the
/// wall time exactly.
#[derive(Debug, Clone, PartialEq)]
pub struct CriticalPath {
    wall: Duration,
    steps: Vec<Step>,
    by_part: [Duration; Part::ALL.len()],
    /// `None` where no execution log was given.
    coverage: Option<Coverage>,
    /// `None` where no action graph was given.
    modeled: Option<Modeled>,
}

/// The steps of a critical path whose action the execution log records
/// spawns of, taken together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Coverage {
    steps: usize,
    own: Duration,
    phases: [Duration; Phase::ALL.len()],
    around: Duration,
    /// Whether the log records a spawn executed remotely anywhere in the
    /// build, on the path or off it: evidence that remote execution was
    /// available to its actions.
    remote_executed: bool,
}

impl Coverage {
    fn new(log: &LoggedSpawns) -> Coverage {
        Coverage {
            steps: 0,
            own: Duration::ZERO,
            phases: [Duration::ZERO; Phase::ALL.len()],
            around: Duration::ZERO,
            remote_executed: log
                .iter()
                .any(|spawn| spawn.ran_on() == SpawnRunner::RemoteExecution),
        }
    }

    fn add(&mut self, own: Duration, spawned: &Spawned) {
        self.steps += 1;
        self.own += own;
        for (sum, phase) in self.phases.iter_mut().zip(spawned.phases) {
            *sum += phase;
        }
        self.around += spawned.around();
    }

    pub fn steps(&self) -> usize {
        self.steps
    }

    /// The steps' own time summed.
    pub fn own(&self) -> Duration {
        self.own
    }

    /// The steps' actions' time on the path in `phase`, as their spawns'
    /// phases are fitted to it.
    pub fn phase(&self, phase: Phase) -> Duration {
        self.phases[phase.index()]
    }

    /// The steps' actions' time on the path outside their spawns' phases.
    pub fn around(&self) -> Duration {
        self.around
    }

    /// The steps' actions' time on the path, which the phases and the time
    /// around them partition.
    pub fn action(&self) -> Duration {
        self.phases.iter().sum::<Duration>() + self.around
    }
}

/// One step of the critical path.
#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    name: String,
    mnemonic: Option<String>,
    target: Option<String>,
    configuration: Option<String>,
    primary_output: Option<String>,
    at: SliceAt,
    start_us: i64,
    gap: Duration,
    work: Work,
    model: Option<StepModel>,
}

/// What a step did with its own time, as far as the profile tells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Work {
    /// An action whose span the profile holds.
    Spanned(SpanSplit),
    /// An action the profile holds no span of. Bazel opens none for an
    /// action its action cache answers, and a slim profile folds short spans
    /// into anonymous ones, so the profile alone cannot tell the two apart.
    Unspanned(Ran),
    /// An action Bazel records as a step taking no time: change pruning found
    /// its inputs unchanged, so it was not run.
    ChangePruned,
}

/// A spanned step's own time, around the action span.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpanSplit {
    /// Where the action span stands in the profile.
    pub at: SliceAt,
    /// From the step's start until the span starts: input discovery and the
    /// action-cache check.
    pub before: Duration,
    /// From the span's start until the step's end. Bazel ends the step from
    /// within the span, so the span outlasts the step by only that ending.
    pub span: Ran,
}

/// The time an action ran for on the path, and the spawns the execution log
/// records of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(
    clippy::large_enum_variant,
    reason = "one per step of a critical path, too few for the unlogged ones' room to matter"
)]
pub enum Ran {
    /// The log records no spawn of the action, or no log was given.
    Unlogged(Duration),
    Spawned(Spawned),
}

/// The spawns an execution log records of one action, their phases fitted
/// into the time the action ran for on the path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spawned {
    runner: SpawnRunner,
    spawns: usize,
    logged: Duration,
    phases: [Duration; Phase::ALL.len()],
    bytes: SpawnBytes,
    fit: Fit,
}

/// How the phases of an action's spawns fit the time it ran for. Spawn times
/// are durations Bazel measured, never instants: a remote spawn's start is
/// the executor's clock, and a cache hit's that of the run it reuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fit {
    /// The phases take their own times, and what is left of the action's time
    /// is Bazel's work around the spawns, which no spawn's metrics count.
    Within { around: Duration },
    /// The spawns' phases add up past the action's time, so each is scaled
    /// down by one ratio to fill it exactly; their mix is kept, not their
    /// times.
    Scaled,
}

/// A share of the wall time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Part {
    /// Until the first step starts.
    LeadIn,
    /// Between one step's end and the next's start.
    Waiting,
    BeforeAction,
    Action,
    /// The own time of a step the profile holds no action span of.
    Unspanned,
    /// After the last step ends.
    Tail,
}

impl Part {
    /// Every part, in the order time passes through them.
    pub const ALL: [Part; 6] = [
        Part::LeadIn,
        Part::Waiting,
        Part::BeforeAction,
        Part::Action,
        Part::Unspanned,
        Part::Tail,
    ];

    /// The part's place in [`ALL`](Self::ALL).
    pub const fn index(self) -> usize {
        self as usize
    }
}

impl Work {
    pub fn own(self) -> Duration {
        self.parts().iter().map(|&(_, time)| time).sum()
    }

    /// The own time by part, parts of no time included.
    fn parts(self) -> [(Part, Duration); 3] {
        let (before, span, unspanned) = match self {
            Work::Spanned(split) => (split.before, split.span.time(), Duration::ZERO),
            Work::Unspanned(own) => (Duration::ZERO, Duration::ZERO, own.time()),
            Work::ChangePruned => (Duration::ZERO, Duration::ZERO, Duration::ZERO),
        };
        [
            (Part::BeforeAction, before),
            (Part::Action, span),
            (Part::Unspanned, unspanned),
        ]
    }

    /// The work with the time its action ran for joined to `spawns`.
    fn with_spawns(self, spawns: &logged::Tally) -> Work {
        let spawned = |ran: Ran| Ran::Spawned(Spawned::fitted(ran.time(), spawns));
        match self {
            Work::Spanned(SpanSplit { at, before, span }) => Work::Spanned(SpanSplit {
                at,
                before,
                span: spawned(span),
            }),
            Work::Unspanned(own) => Work::Unspanned(spawned(own)),
            Work::ChangePruned => Work::ChangePruned,
        }
    }

    /// The spawns the execution log records of the step's action, if any.
    pub fn spawned(&self) -> Option<&Spawned> {
        match self {
            Work::Spanned(SpanSplit {
                span: Ran::Spawned(spawned),
                ..
            })
            | Work::Unspanned(Ran::Spawned(spawned)) => Some(spawned),
            _ => None,
        }
    }
}

impl Ran {
    pub fn time(self) -> Duration {
        match self {
            Ran::Unlogged(time) => time,
            Ran::Spawned(spawned) => spawned.time(),
        }
    }
}

impl Spawned {
    /// The spawns `tally` sums, their phases fitted into `time`.
    fn fitted(time: Duration, tally: &logged::Tally) -> Spawned {
        let spent = tally
            .phases
            .iter()
            .fold(Duration::ZERO, |sum, &phase| sum.saturating_add(phase));
        let (phases, fit) = if spent <= time {
            let around = time - spent;
            (tally.phases, Fit::Within { around })
        } else {
            (scale_to(&tally.phases, spent, time), Fit::Scaled)
        };
        Spawned {
            runner: tally.runner(),
            spawns: tally.spawns,
            logged: tally.logged,
            phases,
            bytes: tally.bytes,
            fit,
        }
    }

    /// What answered the action: of the runners that executed its spawns,
    /// the one they held longest, so that neither a remote attempt failing
    /// fast before a local fallback nor a short secondary spawn decides;
    /// failing any execution, what answered its spawns longest.
    pub fn runner(&self) -> SpawnRunner {
        self.runner
    }

    /// How many spawns the log records of the action.
    pub fn spawns(&self) -> usize {
        self.spawns
    }

    /// The spawns' total times as Bazel measured each, summed.
    pub fn logged(&self) -> Duration {
        self.logged
    }

    /// The time on the path in `phase`.
    pub fn phase(&self, phase: Phase) -> Duration {
        self.phases[phase.index()]
    }

    /// What the spawns wrote and read, summed.
    pub fn bytes(&self) -> SpawnBytes {
        self.bytes
    }

    pub fn fit(&self) -> Fit {
        self.fit
    }

    /// The action's time on the path outside its spawns' phases.
    pub fn around(&self) -> Duration {
        match self.fit {
            Fit::Within { around } => around,
            Fit::Scaled => Duration::ZERO,
        }
    }

    /// The time the action ran for on the path, the phases and the time around
    /// them together.
    pub fn time(&self) -> Duration {
        self.phases.iter().sum::<Duration>() + self.around()
    }
}

/// `phases`, adding up to `spent`, each scaled by one ratio so that they add
/// up to `time` exactly: what rounding down leaves over goes to the longest.
fn scale_to(
    phases: &[Duration; Phase::ALL.len()],
    spent: Duration,
    time: Duration,
) -> [Duration; Phase::ALL.len()] {
    let (spent, time) = (spent.as_nanos(), time.as_nanos());
    let mut scaled = phases.map(|phase| phase.as_nanos() * time / spent);
    let left_over = time - scaled.iter().sum::<u128>();
    let longest = (1..phases.len()).fold(0, |longest, i| match phases[i] > phases[longest] {
        true => i,
        false => longest,
    });
    scaled[longest] += left_over;
    scaled.map(|nanos| Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX)))
}

impl Step {
    /// An action's description, or the name Bazel gives a runfiles tree's
    /// step.
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn mnemonic(&self) -> Option<&str> {
        self.mnemonic.as_deref()
    }

    pub fn target(&self) -> Option<&str> {
        self.target.as_deref()
    }

    /// The checksum of the configuration the step's action ran in.
    pub fn configuration(&self) -> Option<&str> {
        self.configuration.as_deref()
    }

    pub fn primary_output(&self) -> Option<&str> {
        self.primary_output.as_deref()
    }

    /// Where the step's own slice stands in the profile, on its critical-path
    /// lane.
    pub fn at(&self) -> SliceAt {
        self.at
    }

    /// When the step started, on the profile's clock: microseconds since
    /// Bazel began running the command.
    pub fn start_us(&self) -> i64 {
        self.start_us
    }

    /// How long the step waited after the step before it ended; zero for the
    /// first step, whose wait is the path's lead-in.
    pub fn gap(&self) -> Duration {
        self.gap
    }

    /// The time the step adds to the path: of a step that starts before the
    /// one before it ends, only what runs past that end.
    pub fn own(&self) -> Duration {
        self.work.own()
    }

    pub fn work(&self) -> Work {
        self.work
    }

    /// The step's action in the [modeled](CriticalPath::modeled) build,
    /// `None` where no action graph was given or it holds no action of the
    /// step's primary output.
    pub fn model(&self) -> Option<StepModel> {
        self.model
    }

    /// The wait and own time by part, in the order they passed; parts the
    /// step spent no time in are left out.
    pub fn parts(&self) -> impl Iterator<Item = (Part, Duration)> {
        std::iter::once((Part::Waiting, self.gap))
            .chain(self.work.parts())
            .filter(|(_, time)| !time.is_zero())
    }
}

impl CriticalPath {
    /// The critical path the profile records, or `None` where it has no
    /// critical-path lane. Steps stand in the order they started, each joined
    /// to the spawns `log` records of its action where an execution log of
    /// the same invocation is given, and the build modeled where its action
    /// `graph` is.
    pub fn of(
        profile: &Profile,
        log: Option<&LoggedSpawns>,
        graph: Option<&ActionGraph>,
    ) -> Option<CriticalPath> {
        let (lane_at, lane) = profile
            .threads()
            .iter()
            .enumerate()
            .find(|(_, track)| track.lane_class == LaneClass::CriticalPath)?;
        let recorded: Vec<RecordedStep> = profile
            .slices(lane)
            .iter()
            .zip(profile.details(lane))
            .zip(profile.slices_at(lane_at))
            .map(|((slice, detail), at)| {
                let name = profile.str(slice.name);
                let kind = match name.strip_prefix(RUNFILES_NAME_PREFIX) {
                    Some(label) => Recorded::Runfiles { name, label },
                    None => Recorded::Action {
                        description: name
                            .strip_prefix(ACTION_NAME_PREFIX)
                            .and_then(|name| name.strip_suffix(ACTION_NAME_SUFFIX))
                            .unwrap_or(name),
                    },
                };
                RecordedStep {
                    at,
                    start_us: slice.start_us,
                    end_us: slice.end_us(),
                    mnemonic: detail.mnemonic,
                    target: detail.target,
                    configuration: detail.configuration,
                    primary_output: profile.opt_str(detail.out),
                    kind,
                }
            })
            .collect();
        let actions = profile.actions();
        let mut spans = Spans::new(profile, &actions, &recorded);
        let mut cursor = Cursor::new(profile.span());
        let wall = cursor.left();
        let lead_in = recorded
            .first()
            .map_or(Duration::ZERO, |first| cursor.advance_to(first.start_us));
        let mut steps: Vec<Step> = recorded
            .iter()
            .map(|recorded| recorded.resolve(profile, &mut spans, &mut cursor))
            .collect();
        let tail = cursor.left();
        let coverage = log.map(|log| {
            let tallies = logged::join(&steps, log);
            let mut coverage = Coverage::new(log);
            for (step, tally) in steps.iter_mut().zip(tallies) {
                if let Some(tally) = tally {
                    step.work = step.work.with_spawns(&tally);
                    if let Some(spawned) = step.work.spawned() {
                        coverage.add(step.own(), spawned);
                    }
                }
            }
            coverage
        });
        let mut path = CriticalPath::new(wall, lead_in, steps, tail, coverage);
        if let Some(graph) = graph {
            path.modeled = Some(modeled::model(profile, &actions, graph, &mut path));
        }
        Some(path)
    }

    fn new(
        wall: Duration,
        lead_in: Duration,
        steps: Vec<Step>,
        tail: Duration,
        coverage: Option<Coverage>,
    ) -> CriticalPath {
        let mut by_part = [Duration::ZERO; Part::ALL.len()];
        by_part[Part::LeadIn.index()] = lead_in;
        by_part[Part::Tail.index()] = tail;
        for (part, time) in steps.iter().flat_map(Step::parts) {
            by_part[part.index()] += time;
        }
        CriticalPath {
            wall,
            steps,
            by_part,
            coverage,
            modeled: None,
        }
    }

    /// The build modeled from its action graph, `None` where none was given.
    pub fn modeled(&self) -> Option<&Modeled> {
        self.modeled.as_ref()
    }

    /// How much of the path the execution log accounts for, `None` where no
    /// log was given.
    pub fn coverage(&self) -> Option<Coverage> {
        self.coverage
    }

    /// The profile's wall time, from its first event to its last.
    pub fn wall(&self) -> Duration {
        self.wall
    }

    /// From the start of the profile until the first step starts: launching
    /// Bazel, loading, analysis, and whatever else ran before any action of
    /// the path could.
    pub fn lead_in(&self) -> Duration {
        self.time_in(Part::LeadIn)
    }

    pub fn steps(&self) -> &[Step] {
        &self.steps
    }

    /// From the end of the last step until the end of the profile.
    pub fn tail(&self) -> Duration {
        self.time_in(Part::Tail)
    }

    /// The steps' own time summed.
    pub fn path(&self) -> Duration {
        self.steps.iter().map(Step::own).sum()
    }

    /// The wall time spent in `part`; the parts add up to [`Self::wall`].
    pub fn time_in(&self, part: Part) -> Duration {
        self.by_part[part.index()]
    }

    /// The step that waited longest after the step before it ended, if any
    /// waited at all.
    pub fn longest_wait(&self) -> Option<&Step> {
        self.steps
            .iter()
            .filter(|step| !step.gap.is_zero())
            .max_by_key(|step| step.gap)
    }
}

/// What would shorten a critical path, each acting on a share of its time.
/// Levers overlap: a local spawn's execution counts toward
/// [`Execution`](Lever::Execution) and [`LocalOnRemote`](Lever::LocalOnRemote)
/// alike. A phase whose cause differs between where a spawn ran is split
/// into a lever for each, by what answered the step's action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lever {
    /// The lead-in: loading, analysis and fetching repositories.
    LeadIn,
    /// Steps waiting on inputs off the path or for a free job slot.
    Waiting,
    /// Spawns queued for a remote executor.
    RemoteQueue,
    /// Spawns waiting for local resources to run on.
    LocalQueue,
    /// Spawns building their remote execution requests.
    Parse,
    /// Spawns uploading inputs the remote cache lacked.
    Upload,
    /// Spawns waiting on calls to the remote cache.
    Network,
    /// Remote executors staging spawns' inputs.
    RemoteSetup,
    /// Bazel fetching spawns' inputs and setting up their sandboxes or
    /// workers.
    LocalSetup,
    /// Spawns running their commands.
    Execution,
    /// Spawns' outputs moved out of where they ran.
    ProcessOutputs,
    /// Spawns' outputs downloaded.
    Fetch,
    /// Steps executed locally in a build whose execution log shows spawns
    /// executed remotely.
    LocalOnRemote,
    /// Steps with neither an action span nor a logged spawn, likely
    /// action-cache hits, that still took time; only where an execution log
    /// was given, without which a spawned step looks the same.
    CacheHitOverhead,
}

impl Lever {
    pub const ALL: [Lever; 14] = [
        Lever::LeadIn,
        Lever::Waiting,
        Lever::RemoteQueue,
        Lever::LocalQueue,
        Lever::Parse,
        Lever::Upload,
        Lever::Network,
        Lever::RemoteSetup,
        Lever::LocalSetup,
        Lever::Execution,
        Lever::ProcessOutputs,
        Lever::Fetch,
        Lever::LocalOnRemote,
        Lever::CacheHitOverhead,
    ];

    /// The lever's place in [`ALL`](Self::ALL).
    pub const fn index(self) -> usize {
        self as usize
    }
}

impl CriticalPath {
    /// The levers that act on any of the path's time, the one acting on the
    /// most first, and of equal times in the order of [`Lever::ALL`].
    pub fn levers(&self) -> Vec<(Lever, Duration)> {
        let mut levers: Vec<(Lever, Duration)> = Lever::ALL
            .into_iter()
            .map(|lever| (lever, self.acted_on(lever)))
            .filter(|(_, time)| !time.is_zero())
            .collect();
        levers.sort_by_key(|&(_, time)| std::cmp::Reverse(time));
        levers
    }

    /// The most pulling `lever` can shorten the modeled build by: how much
    /// shorter it is were the actions of every step the lever acts on to
    /// take no time. `None` without a model, and for a lever acting on time
    /// the model holds no part of: the lead-in, the waits between steps, and
    /// a step whose action the graph lacks or the model times at none.
    pub fn lever_ceiling(&self, lever: Lever) -> Option<Duration> {
        self.modeled.as_ref()?.lever_ceiling(lever)
    }

    /// The time on the path `lever` acts on.
    fn acted_on(&self, lever: Lever) -> Duration {
        match lever {
            Lever::LeadIn => self.lead_in(),
            Lever::Waiting => self.time_in(Part::Waiting),
            _ => self
                .steps
                .iter()
                .map(|step| self.acted_on_in(lever, step))
                .sum(),
        }
    }

    /// The time of `step`'s own `lever` acts on: none for the lead-in and the
    /// waits, which lie outside every step's own time.
    fn acted_on_in(&self, lever: Lever, step: &Step) -> Duration {
        let work = step.work();
        let spawned = work.spawned();
        let phase_where = |phase: Phase, ran: fn(SpawnRunner) -> bool| {
            spawned
                .filter(|spawned| ran(spawned.runner()))
                .map_or(Duration::ZERO, |spawned| spawned.phase(phase))
        };
        let phase = |phase: Phase| phase_where(phase, |_| true);
        let remotely = |runner: SpawnRunner| runner == SpawnRunner::RemoteExecution;
        let locally =
            |runner: SpawnRunner| runner.executed() && runner != SpawnRunner::RemoteExecution;
        let own_where = |acted_on: bool| match acted_on {
            true => step.own(),
            false => Duration::ZERO,
        };
        match lever {
            Lever::LeadIn | Lever::Waiting => Duration::ZERO,
            Lever::RemoteQueue => phase_where(Phase::Queue, remotely),
            Lever::LocalQueue => phase_where(Phase::Queue, locally),
            Lever::Parse => phase(Phase::Parse),
            Lever::Upload => phase(Phase::Upload),
            Lever::Network => phase(Phase::Network),
            Lever::RemoteSetup => phase_where(Phase::Setup, remotely),
            Lever::LocalSetup => phase_where(Phase::Setup, locally),
            Lever::Execution => phase(Phase::Execute),
            Lever::ProcessOutputs => phase(Phase::ProcessOutputs),
            Lever::Fetch => phase(Phase::Fetch),
            Lever::LocalOnRemote => own_where(
                self.coverage
                    .is_some_and(|coverage| coverage.remote_executed)
                    && spawned.is_some_and(|spawned| locally(spawned.runner())),
            ),
            Lever::CacheHitOverhead => own_where(
                self.coverage.is_some() && matches!(work, Work::Unspanned(Ran::Unlogged(_))),
            ),
        }
    }
}

/// A point walking forward through the profile's wall time, so the times it
/// passes partition the wall time.
struct Cursor {
    at: i64,
    end: i64,
}

impl Cursor {
    fn new(wall: TimeSpan) -> Cursor {
        Cursor {
            at: wall.start_us,
            end: wall.end_us.max(wall.start_us),
        }
    }

    /// Moves on to `to`, held within the wall time left, and returns the time
    /// passed.
    fn advance_to(&mut self, to: i64) -> Duration {
        let to = to.clamp(self.at, self.end);
        let passed = Duration::from_micros(to.abs_diff(self.at));
        self.at = to;
        passed
    }

    /// The wall time left.
    fn left(&self) -> Duration {
        Duration::from_micros(self.end.abs_diff(self.at))
    }
}

/// A step as the critical-path lane records it.
struct RecordedStep<'p> {
    at: SliceAt,
    start_us: i64,
    end_us: i64,
    mnemonic: OptStr,
    target: OptStr,
    configuration: OptStr,
    primary_output: Option<&'p str>,
    kind: Recorded<'p>,
}

enum Recorded<'p> {
    Action { description: &'p str },
    Runfiles { name: &'p str, label: &'p str },
}

impl<'p> RecordedStep<'p> {
    /// The label of the target whose runfiles tree the step built.
    fn runfiles_label(&self) -> Option<&'p str> {
        match self.kind {
            Recorded::Runfiles { label, .. } => Some(label),
            Recorded::Action { .. } => None,
        }
    }

    /// The description of the action span that ran the step, as far as the
    /// step's name gives it. A runfiles tree's action is described by the
    /// tree's directory, named for the target's executable; this takes the
    /// executable to be named for the target, and knows the directory only for
    /// a target of the main repository, whose outputs carry no repository.
    fn span_description(&self) -> Option<Cow<'p, str>> {
        match self.kind {
            Recorded::Action { description } => Some(Cow::Borrowed(description)),
            Recorded::Runfiles { label, .. } => {
                let (package, name) = label.strip_prefix("//")?.split_once(':')?;
                let directory = if package.is_empty() {
                    format!("{name}.runfiles")
                } else {
                    format!("{package}/{name}.runfiles")
                };
                Some(Cow::Owned(format!("{RUNFILES_MNEMONIC} {directory}")))
            }
        }
    }

    /// The step waiting from `cursor` until it starts, its own time running
    /// on from there, its action joined to the span of `spans` that ran it.
    fn resolve(&self, profile: &Profile, spans: &mut Spans, cursor: &mut Cursor) -> Step {
        let gap = cursor.advance_to(self.start_us);
        let (work, span) = if self.end_us == self.start_us {
            (Work::ChangePruned, None)
        } else if let Some(action) = spans.claim(self) {
            let split = SpanSplit {
                at: action.at,
                before: cursor.advance_to(action.span.start_us),
                span: Ran::Unlogged(cursor.advance_to(self.end_us)),
            };
            (Work::Spanned(split), Some(action))
        } else {
            let own = Ran::Unlogged(cursor.advance_to(self.end_us));
            (Work::Unspanned(own), None)
        };
        // What the step records of itself, else what the span that ran it does.
        let said = |of_step: OptStr, of_span: Option<OptStr>| {
            profile
                .opt_str(of_step)
                .or_else(|| profile.opt_str(of_span?))
                .map(str::to_owned)
        };
        let name = match self.kind {
            Recorded::Action { description } => description,
            Recorded::Runfiles { name, .. } => name,
        };
        Step {
            name: name.to_owned(),
            mnemonic: said(self.mnemonic, span.map(|action| action.mnemonic)),
            target: said(self.target, span.map(|action| action.target))
                .or_else(|| self.runfiles_label().map(str::to_owned)),
            configuration: said(self.configuration, span.map(|action| action.configuration)),
            primary_output: self
                .primary_output
                .or_else(|| profile.opt_str(span?.out))
                .map(str::to_owned),
            at: self.at,
            start_us: self.start_us,
            gap,
            work,
            model: None,
        }
    }
}

#[cfg(test)]
mod tests;
