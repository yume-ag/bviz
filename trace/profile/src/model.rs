use bviz_invocation::EpochMicros;

/// An interned-string handle: a byte slice into a text pool. `Copy` and
/// lifetime-free; resolve to `&str` with [`Profile::str`] or by indexing the
/// owning pool with [`Str::range`]. Equality is by `(off, len)` and so is only
/// meaningful within the single pool that produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Str {
    off: u32,
    len: u32,
}

impl Str {
    /// The handle at offset 0, which every pool reserves, so it names no text
    /// a profile holds.
    pub const RESERVED: Str = Str { off: 0, len: 0 };

    pub(crate) const fn new(off: u32, len: u32) -> Str {
        Str { off, len }
    }

    /// The byte range into the owning text pool: `&pool[s.range()]`.
    pub fn range(self) -> std::ops::Range<usize> {
        self.off as usize..(self.off + self.len) as usize
    }

    pub fn is_empty(self) -> bool {
        self.len == 0
    }
}

/// An optional interned-string handle in the size of a bare [`Str`]: offset 0
/// encodes absence. Every interner seeds its pool with a placeholder byte, so
/// no live handle ever has offset 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(transparent)]
pub struct OptStr(Str);

impl OptStr {
    pub const NONE: OptStr = OptStr(Str { off: 0, len: 0 });

    pub fn get(self) -> Option<Str> {
        (self.0.off != 0).then_some(self.0)
    }
}

impl From<Option<Str>> for OptStr {
    fn from(s: Option<Str>) -> OptStr {
        match s {
            Some(s) => {
                debug_assert!(s.off != 0, "offset 0 is the absence encoding");
                OptStr(s)
            }
            None => OptStr::NONE,
        }
    }
}

impl From<OptStr> for Option<Str> {
    fn from(s: OptStr) -> Option<Str> {
        s.get()
    }
}

/// A Bazel `cat` of a profile, as its position among [`Profile::categories`];
/// resolve to the category with [`Profile::category`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CategoryId(std::num::NonZeroU16);

impl CategoryId {
    /// The id of the category at `position`, and `None` past the last position
    /// an id reaches.
    pub(crate) fn at(position: usize) -> Option<CategoryId> {
        let ordinal = u16::try_from(position).ok()?.checked_add(1)?;
        std::num::NonZeroU16::new(ordinal).map(CategoryId)
    }

    /// The category's place in [`Profile::categories`].
    pub fn position(self) -> usize {
        usize::from(self.0.get()) - 1
    }
}

/// A half-open `[off, off + len)` index range into a flat pool. Which pool it
/// indexes depends on the field that holds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Span32 {
    off: u32,
    len: u32,
}

impl Span32 {
    pub fn new(off: u32, len: u32) -> Span32 {
        Span32 { off, len }
    }

    /// The element range into the owning pool: `&pool[span.range()]`.
    pub fn range(self) -> std::ops::Range<usize> {
        self.off as usize..(self.off + self.len) as usize
    }

    /// The same span relocated by `by` elements, for rebasing onto a pool that
    /// has been concatenated after another.
    pub fn shifted(self, by: u32) -> Span32 {
        Span32 {
            off: self.off + by,
            len: self.len,
        }
    }

    pub fn len(self) -> usize {
        self.len as usize
    }

    pub fn is_empty(self) -> bool {
        self.len == 0
    }
}

/// A parsed Bazel profile as flat, string-interned arenas. Every model type
/// holds only `Copy` handles ([`Str`], [`Span32`]) and scalars, so all owned
/// heap lives in this type's text and element pools — dropping a `Profile`
/// frees a handful of large allocations rather than millions of tiny ones, and
/// the type is `Send + Sync` with no lifetime parameters.
#[derive(Debug, PartialEq)]
pub struct Profile {
    /// The one text pool every [`Str`] in this profile indexes.
    pub(crate) text: String,
    pub(crate) slice_pool: Vec<Slice>,
    pub(crate) detail_pool: Vec<SliceDetail>,
    pub(crate) end_max_pool: Vec<i64>,
    pub(crate) args_pool: Vec<(Str, Str)>,
    pub(crate) series_pool: Vec<CounterSeries>,
    pub(crate) point_pool: Vec<CounterPoint>,
    metadata: Metadata,
    threads: Vec<ThreadTrack>,
    counters: Vec<CounterLane>,
    phase_markers: Vec<PhaseMarker>,
    span: TimeSpan,
    distinct_strings: usize,
    categories: Vec<Category>,
}

/// One Bazel `cat` of a profile.
#[derive(Debug, PartialEq)]
pub(crate) struct Category {
    name: Str,
    kinds: Option<Kinds>,
    /// See [`fixed_name`].
    fixed_name: Option<&'static str>,
}

/// What labels the spans of a category whose names say which instance of a
/// kind each is.
#[derive(Debug, PartialEq)]
struct Kinds {
    naming: KindNaming,
    /// The label of each kind of `naming.named`, in that order.
    named: Vec<Str>,
}

impl Category {
    /// The category `name`, whose text is `text`. A kind a span name leads
    /// with is no string of any event's own, so `intern` gives it the handle
    /// that labels its spans.
    pub(crate) fn new(name: Str, text: &str, intern: impl FnMut(&str) -> Str) -> Category {
        let kinds = kind_naming(text).map(|naming| Kinds {
            naming,
            named: naming.named.iter().copied().map(intern).collect(),
        });
        Category {
            name,
            kinds,
            fixed_name: fixed_name(text),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Metadata {
    pub bazel_version: OptStr,
    pub build_id: OptStr,
    pub output_base: OptStr,
    pub date: OptStr,
    /// When Bazel started writing the profile (`otherData.profile_start_ts`).
    /// The profile's clock counts from the command's execution start, taken
    /// before the writer starts, so this is an upper bound on the wall-clock
    /// instant of time zero: later than it by the profiler's startup.
    pub epoch_origin: Option<EpochMicros>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeSpan {
    pub start_us: i64,
    pub end_us: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThreadTrack {
    pub tid: i64,
    pub name: Str,
    pub sort_index: i64,
    /// This track's contiguous run in the slice, detail, and end-max pools
    /// alike — the runs partition all three, but not necessarily in the order
    /// of [`Profile::threads`], so a track's run is reached only through this
    /// span and never through the track's position. Resolve with
    /// [`Profile::slices`], [`Profile::details`], and [`Profile::end_maxes`].
    pub slices: Span32,
    /// Slice rows the track's nesting occupies: one past the deepest
    /// [`Slice::depth`], and zero for a track with no slices.
    pub max_depth: u32,
    pub lane_class: LaneClass,
}

/// Where a slice stands in the profile that handed it out. Only a profile
/// mints one, so a handle names a slice of the profile it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SliceAt {
    pub(crate) track: usize,
    pub(crate) slice: usize,
}

impl SliceAt {
    /// The position of the slice's track in [`Profile::threads`].
    pub fn track(self) -> usize {
        self.track
    }

    /// The position of the slice in its track's [`Profile::slices`].
    pub fn slice(self) -> usize {
        self.slice
    }
}

/// The hot per-slice record: only what culling, hit-testing, and painting read
/// per frame, sized to fit two slices per cache line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slice {
    pub name: Str,
    pub start_us: i64,
    pub dur_us: i64,
    pub depth: u32,
    pub role: SpanRole,
    /// The slice's Bazel `cat`, which its [`SliceDetail::category`] gives as
    /// text; `None` for one of a category past the last an id reaches.
    pub category: Option<CategoryId>,
}

const _: () = assert!(std::mem::size_of::<Slice>() == 32);

impl Slice {
    /// When the slice ends, clamped to the maximum representable time: a
    /// profile's floats round to a start and a duration independently, so their
    /// sum can exceed `i64`.
    pub fn end_us(self) -> i64 {
        self.start_us.saturating_add(self.dur_us)
    }
}

/// The profile's time resolution: timestamps and durations are whole
/// microseconds, so an event shorter than one is recorded at zero duration and
/// stands for the microsecond it fell in.
pub const TIME_RESOLUTION_US: i64 = 1;

/// The time an event starting at `start_us` and lasting `dur_us` occupies: its
/// own extent, and the one [`TIME_RESOLUTION_US`] microsecond it was recorded
/// in where that extent is empty — as it is for an instant-like record, a
/// counter sample or a phase marker.
pub fn occupied_span(start_us: i64, dur_us: i64) -> TimeSpan {
    TimeSpan {
        start_us,
        end_us: start_us.saturating_add(dur_us.max(TIME_RESOLUTION_US)),
    }
}

/// Whether a span reaching `outer_end_us` contains the span
/// `[inner_start_us, inner_end_us)`, asked by a walk in start order so the outer
/// span already starts no later. Containment goes by end, with one
/// [`TIME_RESOLUTION_US`] of overshoot allowed a span that began inside: a span
/// outliving its host by more than that is its sibling, since reading every
/// overlap as containment would stack each merely overlapping span on the last.
///
/// The overshoot is there because a start and a duration are rounded to whole
/// microseconds independently, so a callee's recorded end can land a step past
/// its caller's. Requiring that the callee began inside is what holds spans
/// running back to back apart: those land a step apart exactly as a rounded
/// callee does, so without the test a run of one-step spans would chain into a
/// staircase of false depth.
pub fn encloses(outer_end_us: i64, inner_start_us: i64, inner_end_us: i64) -> bool {
    inner_end_us <= outer_end_us
        || (inner_start_us < outer_end_us
            && inner_end_us <= outer_end_us.saturating_add(TIME_RESOLUTION_US))
}

/// The cold companion of a [`Slice`], read only on demand (tooltips, grouping):
/// the profile's detail pool is exactly parallel to its slice pool, so a
/// slice's detail is the entry at the same index and a thread's `slices` span
/// resolves both. The typed args carry the Bazel keys callers query directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SliceDetail {
    pub category: Str,
    pub target: OptStr,
    pub mnemonic: OptStr,
    pub configuration: OptStr,
    /// The primary output path of an action's span, which Bazel writes under
    /// `--experimental_profile_include_primary_output`.
    pub out: OptStr,
    /// Range into the profile's args pool of the key/value pairs beyond the
    /// typed args; resolve with [`Profile::slice_others`].
    pub others: Span32,
}

const _: () = assert!(std::mem::size_of::<SliceDetail>() == 48);

/// The kind of work a span represents, giving the base hue in the flame views.
/// Derived from the Bazel `cat` plus the span name ([`span_role`]) because a
/// single category can span several kinds: one "general information" `cat` holds
/// the spawned process, the sandbox I/O around it, and the skyframe evaluation
/// envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Workload {
    /// The action body: a span whose duration is the time an action's own
    /// process was running, whether it measures that process directly or stands
    /// for it.
    Subprocess,
    /// Build logic run by the interpreter: Starlark calls, rule bodies, module
    /// and repository evaluation.
    Starlark,
    /// Filesystem work: VFS operations, sandbox construction, globbing, input
    /// and output staging, include scanning.
    Filesystem,
    /// Consulting a cache and moving bytes for it: the action-cache lookup that
    /// decides whether a spawn runs at all, local or remote, plus up/download,
    /// RPC, credential acquisition, and repository fetching.
    RemoteCache,
    /// Scheduling and contention: resource locks, throttle semaphores, queuing,
    /// thread waits, and the gaps between a remote spawn's reported phases.
    Scheduling,
    /// Skyframe graph evaluation and its action-execution envelopes.
    Skyframe,
    /// Java garbage collection.
    Gc,
    /// Whole-build umbrellas, module lifecycle, and anything unrecognized.
    Other,
}

/// A span's coloring role: a [`Workload`] for the base hue, and whether the span
/// is build payload — the user's own work (the compiler, the Starlark that
/// defines the build), drawn vivid — rather than Bazel overhead (scheduling,
/// caching, graph bookkeeping, the sandbox), drawn muted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SpanRole {
    pub workload: Workload,
    pub payload: bool,
}

/// Classify a slice for coloring from its Bazel `cat` and name. A category whose
/// spans are not all one kind of work splits on the name; the rest map by `cat`
/// alone. An unknown or future category falls through to [`Workload::Other`]
/// overhead.
///
/// A span that could be filed either under the work it serves or under the work
/// it does is placed by what its own time goes on: skyfunction frames (including
/// `CONFIGURED_TARGET`) are [`Workload::Skyframe`] overhead — the payload is the
/// Starlark they nest, not the graph node itself; and include scanning and input
/// discovery are [`Workload::Filesystem`] overhead rather than action body,
/// since they compute an action's inputs rather than run it.
pub fn span_role(category: &str, name: &str) -> SpanRole {
    use Workload::*;
    let (workload, payload) = match category {
        "general information" => general_information_role(name),
        "bazel module processing" => bazel_module_role(name),
        "Starlark repository function call" => starlark_repository_role(name),
        "Conflict checking" => conflict_checking_role(name),
        "remote action execution" => remote_execution_role(name),
        "Remote execution upload time" | "action dependency checking" => remote_cache_role(name),

        "action processing"
        | "local action execution"
        | "Local execution process wall time"
        | "local execution in worker"
        | "Local parse to prepare for remote execution"
        | "Remote execution process wall time"
        | "critical path component" => (Subprocess, true),

        "action count (local)" => (Subprocess, false),

        "Starlark Parser"
        | "Starlark user function call"
        | "Starlark builtin function call"
        | "Starlark compiled user function call"
        | "Starlark thread context"
        | "load WebAssembly module"
        | "execute WebAssembly function" => (Starlark, true),

        "VFS stat"
        | "VFS readdir"
        | "VFS readlink"
        | "VFS md5"
        | "VFS xattr"
        | "VFS delete"
        | "VFS open"
        | "VFS read"
        | "VFS write"
        | "VMFS stat"
        | "VMFS readdir"
        | "VMFS read"
        | "globbing"
        | "filesystem traversal"
        | "Staging per-action file system"
        | "setting up inputs for worker"
        | "copying outputs from worker"
        | "include scanner"
        | "discover inputs"
        | "update action information"
        | "Vendoring repository"
        | "logging spawn" => (Filesystem, false),

        "Remote execution file fetching"
        | "remote output download"
        | "remote action cache check"
        | "remote network"
        | "RPC"
        | "calling credential helper"
        | "Fetching repository" => (RemoteCache, false),

        "Remote execution queuing time"
        | "Remote execution setup"
        | "thread wait"
        | "action resource lock"
        | "Acquiring dynamic execution output lock"
        | "borrowing a worker"
        | "waiting for response from worker"
        | "blocked on repo contents cache GC" => (Scheduling, false),

        "skyfunction"
        | "skyframe evaluator"
        | "package creation"
        | "complete action execution"
        | "prepare action rewind plan" => (Skyframe, false),

        "gc notification" => (Gc, false),

        _ => (Other, false),
    };
    SpanRole { workload, payload }
}

/// The role of a "bazel module processing" span: the registry transfer that
/// fetches a module's metadata and archives, and the Starlark that module
/// resolution and extension evaluation run.
fn bazel_module_role(name: &str) -> (Workload, bool) {
    use Workload::*;
    if name.starts_with("download file: ") || name.starts_with("fetch module file: ") {
        (RemoteCache, false)
    } else {
        (Starlark, true)
    }
}

/// The role of a "Starlark repository function call" span: a `local: ` or
/// `remote: ` name is the command line a `repository_ctx.execute` or
/// `execute_remote` spawned; every other name is the repository rule's own
/// Starlark.
fn starlark_repository_role(name: &str) -> (Workload, bool) {
    use Workload::*;
    if name.starts_with("local: ") || name.starts_with("remote: ") {
        (Subprocess, true)
    } else {
        (Starlark, true)
    }
}

/// The role of a "Conflict checking" span. Every name here is a thread blocked,
/// so they split on whose work the block covers: the prior key's signal is a
/// wait for an unrelated top-level key's check to finish, with none of this
/// key's own work in flight, while the rest envelope the pools running this
/// key's conflict check and so carry that check's time.
fn conflict_checking_role(name: &str) -> (Workload, bool) {
    use Workload::*;
    if name == "Awaiting signal from a prior key." {
        (Scheduling, false)
    } else {
        (Skyframe, false)
    }
}

/// The role of a "remote action execution" span: dispatching the Execute RPC is
/// time on the wire to the executor, the rest is the remote action body.
fn remote_execution_role(name: &str) -> (Workload, bool) {
    use Workload::*;
    if name == "send Execute request" {
        (RemoteCache, false)
    } else {
        (Subprocess, true)
    }
}

/// The role of a span in a category that hosts `acquiring semaphore` alongside
/// [`Workload::RemoteCache`] work: that name is a throttle wait, which Bazel
/// profiles under the category of the work it gates rather than one of its own.
fn remote_cache_role(name: &str) -> (Workload, bool) {
    use Workload::*;
    if name == "acquiring semaphore" {
        (Scheduling, false)
    } else {
        (RemoteCache, false)
    }
}

/// The role of a "general information" span, which alone spans several workloads:
/// the real spawn (`subprocess.run`, the one payload here), the sandbox and I/O
/// helpers around it, the remote-cache and repository transfer that the
/// profiler's no-task overload files here, and the skyframe/action-execution
/// envelopes. Whole-build umbrellas (`buildTargets`) and module lifecycle fall
/// through to [`Workload::Other`].
fn general_information_role(name: &str) -> (Workload, bool) {
    use Workload::*;
    if name == "subprocess.run" {
        (Subprocess, true)
    } else if name.starts_with("sandbox.")
        || name.starts_with("trashPath")
        || name == "context.prefetchInputs"
        || name == "AbstractSpawnStrategy.getInputMapping"
        || name == "checkOutputs"
        || name == "action.prepare"
        || name == "Create symlink tree"
    {
        (Filesystem, false)
    } else if name.starts_with("Remote.")
        || name.starts_with("MerkleTree")
        || name.starts_with("BazelOutputService.")
        || name.starts_with("fetching: ")
        || name.starts_with("extracting: ")
        || matches!(
            name,
            "collect digests"
                | "findMissingDigests"
                | "upload"
                | "build upload manifest"
                | "checkForConcurrentModifications"
                | "doExtendLeases"
                | "getServerCapabilities"
                | "getAndVerifyServerCapabilities"
                | "init channel and check server capabilities"
                | "compose prefetches"
                | "mergeBulkTransfer"
        )
    {
        (RemoteCache, false)
    } else if name == "Action.execute"
        || name == "postprocessing.run"
        || name == "runAnalysisAndExecutionPhase"
        || name == "ParallelEvaluator.eval"
        || name.starts_with("skyframeExecutor")
    {
        (Skyframe, false)
    } else {
        (Other, false)
    }
}

/// Whether a span of this Bazel `cat` holds its thread parked: for its own time
/// — what the spans nested in it leave over — the thread is blocked on work that
/// other threads carry out and record on their own lanes, so that time is theirs
/// seen a second time.
///
/// `ProfilerTask.SKYFRAME_EVAL` is such a span whatever its name: its single
/// emitter wraps `ParallelEvaluator.doMutatingEvaluation`, which hands the
/// requested keys to the evaluator pools and waits for them to finish:
/// <https://github.com/bazelbuild/bazel/blob/9.1.0/src/main/java/com/google/devtools/build/skyframe/ParallelEvaluator.java#L670-L672>
pub fn parks_thread(category: &str) -> bool {
    category == "skyframe evaluator"
}

/// A repository, identified by its canonical name: two spans name the same
/// repository only where the names they give it are equal in full.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Repository<'a>(&'a str);

/// What leads a canonical repository name where a span writes the repository
/// as a label would: `RepositoryName.getNameWithAt`,
/// <https://github.com/bazelbuild/bazel/blob/9.1.0/src/main/java/com/google/devtools/build/lib/cmdline/RepositoryName.java#L270-L276>
const CANONICAL_REPOSITORY_SIGIL: &str = "@@";

/// The side of a repository fetch the spans of one Bazel `cat` record. Bazel
/// fetches a repository on two threads, each with a span of its own open for
/// as long as it is at it:
/// <https://github.com/bazelbuild/bazel/blob/9.1.0/src/main/java/com/google/devtools/build/lib/bazel/repository/RepositoryFetchFunction.java#L155-L187>
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchSide {
    /// `ProfilerTask.REPOSITORY_FETCH`, on the Skyframe thread asked for the
    /// repository. That thread hands the fetch to a worker thread and blocks
    /// until the worker finishes or needs a Skyframe restart, which ends the
    /// span; another begins when the thread takes the fetch up again:
    /// <https://github.com/bazelbuild/bazel/blob/9.1.0/src/main/java/com/google/devtools/build/skyframe/WorkerSkyKeyComputeState.java#L116-L128>
    /// So while the worker's span is open the host's time is the worker's
    /// seen a second time, and where the worker runs under no span the host's
    /// is the only record of the fetch.
    Host,
    /// `ProfilerTask.STARLARK_REPOSITORY_FN` around the call of the repository
    /// rule's implementation function on the worker thread, open across the
    /// host's restarts:
    /// <https://github.com/bazelbuild/bazel/blob/9.1.0/src/main/java/com/google/devtools/build/lib/bazel/repository/RepositoryFetchFunction.java#L668-L672>
    Worker,
}

const FETCH_HOST_CATEGORY: &str = "Fetching repository";
const FETCH_WORKER_CATEGORY: &str = "Starlark repository function call";

/// The side of a repository fetch the spans of this Bazel `cat` record, if
/// any.
pub fn fetch_side(category: &str) -> Option<FetchSide> {
    match category {
        FETCH_HOST_CATEGORY => Some(FetchSide::Host),
        FETCH_WORKER_CATEGORY => Some(FetchSide::Worker),
        _ => None,
    }
}

impl FetchSide {
    /// The repository a span of this side named `name` fetches. A host is
    /// named by the repository as a label writes it. A worker is named by the
    /// bare canonical name from Bazel 9.0.0 on:
    /// <https://github.com/bazelbuild/bazel/blob/9.0.0/src/main/java/com/google/devtools/build/lib/bazel/repository/RepositoryFetchFunction.java#L644-L646>
    /// An earlier Bazel names it by the label of the repository rule's target,
    /// which no host's repository equals, so no host of such a profile has a
    /// worker:
    /// <https://github.com/bazelbuild/bazel/blob/8.8.1/src/main/java/com/google/devtools/build/lib/bazel/repository/starlark/StarlarkRepositoryFunction.java#L309-L311>
    /// A worker shares its category with the commands the rule runs, which
    /// are no repository's fetch as a whole and lead with a kind (see
    /// [`kind_naming`]).
    pub fn repository(self, name: &str) -> Option<Repository<'_>> {
        match self {
            FetchSide::Host => name
                .strip_prefix(CANONICAL_REPOSITORY_SIGIL)
                .map(Repository),
            FetchSide::Worker => {
                let command = kind_naming(FETCH_WORKER_CATEGORY)
                    .is_some_and(|naming| matches!(naming.kind_of(name), Some(SpanKind::Named(_))));
                (!command).then_some(Repository(name))
            }
        }
    }
}

/// The Bazel `cat`s whose spans, but for those leading with one of the
/// category's [`NAMED_KINDS`], are named after the one thing each works on — a
/// repository, a package, a label, a path. Every emitter Bazel 9.1.0 has of
/// these `ProfilerTask`s that writes none of those kinds passes such an
/// identifier for the description:
/// <https://github.com/bazelbuild/bazel/blob/9.1.0/src/main/java/com/google/devtools/build/lib/profiler/ProfilerTask.java>
///
/// The identifier is not always one instance's own (see [`FIXED_NAMES`]), and
/// the root package of the main repository is written as the empty string:
/// <https://github.com/bazelbuild/bazel/blob/9.1.0/src/main/java/com/google/devtools/build/lib/cmdline/PackageIdentifier.java#L266-L272>
const PER_INSTANCE_CATEGORIES: [&str; 18] = [
    "Fetching repository",
    "Vendoring repository",
    "package creation",
    "Starlark repository function call",
    "Starlark thread context",
    "Starlark Parser",
    "VFS stat",
    "VFS readdir",
    "VFS readlink",
    "VFS md5",
    "VFS xattr",
    "VFS delete",
    "VFS open",
    "VFS read",
    "VFS write",
    "globbing",
    "filesystem traversal",
    "include scanner",
];

/// What stands between the kind and the instance in a span name that gives both.
const KIND_SEPARATOR: &str = ": ";

/// The kinds a span name leads with, ahead of [`KIND_SEPARATOR`] and the
/// instance the span works on, under each Bazel `cat` whose emitters write
/// such names in Bazel 9.1.0: a download or an extraction by what it is for, a
/// repository rule's command by its command line, a module resolution step by
/// its module, extension or URL, and a wait for local resources by its action.
const NAMED_KINDS: [(&str, &[&str]); 4] = [
    ("general information", &["fetching", "extracting"]),
    ("Starlark repository function call", &["local", "remote"]),
    (
        "bazel module processing",
        &[
            "download file",
            "fetch module file",
            "evaluate module file",
            "evaluate module extension",
            "compute repo spec",
            "getting yanked versions",
        ],
    ),
    ("action resource lock", &["Acquiring resources for"]),
];

/// How the spans of one Bazel `cat` say what kind of thing each is an instance
/// of, where their names say which instance rather than what the span does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct KindNaming {
    /// The kinds a name may lead with.
    named: &'static [&'static str],
    /// Whether a span leading with none of `named` is an instance of the
    /// category itself.
    category: bool,
}

/// What a span is an instance of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SpanKind {
    /// Its category.
    Category,
    /// The kind at this position of its category's [`KindNaming::named`].
    Named(usize),
}

impl KindNaming {
    fn kind_of(self, name: &str) -> Option<SpanKind> {
        let leads_with = |kind: &&str| {
            name.strip_prefix(*kind)
                .is_some_and(|rest| rest.starts_with(KIND_SEPARATOR))
        };
        match self.named.iter().position(leads_with) {
            Some(i) => Some(SpanKind::Named(i)),
            None => self.category.then_some(SpanKind::Category),
        }
    }
}

/// How the spans of this Bazel `cat` say their kind, and `None` for a category
/// whose every span is named after what it does.
fn kind_naming(category: &str) -> Option<KindNaming> {
    let named = NAMED_KINDS
        .iter()
        .find(|(of, _)| *of == category)
        .map_or(&[][..], |(_, kinds)| kinds);
    let category = PER_INSTANCE_CATEGORIES.contains(&category);
    (category || !named.is_empty()).then_some(KindNaming { named, category })
}

/// The name that spans of a Bazel `cat` among the [`PER_INSTANCE_CATEGORIES`]
/// carry where an emitter passes a fixed phrase for the description, which
/// says nothing of what the span works on. A thread's context is whatever its
/// creator set, a label or a repository for most and this for a rule
/// initializer's:
/// <https://github.com/bazelbuild/bazel/blob/9.1.0/src/main/java/com/google/devtools/build/lib/analysis/starlark/StarlarkRuleClassFunctions.java#L1781>
const FIXED_NAMES: [(&str, &str); 1] = [("Starlark thread context", "an initializer")];

/// The name of this Bazel `cat` among the [`FIXED_NAMES`], if it has one.
fn fixed_name(category: &str) -> Option<&'static str> {
    FIXED_NAMES
        .iter()
        .find(|(of, _)| *of == category)
        .map(|&(_, name)| name)
}

/// A counter graph. The parser splits every counter into one lane per numeric
/// series.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CounterLane {
    pub name: Str,
    /// Range into the profile's series pool; resolve with [`Profile::series`].
    /// Spans at most one entry — the lane's own series — and none for a counter
    /// that carried no numeric points.
    pub series: Span32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CounterSeries {
    pub name: Str,
    /// Range into the profile's point pool; resolve with [`Profile::points`].
    pub points: Span32,
    /// Largest point value, precomputed so a painter scales the lane without
    /// walking the points.
    pub max_value: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CounterPoint {
    pub ts_us: i64,
    pub value: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhaseMarker {
    pub name: Str,
    pub start_us: i64,
}

/// Coarse role a thread plays in the build, derived from its tid and readable
/// name ([`lane_class`]). Lets the UI scope to e.g. the critical path or to the
/// loading/analysis/execution role a lane carries — which under Skymeld is the
/// only way to separate analysis from execution, since the two phases merge into
/// one `ANALYZE_AND_EXECUTE` phase marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneClass {
    CriticalPath,
    Main,
    Gc,
    /// Repository-rule fetching (a loading-phase activity).
    RepoFetch,
    /// Configured-target/aspect analysis.
    Analysis,
    /// Action execution and its support threads.
    Execution,
    Other,
}

/// Skyframe evaluator pools whose names carry the execution role. Bazel's
/// `QuiescingExecutorsImpl` routes actions to the `-execution` and `-memoizing`
/// pools (via `NodeEntryVisitor`), so these run execution work.
const EXECUTION_SKYFRAME_INFIXES: [&str; 2] = [
    "skyframe-evaluator-execution",
    "skyframe-evaluator-memoizing",
];

/// Thread-name prefixes for execution support (action processing and its
/// machinery), distinct from the skyframe evaluator pools. The tree-artifact
/// visitor, symlink planter, and tree deleter materialize and clean up an
/// action's outputs.
const EXECUTION_PREFIXES: [&str; 12] = [
    "dynamic-execution",
    "Include scanner",
    "remote-execution",
    "remote-executor",
    "lease-extension",
    "combined-cache-",
    "merkle-tree-",
    "virtual-action-input-pipe-",
    "jdeps-reader-",
    "tree-artifact-visitor",
    "Non-eager Symlink planter",
    "tree-deleter",
];

const REPO_FETCH_PREFIXES: [&str; 3] = ["starlark-repository", "module-extension-", "downloads["];

/// Skyframe evaluator pools running analysis, longest infix first: Bazel routes
/// configured-target and aspect analysis to `-cpu-heavy` and leaves loading and
/// light analysis to the plain pool.
const ANALYSIS_SKYFRAME_INFIXES: [&str; 2] = ["skyframe-evaluator-cpu-heavy", "skyframe-evaluator"];
const GLOBBING_POOL_PREFIX: &str = "globbing pool";
const DISK_CACHE_PREFIX: &str = "disk-cache-";
const SANDBOX_PREFIX: &str = "sandbox-";

/// tid 0 is Bazel's synthetic critical-path thread; the rest are matched on the
/// readable thread name Bazel emits. The second element is the family the arm
/// keyed on: the fragment shared by every thread the arm accepts, stripped of
/// the separator that binds a worker index to it.
///
/// The skyframe-evaluator tests run longest-match-first: the `-execution`/
/// `-memoizing` pools run actions, so the analysis infixes must be tested only
/// after the execution ones are ruled out.
///
/// Module-extension evaluation resolves external dependencies alongside
/// repository fetching, so it is loading work; the `globbing pool` threads run
/// glob I/O for package loading, which merges into analysis like the plain
/// skyframe pools.
fn classify(tid: i64, name: &str) -> (LaneClass, &str) {
    if tid == 0 || name == "Critical Path" {
        (LaneClass::CriticalPath, "Critical Path")
    } else if name == "Main Thread" {
        (LaneClass::Main, name)
    } else if name == "Garbage Collector" {
        (LaneClass::Gc, name)
    } else if let Some(infix) = EXECUTION_SKYFRAME_INFIXES
        .iter()
        .copied()
        .find(|infix| name.contains(infix))
    {
        (LaneClass::Execution, infix)
    } else if let Some(infix) = ANALYSIS_SKYFRAME_INFIXES
        .iter()
        .copied()
        .find(|infix| name.contains(infix))
    {
        (LaneClass::Analysis, infix)
    } else if name.starts_with(GLOBBING_POOL_PREFIX) {
        (LaneClass::Analysis, GLOBBING_POOL_PREFIX)
    } else if let Some(prefix) = REPO_FETCH_PREFIXES
        .iter()
        .copied()
        .find(|prefix| name.starts_with(prefix))
    {
        (LaneClass::RepoFetch, family_of(prefix))
    } else if let Some(prefix) = EXECUTION_PREFIXES
        .iter()
        .copied()
        .find(|prefix| name.starts_with(prefix))
    {
        (LaneClass::Execution, family_of(prefix))
    } else if name.starts_with(DISK_CACHE_PREFIX) && !name.starts_with("disk-cache-gc") {
        (LaneClass::Execution, family_of(DISK_CACHE_PREFIX))
    } else if name.starts_with(SANDBOX_PREFIX) {
        (LaneClass::Execution, family_of(SANDBOX_PREFIX))
    } else {
        (LaneClass::Other, strip_worker_index(name))
    }
}

/// A matched literal without the punctuation that binds a worker index to it.
fn family_of(literal: &str) -> &str {
    literal.trim_end_matches(['-', '_', ' ', '['])
}

/// `name` without a trailing worker index — a run of digits and the single
/// separator ahead of it — or `name` itself when nothing but the index is left.
fn strip_worker_index(name: &str) -> &str {
    let stem = name.trim_end_matches(|c: char| c.is_ascii_digit());
    if stem.len() == name.len() {
        return name;
    }
    let stem = stem.strip_suffix(['-', '_', ' ']).unwrap_or(stem);
    if stem.is_empty() { name } else { stem }
}

/// The role the thread named by `tid`/`name` plays in the build.
pub fn lane_class(tid: i64, name: &str) -> LaneClass {
    classify(tid, name).0
}

/// The pool the thread named by `tid`/`name` belongs to: one value shared by
/// every member of a pool, distinct between pools.
pub fn track_family(tid: i64, name: &str) -> &str {
    classify(tid, name).1
}

impl Profile {
    pub fn metadata(&self) -> &Metadata {
        &self.metadata
    }

    /// The thread tracks in display order — `sort_index` ascending, ties by
    /// `tid` — the position that per-track UI state is indexed by.
    pub fn threads(&self) -> &[ThreadTrack] {
        &self.threads
    }

    /// The counter lanes in display order: grouped by kind — action counts,
    /// system load, CPU, memory, network — with each cluster's aggregate ahead
    /// of its Bazel-only breakdown. Kinds the grouping does not recognise come
    /// last, in first-seen order.
    pub fn counters(&self) -> &[CounterLane] {
        &self.counters
    }

    /// The build's phase markers, sorted by `start_us` ascending.
    pub fn phase_markers(&self) -> &[PhaseMarker] {
        &self.phase_markers
    }

    /// The extent covering the time every thread slice, counter sample, and
    /// phase marker occupies (see [`occupied_span`]), collapsed to zero for a
    /// profile carrying none of them. Restricting an aggregation to this holds
    /// every event, so it aggregates what an unrestricted one does.
    pub fn span(&self) -> TimeSpan {
        self.span
    }

    /// The number of distinct interned strings — an upper bound on the distinct
    /// slice names and mnemonics an aggregation keys on, so a scan or stats
    /// pass sizes its per-text maps once instead of rehashing up from empty.
    pub fn distinct_strings(&self) -> usize {
        self.distinct_strings
    }

    /// How many workers a pass over every lane's slices fans out across: one
    /// for a profile too small to repay starting threads.
    pub fn pass_workers(&self) -> usize {
        crate::parse::slice_pass_workers(self.slice_pool.len())
    }

    /// Every distinct Bazel `cat` of the profile's slices, in the order of
    /// their text.
    pub fn categories(&self) -> impl ExactSizeIterator<Item = Str> + '_ {
        self.categories.iter().map(|category| category.name)
    }

    pub fn category(&self, id: CategoryId) -> Str {
        self.categories[id.position()].name
    }

    /// The kind `slice` is an instance of, as the label that names the kind —
    /// its category, or the kind its name leads with — and `None` for a slice
    /// named after what it does. A slice whose category has no id reads as the
    /// latter.
    pub fn kind_of(&self, slice: &Slice) -> Option<Str> {
        self.kind_named(slice.category?, slice.name)
    }

    /// The kind a span of `category` named `name` is an instance of (see
    /// [`kind_of`](Self::kind_of)).
    pub fn kind_named(&self, category: CategoryId, name: Str) -> Option<Str> {
        let category = &self.categories[category.position()];
        let kinds = category.kinds.as_ref()?;
        Some(match kinds.naming.kind_of(self.str(name))? {
            SpanKind::Category => category.name,
            SpanKind::Named(i) => kinds.named[i],
        })
    }

    /// The name of `slice` where it says which instance of its kind the slice
    /// works on, and `None` where it says none: the empty name, and the fixed
    /// one of its category (see [`FIXED_NAMES`]).
    pub fn instance_of(&self, slice: &Slice) -> Option<Str> {
        let fixed = slice
            .category
            .and_then(|id| self.categories[id.position()].fixed_name);
        let name = slice.name;
        (!name.is_empty() && fixed != Some(self.str(name))).then_some(name)
    }

    /// Every label [`kind_of`](Self::kind_of) can give, whether or not a slice
    /// of the profile takes it.
    pub fn kinds(&self) -> impl Iterator<Item = Str> + '_ {
        self.categories.iter().flat_map(|category| {
            let kinds = category.kinds.as_ref();
            let own = kinds
                .filter(|kinds| kinds.naming.category)
                .map(|_| category.name);
            let named = kinds
                .into_iter()
                .flat_map(|kinds| kinds.named.iter().copied());
            own.into_iter().chain(named)
        })
    }

    /// Resolves an interned handle to its text.
    pub fn str(&self, s: Str) -> &str {
        &self.text[s.range()]
    }

    /// Resolves an optional interned handle to its text, if present.
    pub fn opt_str(&self, s: impl Into<Option<Str>>) -> Option<&str> {
        s.into().map(|s| self.str(s))
    }

    /// The slices of a thread track, in lane order: start ascending, ties
    /// longest-duration-first, so an enclosing span precedes the spans it
    /// contains.
    pub fn slices(&self, track: &ThreadTrack) -> &[Slice] {
        &self.slice_pool[track.slices.range()]
    }

    /// The slice standing `at`, which must come from this profile.
    pub fn slice(&self, at: SliceAt) -> Slice {
        self.slices(&self.threads[at.track])[at.slice]
    }

    /// Where each slice of the track at `track` in [`Profile::threads`]
    /// stands, index-parallel to [`Profile::slices`]; none past the tracks.
    pub fn slices_at(&self, track: usize) -> impl ExactSizeIterator<Item = SliceAt> + use<> {
        let count = self.threads.get(track).map_or(0, |t| t.slices.len());
        (0..count).map(move |slice| SliceAt { track, slice })
    }

    /// The details of a thread track's slices, index-parallel to
    /// [`Profile::slices`].
    pub fn details(&self, track: &ThreadTrack) -> &[SliceDetail] {
        &self.detail_pool[track.slices.range()]
    }

    /// The running maximum [`Slice::end_us`] of a thread track's slices:
    /// element `i` is the max over the track's slices `0..=i`, reset at each
    /// track. Index-parallel to [`Profile::slices`] and monotone nondecreasing
    /// within a track, so the leftmost slice that can still overlap a view is a
    /// binary search (`partition_point`) for the first element reaching the
    /// view's left cull bound, instead of a scan from 0.
    pub fn end_maxes(&self, track: &ThreadTrack) -> &[i64] {
        &self.end_max_pool[track.slices.range()]
    }

    /// The extra (non-typed) args of a slice, as interned key/value pairs.
    pub fn slice_others(&self, detail: &SliceDetail) -> &[(Str, Str)] {
        &self.args_pool[detail.others.range()]
    }

    /// The series of a counter lane.
    pub fn series(&self, lane: &CounterLane) -> &[CounterSeries] {
        &self.series_pool[lane.series.range()]
    }

    /// The points of a counter series, sorted by `ts_us` ascending so callers
    /// can binary-search by time.
    pub fn points(&self, series: &CounterSeries) -> &[CounterPoint] {
        &self.point_pool[series.points.range()]
    }

    /// Whether the profile has a critical-path lane; the UI disables the
    /// critical-path view when absent (e.g. `--noexperimental_collect_*`).
    pub fn has_critical_path(&self) -> bool {
        self.threads
            .iter()
            .any(|t| t.lane_class == LaneClass::CriticalPath)
    }

    /// Phase markers as half-open `[start, end)` intervals: each runs to the next
    /// marker's start, the last to `span.end_us`. Mirrors Bazel's `JsonProfile`
    /// phase-summary reconstruction from consecutive markers. Each name is an
    /// interned handle; resolve with [`Profile::str`].
    pub fn phase_intervals(&self) -> Vec<(Str, i64, i64)> {
        let markers = &self.phase_markers;
        markers
            .iter()
            .enumerate()
            .map(|(i, marker)| {
                let end = markers
                    .get(i + 1)
                    .map_or(self.span.end_us, |next| next.start_us);
                (marker.name, marker.start_us, end)
            })
            .collect()
    }
}

impl Profile {
    /// Assembles a profile from filled pools and resolved top-level vectors. The
    /// flat-pool fields are crate-internal; the parser builds them.
    pub(crate) fn from_parts(parts: ProfileParts) -> Profile {
        let ProfileParts {
            text,
            slice_pool,
            detail_pool,
            end_max_pool,
            args_pool,
            series_pool,
            point_pool,
            metadata,
            threads,
            counters,
            mut phase_markers,
            span,
            distinct_strings,
            categories,
        } = parts;
        phase_markers.sort_by_key(|m| m.start_us);
        Profile {
            text,
            slice_pool,
            detail_pool,
            end_max_pool,
            args_pool,
            series_pool,
            point_pool,
            metadata,
            threads,
            counters,
            phase_markers,
            span,
            distinct_strings,
            categories,
        }
    }
}

/// The owned components the parser fills before sealing them into a [`Profile`].
pub(crate) struct ProfileParts {
    pub text: String,
    pub slice_pool: Vec<Slice>,
    pub detail_pool: Vec<SliceDetail>,
    pub end_max_pool: Vec<i64>,
    pub args_pool: Vec<(Str, Str)>,
    pub series_pool: Vec<CounterSeries>,
    pub point_pool: Vec<CounterPoint>,
    pub metadata: Metadata,
    pub threads: Vec<ThreadTrack>,
    pub counters: Vec<CounterLane>,
    pub phase_markers: Vec<PhaseMarker>,
    pub span: TimeSpan,
    pub distinct_strings: usize,
    pub categories: Vec<Category>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::concurrent_interner::ConcurrentInterner;
    use crate::parse::build_for_test;

    /// One action's `describe()` text, which a profile repeats verbatim across
    /// every category that names an action.
    const ACTION_DESCRIPTION: &str = "Compiling Rust rlib //trace/profile:profile (9 files)";

    #[test]
    fn span_role_maps_categories_and_names_to_workload_and_payload() {
        let role = |cat, name| span_role(cat, name);
        let payload = |w| SpanRole {
            workload: w,
            payload: true,
        };
        let overhead = |w| SpanRole {
            workload: w,
            payload: false,
        };
        // The action body and Starlark are payload.
        assert_eq!(
            role("action processing", ACTION_DESCRIPTION),
            payload(Workload::Subprocess)
        );
        assert_eq!(
            role("remote action execution", "execute remotely"),
            payload(Workload::Subprocess)
        );
        assert_eq!(
            role("remote action execution", "send Execute request"),
            overhead(Workload::RemoteCache)
        );
        let replayed = format!("action '{ACTION_DESCRIPTION}'");
        assert_eq!(
            role("critical path component", &replayed),
            payload(Workload::Subprocess)
        );
        assert_eq!(
            role("Starlark user function call", "_cc_library_impl"),
            payload(Workload::Starlark)
        );
        // A repository rule's spawns split from its interpreter time.
        assert_eq!(
            role(
                "Starlark repository function call",
                "local: /usr/bin/uname -m"
            ),
            payload(Workload::Subprocess)
        );
        assert_eq!(
            role(
                "Starlark repository function call",
                "remote: /usr/bin/uname -m"
            ),
            payload(Workload::Subprocess)
        );
        assert_eq!(
            role("Starlark repository function call", "crate_index"),
            payload(Workload::Starlark)
        );
        // Module resolution splits registry transfer from its Starlark.
        assert_eq!(
            role(
                "bazel module processing",
                "download file: https://bcr/modules/foo"
            ),
            overhead(Workload::RemoteCache)
        );
        assert_eq!(
            role(
                "bazel module processing",
                "fetch module file: rules_cc@0.1.5"
            ),
            overhead(Workload::RemoteCache)
        );
        assert_eq!(
            role(
                "bazel module processing",
                "evaluate module extension: crate"
            ),
            payload(Workload::Starlark)
        );
        // Overhead workloads.
        assert_eq!(role("VFS stat", ""), overhead(Workload::Filesystem));
        assert_eq!(role("VFS md5", ""), overhead(Workload::Filesystem));
        assert_eq!(role("include scanner", ""), overhead(Workload::Filesystem));
        assert_eq!(
            role(
                "update action information",
                "Compiling gnulib/lib/c-ctype.c [for tool]"
            ),
            overhead(Workload::Filesystem)
        );
        assert_eq!(role("RPC", ""), overhead(Workload::RemoteCache));
        assert_eq!(
            role("calling credential helper", "calling credential helper"),
            overhead(Workload::RemoteCache)
        );
        // Every span the remote-execution setup category carries is the action
        // waiting: on the local throttle, or on the executor to reach the next
        // phase of the spawn.
        assert_eq!(
            role("Remote execution setup", "acquiring semaphore"),
            overhead(Workload::Scheduling)
        );
        assert_eq!(
            role("Remote execution setup", "pre-fetch"),
            overhead(Workload::Scheduling)
        );
        assert_eq!(
            role("Remote execution setup", "pre-execute"),
            overhead(Workload::Scheduling)
        );
        assert_eq!(
            role("Remote execution setup", "pre-upload"),
            overhead(Workload::Scheduling)
        );
        // The throttle wait splits away from the transfer it gates.
        assert_eq!(
            role("Remote execution upload time", "acquiring semaphore"),
            overhead(Workload::Scheduling)
        );
        assert_eq!(
            role("Remote execution upload time", "upload outputs"),
            overhead(Workload::RemoteCache)
        );
        // The local action-cache lookup carries the bare description of the
        // action it checks — the same text the action's own span carries, and
        // the form the flame graph's mnemonic index is keyed by. Only the
        // critical-path replay wraps that description in `action '…'`.
        assert_eq!(
            role("action dependency checking", ACTION_DESCRIPTION),
            overhead(Workload::RemoteCache)
        );
        assert_eq!(
            role("action dependency checking", "acquiring semaphore"),
            overhead(Workload::Scheduling)
        );
        // The held resource slot envelopes the action body, so it takes the
        // action's hue but stays overhead.
        assert_eq!(
            role("action count (local)", "Resources acquired"),
            overhead(Workload::Subprocess)
        );
        // The skyfunction frame is overhead; its Starlark children carry the work.
        assert_eq!(
            role("skyfunction", "CONFIGURED_TARGET"),
            overhead(Workload::Skyframe)
        );
        assert_eq!(
            role("Conflict checking", "Go through actions"),
            overhead(Workload::Skyframe)
        );
        assert_eq!(
            role("Conflict checking", "Awaiting signal from a prior key."),
            overhead(Workload::Scheduling)
        );
        assert_eq!(role("gc notification", "minor GC"), overhead(Workload::Gc));
        // "general information" splits by name.
        assert_eq!(
            role("general information", "subprocess.run"),
            payload(Workload::Subprocess)
        );
        assert_eq!(
            role("general information", "sandbox.createInputs"),
            overhead(Workload::Filesystem)
        );
        assert_eq!(
            role("general information", "Action.execute"),
            overhead(Workload::Skyframe)
        );
        assert_eq!(
            role("general information", "postprocessing.run"),
            overhead(Workload::Skyframe)
        );
        for name in [
            "Remote.download",
            "MerkleTreeComputer.buildForSpawn",
            "BazelOutputService.StageArtifacts",
            "collect digests",
            "findMissingDigests",
            "upload",
            "build upload manifest",
            "checkForConcurrentModifications",
            "doExtendLeases",
            "getServerCapabilities",
            "getAndVerifyServerCapabilities",
            "init channel and check server capabilities",
            "compose prefetches",
            "mergeBulkTransfer",
            "fetching: repository @@rules_m4++default_toolchain_ext+m4_v1.4.18",
            "extracting: repository @@rules_m4++default_toolchain_ext+m4_v1.4.18",
            "fetching: module extension @@rules_go+//go:extensions.bzl%go_sdk",
            "extracting: module extension @@rules_go+//go:extensions.bzl%go_sdk",
        ] {
            assert_eq!(
                role("general information", name),
                overhead(Workload::RemoteCache),
                "{name}"
            );
        }
        // Real names that fall just outside each open-ended prefix, so
        // loosening one is a test failure rather than a silent reclassification.
        for name in [
            "buildTargets",
            "RemoteModule.beforeCommand",
            "BazelRepositoryModule.beforeCommand",
            "fetch module file: platforms@1.0.0",
        ] {
            assert_eq!(
                role("general information", name),
                overhead(Workload::Other),
                "{name}"
            );
        }
        assert_eq!(
            role(
                "Starlark repository function call",
                "rules_java++toolchains+remote_jdk8_linux_toolchain_config_repo"
            ),
            payload(Workload::Starlark)
        );
        // An unknown category is Other overhead.
        assert_eq!(role("some future task", ""), overhead(Workload::Other));
    }

    /// The kind [`kind_naming`] reads a span as an instance of, as text.
    fn kind(category: &'static str, name: &str) -> Option<&'static str> {
        let naming = kind_naming(category)?;
        match naming.kind_of(name)? {
            SpanKind::Category => Some(category),
            SpanKind::Named(i) => Some(naming.named[i]),
        }
    }

    #[test]
    fn a_span_named_after_what_it_works_on_is_an_instance_of_its_category() {
        for (category, name) in [
            ("Fetching repository", "@@rules_cc+"),
            ("Vendoring repository", "@@rules_cc+"),
            ("package creation", "@@rules_cc+//cc"),
            ("package creation", ""),
            ("Starlark thread context", "repository @@rules_cc+"),
            ("Starlark thread context", "//cc:defs"),
            ("Starlark repository function call", "rules_cc+"),
            ("Starlark Parser", "/work/cc/BUILD.bazel"),
            ("VFS stat", "/work/cc"),
            ("VFS md5", "/work/cc/defs.bzl"),
            ("globbing", "/work/cc"),
            (
                "filesystem traversal",
                "DirectTraversalRoot{rootPart=/work}",
            ),
            ("include scanner", "cc/defs.h"),
            // A separator alone makes no kind of what stands ahead of it.
            ("Fetching repository", "local: @@rules_cc+"),
            ("VFS stat", "/work/a: b"),
        ] {
            assert_eq!(kind(category, name), Some(category), "{category} / {name}");
        }
    }

    #[test]
    fn a_span_named_kind_then_instance_is_an_instance_of_the_kind_it_leads_with() {
        for (category, name, expected) in [
            (
                "general information",
                "fetching: repository @@rules_cc+",
                "fetching",
            ),
            (
                "general information",
                "extracting: module extension @@rules_go+//go:extensions.bzl%go_sdk",
                "extracting",
            ),
            (
                "Starlark repository function call",
                "local: /usr/bin/uname -m",
                "local",
            ),
            (
                "Starlark repository function call",
                "remote: /usr/bin/uname -m",
                "remote",
            ),
            (
                "bazel module processing",
                "download file: https://bcr.example/modules/foo/1.0/MODULE.bazel",
                "download file",
            ),
            (
                "bazel module processing",
                "fetch module file: rules_cc@0.1.5",
                "fetch module file",
            ),
            (
                "bazel module processing",
                "evaluate module file: rules_cc@0.1.5",
                "evaluate module file",
            ),
            (
                "bazel module processing",
                "evaluate module extension: crate",
                "evaluate module extension",
            ),
            (
                "bazel module processing",
                "compute repo spec: rules_cc@0.1.5",
                "compute repo spec",
            ),
            (
                "bazel module processing",
                "getting yanked versions: rules_cc",
                "getting yanked versions",
            ),
            (
                "action resource lock",
                "Acquiring resources for: Compiling cc/defs.cc",
                "Acquiring resources for",
            ),
            // Only the first separator divides the kind from the instance.
            (
                "general information",
                "fetching: repository @@a: b: c",
                "fetching",
            ),
            (
                "Starlark repository function call",
                "local: sh -c echo fetching: x",
                "local",
            ),
        ] {
            assert_eq!(kind(category, name), Some(expected), "{category} / {name}");
        }
    }

    #[test]
    fn a_span_named_after_what_it_does_is_an_instance_of_nothing() {
        for (category, name) in [
            // Fixed step names beside the per-instance ones of their category.
            ("general information", "buildTargets"),
            ("general information", "subprocess.run"),
            ("bazel module processing", "check bazel compatibility"),
            ("bazel module processing", "discovery"),
            ("action resource lock", "action.enterActionPreparation"),
            // A kind without its separator, with nothing after it, under
            // another category, or past the head of the name.
            ("general information", "fetching"),
            ("general information", "fetching:"),
            ("general information", "fetching repository @@a"),
            ("general information", "local: /usr/bin/uname -m"),
            ("general information", "prefetching: x"),
            ("general information", "upload fetching: x"),
            ("bazel module processing", "fetching: repository @@a"),
            ("bazel module processing", "check lockfile for crate"),
            // A separator after a kind no emitter writes.
            ("general information", "Remote.download: x"),
            ("bazel module processing", "parse lockfile: x"),
            // Function names, shared by every call.
            ("Starlark user function call", "_impl"),
            ("skyfunction", "CONFIGURED_TARGET"),
            // Named by the action, which its mnemonic groups.
            ("action processing", "Compiling cc/defs.cc"),
            ("some future task", "x: y"),
            ("", ""),
        ] {
            assert_eq!(kind(category, name), None, "{category} / {name}");
        }
    }

    #[test]
    fn a_name_leading_with_a_kind_and_its_separator_alone_is_an_instance_of_that_kind() {
        assert_eq!(kind("general information", "fetching: "), Some("fetching"));
    }

    #[test]
    fn the_spans_of_a_kind_share_one_role() {
        // A kind labels one frame for all its spans, which a role differing by
        // instance would split in two.
        for category in PER_INSTANCE_CATEGORIES {
            assert_eq!(
                span_role(category, "@@rules_cc+"),
                span_role(category, "/work/cc/defs.bzl"),
                "{category}"
            );
        }
        for (category, kinds) in NAMED_KINDS {
            for kind in kinds {
                let role =
                    |instance| span_role(category, &format!("{kind}{KIND_SEPARATOR}{instance}"));
                assert_eq!(
                    role("@@rules_cc+"),
                    role("/work/cc/defs.bzl"),
                    "{category} / {kind}"
                );
            }
        }
    }

    #[test]
    fn a_profile_gives_the_kind_of_each_of_its_slices() {
        let events = [
            ("Fetching repository", "@@rules_cc+"),
            ("general information", "buildTargets"),
            ("general information", "fetching: repository @@rules_cc+"),
            ("general information", "extracting: repository @@rules_cc+"),
            ("package creation", ""),
        ]
        .map(|(category, name)| {
            format!(r#"{{"cat":"{category}","name":"{name}","ph":"X","ts":0,"dur":1,"tid":1}}"#)
        });
        let json = format!(r#"{{"traceEvents":[{}]}}"#, events.join(","));
        let p = Profile::from_bytes(json.as_bytes())
            .expect("parse")
            .into_inner();
        let kind = |name| {
            let slices = p.slices(&p.threads()[0]);
            let slice = slices.iter().find(|s| p.str(s.name) == name).expect(name);
            p.kind_of(slice).map(|kind| p.str(kind))
        };
        assert_eq!(kind("@@rules_cc+"), Some("Fetching repository"));
        assert_eq!(kind("buildTargets"), None);
        assert_eq!(kind("fetching: repository @@rules_cc+"), Some("fetching"));
        assert_eq!(
            kind("extracting: repository @@rules_cc+"),
            Some("extracting")
        );
        assert_eq!(kind(""), Some("package creation"));
        // A kind of a category the profile has is a label whether or not a
        // span leads with it.
        let kinds: Vec<&str> = p.kinds().map(|kind| p.str(kind)).collect();
        assert_eq!(
            kinds,
            [
                "Fetching repository",
                "fetching",
                "extracting",
                "package creation"
            ]
        );
    }

    #[test]
    fn a_name_that_is_empty_or_fixed_for_its_category_tells_of_no_instance() {
        let events = [
            ("Starlark thread context", "//pkg:target"),
            ("Starlark thread context", "an initializer"),
            ("package creation", ""),
            // The phrase is fixed under the one category alone.
            ("package creation", "an initializer"),
        ]
        .map(|(category, name)| {
            format!(r#"{{"cat":"{category}","name":"{name}","ph":"X","ts":0,"dur":1,"tid":1}}"#)
        });
        let json = format!(r#"{{"traceEvents":[{}]}}"#, events.join(","));
        let p = Profile::from_bytes(json.as_bytes())
            .expect("parse")
            .into_inner();
        let instances: Vec<(&str, Option<&str>)> = p
            .slices(&p.threads()[0])
            .iter()
            .map(|s| {
                let category = s.category.expect("an id reaches every category here");
                (p.str(p.category(category)), p.opt_str(p.instance_of(s)))
            })
            .collect();
        assert_eq!(
            instances,
            [
                ("Starlark thread context", Some("//pkg:target")),
                ("Starlark thread context", None),
                ("package creation", None),
                ("package creation", Some("an initializer")),
            ]
        );
    }

    #[test]
    fn the_two_sides_of_a_fetch_name_one_repository() {
        let host = fetch_side("Fetching repository").expect("the host's category");
        let worker = fetch_side("Starlark repository function call").expect("the worker's");
        assert_eq!((host, worker), (FetchSide::Host, FetchSide::Worker));
        let repository = "rules_java++toolchains+remote_jdk8_linux";
        assert_eq!(
            host.repository(&format!("@@{repository}")),
            worker.repository(repository)
        );
        assert_eq!(worker.repository(repository), Some(Repository(repository)));
        // A name that only begins as another does is another repository.
        assert_ne!(
            host.repository("@@rules_cc+"),
            worker.repository("rules_cc++cc_configure_extension+local_config_cc")
        );
        assert_ne!(
            host.repository("@@rules_cc+"),
            worker.repository("rules_cc")
        );
    }

    #[test]
    fn a_span_that_fetches_no_repository_as_a_whole_is_on_neither_side() {
        for category in [
            "general information",
            "Starlark thread context",
            "Vendoring repository",
            "bazel module processing",
            "skyframe evaluator",
            "",
        ] {
            assert_eq!(fetch_side(category), None, "{category}");
        }
        // A host names its repository as a label does.
        assert_eq!(FetchSide::Host.repository("rules_cc+"), None);
        assert_eq!(FetchSide::Host.repository("@rules_cc+"), None);
        // The commands a repository rule runs share the worker's category.
        for name in ["local: /usr/bin/uname -m", "remote: /usr/bin/uname -m"] {
            assert_eq!(FetchSide::Worker.repository(name), None, "{name}");
        }
    }

    fn timed_slice(start_us: i64, dur_us: i64) -> Slice {
        Slice {
            name: Str::default(),
            start_us,
            dur_us,
            depth: 0,
            role: span_role("", ""),
            category: None,
        }
    }

    #[test]
    fn a_slices_end_is_its_start_plus_its_duration() {
        assert_eq!(timed_slice(100, 25).end_us(), 125);
        assert_eq!(timed_slice(100, 0).end_us(), 100);
    }

    #[test]
    fn a_slice_whose_start_and_duration_overflow_ends_at_the_greatest_representable_time() {
        assert_eq!(timed_slice(i64::MAX, i64::MAX).end_us(), i64::MAX);
        assert_eq!(timed_slice(i64::MAX - 1, 25).end_us(), i64::MAX);
    }

    #[test]
    fn a_span_begun_inside_another_nests_when_it_overshoots_by_one_step_but_not_two() {
        let began_inside = |end| encloses(100, 10, end);
        assert!(began_inside(40));
        assert!(began_inside(100));
        assert!(began_inside(100 + TIME_RESOLUTION_US));
        assert!(!began_inside(100 + 2 * TIME_RESOLUTION_US));
    }

    #[test]
    fn a_span_begun_at_or_after_anothers_end_stays_its_sibling() {
        assert!(!encloses(100, 100, 100 + TIME_RESOLUTION_US));
        assert!(!encloses(
            100,
            100 + TIME_RESOLUTION_US,
            100 + TIME_RESOLUTION_US
        ));
        // Ending within the other's own extent is nesting whatever the start.
        assert!(encloses(100, 100, 100));
    }

    #[test]
    fn a_span_ending_at_the_maximum_representable_time_nests_by_the_ordinary_rule() {
        assert!(encloses(i64::MAX, 0, i64::MAX));
        assert!(encloses(i64::MAX - TIME_RESOLUTION_US, 0, i64::MAX));
        assert!(!encloses(i64::MAX - 2 * TIME_RESOLUTION_US, 0, i64::MAX));
    }

    #[test]
    fn lane_class_derivation() {
        assert_eq!(lane_class(0, "Critical Path"), LaneClass::CriticalPath);
        assert_eq!(lane_class(7, "Main Thread"), LaneClass::Main);
        assert_eq!(lane_class(8, "Garbage Collector"), LaneClass::Gc);
        // The execution pools must win over the plain skyframe substring.
        assert_eq!(
            lane_class(12, "skyframe-evaluator-execution-3"),
            LaneClass::Execution
        );
        assert_eq!(
            lane_class(13, "skyframe-evaluator-cpu-heavy-2"),
            LaneClass::Analysis
        );
        assert_eq!(lane_class(14, "skyframe-evaluator-5"), LaneClass::Analysis);
        assert_eq!(
            lane_class(15, "starlark-repository-foo (Virtual)"),
            LaneClass::RepoFetch
        );
        assert_eq!(
            lane_class(
                16,
                "module-extension-@@rules_rs+//rs:extensions.bzl%crate-0 (Virtual)"
            ),
            LaneClass::RepoFetch
        );
        assert_eq!(lane_class(17, "globbing pool-3"), LaneClass::Analysis);
        assert_eq!(
            lane_class(18, "tree-artifact-visitor-2"),
            LaneClass::Execution
        );
        assert_eq!(
            lane_class(19, "Non-eager Symlink planter 1"),
            LaneClass::Execution
        );
        assert_eq!(lane_class(99, "collect-local-resources"), LaneClass::Other);
    }

    #[test]
    fn track_family_names_the_pool_a_thread_belongs_to() {
        assert_eq!(track_family(0, "Critical Path"), "Critical Path");
        assert_eq!(track_family(7, "Main Thread"), "Main Thread");
        assert_eq!(track_family(8, "Garbage Collector"), "Garbage Collector");
        assert_eq!(
            track_family(12, "skyframe-evaluator-execution-3"),
            "skyframe-evaluator-execution"
        );
        assert_eq!(
            track_family(13, "skyframe-evaluator-memoizing-3"),
            "skyframe-evaluator-memoizing"
        );
        assert_eq!(
            track_family(14, "skyframe-evaluator-cpu-heavy-2"),
            "skyframe-evaluator-cpu-heavy"
        );
        assert_eq!(
            track_family(15, "skyframe-evaluator-4"),
            "skyframe-evaluator"
        );
        assert_eq!(track_family(15, "globbing pool-3"), "globbing pool");
        assert_eq!(
            track_family(16, "starlark-repository-foo (Virtual)"),
            "starlark-repository"
        );
        assert_eq!(
            track_family(
                17,
                "module-extension-@@rules_rs+//rs:extensions.bzl%crate-0 (Virtual)"
            ),
            "module-extension"
        );
        assert_eq!(track_family(18, "downloads[3]"), "downloads");
        assert_eq!(track_family(19, "combined-cache-7"), "combined-cache");
        assert_eq!(
            track_family(20, "Non-eager Symlink planter 1"),
            "Non-eager Symlink planter"
        );
        assert_eq!(track_family(21, "disk-cache-4"), "disk-cache");
        assert_eq!(track_family(22, "sandbox-2"), "sandbox");
    }

    #[test]
    fn track_family_is_shared_within_a_pool_and_distinct_between_pools() {
        assert_eq!(
            track_family(12, "skyframe-evaluator-execution-3"),
            track_family(13, "skyframe-evaluator-execution-11")
        );
        assert_ne!(
            track_family(12, "skyframe-evaluator-execution-3"),
            track_family(13, "skyframe-evaluator-memoizing-3")
        );
    }

    #[test]
    fn an_unrecognised_track_drops_its_worker_index() {
        assert_eq!(track_family(99, "pid-file-watcher-0"), "pid-file-watcher");
        assert_eq!(
            track_family(99, "collect-local-resources"),
            "collect-local-resources"
        );
        assert_eq!(track_family(99, "42"), "42");
        assert_eq!(track_family(99, "-7"), "-7");
    }

    /// Builds a profile from phase markers and a span end via the parser's flat
    /// pools, so tests inspect the real public model rather than a hand-rolled one.
    fn profile_with(markers: Vec<(&str, i64)>, span_end: i64) -> Profile {
        let interner = ConcurrentInterner::new(1024, 16).expect("a small pool");
        let phase_markers = {
            let mut writer = interner.writer();
            markers
                .into_iter()
                .map(|(name, start_us)| PhaseMarker {
                    name: writer.intern(name),
                    start_us,
                })
                .collect()
        };
        build_for_test(
            interner,
            phase_markers,
            TimeSpan {
                start_us: 0,
                end_us: span_end,
            },
            Vec::new(),
        )
    }

    #[test]
    fn phase_intervals_abut_and_last_runs_to_span_end() {
        let p = profile_with(vec![("Loading", 30), ("Launch", 0), ("Execute", 100)], 250);
        let resolved: Vec<_> = p
            .phase_intervals()
            .into_iter()
            .map(|(name, start, end)| (p.str(name).to_owned(), start, end))
            .collect();
        assert_eq!(
            resolved,
            vec![
                ("Launch".to_owned(), 0, 30),
                ("Loading".to_owned(), 30, 100),
                ("Execute".to_owned(), 100, 250),
            ]
        );
    }

    #[test]
    fn phase_intervals_handles_empty_and_single() {
        assert!(profile_with(vec![], 250).phase_intervals().is_empty());
        let p = profile_with(vec![("only", 10)], 250);
        let resolved: Vec<_> = p
            .phase_intervals()
            .into_iter()
            .map(|(name, start, end)| (p.str(name).to_owned(), start, end))
            .collect();
        assert_eq!(resolved, vec![("only".to_owned(), 10, 250)]);
    }

    #[test]
    fn has_critical_path_reflects_lane_presence() {
        let track = |tid, lane_class| ThreadTrack {
            tid,
            name: Str::default(),
            sort_index: 0,
            slices: Span32::default(),
            max_depth: 0,
            lane_class,
        };
        let p = profile_with(vec![], 0);
        assert!(
            !build_for_test(
                ConcurrentInterner::new(1024, 16).expect("a small pool"),
                Vec::new(),
                p.span(),
                vec![track(5, LaneClass::Execution)],
            )
            .has_critical_path()
        );
        assert!(
            build_for_test(
                ConcurrentInterner::new(1024, 16).expect("a small pool"),
                Vec::new(),
                p.span(),
                vec![
                    track(5, LaneClass::Execution),
                    track(0, LaneClass::CriticalPath),
                ],
            )
            .has_critical_path()
        );
    }
}
