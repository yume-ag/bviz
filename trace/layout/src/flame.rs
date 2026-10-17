use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use bviz_parallel::{worker_count, workers};
use bviz_trace_profile::{
    CategoryId, OptStr, Profile, Slice, SliceDetail, Span32, SpanRole, Str, ThreadTrack, TimeSpan,
    Workload, encloses, occupied_span,
};
use rayon::iter::{IntoParallelRefIterator, IntoParallelRefMutIterator, ParallelIterator};
use rayon::slice::{ParallelSlice, ParallelSliceMut};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::paint::{Span, painted};
use crate::selection::TimeSelection;
use crate::wall_share::{LaneShare, WallShare, WorkingLanes};

/// One key segment of a slice's aggregation path: a frame label — a [`Str`]
/// handle into the source profile's text — and the work class it carries (for
/// coloring and class-keyed merging).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Segment {
    label: Str,
    role: SpanRole,
}

/// The one category that spans recorded under `a` share with spans recorded
/// under `b`, each `None` where its own spans share none that an id reaches.
#[inline]
fn shared_category(a: Option<CategoryId>, b: Option<CategoryId>) -> Option<CategoryId> {
    a.filter(|_| a == b)
}

/// How a span aggregates under a labelling.
struct SpanLabels {
    segment: Segment,
    /// The span's own name where its label is the kind it is an instance of,
    /// which labels the detail frame breaking its self time out by instance
    /// (see [`emit_lane_rows`]); `None` for a span labelled by its name, or
    /// whose name says no instance ([`Profile::instance_of`]), which inherits
    /// its containment parent's.
    instance: Option<Str>,
}

/// Lexicographic rank of each distinct scanned label handle: dense `u32`s
/// assigned by one byte-wise sort of the scanned distinct texts. The profile
/// interns each distinct string to one handle, so the handles biject with
/// texts and rank order equals byte-wise text order, making every later
/// ordering comparison integer-only. Ranks are dense per scan and never
/// compare across builds.
struct Ranks {
    rank: FxHashMap<Str, u32>,
}

impl Ranks {
    /// The lexicographic rank of a label handle's text. An unscanned handle
    /// panics — the coverage invariant of [`scan`] is what keeps ranking sound.
    fn rank_of(&self, s: Str) -> u32 {
        match self.rank.get(&s) {
            Some(&r) => r,
            None => panic!("unscanned label handle: the scan must cover every lane a build walks"),
        }
    }
}

/// A map from a mnemonic-bearing slice's name to its mnemonic, over the whole
/// profile. The "action processing" slices carry both the action description as
/// their name and the mnemonic in their args, so this recovers the mnemonic for
/// any other slice that names the same action — notably critical-path
/// components, whose name is `action '<desc>'` and which carry no mnemonic of
/// their own. `by_name` is keyed by the name's [`Str`] handle, so the per-span
/// lookup hashes a small `Copy` key rather than the full string; `by_text`
/// holds the same entries keyed by name text, for the critical-path components
/// whose stripped `action '…'` inner has no handle of its own. Values are
/// mnemonic handles.
struct MnemonicIndex {
    by_name: FxHashMap<Str, Str>,
    by_text: FxHashMap<Box<str>, Str>,
}

/// One worker's share of the name scan.
#[derive(Default)]
struct ScanShard<'p> {
    /// Text → its handle. Each distinct text has one handle profile-wide.
    by_text: FxHashMap<&'p str, Str>,
    /// Distinct name/mnemonic handles seen, to skip the text hash of an
    /// already-noted handle.
    handles: FxHashSet<Str>,
    /// Mnemonic-bearing slice name text → its smallest mnemonic text. A name
    /// carrying several mnemonics resolves to the smallest, so the index is
    /// independent of visit order.
    mnemonics: FxHashMap<&'p str, &'p str>,
}

impl<'p> ScanShard<'p> {
    fn note(&mut self, profile: &'p Profile, s: Str) {
        if self.handles.insert(s) {
            self.by_text.entry(profile.str(s)).or_insert(s);
        }
    }
}

/// Scan slice names and mnemonics into the rank table and the mnemonic index,
/// sharding the lanes across `workers`. Names are noted — entered into the rank
/// table — only on the lanes `note_lane` admits (by thread index), and the scan
/// must admit every lane whose slices a build subsequently walks: an unnoted
/// handle has no rank, and [`Ranks::rank_of`] panics on it. Mnemonic handles
/// and the mnemonic index cover every lane regardless — hashing only the
/// mnemonic-bearing slices, a small fraction of the profile — because a
/// mnemonic-less slice's label can resolve to a mnemonic carried on a lane
/// outside the scan, and that label needs a rank like any other. So does every
/// kind of the profile ([`Profile::kinds`]), which labels a span of any lane.
fn scan(
    profile: &Profile,
    workers: usize,
    note_lane: impl Fn(usize) -> bool + Sync,
) -> (Ranks, MnemonicIndex) {
    let threads = profile.threads();
    // Each distinct name/mnemonic is noted once, keyed by its text, so a shard
    // holds at most the profile's distinct strings; sharing them roughly evenly
    // over the shards, `distinct / shards` sizes each map so the scan reserves
    // once rather than rehashing every string key through a dozen doublings. A
    // shard that steals more than its share rehashes from this already-large
    // base, a couple of doublings at most.
    let shard_count = workers.clamp(1, threads.len().max(1));
    let per_shard = profile.distinct_strings().div_ceil(shard_count);
    let shards = run_sharded(workers, threads.len(), |shard: &mut ScanShard, i| {
        if shard.by_text.capacity() == 0 {
            shard.by_text.reserve(per_shard);
            shard.handles.reserve(per_shard);
            shard.mnemonics.reserve(per_shard);
        }
        let thread = &threads[i];
        let noted = note_lane(i);
        for (s, d) in profile.slices(thread).iter().zip(profile.details(thread)) {
            if noted {
                shard.note(profile, s.name);
            }
            if let Some(m) = d.mnemonic.get() {
                shard.note(profile, m);
                let mn = profile.str(m);
                shard
                    .mnemonics
                    .entry(profile.str(s.name))
                    .and_modify(|cur| {
                        if mn < *cur {
                            *cur = mn;
                        }
                    })
                    .or_insert(mn);
            }
        }
    });

    let (mut by_text, mnemonics) = merge_shards(shards);
    for kind in profile.kinds() {
        by_text.insert(profile.str(kind), kind);
    }
    (build_ranks(&by_text), build_index(&by_text, &mnemonics))
}

/// Merge the per-worker scan shards into one text→handle map and one
/// name→smallest-mnemonic map. Seeds from the first shard by move — a
/// single-worker scan merges for free — and reserves for the rest up front, so
/// the merge never regrows (a regrow re-hashes every string key).
fn merge_shards<'p>(
    mut shards: Vec<ScanShard<'p>>,
) -> (FxHashMap<&'p str, Str>, FxHashMap<&'p str, &'p str>) {
    let (mut by_text, mut mnemonics) = {
        let first = &mut shards[0];
        (
            std::mem::take(&mut first.by_text),
            std::mem::take(&mut first.mnemonics),
        )
    };
    by_text.reserve(shards.iter().skip(1).map(|s| s.by_text.len()).sum());
    mnemonics.reserve(shards.iter().skip(1).map(|s| s.mnemonics.len()).sum());
    for shard in shards.iter().skip(1) {
        for (&text, &handle) in &shard.by_text {
            by_text.entry(text).or_insert(handle);
        }
        for (&name, &mn) in &shard.mnemonics {
            mnemonics
                .entry(name)
                .and_modify(|cur| {
                    if mn < *cur {
                        *cur = mn;
                    }
                })
                .or_insert(mn);
        }
    }
    (by_text, mnemonics)
}

/// Rank the distinct scanned texts by one sort under `&str`'s byte-wise `Ord`,
/// so comparing two ranks is exactly comparing the texts.
fn build_ranks<'p>(by_text: &FxHashMap<&'p str, Str>) -> Ranks {
    let mut ordered: Vec<(&'p str, Str)> = by_text.iter().map(|(&t, &h)| (t, h)).collect();
    sort_pairs_by_text(&mut ordered);
    let rank = ordered
        .iter()
        .enumerate()
        .map(|(i, &(_, h))| (h, i as u32))
        .collect();
    Ranks { rank }
}

/// Build the mnemonic index from the merged maps: a text-keyed table over every
/// scanned name, and a handle-keyed table restricted to names with a noted
/// handle. A name seen only on unadmitted lanes has no noted handle and needs
/// no handle-keyed entry: only a walked — hence noted — slice's name is ever
/// looked up by handle, while text-keyed lookups cover every name.
fn build_index(by_text: &FxHashMap<&str, Str>, mnemonics: &FxHashMap<&str, &str>) -> MnemonicIndex {
    let name_to_mnemonic: Vec<(&str, Str)> =
        mnemonics.iter().map(|(&n, &m)| (n, by_text[m])).collect();
    let by_name = name_to_mnemonic
        .iter()
        .filter_map(|&(n, m)| by_text.get(n).map(|&h| (h, m)))
        .collect();
    let by_text_idx = name_to_mnemonic
        .into_iter()
        .map(|(n, m)| (Box::from(n), m))
        .collect();
    MnemonicIndex {
        by_name,
        by_text: by_text_idx,
    }
}

/// Run `task` over the indexes `0..n`, each worker folding into its own `S`;
/// returns the per-worker shards. Workers steal indexes one at a time through a
/// shared cursor, since per-index work is heavily skewed (a few big evaluator
/// lanes). A single worker runs inline.
pub(crate) fn run_sharded<S: Default + Send>(
    workers: usize,
    n: usize,
    task: impl Fn(&mut S, usize) + Sync,
) -> Vec<S> {
    let workers = workers.clamp(1, n.max(1));
    let next = AtomicUsize::new(0);
    let run_one = || {
        let mut shard = S::default();
        loop {
            let i = next.fetch_add(1, Ordering::Relaxed);
            if i >= n {
                break;
            }
            task(&mut shard, i);
        }
        shard
    };
    if workers == 1 {
        return vec![run_one()];
    }
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..workers).map(|_| scope.spawn(run_one)).collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    })
}

/// Sort `(text, handle)` pairs into byte-wise text order. The texts are
/// distinct, so the result is the one total order. Small inputs sort serially —
/// the parallel split only pays off at scale.
fn sort_pairs_by_text<H: Send>(pairs: &mut [(&str, H)]) {
    /// Input size below which the serial sort wins over the parallel split.
    const PARALLEL_MIN: usize = 1 << 13;
    if pairs.len() < PARALLEL_MIN {
        pairs.sort_unstable_by_key(|&(t, _)| t);
    } else {
        workers().install(|| pairs.par_sort_unstable_by_key(|&(t, _)| t));
    }
}

/// One node of the aggregated flame graph: a frame merged across every span that
/// shares this label at the same place in the time-containment tree. `total_us`
/// is the summed weight of the spans merged here (each counted once, at its own
/// node), a span weighing what the graph's [`FlameWeight`] gives it; `self_us` is
/// that minus the children's totals (`max(0, …)`); `count` is how many spans
/// merged at this node. Under
/// [`BottomUp`](FlameDirection::BottomUp) a node is instead one step of a caller
/// chain, and `total_us` and `count` are summed over the contributions that
/// passed through it — the self time and span count of the frames whose chain
/// runs through this node — so one frame is counted at every node of its chain.
/// `width_us` is the layout width
/// `max(total_us, Σ children.width_us)` — a time-contained child may overflow its
/// parent's end (Bazel's catch-all frames), so laying out by `width_us` keeps
/// children inside their parent. `children` is a contiguous range into the
/// owning [`FlameGraph`]'s node array, sorted widest-first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlameNode {
    pub name: Str,
    /// Lexicographic rank of `name`'s text among the distinct names scanned
    /// for this graph's build — rank order equals byte-wise text order — so
    /// name ordering compares integers without resolving text. Ranks are dense
    /// per build and only order names within one graph, never across graphs.
    pub rank: u32,
    pub total_us: i64,
    pub self_us: i64,
    pub width_us: i64,
    pub role: SpanRole,
    /// The category the spans this frame stands for were recorded under — for
    /// a detail frame, the spans of the instance it names — and absent where
    /// they share none that an id reaches (see [`Slice::category`]).
    pub category: OptStr,
    pub count: u32,
    pub depth: u32,
    /// The deepest node depth in this node's subtree (itself included), which
    /// layout reads for how deep the members of a remainder reach.
    pub subtree_max_depth: u32,
    pub children: Span32,
}

/// An aggregated flame graph as struct-of-arrays over its nodes: index 0 is the
/// synthetic root, every field vector is parallel to the node index, and edges
/// live in a compressed-sparse-row pair (`child_off`, `child_pool`) — node `i`'s
/// children are `child_pool[child_off[i]..child_off[i + 1]]`, listed widest-first.
/// All node names are [`Str`] handles into the text of the [`Profile`] the graph
/// holds — every label is already a profile string, so the graph carries no text
/// of its own. Holding the profile by `Arc` keeps the graph free of lifetime
/// parameters, so it is built off the UI thread and handed back across threads.
///
/// Node order is path-sorted: a parent's index precedes every descendant's and
/// each subtree is one contiguous block, the order [`reconstruct`] emits. A pass
/// deriving a node's state from its parent's therefore takes one forward sweep,
/// and one folding children into their parent one reverse sweep.
///
/// Node depth is not stored: layout threads it top-down, and nothing else needs
/// it per node.
#[derive(Debug, Clone)]
pub struct FlameGraph {
    profile: Arc<Profile>,
    name: Vec<Str>,
    rank: Vec<u32>,
    total_us: Vec<i64>,
    self_us: Vec<i64>,
    width_us: Vec<i64>,
    role: Vec<SpanRole>,
    category: Vec<Option<CategoryId>>,
    detail: Vec<bool>,
    count: Vec<u32>,
    subtree_max_depth: Vec<u32>,
    /// CSR row offsets, length `len + 1`.
    child_off: Vec<u32>,
    /// CSR child node ids, one per non-root node, widest-first within each row.
    child_pool: Vec<u32>,
    /// Deepest node depth, found once at build so [`flame_height`] need not scan
    /// every node on each paint.
    max_depth: u32,
}

/// The synthetic root's name. Its label ("All") is the one frame name not in the
/// profile, so it takes the reserved handle, which cannot collide with a real
/// name.
const ROOT_LABEL: Str = Str::RESERVED;

impl FlameGraph {
    /// Assemble node `id`'s [`FlameNode`] at `depth`, leaving `children` for the
    /// caller to fill from the breadth-first layout.
    fn node_at(&self, id: usize, depth: u32) -> FlameNode {
        FlameNode {
            name: self.name[id],
            rank: self.rank[id],
            total_us: self.total_us[id],
            self_us: self.self_us[id],
            width_us: self.width_us[id],
            role: self.role[id],
            category: self.category(id).into(),
            count: self.count[id],
            depth,
            subtree_max_depth: self.subtree_max_depth[id],
            children: Span32::default(),
        }
    }

    /// Materialize the full breadth-first [`FlameNode`] array: root first, then
    /// each node's children as one contiguous block, depths threaded top-down and
    /// each node's `children` range recorded.
    pub fn nodes(&self) -> Vec<FlameNode> {
        let n = self.len();
        let mut out: Vec<FlameNode> = Vec::with_capacity(n);
        out.push(self.node_at(0, 0));
        let mut queue: std::collections::VecDeque<(usize, usize, u32)> =
            std::collections::VecDeque::new();
        queue.push_back((0, 0, 0));
        while let Some((id, oi, depth)) = queue.pop_front() {
            let off = out.len() as u32;
            for k in 0..self.child_count(id) {
                let c = self.child_id(id, k);
                let child_oi = out.len();
                out.push(self.node_at(c, depth + 1));
                queue.push_back((c, child_oi, depth + 1));
            }
            out[oi].children = Span32::new(off, out.len() as u32 - off);
        }
        out
    }

    /// The synthetic root frame (node 0), under which every lane's forest merges.
    pub fn root(&self) -> FlameNode {
        self.nodes()[0]
    }

    /// The direct children of a node, in widest-first order. The node's
    /// `children` range indexes the breadth-first array [`nodes`](Self::nodes)
    /// yields, so this and the node must come from the same graph.
    pub fn children(&self, node: &FlameNode) -> Vec<FlameNode> {
        self.nodes()[node.children.range()].to_vec()
    }

    pub(crate) fn len(&self) -> usize {
        self.name.len()
    }

    /// The root frame's summed weight (node 0's total).
    pub fn root_total_us(&self) -> i64 {
        self.total_us[0]
    }

    pub(crate) fn name(&self, i: usize) -> Str {
        self.name[i]
    }

    pub(crate) fn rank(&self, i: usize) -> u32 {
        self.rank[i]
    }

    pub(crate) fn role(&self, i: usize) -> SpanRole {
        self.role[i]
    }

    /// Node `i`'s category (see [`FlameNode::category`]).
    fn category(&self, i: usize) -> Option<Str> {
        self.category[i].map(|id| self.profile.category(id))
    }

    /// What the name of the frame `node` leaves open about its spans: the
    /// category they were recorded under, or for a detail frame the kind its
    /// instance is one of, wherever the instance's name and category tell that
    /// kind — as they do of every kind but a mnemonic. `None` where the spans
    /// share no category that an id reaches.
    pub fn kind_or_category(&self, node: FlameNodeId) -> Option<Str> {
        let i = node.0 as usize;
        let category = self.category[i]?;
        let kind = self.detail[i]
            .then(|| self.profile.kind_named(category, self.name[i]))
            .flatten();
        Some(kind.unwrap_or_else(|| self.profile.category(category)))
    }

    pub(crate) fn total_us(&self, i: usize) -> i64 {
        self.total_us[i]
    }

    pub(crate) fn self_us(&self, i: usize) -> i64 {
        self.self_us[i]
    }

    /// The [`SpanSelf`] of node `i`, `total_us` giving each node's total.
    pub(crate) fn span_self_by(&self, i: usize, total_us: &impl Fn(usize) -> i64) -> SpanSelf {
        let mut spanned = 0i64;
        let mut largest_detail_us = None;
        for k in 0..self.child_count(i) {
            let child = self.child_id(i, k);
            let child_us = total_us(child);
            if self.is_detail(child) {
                largest_detail_us = largest_detail_us.max(Some(child_us));
            } else {
                spanned += child_us;
            }
        }
        let self_us = (total_us(i) - spanned).max(0);
        SpanSelf {
            self_us,
            peak_us: largest_detail_us.map_or(self_us, |peak| peak.min(self_us)),
        }
    }

    /// The summed [`FlameNode::total_us`] of `remainder`'s members. Summed when
    /// asked for, since a layout visits every member of every run it lays out
    /// and a reader wants the sum of the one run under the cursor.
    pub fn remainder_total_us(&self, remainder: &FlameRemainder) -> i64 {
        let first = remainder.first as usize;
        self.children_of(remainder.parent.0 as usize)[first..first + remainder.frames as usize]
            .iter()
            .map(|&member| self.total_us[member as usize])
            .sum()
    }

    /// The [`SpanSelf::self_us`] of the frame `node`, in the graph's own
    /// weight.
    pub fn span_self_us(&self, node: FlameNodeId) -> i64 {
        self.span_self_by(node.0 as usize, &|i| self.total_us[i])
            .self_us
    }

    pub(crate) fn width_us(&self, i: usize) -> i64 {
        self.width_us[i]
    }

    pub(crate) fn count(&self, i: usize) -> u32 {
        self.count[i]
    }

    /// Whether node `i` is a detail frame — part of its parent's self time
    /// re-labelled with the name of the instance it went to (see
    /// [`emit_lane_rows`]) — rather than a frame of merged spans. A detail
    /// frame is always a leaf: [`emit_lane_rows`] extends no key that carries
    /// one, so no aggregation path runs past a detail frame.
    pub(crate) fn is_detail(&self, i: usize) -> bool {
        self.detail[i]
    }

    pub(crate) fn subtree_max_depth(&self, i: usize) -> u32 {
        self.subtree_max_depth[i]
    }

    /// The number of direct children of node `i`.
    pub(crate) fn child_count(&self, i: usize) -> usize {
        (self.child_off[i + 1] - self.child_off[i]) as usize
    }

    /// The node id of node `i`'s `k`-th child, in widest-first order.
    pub(crate) fn child_id(&self, i: usize, k: usize) -> usize {
        self.child_pool[self.child_off[i] as usize + k] as usize
    }

    /// The node ids of node `i`'s children, in widest-first order.
    pub(crate) fn children_of(&self, i: usize) -> &[u32] {
        &self.child_pool[self.child_off[i] as usize..self.child_off[i + 1] as usize]
    }

    /// Resolves a node name (or a [`StatRow`](crate::StatRow) key) to its text,
    /// through the profile the graph was built from.
    pub fn str(&self, s: Str) -> &str {
        if s == ROOT_LABEL {
            return "All";
        }
        self.profile.str(s)
    }
}

/// The time the spans a node stands for spent in themselves.
pub(crate) struct SpanSelf {
    /// The node's total less its span children's, floored at zero because a
    /// node's children can sum past it. The detail frames beneath the node are
    /// this time broken out by instance, so it counts them in where
    /// [`FlameNode::self_us`] leaves them out.
    pub(crate) self_us: i64,
    /// The most of `self_us` that one instance accounts for: the node's
    /// largest detail frame, and all of `self_us` for a node with none, whose
    /// spans tell no instance from another. A span floors its own self time
    /// at zero as the node does its spans' together, so a detail frame can
    /// exceed `self_us`, which bounds this.
    pub(crate) peak_us: i64,
}

/// Vertical sizing of the flame graph, in pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FlameMetrics {
    pub row_height: f32,
}

/// A box of the flame graph laid out for painting, hit-testing and tooltips.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FlameBox {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    /// The row the box's top edge lies on.
    pub depth: u32,
    pub content: FlameBoxContent,
}

/// What a [`FlameBox`] stands for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FlameBoxContent {
    Frame(FlameFrame),
    Remainder(FlameRemainder),
    /// The rows beneath a remainder that the subtrees of its members in the
    /// box's columns reach down to. It stands for no frame of those subtrees,
    /// none of which is laid out.
    RemainderDepth(FlameRemainder),
}

/// One node of a [`FlameGraph`], as its [`FlameNode`] gives it; `name` is
/// resolved with that graph's [`str`](FlameGraph::str).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FlameFrame {
    pub node: FlameNodeId,
    pub name: Str,
    pub total_us: i64,
    pub role: SpanRole,
    pub count: u32,
}

/// A node of the [`FlameGraph`] a layout was made from, which alone resolves
/// it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlameNodeId(u32);

/// A run of two or more consecutive siblings, each too narrow to draw on its
/// own at the zoom the box was laid out for. Only [`lay_out_flame`] makes one,
/// so a single sibling is always a [`FlameFrame`]. It names its members in the
/// graph the layout was made from, which alone resolves them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FlameRemainder {
    /// The node the members are children of.
    parent: FlameNodeId,
    /// The first member's index among that node's children.
    first: u32,
    frames: u32,
    role: Option<SpanRole>,
    siblings: RemainderSiblings,
}

/// What a remainder's parent holds beside the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemainderSiblings {
    /// Nothing: the run is every child of its parent.
    None,
    /// Children wide enough for a box of their own.
    Wider,
}

impl FlameRemainder {
    /// How many siblings the run holds.
    pub fn frames(&self) -> u32 {
        self.frames
    }

    /// The role the siblings share, `None` where they differ.
    pub fn role(&self) -> Option<SpanRole> {
        self.role
    }

    pub fn siblings(&self) -> RemainderSiblings {
        self.siblings
    }
}

/// The inner of an `action '…'` name, `None` for any other shape. A
/// critical-path component is named `action '<desc>'` where `<desc>` is the
/// action processing slice's name elsewhere; stripping the wrapper lets the
/// name index recover its mnemonic.
fn action_inner(name: &str) -> Option<&str> {
    name.strip_prefix("action '")?.strip_suffix('\'')
}

/// Which text labels a flame frame, and so which spans merge into one frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FlameLabels {
    /// The kind of thing a span is an instance of, where its name says which
    /// instance: an action's mnemonic where one resolves ("Rustc"), the category
    /// of a span named after what it works on ("Fetching repository"), and the
    /// kind a name leads with ("fetching"). Every span of one kind merges into
    /// one frame.
    Kind,
    /// Every span's own name, so an action frame names its target ("Compiling
    /// //x/y:z") and the tree is the profile's own nesting.
    Name,
}

/// Which way a flame graph reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FlameDirection {
    /// Outward from each action, following the profile's own nesting: a frame's
    /// children are the frames it contains.
    TopDown,
    /// Outward from each frame to its callers: the root's children are the
    /// frames themselves, each carrying the time spent in it wherever it
    /// occurs, and depth walks back up the chains that reached it.
    BottomUp,
}

/// What a span weighs in a flame graph, and so what a frame's width measures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FlameWeight {
    /// The span's own duration. Lanes run side by side, so the graph totals
    /// the time spent across all of them, which exceeds the time that elapsed
    /// wherever two overlap.
    Duration,
    /// The span's lane's share of the time that elapsed over the span (see
    /// [`WallShare`]), so the graph of every sharing lane totals the elapsed
    /// time any of them had a span open in, and a subset of them its share of
    /// that, counting twice the time shared by two spans of one lane that
    /// overlap without nesting.
    WallShare,
}

/// How a flame graph presents its spans.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FlameMode {
    pub direction: FlameDirection,
    pub labels: FlameLabels,
    pub weight: FlameWeight,
}

impl Default for FlameMode {
    fn default() -> Self {
        FlameMode {
            direction: FlameDirection::TopDown,
            labels: FlameLabels::Kind,
            weight: FlameWeight::Duration,
        }
    }
}

/// Resolves each span to its aggregation label. Every frame label is an interned
/// handle of the source profile — a span's mnemonic and name already live in its
/// text. The profile interns each distinct string to one handle, so equal text ⇒
/// equal handle: frames key by handle directly, and merge keys compare and hash
/// as plain integers with no per-span string work. Read-only once built, so
/// merge workers share one.
pub(crate) struct Flamer {
    profile: Arc<Profile>,
    idx: MnemonicIndex,
    ranks: Ranks,
}

impl Flamer {
    /// A flamer whose name scan covers every lane, for a build that may walk
    /// any of them.
    pub(crate) fn new(profile: &Arc<Profile>, workers: usize) -> Self {
        Flamer::scoped(profile, workers, |_| true)
    }

    /// A flamer whose name scan covers only the lanes `note_lane` admits —
    /// which must include every lane the build walks (see [`scan`]). The scan
    /// is the dominant cost of a single-scope build, so a build that walks few
    /// lanes scans only those.
    pub(crate) fn scoped(
        profile: &Arc<Profile>,
        workers: usize,
        note_lane: impl Fn(usize) -> bool + Sync,
    ) -> Self {
        let (ranks, idx) = scan(profile, workers, note_lane);
        Flamer {
            profile: Arc::clone(profile),
            idx,
            ranks,
        }
    }

    /// How a span aggregates under `labels`. A span whose mnemonic resolves is
    /// labelled by it, so all Rustc collapse together, and one whose name says
    /// which instance of a kind it is by that kind; any other span — and
    /// every span under [`FlameLabels::Name`] — is labelled by its own name (the
    /// informative label for the rest: "checkOutputs", "subprocess.run", …). The
    /// tree shape comes from time-containment, not from these labels; a label
    /// only decides which spans at the same place merge into one frame.
    fn span_labels(&self, labels: FlameLabels, slice: &Slice, detail: &SliceDetail) -> SpanLabels {
        let kind = match labels {
            FlameLabels::Kind => self
                .mnemonic_of(slice, detail)
                .or_else(|| self.profile.kind_of(slice)),
            FlameLabels::Name => None,
        };
        SpanLabels {
            segment: Segment {
                label: kind.unwrap_or(slice.name),
                role: slice.role,
            },
            instance: kind.and_then(|_| self.profile.instance_of(slice)),
        }
    }

    /// A span's action mnemonic — from its own args if present, else recovered
    /// from the name index by the slice's name or, for a critical-path
    /// component, by its unwrapped `action '…'` description — and `None` for a
    /// span that names no action. Only the critical-path unwrapping consults the
    /// text-keyed index, so the hot per-span path never hashes a full string.
    fn mnemonic_of(&self, slice: &Slice, detail: &SliceDetail) -> Option<Str> {
        if let Some(m) = detail.mnemonic.get() {
            return Some(m);
        }
        if let Some(&m) = self.idx.by_name.get(&slice.name) {
            return Some(m);
        }
        action_inner(self.profile.str(slice.name))
            .and_then(|inner| self.idx.by_text.get(inner).copied())
    }
}

/// A whole-profile name scan (rank table + mnemonic index) held so a
/// track-selection flame graph can be rebuilt for any lane subset without
/// rescanning, and the profile's wall share, held for every reader of it. The
/// scan covers every lane, so it ranks and labels any selection the caller
/// later chooses. Each is computed on first use, so a flamer costs nothing to
/// make until one is asked for.
pub struct TrackFlamer {
    profile: Arc<Profile>,
    flamer: OnceLock<Flamer>,
    /// The lanes reduced over the whole timeline, filled on first use. A lane's
    /// spans emit path keys and reduce into per-node totals that do not depend
    /// on the selection but do key by label.
    lane_tables: ByLabels<OnceLock<Arc<LaneTables>>>,
    /// The lanes last reduced under a restricting window.
    windowed: ByLabels<WindowMemo>,
    /// Built on first use, so a view weighing by duration alone never pays for
    /// it.
    wall_share: OnceLock<WallShare>,
    /// How many times the lanes have been walked under a restricting window,
    /// to reduce them or to fill their wall column, so a test can hold the memo
    /// and the column to their reuse.
    #[cfg(test)]
    reductions: AtomicUsize,
}

/// Every lane's [`LaneTable`], one per [`Profile::threads`] entry, reduced
/// under one labelling and window. A lane never selected (a critical-path lane)
/// still has a table, so the lanes index by thread index directly.
struct LaneTables {
    labels: FlameLabels,
    /// Empty for the whole timeline.
    window: TimeSelection,
    lanes: Vec<LaneTable>,
    /// Each node's summed wall share, per lane and index-parallel to that
    /// lane's `nodes`; filled on first use (see [`wall_columns`]) unless the
    /// reduction carried it.
    wall_us: OnceLock<Vec<Vec<i64>>>,
}

/// A memo of the lanes reduced under one window; the tables admit only what
/// that window does, so asking under another reduces afresh. Its lock spans a
/// read or a write alone and never a reduction, so callers that miss together
/// each reduce and the memo keeps whichever finishes last. The tables sit
/// behind an [`Arc`] so a build takes a handle of its own and merges with the
/// lock released.
#[derive(Default)]
struct WindowMemo(Mutex<Option<Arc<LaneTables>>>);

impl WindowMemo {
    /// The entry, taken up as it stands where a panic elsewhere poisoned the
    /// lock: an entry is only ever replaced wholesale by already-reduced
    /// tables, so no panic can leave one half-written.
    fn entry(&self) -> MutexGuard<'_, Option<Arc<LaneTables>>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn get(&self, window: &TimeSelection) -> Option<Arc<LaneTables>> {
        self.entry()
            .as_ref()
            .filter(|held| held.window == *window)
            .map(Arc::clone)
    }

    fn put(&self, tables: &Arc<LaneTables>) {
        *self.entry() = Some(Arc::clone(tables));
    }
}

/// One `T` per [`FlameLabels`] variant, reached through [`of`](Self::of) — whose
/// exhaustive match is what binds the slots to the variants, so a labelling
/// cannot be added without one.
#[derive(Default)]
struct ByLabels<T> {
    kind: T,
    name: T,
}

impl<T> ByLabels<T> {
    fn of(&self, labels: FlameLabels) -> &T {
        match labels {
            FlameLabels::Kind => &self.kind,
            FlameLabels::Name => &self.name,
        }
    }
}

impl TrackFlamer {
    pub fn new(profile: &Arc<Profile>) -> TrackFlamer {
        TrackFlamer {
            profile: Arc::clone(profile),
            flamer: OnceLock::new(),
            lane_tables: ByLabels::default(),
            windowed: ByLabels::default(),
            wall_share: OnceLock::new(),
            #[cfg(test)]
            reductions: AtomicUsize::new(0),
        }
    }

    /// A flamer whose scan is done, so that making one times the scan alone.
    pub fn scanned(profile: &Arc<Profile>) -> TrackFlamer {
        let scanned = TrackFlamer::new(profile);
        scanned.flamer();
        scanned
    }

    pub fn profile(&self) -> &Arc<Profile> {
        &self.profile
    }

    fn flamer(&self) -> &Flamer {
        self.flamer
            .get_or_init(|| Flamer::new(&self.profile, worker_count()))
    }

    fn wall_share(&self) -> &WallShare {
        self.wall_share
            .get_or_init(|| WallShare::new(&self.profile, worker_count()))
    }

    /// The [`WorkingLanes`] of the wall share this flamer weighs by.
    pub fn working_lanes(&self) -> WorkingLanes {
        self.wall_share().working_lanes()
    }

    #[cfg(test)]
    fn count_walk(&self, window: &TimeSelection) {
        if !window.is_empty() {
            self.reductions.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn reduce_lanes<W: Weigher>(
        &self,
        labels: FlameLabels,
        window: &TimeSelection,
        weigher_of: impl Fn(usize) -> W + Sync,
    ) -> (Vec<LaneTable>, Vec<Vec<W::Weight>>) {
        #[cfg(test)]
        self.count_walk(window);
        build_lane_tables(self.flamer(), labels, window, worker_count(), weigher_of)
    }

    fn reduce(
        &self,
        labels: FlameLabels,
        window: &TimeSelection,
        weight: FlameWeight,
    ) -> LaneTables {
        let (lanes, wall_us) = match weight {
            FlameWeight::Duration => {
                let (lanes, _) = self.reduce_lanes(labels, window, |_| DurationOnly);
                (lanes, OnceLock::new())
            }
            FlameWeight::WallShare => {
                let share = self.wall_share();
                let (lanes, wall_us) = self.reduce_lanes(labels, window, |lane| share.lane(lane));
                (lanes, OnceLock::from(wall_us))
            }
        };
        LaneTables {
            labels,
            window: window.clone(),
            lanes,
            wall_us,
        }
    }

    /// The lanes reduced under `labels` and `window`, reduced on the first ask
    /// and held: once per labelling for the whole timeline, and for the window
    /// last asked for.
    fn tables(
        &self,
        labels: FlameLabels,
        window: &TimeSelection,
        weight: FlameWeight,
    ) -> Arc<LaneTables> {
        if window.is_empty() {
            let whole = self
                .lane_tables
                .of(labels)
                .get_or_init(|| Arc::new(self.reduce(labels, window, weight)));
            return Arc::clone(whole);
        }
        let memo = self.windowed.of(labels);
        if let Some(held) = memo.get(window) {
            return held;
        }
        let tables = Arc::new(self.reduce(labels, window, weight));
        memo.put(&tables);
        tables
    }

    fn wall_us<'t>(&self, tables: &'t LaneTables) -> &'t [Vec<i64>] {
        tables.wall_us.get_or_init(|| {
            #[cfg(test)]
            self.count_walk(&tables.window);
            wall_columns(self.flamer(), tables, self.wall_share(), worker_count())
        })
    }

    #[cfg(test)]
    fn reductions(&self) -> usize {
        self.reductions.load(Ordering::Relaxed)
    }

    /// Build the selection's flame graph, weighed by duration, by merging the
    /// selected lanes' [`LaneTable`]s: concatenate their reduced nodes, sort
    /// the union by path key, and reconstruct. The lanes are reduced under
    /// `window` (see [`emit_lane_rows`]) and the reduction is held, so it is
    /// paid once per labelling and window asked for, never for one that is
    /// not, and a change of selection alone pays only the merge.
    pub(crate) fn build(
        &self,
        labels: FlameLabels,
        window: &TimeSelection,
        selected: impl Fn(usize) -> bool + Sync,
    ) -> FlameGraph {
        let tables = self.tables(labels, window, FlameWeight::Duration);
        let lanes = &tables.lanes;
        let elems = sorted_nodes(lanes, &selected);
        let (arena, _) = reconstruct(&elems, |run| {
            let first = run[0].src.of(lanes);
            let mut total_us = 0i64;
            let mut count = 0u32;
            let mut category = first.category;
            for e in run {
                let n = e.src.of(lanes);
                total_us += n.total_us;
                count += n.count;
                category = shared_category(category, n.category);
            }
            (first.arena_node(total_us, count, category), ())
        });
        drop(elems);
        let kids = csr_from_parents(&arena.parents);
        seal_arena(&self.profile, arena, kids)
    }

    /// [`build`](Self::build)'s graph weighed by wall share, merged from the
    /// same held reduction.
    pub(crate) fn build_wall_share(
        &self,
        labels: FlameLabels,
        window: &TimeSelection,
        selected: impl Fn(usize) -> bool + Sync,
    ) -> WallShareFlame {
        let tables = self.tables(labels, window, FlameWeight::WallShare);
        let lanes = &tables.lanes;
        let wall_us = self.wall_us(&tables);
        let elems = sorted_nodes(lanes, &selected);
        let (arena, duration_us) = reconstruct(&elems, |run| {
            let first = run[0].src.of(lanes);
            let mut total_us = 0i64;
            let mut duration_us = 0i64;
            let mut count = 0u32;
            let mut category = first.category;
            for e in run {
                let n = e.src.of(lanes);
                total_us += wall_us[e.src.lane as usize][e.src.node as usize];
                duration_us += n.total_us;
                count += n.count;
                category = shared_category(category, n.category);
            }
            (first.arena_node(total_us, count, category), duration_us)
        });
        drop(elems);
        let kids = csr_from_parents(&arena.parents);
        WallShareFlame {
            flame: seal_arena(&self.profile, arena, kids),
            duration_us,
        }
    }
}

/// A flame graph weighed by [`FlameWeight::WallShare`], with the durations a
/// duration-weighed graph of the same spans would total.
pub(crate) struct WallShareFlame {
    pub(crate) flame: FlameGraph,
    /// Each node's summed span duration, index-parallel to the graph's nodes.
    pub(crate) duration_us: Vec<i64>,
}

/// The nodes of the lanes `selected` admits, sorted by path key.
fn sorted_nodes<'t>(
    lanes: &'t [LaneTable],
    selected: &(impl Fn(usize) -> bool + Sync),
) -> Vec<KeyRef<'t>> {
    let union_len: usize = lanes
        .iter()
        .enumerate()
        .filter(|&(i, _)| selected(i))
        .map(|(_, t)| t.nodes.len())
        .sum();
    let mut elems: Vec<KeyRef> = Vec::with_capacity(union_len);
    for (i, table) in lanes.iter().enumerate() {
        if !selected(i) {
            continue;
        }
        for (node_idx, node) in table.nodes.iter().enumerate() {
            elems.push(KeyRef {
                key: &table.key_pool[node.key.range()],
                src: Src {
                    lane: i as u32,
                    node: node_idx as u32,
                },
            });
        }
    }
    workers().install(|| elems.par_sort_by(|a, b| a.key.cmp(b.key)));
    elems
}

/// One lane's path-reduced node table: every distinct aggregation path in the
/// lane, reduced to one [`LaneNode`], with the paths' key bytes packed into one
/// shared `key_pool` (rather than a `Vec<u8>` per node) and `nodes` sorted
/// ascending by key bytes.
struct LaneTable {
    key_pool: Vec<u8>,
    nodes: Vec<LaneNode>,
}

impl LaneTable {
    /// Each node's summed weight over the `rows` on its path. `rows` ascend by
    /// key, as the nodes do.
    fn weights_of(&self, rows: &[Row<i64>]) -> Vec<i64> {
        use std::cmp::Ordering::{Equal, Greater, Less};
        let mut rows = rows.iter().peekable();
        self.nodes
            .iter()
            .map(|node| {
                let key = &self.key_pool[node.key.range()];
                let mut weight = 0i64;
                while let Some(row) = rows.peek() {
                    match row.key.as_slice().cmp(key) {
                        Less => {}
                        Equal => weight += row.weight,
                        Greater => break,
                    }
                    rows.next();
                }
                weight
            })
            .collect()
    }
}

/// One reduced node of a [`LaneTable`]: the frame a lane's spans on one
/// aggregation path merge into. `key` is the byte range of that path's key in
/// the owning table's `key_pool`; `total_us`/`count` sum the lane's spans on
/// this path.
struct LaneNode {
    key: Span32,
    label: Str,
    rank: u32,
    role: SpanRole,
    category: Option<CategoryId>,
    detail: bool,
    total_us: i64,
    count: u32,
}

impl LaneNode {
    /// The arena node merging this node with the others on its path, which
    /// share everything but what they sum to and the category they were
    /// recorded under.
    fn arena_node(&self, total_us: i64, count: u32, category: Option<CategoryId>) -> ArenaNode {
        ArenaNode {
            name: self.label,
            rank: self.rank,
            role: self.role,
            category,
            detail: self.detail,
            total_us,
            count,
        }
    }
}

/// A weight summed per node beside the one the node carries: none, or a second
/// time in microseconds.
trait Weight: Copy + Default + Send {
    fn add(&mut self, more: Self);

    /// What is left of a span's weight with its nested spans' taken out,
    /// floored at zero as its self time is.
    fn less(self, nested: Self) -> Self;
}

impl Weight for () {
    fn add(&mut self, (): ()) {}

    fn less(self, (): ()) {}
}

impl Weight for i64 {
    fn add(&mut self, more: i64) {
        *self += more;
    }

    fn less(self, nested: i64) -> i64 {
        (self - nested).max(0)
    }
}

/// What a lane's reduction weighs each slice by beside its duration.
trait Weigher {
    type Weight: Weight;

    /// The weight of what `window` holds of `slice`.
    fn weigh(&mut self, window: &TimeSelection, slice: &Slice) -> Self::Weight;
}

/// Weighs nothing, for a reduction summing durations alone.
struct DurationOnly;

impl Weigher for DurationOnly {
    type Weight = ();

    fn weigh(&mut self, _: &TimeSelection, _: &Slice) {}
}

/// Weighs a slice by its lane's share of each stretch of it the window holds,
/// clipped in time before it is weighed.
impl Weigher for LaneShare<'_> {
    type Weight = i64;

    fn weigh(&mut self, window: &TimeSelection, slice: &Slice) -> i64 {
        if window.is_empty() {
            return self.between(slice.start_us, slice.end_us());
        }
        window
            .clipped(slice.start_us, slice.end_us())
            .map(|held| self.between(held.start_us, held.end_us))
            .sum()
    }
}

/// `of(i)` for each lane index `i`, in lane order, computed across `workers`.
fn per_lane<T: Send>(flamer: &Flamer, workers: usize, of: impl Fn(usize) -> T + Sync) -> Vec<T> {
    let n = flamer.profile.threads().len();
    let shards = run_sharded(workers, n, |out: &mut Vec<(usize, T)>, i| {
        out.push((i, of(i)));
    });
    let mut built: Vec<(usize, T)> = Vec::with_capacity(n);
    built.extend(shards.into_iter().flatten());
    built.sort_unstable_by_key(|&(i, _)| i);
    built.into_iter().map(|(_, lane)| lane).collect()
}

/// Build every lane's [`LaneTable`], each beside its nodes' summed second
/// weights, index-parallel to them. `weigher_of` gives the lane at an index its
/// weigher.
fn build_lane_tables<W: Weigher>(
    flamer: &Flamer,
    labels: FlameLabels,
    window: &TimeSelection,
    workers: usize,
    weigher_of: impl Fn(usize) -> W + Sync,
) -> (Vec<LaneTable>, Vec<Vec<W::Weight>>) {
    per_lane(flamer, workers, |i| {
        build_lane_table(flamer, labels, window, i, weigher_of(i))
    })
    .into_iter()
    .unzip()
}

/// The wall column of `tables`: each lane's rows weighed by `share` and summed
/// onto the lane's held nodes.
fn wall_columns(
    flamer: &Flamer,
    tables: &LaneTables,
    share: &WallShare,
    workers: usize,
) -> Vec<Vec<i64>> {
    per_lane(flamer, workers, |i| {
        let rows = sorted_lane_rows(flamer, tables.labels, &tables.window, i, share.lane(i));
        tables.lanes[i].weights_of(&rows)
    })
}

/// Lane `lane_index`'s rows (see [`emit_lane_rows`]), ascending by key.
fn sorted_lane_rows<W: Weigher>(
    flamer: &Flamer,
    labels: FlameLabels,
    window: &TimeSelection,
    lane_index: usize,
    mut weigher: W,
) -> Vec<Row<W::Weight>> {
    let thread = &flamer.profile.threads()[lane_index];
    let mut rows = Vec::new();
    emit_lane_rows(flamer, labels, thread, window, &mut weigher, &mut rows);
    rows.sort_unstable_by(|a, b| a.key.cmp(&b.key));
    rows
}

/// Reduce lane `lane_index` to its [`LaneTable`]: emit one keyed row per span,
/// sort by key, segment-reduce equal keys into nodes, and pack each distinct key
/// into the shared `key_pool`. The second weights the rows carry reduce beside
/// the nodes, index-parallel to them.
fn build_lane_table<W: Weigher>(
    flamer: &Flamer,
    labels: FlameLabels,
    window: &TimeSelection,
    lane_index: usize,
    weigher: W,
) -> (LaneTable, Vec<W::Weight>) {
    let rows = sorted_lane_rows(flamer, labels, window, lane_index, weigher);

    let mut key_pool: Vec<u8> = Vec::new();
    let mut nodes: Vec<LaneNode> = Vec::with_capacity(rows.len());
    let mut weights: Vec<W::Weight> = Vec::with_capacity(rows.len());
    let mut i = 0;
    while i < rows.len() {
        let mut total = rows[i].dur;
        let mut weight = rows[i].weight;
        let mut count = 1u32;
        let mut category = rows[i].category;
        let mut j = i + 1;
        while j < rows.len() && rows[j].key == rows[i].key {
            total += rows[j].dur;
            weight.add(rows[j].weight);
            count += 1;
            category = shared_category(category, rows[j].category);
            j += 1;
        }
        let off = key_pool.len() as u32;
        key_pool.extend_from_slice(&rows[i].key);
        nodes.push(LaneNode {
            key: Span32::new(off, rows[i].key.len() as u32),
            label: rows[i].label,
            rank: rows[i].rank,
            role: rows[i].role,
            category,
            detail: ends_at_detail(&rows[i].key),
            total_us: total,
            count,
        });
        weights.push(weight);
        i = j;
    }
    (LaneTable { key_pool, nodes }, weights)
}

/// Bytes encoding one aggregation-path segment in a [`Row`] key: the label's
/// [`Ranks`] rank as 4 big-endian bytes, then one [`segment_flags`] byte.
/// Big-endian keeps lexicographic key order equal to rank order, and equal
/// segments produce equal bytes, so a path's key is a byte prefix of every
/// descendant's key.
const SEGMENT_BYTES: usize = 5;

/// One byte distinguishing a segment by everything about it but its label: the
/// [`SpanRole`]'s payload flag in bit 0 and its 8-variant workload in bits 1..4,
/// and `detail` in bit 7 — so a detail frame never merges with a same-named real
/// child of the same role. Equal segments encode equally.
fn segment_flags(role: SpanRole, detail: bool) -> u8 {
    ((detail as u8) << DETAIL_BIT) | ((role.workload as u8) << 1) | (role.payload as u8)
}

/// The bit of [`segment_flags`] that marks a detail frame.
const DETAIL_BIT: u32 = 7;

/// Whether the frame an aggregation-path key ends at is a detail frame.
fn ends_at_detail(key: &[u8]) -> bool {
    key.last().is_some_and(|flags| flags >> DETAIL_BIT != 0)
}

/// One emitted frame contribution in the path-sort aggregation: its full
/// aggregation-path key (5 bytes per segment, root-to-frame), the frame it
/// merges into, and the duration and second weight it brings. Rows sharing a
/// key merge into one node. A row whose key [`ends_at_detail`] is a detail row
/// — a span's self time broken out under the instance it belongs to — rather
/// than a span of its own.
struct Row<X> {
    key: Vec<u8>,
    label: Str,
    rank: u32,
    role: SpanRole,
    /// The category of the span the row is for, and in a detail row that of
    /// the span whose name it carries.
    category: Option<CategoryId>,
    dur: i64,
    weight: X,
}

// A lane's rows are sorted by value, so a row's size is what the sort moves.
#[cfg(target_pointer_width = "64")]
const _: () = assert!(std::mem::size_of::<Row<()>>() == 48);

/// One node's field values as [`Arena::push`] takes them, decomposed into the
/// arena's parallel vectors on the way in.
struct ArenaNode {
    name: Str,
    rank: u32,
    role: SpanRole,
    category: Option<CategoryId>,
    detail: bool,
    total_us: i64,
    count: u32,
}

/// An aggregation tree under reconstruction, seeded with the synthetic root at
/// index 0. The node fields carry the [`FlameGraph`]'s own struct-of-arrays
/// shape, so sealing moves the vectors out; the capacity an arena is built with
/// is therefore the graph's own, and must be a node count rather than a bound
/// on one. `parents` is parallel to the node fields and the root's entry is
/// `u32::MAX`, which no node index reaches.
struct Arena {
    name: Vec<Str>,
    rank: Vec<u32>,
    role: Vec<SpanRole>,
    category: Vec<Option<CategoryId>>,
    detail: Vec<bool>,
    total_us: Vec<i64>,
    count: Vec<u32>,
    parents: Vec<u32>,
}

impl Arena {
    fn with_capacity(n: usize) -> Arena {
        let mut arena = Arena {
            name: Vec::with_capacity(n),
            rank: Vec::with_capacity(n),
            role: Vec::with_capacity(n),
            category: Vec::with_capacity(n),
            detail: Vec::with_capacity(n),
            total_us: Vec::with_capacity(n),
            count: Vec::with_capacity(n),
            parents: Vec::with_capacity(n),
        };
        arena.push(
            ArenaNode {
                name: ROOT_LABEL,
                rank: 0,
                role: SpanRole {
                    workload: Workload::Other,
                    payload: false,
                },
                category: None,
                detail: false,
                total_us: 0,
                count: 0,
            },
            u32::MAX,
        );
        arena
    }

    fn len(&self) -> usize {
        self.name.len()
    }

    /// Places `node` under `parent`, returning its index.
    // A reconstruction loop pushes once per node of the graph, and inlined
    // there each field goes from its source to its vector without the node
    // being assembled on the stack for a call.
    #[inline(always)]
    fn push(&mut self, node: ArenaNode, parent: u32) -> usize {
        let idx = self.len();
        self.name.push(node.name);
        self.rank.push(node.rank);
        self.role.push(node.role);
        self.category.push(node.category);
        self.detail.push(node.detail);
        self.total_us.push(node.total_us);
        self.count.push(node.count);
        self.parents.push(parent);
        idx
    }

    /// Adds a finished node's aggregates to its parent's, so a parent totals its
    /// whole subtree. A detail frame's spans are its parent's own, which the
    /// parent counts itself.
    fn fold_up(&mut self, node: usize, parent: usize) {
        self.total_us[parent] += self.total_us[node];
        if !self.detail[node] {
            self.count[parent] += self.count[node];
        }
    }
}

/// Parent-child edges of the reconstruction arena in compressed-sparse-row form:
/// node `i`'s children are `pool[off[i]..off[i + 1]]`. `off` has length n+1;
/// `pool` holds one entry per non-root node, listing children in ascending
/// creation index until [`sort_child_ranges`] sorts each range widest-first.
struct ArenaChildren {
    off: Vec<u32>,
    pool: Vec<u32>,
}

impl ArenaChildren {
    fn of(&self, i: usize) -> &[u32] {
        &self.pool[self.off[i] as usize..self.off[i + 1] as usize]
    }
}

/// The containment parent of each slice: the nearest enclosing earlier slice, or
/// `None` for a top-level slice. A depth stack of open slices, popped by
/// [`encloses`], yields each slice's parent index directly.
///
/// Children of one parent can overlap each other, and each can overrun the
/// parent, so their durations can sum past the parent's own and a self time
/// derived by subtracting them is floored at zero.
fn containment_parents(slices: &[Slice]) -> Vec<Option<usize>> {
    debug_assert!(
        slices.windows(2).all(|w| {
            w[0].start_us < w[1].start_us
                || (w[0].start_us == w[1].start_us && w[0].dur_us >= w[1].dur_us)
        }),
        "slices must be in lane order: start ascending, ties longest-first"
    );
    let mut parents = vec![None; slices.len()];
    let mut stack: Vec<usize> = Vec::new();
    for (i, slice) in slices.iter().enumerate() {
        let slice_end = slice.end_us();
        while stack
            .last()
            .is_some_and(|&top| !encloses(slices[top].end_us(), slice.start_us, slice_end))
        {
            stack.pop();
        }
        parents[i] = stack.last().copied();
        stack.push(i);
    }
    parents
}

/// The range of a track's slices that can reach `span`, holding with them the
/// slices those key onto, so a containment walk over the range gives each of
/// them the parent chain the whole lane would. Found by binary search on either
/// end: starts are ascending, so none starting past the span's end reaches it,
/// and `end_maxes` is monotone, so the first element reaching the span's start
/// is where slices ending before it stop. `end_maxes` must be the track's
/// [`Profile::end_maxes`], index-parallel to `slices`.
///
/// Holding those ancestors needs no slack under the span's start, though
/// [`encloses`] does let a child's end overshoot its parent's. A slice keys a
/// row only when it and every ancestor of it in range reaches the span, which
/// puts each of their ends at or past the span's start — a zero-duration slice
/// at the start itself, by the microsecond it occupies (see [`occupied_span`]),
/// any other past it. Their outermost was taken by its own parent either on its
/// end, leaving that parent ending no earlier than it does, or on the overlap,
/// leaving that parent ending past its start: past the span's start for the
/// zero-duration case, whose start is its end, and otherwise one step back from
/// an end already one clear. So no parent of a keyed slice ends before the span
/// starts, and the prefix this drops holds none of them.
fn reaching(slices: &[Slice], end_maxes: &[i64], span: TimeSpan) -> std::ops::Range<usize> {
    let upper = slices.partition_point(|s| s.start_us <= span.end_us);
    let lower = end_maxes[..upper].partition_point(|&e| e < span.start_us);
    lower..upper
}

/// The microseconds of `slice` that lie inside `window`, and `None` for a slice
/// the window holds no part of. A slice contributes when the time it occupies
/// (see [`occupied_span`]) reaches into the window, and brings what the window
/// holds of its own extent — nothing, for a slice of zero duration.
fn windowed_dur_us(window: &TimeSelection, slice: &Slice) -> Option<i64> {
    if window.is_empty() {
        return Some(slice.dur_us);
    }
    let occupied = occupied_span(slice.start_us, slice.dur_us);
    window
        .overlaps(occupied.start_us, occupied.end_us)
        .then(|| window.overlap_us(slice.start_us, slice.end_us()))
}

/// Emit one [`Row`] per slice of `thread` into `out`, keyed by its aggregation
/// path. Slices are in lane order, so a slice's containment parent precedes it
/// and its key is the parent's key extended by this slice's segment.
///
/// A slice that belongs to an instance of a kind — itself, where its label is
/// its kind, or else the nearest enclosing such slice — also emits a detail
/// row: an extra child frame labelled with that instance's name and carrying
/// the slice's self time, so grouping by kind still shows which target,
/// repository or path the time went to. Under [`FlameLabels::Name`] no slice is
/// labelled by a kind and no detail row is emitted.
///
/// Durations are what `window` holds of them (see [`windowed_dur_us`]), and a
/// slice it holds none of emits no row at all, so a frame's total, self time and
/// span count all read as of the window. Which rows there are goes by duration
/// alone, whatever `weigher` weighs them by beside it, so every reduction of a
/// lane under one labelling and window emits the same keys.
fn emit_lane_rows<W: Weigher>(
    flamer: &Flamer,
    labels: FlameLabels,
    thread: &ThreadTrack,
    window: &TimeSelection,
    weigher: &mut W,
    out: &mut Vec<Row<W::Weight>>,
) {
    // Regions spread far apart bracket to nearly the whole lane; only a narrow
    // selection shrinks the walk.
    let lane = flamer.profile.slices(thread);
    let bracket = match window.bounds() {
        Some(bounds) => reaching(lane, flamer.profile.end_maxes(thread), *bounds),
        None => 0..lane.len(),
    };
    let slices = &lane[bracket.clone()];
    let details = &flamer.profile.details(thread)[bracket];
    let parents = containment_parents(slices);
    // What each slice brings, `None` for one left out. A slice whose containment
    // parent is left out is left out with it, so whatever a slice keys onto is
    // itself keyed; one forward pass carries that down the chain.
    let mut brought: Vec<Option<(i64, W::Weight)>> = Vec::with_capacity(slices.len());
    for i in 0..slices.len() {
        let dur_us = match parents[i] {
            Some(p) if brought[p].is_none() => None,
            _ => windowed_dur_us(window, &slices[i]),
        };
        brought.push(dur_us.map(|dur| (dur, weigher.weigh(window, &slices[i]))));
    }
    // Each slice's direct containment children's summed duration; its own
    // duration less this is its self time. Each child is clipped before it is
    // summed, so the window's figure is built from the same terms as the whole
    // timeline's.
    let mut nested: Vec<(i64, W::Weight)> = vec![Default::default(); slices.len()];
    for i in 0..slices.len() {
        if let (Some(p), Some((dur, weight))) = (parents[i], brought[i]) {
            nested[p].0 += dur;
            nested[p].1.add(weight);
        }
    }
    // At most one detail row per slice.
    out.reserve(2 * slices.len());
    let mut keys: Vec<Vec<u8>> = Vec::with_capacity(slices.len());
    // Each slice's instance, with the category it was recorded under.
    let mut instances: Vec<Option<(Str, Option<CategoryId>)>> = Vec::with_capacity(slices.len());
    for i in 0..slices.len() {
        let Some((dur, weight)) = brought[i] else {
            // Placeholders, so both vectors stay indexed by slice.
            keys.push(Vec::new());
            instances.push(None);
            continue;
        };
        let span = flamer.span_labels(labels, &slices[i], &details[i]);
        let seg = span.segment;
        let rank = flamer.ranks.rank_of(seg.label);
        let parent_key = match parents[i] {
            Some(p) => keys[p].as_slice(),
            None => &[],
        };
        let mut key = Vec::with_capacity(parent_key.len() + SEGMENT_BYTES);
        key.extend_from_slice(parent_key);
        key.extend_from_slice(&rank.to_be_bytes());
        key.push(segment_flags(seg.role, false));
        out.push(Row {
            key: key.clone(),
            label: seg.label,
            rank,
            role: seg.role,
            category: slices[i].category,
            dur,
            weight,
        });

        let instance = span
            .instance
            .map(|name| (name, slices[i].category))
            .or_else(|| parents[i].and_then(|p| instances[p]));
        let self_us = (dur - nested[i].0).max(0);
        if let Some((instance, category)) = instance.filter(|_| self_us > 0) {
            let rank = flamer.ranks.rank_of(instance);
            let mut detail_key = Vec::with_capacity(key.len() + SEGMENT_BYTES);
            detail_key.extend_from_slice(&key);
            detail_key.extend_from_slice(&rank.to_be_bytes());
            detail_key.push(segment_flags(seg.role, true));
            out.push(Row {
                key: detail_key,
                label: instance,
                rank,
                role: seg.role,
                category,
                dur: self_us,
                weight: weight.less(nested[i].1),
            });
        }
        keys.push(key);
        instances.push(instance);
    }
}

/// A node's place among its siblings: widest first, ties broken by name rank,
/// then role, then the detail flag. A parent holds at most one child per
/// (label, role, detail) triple and a rank bijects with its label, so two
/// siblings differ in at least one of the last three. [`sort_child_ranges`]
/// nonetheless sorts on this paired with the child id, so what its unstable
/// sorts produce is fixed by the comparison alone rather than by that
/// uniqueness.
type ChildOrder = (std::cmp::Reverse<i64>, u32, SpanRole, bool);

fn child_order(arena: &Arena, i: usize) -> ChildOrder {
    (
        std::cmp::Reverse(arena.total_us[i]),
        arena.rank[i],
        arena.role[i],
        arena.detail[i],
    )
}

/// Entries a child range must hold before spreading its sort across the workers
/// beats sorting it whole on one.
fn wide_range_min() -> usize {
    const PER_WORKER: usize = 1 << 13;
    PER_WORKER * worker_count()
}

/// Sort each node's CSR child range into [`ChildOrder`]. A range sorts on keys
/// gathered into a buffer beside its child ids, so a comparison reads that
/// buffer rather than indexing the arena at a child's id. Each node's range is
/// disjoint, so the pool tiles into per-node mutable sub-slices that sort across
/// nodes in parallel. Range widths are heavily skewed — the inverted root holds
/// one child per distinct frame — so a range [`wide_range_min`] admits is itself
/// spread over the workers.
fn sort_child_ranges(kids: &mut ArenaChildren, n: usize, arena: &Arena) {
    /// Arena size below which sorting the ranges serially wins over the
    /// parallel split.
    const PARALLEL_MIN: usize = 1 << 13;

    let wide_min = wide_range_min();
    let off = &kids.off;
    let range_len = |i: usize| (off[i + 1] - off[i]) as usize;
    let (mut tile_count, mut wide_count) = (0usize, 0usize);
    for i in 0..n {
        let len = range_len(i);
        wide_count += (len >= wide_min) as usize;
        tile_count += (len >= 2 && len < wide_min) as usize;
    }
    let mut tiles: Vec<&mut [u32]> = Vec::with_capacity(tile_count);
    let mut wide: Vec<&mut [u32]> = Vec::with_capacity(wide_count);
    let mut rest: &mut [u32] = &mut kids.pool;
    for i in 0..n {
        let (head, tail) = rest.split_at_mut(range_len(i));
        rest = tail;
        if head.len() >= wide_min {
            wide.push(head);
        } else if head.len() >= 2 {
            tiles.push(head);
        }
    }
    workers().install(|| {
        for tile in &mut wide {
            let mut buf: Vec<(ChildOrder, u32)> = tile
                .par_iter()
                .map(|&c| (child_order(arena, c as usize), c))
                .collect();
            buf.par_sort_unstable();
            for (dst, &(_, c)) in tile.iter_mut().zip(buf.iter()) {
                *dst = c;
            }
        }
    });
    let sort_tile = |buf: &mut Vec<(ChildOrder, u32)>, tile: &mut [u32]| {
        buf.clear();
        buf.extend(tile.iter().map(|&c| (child_order(arena, c as usize), c)));
        buf.sort_unstable();
        for (dst, &(_, c)) in tile.iter_mut().zip(buf.iter()) {
            *dst = c;
        }
    };
    if n < PARALLEL_MIN {
        let mut buf = Vec::new();
        for tile in &mut tiles {
            sort_tile(&mut buf, tile);
        }
    } else {
        workers().install(|| {
            tiles
                .par_iter_mut()
                .for_each_init(Vec::new, |buf, tile| sort_tile(buf, tile));
        });
    }
}

/// Seal a reconstruction arena into a [`FlameGraph`], whose node order it
/// carries unchanged and whose vectors it moves out of the arena.
fn seal_arena(profile: &Arc<Profile>, arena: Arena, mut kids: ArenaChildren) -> FlameGraph {
    let n = arena.len();
    sort_child_ranges(&mut kids, n, &arena);
    let Arena {
        name,
        rank,
        role,
        category,
        detail,
        total_us,
        count,
        parents,
    } = arena;

    let mut depth = vec![0u32; n];
    let mut max_depth = 0u32;
    for i in 1..n {
        let d = depth[parents[i] as usize] + 1;
        depth[i] = d;
        max_depth = max_depth.max(d);
    }

    let mut self_us = vec![0i64; n];
    let mut width_us = vec![0i64; n];
    let mut subtree_max_depth = vec![0u32; n];
    for i in (0..n).rev() {
        let mut children_total = 0i64;
        let mut children_width = 0i64;
        let mut smd = depth[i];
        for &c in kids.of(i) {
            let c = c as usize;
            children_total += total_us[c];
            children_width += width_us[c];
            smd = smd.max(subtree_max_depth[c]);
        }
        self_us[i] = (total_us[i] - children_total).max(0);
        width_us[i] = total_us[i].max(children_width);
        subtree_max_depth[i] = smd;
    }

    FlameGraph {
        profile: Arc::clone(profile),
        name,
        rank,
        total_us,
        self_us,
        width_us,
        role,
        category,
        detail,
        count,
        subtree_max_depth,
        child_off: kids.off,
        child_pool: kids.pool,
        max_depth,
    }
}

/// One path-sorted element awaiting tree reconstruction: the aggregation-path
/// key bytes (borrowed from the source lane's `key_pool`) sorted on, and `src`
/// locating the [`LaneNode`] the coalesced frame draws from. Holding the
/// key slice keeps the sort comparator a single deref into `key_pool`; the heavy
/// payload is read back through `src` at reconstruction.
#[derive(Clone, Copy)]
struct KeyRef<'a> {
    key: &'a [u8],
    src: Src,
}

/// Locates a [`LaneNode`] within one labelling's lane tables: `lane` indexes the
/// tables, `node` that table's `.nodes`.
#[derive(Clone, Copy)]
struct Src {
    lane: u32,
    node: u32,
}

impl Src {
    fn of(self, lanes: &[LaneTable]) -> &LaneNode {
        &lanes[self.lane as usize].nodes[self.node as usize]
    }
}

/// Rebuild the aggregation tree from key-sorted elements, returning the arena
/// for [`seal_arena`]. Ascending key order lists every parent before its
/// descendants, so a stack indexed by depth locates each node's parent; each
/// maximal run of equal keys is one node, built by `node_of` from the run
/// together with a second weight for it. Those come back index-parallel to the
/// arena's nodes. The root's total, and its second weight, is the sum over the
/// depth-1 nodes.
fn reconstruct<X: Weight>(
    elems: &[KeyRef],
    node_of: impl Fn(&[KeyRef]) -> (ArenaNode, X),
) -> (Arena, Vec<X>) {
    let run_breaks = workers().install(|| {
        elems
            .par_windows(2)
            .filter(|w| w[0].key != w[1].key)
            .count()
    });
    // One node per maximal run of equal keys, and the root.
    let nodes = 1 + usize::from(!elems.is_empty()) + run_breaks;
    let mut arena = Arena::with_capacity(nodes);
    let mut weights: Vec<X> = Vec::with_capacity(nodes);
    weights.push(X::default());
    // `stack[d]` holds the arena index of the current path's depth-`d` node; the
    // root seeds depth 0. A node at depth `d` finds its parent at `stack[d - 1]`,
    // still in place from when that parent was reduced.
    let mut stack: Vec<usize> = vec![0];
    let mut root_total = 0i64;
    let mut i = 0;
    while i < elems.len() {
        let mut j = i + 1;
        while j < elems.len() && elems[j].key == elems[i].key {
            j += 1;
        }
        let (node, weight) = node_of(&elems[i..j]);
        let depth = elems[i].key.len() / SEGMENT_BYTES;
        if depth == 1 {
            root_total += node.total_us;
            weights[0].add(weight);
        }
        let idx = arena.push(node, stack[depth - 1] as u32);
        weights.push(weight);
        stack.truncate(depth);
        stack.push(idx);
        i = j;
    }
    arena.total_us[0] = root_total;
    debug_assert_eq!(arena.len(), nodes, "the run count must be exact");
    (arena, weights)
}

/// The CSR child edges of an arena described by its parent array. `off[p]`
/// doubles as `p`'s fill cursor, leaving every offset one slot past where it
/// belongs, which shifting the array right restores. The ascending-index fill
/// lists each parent's children in creation order, the pre-sort order
/// [`seal_arena`] reorders widest-first.
fn csr_from_parents(parents: &[u32]) -> ArenaChildren {
    let n = parents.len();
    let mut off = vec![0u32; n + 1];
    for i in 1..n {
        off[parents[i] as usize + 1] += 1;
    }
    for k in 0..n {
        off[k + 1] += off[k];
    }
    let mut pool = vec![0u32; n - 1];
    for (i, &parent) in parents.iter().enumerate().skip(1) {
        let p = parent as usize;
        pool[off[p] as usize] = i as u32;
        off[p] += 1;
    }
    off.copy_within(0..n, 1);
    off[0] = 0;
    ArenaChildren { off, pool }
}

/// Write one [`SEGMENT_BYTES`]-wide aggregation-path segment at the start of
/// `out`.
fn write_segment(out: &mut [u8], rank: u32, role: SpanRole, detail: bool) {
    let rank = rank.to_be_bytes();
    out[..rank.len()].copy_from_slice(&rank);
    out[rank.len()] = segment_flags(role, detail);
}

/// Every node's caller-chain key — the path the inversion hangs that node's
/// frame on, one [`SEGMENT_BYTES`] segment per frame from the root of the
/// inverted tree outward — packed into one byte pool in node order, indexed by
/// `segments`.
struct ChainKeys {
    pool: Vec<u8>,
    segments: ChainSegments,
}

/// Where each node's caller-chain key lies and what its segments name: node
/// `i`'s key occupies segments `at[i]..at[i + 1]`.
struct ChainSegments {
    /// The graph node each segment was encoded from, so a key prefix names the
    /// frame it ends at.
    owner: Vec<u32>,
    /// Segment offsets, one per node plus a terminator.
    at: Vec<u32>,
}

impl ChainKeys {
    fn key(&self, i: usize) -> &[u8] {
        let at = &self.segments.at;
        &self.pool[at[i] as usize * SEGMENT_BYTES..at[i + 1] as usize * SEGMENT_BYTES]
    }
}

/// Whether node `i` has self time for the inversion to hang on its caller chain.
fn contributes(graph: &FlameGraph, i: usize) -> bool {
    graph.self_us(i) != 0
}

/// Pack `graph`'s [`ChainKeys`] and count the frames [`contributes`] admits.
/// `parent[i]` is node `i`'s parent, and one forward pass extends each parent's
/// key into its children's.
fn chain_keys(graph: &FlameGraph, parent: &[u32]) -> (ChainKeys, usize) {
    let n = graph.len();
    let mut at = vec![0u32; n + 1];
    let mut contributors = 0usize;
    for i in 1..n {
        let p = parent[i] as usize;
        debug_assert!(p < i, "a node's parent must precede it");
        at[i + 1] = at[i]
            .checked_add(at[p + 1] - at[p] + 1)
            .expect("summed caller-chain depth overflows the chain segment index");
        contributors += contributes(graph, i) as usize;
    }

    let segments = at[n] as usize;
    let mut pool = vec![0u8; segments * SEGMENT_BYTES];
    let mut owner = vec![0u32; segments];
    for i in 1..n {
        let p = parent[i] as usize;
        let (from, len) = (at[p] as usize, (at[p + 1] - at[p]) as usize);
        let to = at[i] as usize;
        let detail = graph.is_detail(i);
        // A detail frame trails the chain it breaks out; any other frame leads
        // its own.
        let (chain, own) = if detail { (to, to + len) } else { (to + 1, to) };
        pool.copy_within(
            from * SEGMENT_BYTES..(from + len) * SEGMENT_BYTES,
            chain * SEGMENT_BYTES,
        );
        owner.copy_within(from..from + len, chain);
        write_segment(
            &mut pool[own * SEGMENT_BYTES..],
            graph.rank(i),
            graph.role(i),
            detail,
        );
        owner[own] = i as u32;
    }
    let keys = ChainKeys {
        pool,
        segments: ChainSegments { owner, at },
    };
    (keys, contributors)
}

/// One contributing frame's whole caller-chain key, sorted on to place the
/// frame's self time and span count in the inverted tree; `src` is that frame.
/// Holding the key slice keeps the sort comparator a single deref into the
/// [`ChainKeys`] pool.
struct ChainRef<'a> {
    key: &'a [u8],
    src: u32,
}

/// How many whole leading segments two caller-chain keys share.
fn common_segments(a: &[u8], b: &[u8]) -> usize {
    let shared = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    shared / SEGMENT_BYTES
}

/// Rebuild the inverted tree from the caller chains of `srcs` — the contributing
/// frames in ascending key order — returning the arena for [`seal_arena`].
/// Ascending order is the tree's pre-order, so each chain branches off the one
/// before it past the `lcp[k]` segments they share: the nodes deeper than that
/// are complete and fold into their parents, and the chain's remaining segments
/// open fresh nodes labelled from the graph node each was encoded from. The
/// frame then charges its self time to the node its chain ends at, which every
/// ancestor picks up as it is folded. Span counts fold the same way, each
/// frame's charged once to the node its own chain ends at, by the first chain to
/// end there or on a detail frame beneath. `nodes` sizes the arena.
fn reconstruct_chains(
    graph: &FlameGraph,
    chain: &ChainSegments,
    srcs: &[u32],
    lcp: &[u32],
    nodes: usize,
) -> Arena {
    let mut arena = Arena::with_capacity(nodes);
    let mut stack: Vec<usize> = vec![0];
    // Parallel to `stack`: whether the frame whose chain ends at that node has
    // charged its span count there. No frame's chain ends at the root.
    let mut counted: Vec<bool> = vec![true];
    let mut depth = 0usize;
    for (k, &src) in srcs.iter().enumerate() {
        let src = src as usize;
        let shared = lcp[k] as usize;
        while depth > shared {
            arena.fold_up(stack[depth], stack[depth - 1]);
            depth -= 1;
        }
        stack.truncate(shared + 1);
        counted.truncate(shared + 1);
        let base = chain.at[src] as usize;
        let end = chain.at[src + 1] as usize - base;
        // A node shared with the chain before stands for this chain's frame at
        // that step too, which equal key segments leave free to be of another
        // category.
        for (&node, &owner) in stack[1..=shared].iter().zip(&chain.owner[base..]) {
            let held = &mut arena.category[node];
            *held = shared_category(*held, graph.category[owner as usize]);
        }
        for d in shared + 1..=end {
            let owner = chain.owner[base + d - 1] as usize;
            let node = ArenaNode {
                name: graph.name(owner),
                rank: graph.rank(owner),
                role: graph.role(owner),
                category: graph.category[owner],
                detail: graph.is_detail(owner),
                total_us: 0,
                count: 0,
            };
            let idx = arena.push(node, stack[d - 1] as u32);
            stack.push(idx);
            counted.push(false);
        }
        depth = end;
        let leaf = stack[depth];
        arena.total_us[leaf] += graph.self_us(src);
        // The frame a detail frame breaks out leads the detail's chain, and its
        // node is the step before the detail's.
        let (frame, at) = if graph.is_detail(src) {
            arena.count[leaf] = graph.count(src);
            (chain.owner[base] as usize, depth - 1)
        } else {
            (src, depth)
        };
        if !counted[at] {
            counted[at] = true;
            arena.count[stack[at]] += graph.count(frame);
        }
    }
    while depth > 0 {
        arena.fold_up(stack[depth], stack[depth - 1]);
        depth -= 1;
    }
    // The synthetic root spans no frame, so it carries the summed time alone.
    arena.count[0] = 0;
    debug_assert_eq!(
        arena.len(),
        nodes,
        "the prefix scan's node count must be exact"
    );
    arena
}

/// The bottom-up form of `graph`: each frame's self time travels one path from
/// the root outward through that frame's callers and accumulates into every node
/// it passes, so a node's total is the summed self time of the frames whose
/// caller chain runs through it.
///
/// A frame's per-instance detail sits at the deep end of the path it hangs on,
/// so the graph reads from a frame outward through its callers to the instance
/// that spent the time.
///
/// The root's total is that whole summed self time, which meets the top-down
/// root's total only where no frame's children overrun it: enclosed spans may
/// overlap each other, and a frame's self time floors at zero rather than going
/// negative, so an overrun is charged to the children and subtracted from
/// nothing.
pub(crate) fn invert(graph: FlameGraph) -> FlameGraph {
    let profile = Arc::clone(&graph.profile);
    let n = graph.len();
    let mut parent = vec![0u32; n];
    for i in 0..n {
        for k in 0..graph.child_count(i) {
            parent[graph.child_id(i, k)] = i as u32;
        }
    }
    let (keys, contributors) = chain_keys(&graph, &parent);
    drop(parent);

    let mut elems: Vec<ChainRef> = Vec::with_capacity(contributors);
    for i in 1..n {
        if contributes(&graph, i) {
            elems.push(ChainRef {
                key: keys.key(i),
                src: i as u32,
            });
        }
    }
    workers().install(|| elems.par_sort_by(|a, b| a.key.cmp(b.key)));

    // Each key opens one node per segment past the prefix it shares with the key
    // before it, so the scan recording those prefixes also counts the nodes.
    let mut lcp: Vec<u32> = Vec::with_capacity(elems.len());
    let mut nodes = 1usize;
    let mut prev: &[u8] = &[];
    for e in &elems {
        let shared = common_segments(prev, e.key);
        lcp.push(shared as u32);
        nodes += e.key.len() / SEGMENT_BYTES - shared;
        prev = e.key;
    }

    let srcs: Vec<u32> = elems.iter().map(|e| e.src).collect();
    drop(elems);
    // Peak live bytes fall where the arena is largest, and nothing past the
    // prefix scan reads key bytes, so the pool goes before the arena grows.
    let ChainKeys { pool, segments } = keys;
    drop(pool);

    let arena = reconstruct_chains(&graph, &segments, &srcs, &lcp, nodes);
    // The reconstruction is the last reader of everything it takes.
    drop((segments, srcs, lcp, graph));
    let kids = csr_from_parents(&arena.parents);
    seal_arena(&profile, arena, kids)
}

/// Lay the visible window of the graph into pixels, mirroring the flame chart's
/// [`visible_slices`](crate::visible_slices). The graph occupies `content_w`
/// pixels at the current zoom; `scroll_x` is the content pixel at the left edge
/// of a `view_w`-wide viewport. Each node's children are placed left-to-right
/// with width proportional to `width_us` (the right-hand remainder being the
/// parent's self time), so dividing by `width_us` keeps an overflowing child
/// inside its parent. Depth maps to y via `row_height`.
///
/// Returned box positions are **viewport-relative** (x `0` is the left edge),
/// and only boxes overlapping the viewport are laid out — an off-screen subtree
/// is pruned — so the layout is local to what is on screen, not the whole graph.
///
/// A run of consecutive siblings each narrower than the minimum box width is
/// one [`FlameRemainder`] box, and no member's subtree is laid out beneath it.
///
/// Coordinates are carried in `f64` so the proportional division holds
/// precision at deep zoom, and each box is clamped to a viewport-sized margin so
/// no huge value reaches its `f32` fields. Boxes come back in paint order,
/// parents before children.
pub fn lay_out_flame(
    graph: &FlameGraph,
    content_w: f64,
    placement: FlamePlacement,
) -> Vec<FlameBox> {
    let mut layout = Layout {
        graph,
        placement,
        covered: vec![f64::NEG_INFINITY; graph.max_depth as usize + 1],
        reach: Vec::new(),
        out: Vec::new(),
    };
    let content = Span {
        x: 0.0,
        w: content_w,
    };
    let root = in_view(content, placement.scroll_x);
    if let Some(paint) = painted(root, layout.covered[0], placement.min_px as f64) {
        layout.place(0, 0, content, paint);
    }
    layout.out
}

/// Horizontal scroll offset, viewport width, row sizing, and minimum box width
/// — the parameters that place the flame graph's visible window into pixels.
#[derive(Debug, Clone, Copy)]
pub struct FlamePlacement<'a> {
    pub scroll_x: f64,
    pub view_w: f32,
    pub metrics: &'a FlameMetrics,
    pub min_px: f32,
}

/// A content-pixel span in viewport coordinates.
fn in_view(content: Span, scroll_x: f64) -> Span {
    Span {
        x: content.x - scroll_x,
        w: content.w,
    }
}

/// A layout in progress.
struct Layout<'a> {
    graph: &'a FlameGraph,
    placement: FlamePlacement<'a>,
    /// The painted right edge per depth row.
    covered: Vec<f64>,
    /// The deepest row the subtrees of the last scanned run's members reach,
    /// per column of its [`ThinRun::shown`], left to right.
    reach: Vec<u32>,
    out: Vec<FlameBox>,
}

/// The consecutive siblings, from one narrower than the minimum box on, up to
/// the next at least that wide.
struct ThinRun {
    /// The content x past the whole run.
    end: f64,
    /// The index of the first sibling beyond the run.
    next: usize,
    /// What stands for the run where it holds more than the one sibling, whose
    /// own frame stands for it otherwise.
    remainder: Option<FlameRemainder>,
    /// The run's columns — each as wide as the minimum box, counted from the
    /// run's left edge — that a member on the viewport starts in.
    shown: std::ops::Range<usize>,
}

impl Layout<'_> {
    fn min(&self) -> f64 {
        self.placement.min_px as f64
    }

    /// Emit a box of `content` at `paint`, the viewport span it is drawn at,
    /// from row `depth` down `rows` rows, recording it in `covered`. `false`
    /// where `paint` lies off the viewport, which emits nothing.
    fn push_box(&mut self, content: FlameBoxContent, depth: u32, rows: u32, paint: Span) -> bool {
        let view_w = self.placement.view_w as f64;
        if paint.end() <= 0.0 || paint.x >= view_w {
            return false;
        }
        for covered in &mut self.covered[depth as usize..(depth + rows) as usize] {
            *covered = covered.max(paint.end());
        }
        // Clamp to a viewport-sized margin so a wide box spanning far off-screen
        // never stores a huge coordinate; a visible marker's left edge is already
        // small.
        let px = paint.x.max(-view_w);
        let pr = paint.end().min(2.0 * view_w);
        let row_height = self.placement.metrics.row_height;
        self.out.push(FlameBox {
            x: px as f32,
            y: depth as f32 * row_height,
            w: (pr - px) as f32,
            h: rows as f32 * row_height,
            depth,
            content,
        });
        true
    }

    /// Emit `idx`'s box at `paint` and lay its children out across `content`,
    /// its own span in content pixels.
    fn place(&mut self, idx: usize, depth: u32, content: Span, paint: Span) {
        let frame = FlameBoxContent::Frame(frame_of(self.graph, idx));
        if self.push_box(frame, depth, 1, paint) {
            self.place_children(idx, depth, content);
        }
    }

    /// Lay the children of `idx`, a node at `depth`, out across `content`, its
    /// own span in content pixels.
    fn place_children(&mut self, idx: usize, depth: u32, content: Span) {
        let graph = self.graph;
        let width = graph.width_us(idx);
        if width == 0 {
            return;
        }
        let (scroll_x, min) = (self.placement.scroll_x, self.min());
        let child_w = |c: usize| content.w * graph.width_us(c) as f64 / width as f64;
        let row = depth + 1;
        let children = graph.children_of(idx);
        let mut cursor = content.x;
        let mut i = 0;
        while let Some(&child) = children.get(i) {
            let child = child as usize;
            let cc = Span {
                x: cursor,
                w: child_w(child),
            };
            if cc.w >= min {
                self.place(child, row, cc, in_view(cc, scroll_x));
                cursor = cc.end();
                i += 1;
                continue;
            }
            let run = self.thin_run(idx, i, cc, &child_w);
            let covered = self.covered[row as usize];
            match run.remainder {
                None => {
                    if let Some(paint) = painted(in_view(cc, scroll_x), covered, min) {
                        self.place(child, row, cc, paint);
                    }
                }
                Some(remainder) => {
                    let span = Span {
                        x: cc.x,
                        w: run.end - cc.x,
                    };
                    if let Some(paint) = painted(in_view(span, scroll_x), covered, min)
                        && self.push_box(FlameBoxContent::Remainder(remainder), row, 1, paint)
                    {
                        let left = in_view(cc, scroll_x).x;
                        self.place_reach(remainder, row, run.shown, left, paint);
                    }
                }
            }
            cursor = run.end;
            i = run.next;
        }
    }

    /// The [`ThinRun`] opening at `parent`'s `start`-th child, which spans
    /// `first`, leaving in `reach` how deep its members go.
    fn thin_run(
        &mut self,
        parent: usize,
        start: usize,
        first: Span,
        child_w: &impl Fn(usize) -> f64,
    ) -> ThinRun {
        let graph = self.graph;
        let min = self.min();
        let (scroll_x, view_w) = (self.placement.scroll_x, self.placement.view_w as f64);
        // The first column to reach into the viewport, the content x it starts
        // at, and the content x the viewport ends at.
        let first_shown = ((scroll_x - first.x) / min).floor().max(0.0);
        let shown_from = first.x + first_shown * min;
        let shown_until = scroll_x + view_w;
        // The viewport cuts a column at either edge of those it holds whole.
        let most_shown = (view_w / min) as usize + 2;
        if self.reach.len() < most_shown {
            self.reach.resize(most_shown, 0);
        }
        let children = graph.children_of(parent);
        let mut scan = RunScan {
            graph,
            children,
            child_w,
            min,
            next: start,
            end: first.x,
            w: Some(first.w),
            role: Some(graph.role(children[start] as usize)),
        };
        while scan.take_before(shown_from).is_some() {}
        // Each column holds a member's start: a member is narrower than a
        // column, so the one after the last to start in a column starts in the
        // next.
        let mut shown = 0;
        while scan.starts_before(shown_until) {
            let column_end = shown_from + (shown + 1) as f64 * min;
            let mut deepest = 0;
            while let Some(member) = scan.take_before(column_end) {
                deepest = deepest.max(graph.subtree_max_depth(member));
            }
            self.reach[shown] = deepest;
            shown += 1;
        }
        while scan.take_before(f64::INFINITY).is_some() {}
        let frames = scan.next - start;
        let siblings = match frames == children.len() {
            true => RemainderSiblings::None,
            false => RemainderSiblings::Wider,
        };
        let remainder = (frames > 1).then_some(FlameRemainder {
            parent: FlameNodeId(parent as u32),
            first: start as u32,
            frames: frames as u32,
            role: scan.role,
            siblings,
        });
        let first_shown = first_shown as usize;
        ThinRun {
            end: scan.end,
            next: scan.next,
            remainder,
            shown: first_shown..first_shown + shown,
        }
    }

    /// Emit, beneath `remainder`'s box on `row` at `paint`, how deep its
    /// members reach: one box per stretch of its `shown` columns that `reach`
    /// has reaching the same row. `left` is the viewport x the run's own span
    /// starts at, which its columns are counted from.
    fn place_reach(
        &mut self,
        remainder: FlameRemainder,
        row: u32,
        shown: std::ops::Range<usize>,
        left: f64,
        paint: Span,
    ) {
        let min = self.min();
        let content = FlameBoxContent::RemainderDepth(remainder);
        // A run narrower than one column is painted as wide as a column, from
        // where `painted` put it.
        let edge = |column: usize| match column {
            0 => paint.x,
            _ => (left + column as f64 * min).min(paint.end()),
        };
        let columns = shown.len();
        let mut i = 0;
        while i < columns {
            let deepest = self.reach[i];
            let mut j = i + 1;
            while j < columns && self.reach[j] == deepest {
                j += 1;
            }
            let x = edge(shown.start + i);
            let end = match j < columns {
                true => edge(shown.start + j),
                false => paint.end(),
            };
            if deepest > row && end > x {
                self.push_box(content, row + 1, deepest - row, Span { x, w: end - x });
            }
            i = j;
        }
    }
}

/// A walk along the siblings of a [`ThinRun`].
struct RunScan<'a, W> {
    graph: &'a FlameGraph,
    /// Every child of the run's parent.
    children: &'a [u32],
    child_w: &'a W,
    min: f64,
    /// The index of the sibling to take next.
    next: usize,
    /// The content x that sibling starts at.
    end: f64,
    /// That sibling's width where it is of the run, and `None` past the run's
    /// last.
    w: Option<f64>,
    /// The role the siblings taken share.
    role: Option<SpanRole>,
}

impl<W: Fn(usize) -> f64> RunScan<'_, W> {
    /// Whether the run holds a sibling yet to take that starts before `x`.
    fn starts_before(&self, x: f64) -> bool {
        self.w.is_some() && self.end < x
    }

    /// Take the next sibling into the run where it starts before `x`,
    /// returning its node.
    fn take_before(&mut self, x: f64) -> Option<usize> {
        let w = self.w.filter(|_| self.end < x)?;
        let member = self.children[self.next] as usize;
        if self
            .role
            .is_some_and(|shared| shared != self.graph.role(member))
        {
            self.role = None;
        }
        self.end += w;
        self.next += 1;
        self.w = self
            .children
            .get(self.next)
            .map(|&sibling| (self.child_w)(sibling as usize))
            .filter(|&w| w < self.min);
        Some(member)
    }
}

fn frame_of(graph: &FlameGraph, idx: usize) -> FlameFrame {
    FlameFrame {
        node: FlameNodeId(idx as u32),
        name: graph.name(idx),
        total_us: graph.total_us(idx),
        role: graph.role(idx),
        count: graph.count(idx),
    }
}

/// Total painted height: one row per depth level the graph reaches.
pub fn flame_height(graph: &FlameGraph, metrics: &FlameMetrics) -> f32 {
    (graph.max_depth + 1) as f32 * metrics.row_height
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selection::span_between;
    use crate::test_profile::{
        TestSlice, action, action_named, profile, random_profile, slice, track,
    };
    use crate::tests::assert_gapless;
    use bviz_trace_profile::LaneClass;
    use fastrand::Rng;

    fn child(graph: &FlameGraph, node: &FlameNode, name: &str) -> FlameNode {
        graph
            .children(node)
            .into_iter()
            .find(|c| graph.str(c.name) == name)
            .unwrap_or_else(|| panic!("no child {name}"))
    }

    /// The all-lanes flame graph under `labels`, restricted to `window`: every
    /// real lane, the critical-path lane's redirected copies of worker time
    /// excluded.
    fn flame_within(p: &Arc<Profile>, labels: FlameLabels, window: &TimeSelection) -> FlameGraph {
        let t = p.threads();
        TrackFlamer::new(p).build(labels, window, |i| {
            t[i].lane_class != LaneClass::CriticalPath
        })
    }

    fn flame_as(p: &Arc<Profile>, labels: FlameLabels) -> FlameGraph {
        flame_within(p, labels, &TimeSelection::default())
    }

    fn flame(p: &Arc<Profile>) -> FlameGraph {
        flame_as(p, FlameLabels::Kind)
    }

    /// A selection of the given regions, each inserted in turn.
    fn window(regions: &[(i64, i64)]) -> TimeSelection {
        let mut sel = TimeSelection::default();
        for &(a, b) in regions {
            sel.insert(span_between(a, b).unwrap_or_else(|| panic!("({a}, {b}) spans no time")));
        }
        sel
    }

    /// A graph's nodes with names resolved, so two graphs over one profile
    /// compare structurally.
    fn resolved(g: &FlameGraph) -> Vec<(String, i64, i64, i64, u32, u32)> {
        g.nodes()
            .iter()
            .map(|n| {
                (
                    g.str(n.name).to_owned(),
                    n.total_us,
                    n.self_us,
                    n.width_us,
                    n.count,
                    n.depth,
                )
            })
            .collect()
    }

    /// The bottom-up form of [`flame_as`]'s graph.
    fn inverted_as(p: &Arc<Profile>, labels: FlameLabels) -> FlameGraph {
        invert(flame_as(p, labels))
    }

    /// Every child of `node`, as (text, total) pairs sorted by text.
    fn child_totals(graph: &FlameGraph, node: &FlameNode) -> Vec<(String, i64)> {
        let mut kids: Vec<(String, i64)> = graph
            .children(node)
            .iter()
            .map(|c| (graph.str(c.name).to_owned(), c.total_us))
            .collect();
        kids.sort();
        kids
    }

    const METRICS: FlameMetrics = FlameMetrics { row_height: 10.0 };

    // The graph is built off the UI thread and handed back, so it must cross
    // threads with no borrow of the source profile.
    fn _assert_thread_safe<T: Send + Sync + 'static>() {}
    #[test]
    fn flame_types_are_send_sync_and_lifetime_free() {
        _assert_thread_safe::<FlameGraph>();
        _assert_thread_safe::<FlameNode>();
        _assert_thread_safe::<FlameBox>();
        _assert_thread_safe::<TrackFlamer>();
        _assert_thread_safe::<crate::StatRow>();
    }

    #[test]
    fn parallel_text_sort_matches_serial() {
        // Enough pairs to cross the parallel threshold, with heavy shared
        // prefixes, case splits, and non-ASCII segments, so the parallel sort is
        // exercised on clustered keys.
        let texts: Vec<String> = (0..20_000)
            .map(|i| format!("prefix/{}/x{:05}", ["a", "A", "é", ""][i % 4], i))
            .collect();
        let pairs: Vec<(&str, usize)> = texts
            .iter()
            .enumerate()
            .map(|(i, t)| (t.as_str(), i))
            .collect();
        let serial = {
            let mut p = pairs.clone();
            p.sort_unstable_by_key(|&(t, _)| t);
            p
        };
        let mut parallel = pairs;
        sort_pairs_by_text(&mut parallel);
        assert_eq!(parallel, serial);
    }

    #[test]
    fn conflicting_mnemonics_resolve_to_the_smallest_text() {
        // Two lanes carry the same action name under different mnemonics; a
        // third slice shares the name but has no mnemonic of its own, so its
        // label comes from the index — which must pick the byte-wise smallest
        // mnemonic regardless of lane visit order.
        let p = profile(vec![
            track(
                1,
                LaneClass::Execution,
                vec![action_named("Compiling foo", "Rustc", 0, 100)],
            ),
            track(
                2,
                LaneClass::Execution,
                vec![action_named("Compiling foo", "Javac", 200, 50)],
            ),
            track(
                3,
                LaneClass::Execution,
                vec![slice("Compiling foo", "action processing", 400, 30)],
            ),
        ]);
        let g = flame(&p);
        // A slice's own mnemonic always wins; only the mnemonic-less slice
        // resolves through the index and joins the smallest-text frame.
        assert_eq!(child(&g, &g.root(), "Rustc").total_us, 100);
        let javac = child(&g, &g.root(), "Javac");
        assert_eq!(javac.total_us, 80);
        assert_eq!(javac.count, 2);
        assert!(
            g.children(&g.root())
                .iter()
                .all(|c| g.str(c.name) != "Compiling foo")
        );
    }

    #[test]
    fn nested_spans_form_a_call_stack_without_double_counting() {
        // A lane with Rustc(0..100) containing Action.execute(0..90) containing
        // subprocess.run(0..80): the tree nests by time, each span's duration
        // counted once at its own node. Root total is the action's wall time
        // (100), NOT the inflated 270 that summing siblings would give. Labelled
        // by name, so the frames are exactly the three spans.
        let p = profile(vec![track(
            8,
            LaneClass::Execution,
            vec![
                action("Rustc", 0, 100),
                slice("Action.execute", "general information", 0, 90),
                slice("subprocess.run", "subprocess run", 0, 80),
            ],
        )]);
        let g = flame_as(&p, FlameLabels::Name);
        assert_eq!(g.root().total_us, 100);
        let rustc = child(&g, &g.root(), "Rustc");
        assert_eq!(rustc.total_us, 100);
        assert_eq!(rustc.self_us, 10);
        let exec = child(&g, &rustc, "Action.execute");
        assert_eq!(exec.total_us, 90);
        let sub = child(&g, &exec, "subprocess.run");
        assert_eq!(sub.total_us, 80);
        assert_eq!(sub.self_us, 80);
    }

    #[test]
    fn identical_spans_merge_across_lanes() {
        // Two Rustc actions on two execution lanes, each containing its own
        // Action.execute, merge into one Rustc frame whose Action.execute child
        // sums both.
        let lane = |start| {
            vec![
                action("Rustc", start, 100),
                slice("Action.execute", "general information", start, 80),
            ]
        };
        let p = profile(vec![
            track(8, LaneClass::Execution, lane(0)),
            track(9, LaneClass::Execution, lane(0)),
        ]);
        let g = flame(&p);
        assert_eq!(g.children(&g.root()).len(), 1);
        let rustc = child(&g, &g.root(), "Rustc");
        assert_eq!(rustc.total_us, 200);
        assert_eq!(rustc.count, 2);
        let exec = child(&g, &rustc, "Action.execute");
        assert_eq!(exec.total_us, 160);
        assert_eq!(exec.count, 2);
    }

    #[test]
    fn name_labels_keep_one_mnemonics_targets_apart() {
        // Two actions of the same mnemonic on different targets: under `Name`
        // each keeps its own frame reading its action name, under `Kind`
        // the two merge into one.
        let p = profile(vec![track(
            1,
            LaneClass::Execution,
            vec![
                action_named("Compiling //a:a", "Rustc", 0, 100),
                action_named("Compiling //b:b", "Rustc", 100, 40),
            ],
        )]);
        let named = flame_as(&p, FlameLabels::Name);
        assert_eq!(named.children(&named.root()).len(), 2);
        assert_eq!(
            child(&named, &named.root(), "Compiling //a:a").total_us,
            100
        );
        assert_eq!(child(&named, &named.root(), "Compiling //b:b").total_us, 40);
        let merged = flame_as(&p, FlameLabels::Kind);
        assert_eq!(merged.children(&merged.root()).len(), 1);
        let rustc = child(&merged, &merged.root(), "Rustc");
        assert_eq!(rustc.total_us, 140);
        assert_eq!(rustc.count, 2);
    }

    #[test]
    fn detail_frames_break_each_action_frame_self_time_out_by_name() {
        // Rustc(0..100) contains Action.execute(0..90) contains
        // subprocess.run(0..80). Every frame belongs to the one action, so each
        // gains a detail child named after it carrying that frame's self time:
        // 10 for Rustc, 10 for Action.execute, 80 for the leaf.
        let p = profile(vec![track(
            1,
            LaneClass::Execution,
            vec![
                action_named("Compiling //x:y", "Rustc", 0, 100),
                slice("Action.execute", "general information", 0, 90),
                slice("subprocess.run", "subprocess run", 0, 80),
            ],
        )]);
        let g = flame(&p);
        let rustc = child(&g, &g.root(), "Rustc");
        let exec = child(&g, &rustc, "Action.execute");
        let sub = child(&g, &exec, "subprocess.run");
        for (frame, self_us) in [(rustc, 10), (exec, 10), (sub, 80)] {
            let detail = child(&g, &frame, "Compiling //x:y");
            assert_eq!(detail.total_us, self_us);
            assert!(g.children(&detail).is_empty());
            // All of the frame's own time sits in that child.
            assert_eq!(frame.self_us, 0);
        }
        // Broken out, not double-counted: the tree still totals the wall time.
        assert_eq!(g.root().total_us, 100);
        assert_eq!(g.nodes().iter().map(|n| n.self_us).sum::<i64>(), 100);
    }

    #[test]
    fn a_merged_frame_details_each_of_its_targets() {
        // Two targets of one mnemonic merge into a single Rustc frame, which
        // then breaks its time back out per target.
        let p = profile(vec![track(
            1,
            LaneClass::Execution,
            vec![
                action_named("Compiling //a:a", "Rustc", 0, 100),
                action_named("Compiling //b:b", "Rustc", 100, 40),
            ],
        )]);
        let g = flame(&p);
        let rustc = child(&g, &g.root(), "Rustc");
        assert_eq!(rustc.total_us, 140);
        assert_eq!(child(&g, &rustc, "Compiling //a:a").total_us, 100);
        assert_eq!(child(&g, &rustc, "Compiling //b:b").total_us, 40);
        assert_eq!(g.children(&rustc).len(), 2);
        // Labelled by name there is nothing to break out: the two targets are
        // frames of their own.
        let named = flame_as(&p, FlameLabels::Name);
        assert!(
            named
                .children(&named.root())
                .iter()
                .all(|c| named.children(c).is_empty())
        );
        assert_eq!(named.nodes().len(), 3);
    }

    #[test]
    fn mnemonicless_slice_keeps_its_name() {
        // A top-level slice with no mnemonic keeps its own informative name, not a
        // coarse work-class bucket — so distinct operations stay distinct.
        let p = profile(vec![track(
            1,
            LaneClass::Execution,
            vec![
                action("CppCompile", 0, 100),
                slice("checkOutputs", "general information", 100, 30),
            ],
        )]);
        let g = flame(&p);
        assert_eq!(child(&g, &g.root(), "CppCompile").total_us, 100);
        assert_eq!(child(&g, &g.root(), "checkOutputs").total_us, 30);
    }

    /// A repository fetch: a span named after the repository alone.
    fn fetch(repository: &str, start_us: i64, dur_us: i64) -> TestSlice {
        slice(repository, "Fetching repository", start_us, dur_us)
    }

    #[test]
    fn a_span_named_per_instance_is_labelled_by_its_category_and_detailed_by_its_name() {
        let p = profile(vec![track(
            1,
            LaneClass::Analysis,
            vec![
                fetch("@@a", 0, 100),
                fetch("@@b", 100, 40),
                slice("buildTargets", "general information", 200, 30),
            ],
        )]);
        let g = flame(&p);
        assert_eq!(
            child_totals(&g, &g.root()),
            [
                ("Fetching repository".to_owned(), 140),
                ("buildTargets".to_owned(), 30)
            ]
        );
        let fetching = child(&g, &g.root(), "Fetching repository");
        assert_eq!(fetching.count, 2);
        assert_eq!(
            child_totals(&g, &fetching),
            [("@@a".to_owned(), 100), ("@@b".to_owned(), 40)]
        );
        assert_eq!(
            (1..g.len()).filter(|&i| g.is_detail(i)).count(),
            2,
            "each repository is a detail frame, and the span named for what it does has none"
        );
        // Labelled by name, each span is a frame of its own with nothing broken
        // out beneath it.
        let named = flame_as(&p, FlameLabels::Name);
        assert_eq!(
            resolved(&named),
            [
                ("All".to_owned(), 170, 0, 170, 0, 0),
                ("@@a".to_owned(), 100, 100, 100, 1, 1),
                ("@@b".to_owned(), 40, 40, 40, 1, 1),
                ("buildTargets".to_owned(), 30, 30, 30, 1, 1),
            ]
        );
        assert!((0..named.len()).all(|i| !named.is_detail(i)));
    }

    #[test]
    fn a_span_named_kind_then_instance_is_labelled_by_the_kind_and_detailed_by_its_name() {
        let info = |name, start, dur| slice(name, "general information", start, dur);
        let p = profile(vec![track(
            1,
            LaneClass::Analysis,
            vec![
                info("fetching: repository @@a", 0, 100),
                info("fetching: repository @@b: c", 100, 40),
                info("extracting: repository @@a", 200, 30),
                // No kind leads these: one lacks the separator, the other
                // carries it after a kind that is not this category's.
                info("fetching", 300, 20),
                info("local: sh", 400, 10),
            ],
        )]);
        let g = flame(&p);
        // The span named by the kind alone is no instance of it: it keeps the
        // role of a span named for what it does, and so a frame of its own.
        assert_eq!(
            child_totals(&g, &g.root()),
            [
                ("extracting".to_owned(), 30),
                ("fetching".to_owned(), 20),
                ("fetching".to_owned(), 140),
                ("local: sh".to_owned(), 10),
            ]
        );
        let fetching = g
            .children(&g.root())
            .into_iter()
            .find(|c| g.str(c.name) == "fetching" && c.role.workload == Workload::RemoteCache)
            .expect("the kind's frame");
        assert_eq!((fetching.total_us, fetching.count), (140, 2));
        assert_eq!(
            child_totals(&g, &fetching),
            [
                ("fetching: repository @@a".to_owned(), 100),
                ("fetching: repository @@b: c".to_owned(), 40),
            ]
        );
        let extracting = child(&g, &g.root(), "extracting");
        assert_eq!(
            child_totals(&g, &extracting),
            [("extracting: repository @@a".to_owned(), 30)]
        );
        assert!(g.children(&child(&g, &g.root(), "local: sh")).is_empty());
    }

    #[test]
    fn a_command_of_a_repository_rule_is_a_kind_apart_from_the_rule_that_runs_it() {
        let rule = |name, start, dur| slice(name, "Starlark repository function call", start, dur);
        let p = profile(vec![track(
            1,
            LaneClass::RepoFetch,
            vec![
                rule("rules_cc+", 0, 100),
                rule("local: /bin/sh -c a: b", 10, 60),
                rule("rules_go+", 100, 50),
                rule("local: /bin/true", 110, 20),
            ],
        )]);
        let g = flame(&p);
        let rules = child(&g, &g.root(), "Starlark repository function call");
        assert_eq!(g.children(&g.root()).len(), 1);
        assert_eq!((rules.total_us, rules.count), (150, 2));
        assert_eq!(
            child_totals(&g, &rules),
            [
                ("local".to_owned(), 80),
                ("rules_cc+".to_owned(), 40),
                ("rules_go+".to_owned(), 30),
            ]
        );
        let local = child(&g, &rules, "local");
        assert_eq!(local.role.workload, Workload::Subprocess);
        assert_eq!(rules.role.workload, Workload::Starlark);
        assert_eq!(
            child_totals(&g, &local),
            [
                ("local: /bin/sh -c a: b".to_owned(), 60),
                ("local: /bin/true".to_owned(), 20),
            ]
        );
        // Bottom-up the commands are one root and the rules another.
        let inverted = inverted_as(&p, FlameLabels::Kind);
        assert_eq!(
            child_totals(&inverted, &inverted.root()),
            [
                ("Starlark repository function call".to_owned(), 70),
                ("local".to_owned(), 80),
            ]
        );
        // Labelled by name every span is a frame of its own.
        let named = flame_as(&p, FlameLabels::Name);
        assert_eq!(
            child_totals(&named, &named.root()),
            [("rules_cc+".to_owned(), 100), ("rules_go+".to_owned(), 50)]
        );
        assert!((0..named.len()).all(|i| !named.is_detail(i)));
    }

    #[test]
    fn a_kind_no_span_of_the_lanes_built_carries_still_builds() {
        let p = profile(vec![
            track(
                1,
                LaneClass::Analysis,
                vec![slice("buildTargets", "general information", 0, 10)],
            ),
            track(
                2,
                LaneClass::RepoFetch,
                vec![slice(
                    "fetching: repository @@a",
                    "general information",
                    0,
                    10,
                )],
            ),
        ]);
        let t = p.threads();
        let analysis = |i: usize| t[i].lane_class == LaneClass::Analysis;
        let scoped = Flamer::scoped(&p, 1, analysis);
        assert!(p.kinds().all(|kind| scoped.ranks.rank.contains_key(&kind)));
        let g = TrackFlamer::new(&p).build(FlameLabels::Kind, &TimeSelection::default(), |i| {
            !analysis(i)
        });
        assert_eq!(child_totals(&g, &g.root()), [("fetching".to_owned(), 10)]);
    }

    #[test]
    fn a_mnemonic_labels_a_span_ahead_of_its_category() {
        let p = profile(vec![track(
            1,
            LaneClass::Analysis,
            vec![TestSlice {
                mnemonic: Some("Fetch".to_owned()),
                ..fetch("@@a", 0, 100)
            }],
        )]);
        let g = flame(&p);
        assert_eq!(child_totals(&g, &g.root()), [("Fetch".to_owned(), 100)]);
        let labelled = child(&g, &g.root(), "Fetch");
        assert_eq!(child_totals(&g, &labelled), [("@@a".to_owned(), 100)]);
    }

    #[test]
    fn a_span_inherits_the_nearest_enclosing_instance() {
        // The fetch holds a span named for what it does, which holds a span that
        // is an instance of its own: the first is detailed by the repository
        // around it, the second by itself.
        let p = profile(vec![track(
            1,
            LaneClass::Analysis,
            vec![
                fetch("@@a", 0, 100),
                slice("download", "generic", 10, 60),
                slice("/tmp/a.tar", "VFS write", 20, 10),
            ],
        )]);
        let g = flame(&p);
        let fetching = child(&g, &g.root(), "Fetching repository");
        let download = child(&g, &fetching, "download");
        let write = child(&g, &download, "VFS write");
        assert_eq!(child(&g, &fetching, "@@a").total_us, 40);
        assert_eq!(child(&g, &download, "@@a").total_us, 50);
        assert_eq!(child_totals(&g, &write), [("/tmp/a.tar".to_owned(), 10)]);
        assert_eq!(g.root().total_us, 100);
        assert_eq!(g.nodes().iter().map(|n| n.self_us).sum::<i64>(), 100);
    }

    #[test]
    fn an_instance_nested_in_an_action_is_detailed_by_its_own_name() {
        // The read is an instance of its own, so the action's target details
        // the action's time and the step between them, and stops at the read.
        let p = profile(vec![track(
            1,
            LaneClass::Execution,
            vec![
                action_named("Compiling //x:y", "Rustc", 0, 100),
                slice("stage inputs", "generic", 10, 60),
                slice("/work/x.rs", "VFS read", 20, 10),
            ],
        )]);
        let g = flame(&p);
        let rustc = child(&g, &g.root(), "Rustc");
        let stage = child(&g, &rustc, "stage inputs");
        let read = child(&g, &stage, "VFS read");
        assert_eq!(child(&g, &rustc, "Compiling //x:y").total_us, 40);
        assert_eq!(child(&g, &stage, "Compiling //x:y").total_us, 50);
        assert_eq!(child_totals(&g, &read), [("/work/x.rs".to_owned(), 10)]);
        assert_eq!(g.nodes().iter().map(|n| n.self_us).sum::<i64>(), 100);
    }

    #[test]
    fn an_action_nested_in_an_instance_is_detailed_by_its_own_name() {
        // The action is an instance of its own, so the repository details the
        // fetch's time and stops at the action, whose target details it and the
        // step nested in it.
        let p = profile(vec![track(
            1,
            LaneClass::Analysis,
            vec![
                fetch("@@a", 0, 100),
                action_named("Unpacking //x:y", "Untar", 10, 60),
                slice("write outputs", "generic", 20, 10),
            ],
        )]);
        let g = flame(&p);
        let fetching = child(&g, &g.root(), "Fetching repository");
        let untar = child(&g, &fetching, "Untar");
        let write = child(&g, &untar, "write outputs");
        assert_eq!(
            child_totals(&g, &fetching),
            [("@@a".to_owned(), 40), ("Untar".to_owned(), 60)]
        );
        assert_eq!(child(&g, &untar, "Unpacking //x:y").total_us, 50);
        assert_eq!(
            child_totals(&g, &write),
            [("Unpacking //x:y".to_owned(), 10)]
        );
        assert_eq!(g.nodes().iter().map(|n| n.self_us).sum::<i64>(), 100);
    }

    #[test]
    fn a_span_of_a_kind_that_has_no_name_takes_the_instance_around_it() {
        // The main repository's root package is written as the empty string:
        // its own time stays on the kind's frame where nothing around it is an
        // instance, and goes to the instance that is.
        let creation = |name, start, dur| slice(name, "package creation", start, dur);
        let p = profile(vec![track(
            1,
            LaneClass::Analysis,
            vec![
                creation("", 0, 100),
                slice("load", "generic", 10, 60),
                fetch("@@a", 200, 100),
                creation("", 210, 30),
            ],
        )]);
        let g = flame(&p);
        let top = child(&g, &g.root(), "package creation");
        assert_eq!((top.total_us, top.self_us), (100, 40));
        let load = child(&g, &top, "load");
        assert_eq!(child_totals(&g, &top), [("load".to_owned(), 60)]);
        assert!(g.children(&load).is_empty());
        let fetching = child(&g, &g.root(), "Fetching repository");
        let nested = child(&g, &fetching, "package creation");
        assert_eq!(child_totals(&g, &nested), [("@@a".to_owned(), 30)]);
        assert!(g.nodes().iter().all(|n| !g.str(n.name).is_empty()));
    }

    #[test]
    fn a_span_named_by_its_categorys_fixed_name_takes_the_instance_around_it() {
        // A rule initializer's thread says nothing of the package it runs for:
        // its time and the calls it makes go to the package loading around it,
        // and its own time stays on the kind's frame where nothing is.
        let context = |start, dur| slice("an initializer", "Starlark thread context", start, dur);
        let call = |start| slice("rust_crate", "Starlark user function call", start, 30);
        let creation = |package, start| slice(package, "package creation", start, 100);
        let p = profile(vec![track(
            1,
            LaneClass::Analysis,
            vec![
                creation("a", 0),
                context(10, 50),
                call(20),
                creation("b", 100),
                context(110, 50),
                call(120),
                context(300, 10),
            ],
        )]);
        let g = flame(&p);
        let loading = child(&g, &g.root(), "package creation");
        let nested = child(&g, &loading, "Starlark thread context");
        assert_eq!(
            child_totals(&g, &nested),
            [
                ("a".to_owned(), 20),
                ("b".to_owned(), 20),
                ("rust_crate".to_owned(), 60)
            ]
        );
        assert_eq!(
            child_totals(&g, &child(&g, &nested, "rust_crate")),
            [("a".to_owned(), 30), ("b".to_owned(), 30)]
        );
        let alone = child(&g, &g.root(), "Starlark thread context");
        assert_eq!((alone.total_us, alone.self_us), (10, 10));
        assert!(g.children(&alone).is_empty());
        assert!(g.nodes().iter().all(|n| g.str(n.name) != "an initializer"));
    }

    #[test]
    fn a_kind_is_one_bottom_up_root_whose_paths_end_on_its_instances() {
        let lane = |repository, start| {
            vec![
                fetch(repository, start, 100),
                slice("download", "generic", start, 70),
            ]
        };
        let p = profile(vec![
            track(1, LaneClass::Analysis, lane("@@a", 0)),
            track(2, LaneClass::Analysis, lane("@@b", 0)),
        ]);
        let g = inverted_as(&p, FlameLabels::Kind);
        assert_eq!(
            child_totals(&g, &g.root()),
            [
                ("Fetching repository".to_owned(), 60),
                ("download".to_owned(), 140)
            ]
        );
        let fetching = child(&g, &g.root(), "Fetching repository");
        assert_eq!(
            child_totals(&g, &fetching),
            [("@@a".to_owned(), 30), ("@@b".to_owned(), 30)]
        );
        let download = child(&g, &g.root(), "download");
        let caller = child(&g, &download, "Fetching repository");
        assert_eq!(
            child_totals(&g, &caller),
            [("@@a".to_owned(), 70), ("@@b".to_owned(), 70)]
        );
        // Labelled by name each repository roots a path of its own.
        let named = inverted_as(&p, FlameLabels::Name);
        assert_eq!(
            child_totals(&named, &named.root()),
            [
                ("@@a".to_owned(), 30),
                ("@@b".to_owned(), 30),
                ("download".to_owned(), 140)
            ]
        );
    }

    #[test]
    fn a_window_clips_a_kind_frame_and_each_instance_beneath_it() {
        let p = profile(vec![track(
            1,
            LaneClass::Analysis,
            vec![
                fetch("@@a", 0, 100),
                fetch("@@b", 100, 100),
                fetch("@@c", 200, 100),
            ],
        )]);
        let g = flame_within(&p, FlameLabels::Kind, &window(&[(50, 130)]));
        let fetching = child(&g, &g.root(), "Fetching repository");
        assert_eq!((fetching.total_us, fetching.count), (80, 2));
        assert_eq!(
            child_totals(&g, &fetching),
            [("@@a".to_owned(), 50), ("@@b".to_owned(), 30)]
        );
    }

    /// The category of the one node named `name` at `depth`, as text.
    fn category_at(g: &FlameGraph, name: &str, depth: u32) -> Option<String> {
        let nodes = g.nodes();
        let mut found = nodes
            .iter()
            .filter(|n| n.depth == depth && g.str(n.name) == name);
        let node = found
            .next()
            .unwrap_or_else(|| panic!("no {name} at depth {depth}"));
        assert!(found.next().is_none(), "several {name} at depth {depth}");
        node.category.get().map(|c| g.str(c).to_owned())
    }

    #[test]
    fn a_frame_carries_the_category_its_spans_were_recorded_under() {
        let p = profile(vec![track(
            1,
            LaneClass::Analysis,
            vec![
                fetch("@@a", 0, 100),
                slice("download", "generic", 10, 60),
                slice("/tmp/a.tar", "VFS write", 20, 10),
                action_named("Compiling //x:y", "Rustc", 200, 50),
            ],
        )]);
        let general = Some("generic".to_owned());
        let fetching = Some("Fetching repository".to_owned());
        let writing = Some("VFS write".to_owned());
        let g = flame(&p);
        assert_eq!(g.root().category.get(), None);
        assert_eq!(category_at(&g, "Fetching repository", 1), fetching);
        assert_eq!(category_at(&g, "download", 2), general);
        assert_eq!(category_at(&g, "VFS write", 3), writing);
        assert_eq!(
            category_at(&g, "Rustc", 1),
            Some("action processing".to_owned())
        );
        // A detail frame carries the category of the instance it names, which
        // is not that of the frame it hangs under where the instance was
        // inherited.
        assert_eq!(category_at(&g, "@@a", 2), fetching);
        assert_eq!(category_at(&g, "@@a", 3), fetching);
        assert_eq!(category_at(&g, "/tmp/a.tar", 4), writing);
        assert_eq!(
            category_at(&g, "Compiling //x:y", 2),
            Some("action processing".to_owned())
        );
        let named = flame_as(&p, FlameLabels::Name);
        assert_eq!(category_at(&named, "@@a", 1), fetching);
        assert_eq!(category_at(&named, "download", 2), general);
        // Bottom-up, a frame roots its path and its callers and instance follow.
        let inverted = inverted_as(&p, FlameLabels::Kind);
        assert_eq!(category_at(&inverted, "download", 1), general);
        assert_eq!(category_at(&inverted, "Fetching repository", 2), fetching);
        assert_eq!(category_at(&inverted, "@@a", 3), fetching);
        assert_eq!(category_at(&inverted, "VFS write", 1), writing);
        assert_eq!(category_at(&inverted, "/tmp/a.tar", 4), writing);
    }

    #[test]
    fn a_frame_merging_spans_of_two_categories_carries_neither() {
        // The two lanes' waits merge into one top-level frame, while each
        // nested one stays under a parent of its own.
        let wait = |category, start, dur| slice("acquiring semaphore", category, start, dur);
        let p = profile(vec![
            track(
                1,
                LaneClass::Execution,
                vec![
                    wait("Remote execution setup", 0, 10),
                    slice("upload", "generic", 20, 30),
                    wait("Remote execution upload time", 20, 10),
                ],
            ),
            track(
                2,
                LaneClass::Execution,
                vec![
                    wait("Remote execution upload time", 0, 10),
                    slice("setup", "generic", 20, 30),
                    wait("Remote execution setup", 20, 10),
                ],
            ),
        ]);
        for labels in [FlameLabels::Kind, FlameLabels::Name] {
            let g = flame_as(&p, labels);
            assert_eq!(
                category_at(&g, "acquiring semaphore", 1),
                None,
                "{labels:?}"
            );
            let nested = |parent| {
                let parent = child(&g, &g.root(), parent);
                let wait = child(&g, &parent, "acquiring semaphore");
                wait.category.get().map(|c| g.str(c).to_owned())
            };
            assert_eq!(
                nested("upload"),
                Some("Remote execution upload time".to_owned()),
                "{labels:?}"
            );
            assert_eq!(
                nested("setup"),
                Some("Remote execution setup".to_owned()),
                "{labels:?}"
            );
        }
    }

    #[test]
    fn a_caller_reached_from_spans_of_two_categories_carries_neither_bottom_up() {
        // "step" is recorded under one category on the first lane and another
        // on the second, where it has a caller of its own. Bottom-up both
        // chains from "leaf" run through one "step" node.
        let lane = |category, start| {
            vec![
                slice("step", category, start, 100),
                slice("leaf", "generic", start, 60),
            ]
        };
        let mut second = vec![slice("outer", "generic", 0, 200)];
        second.extend(lane("second", 0));
        let p = profile(vec![
            track(1, LaneClass::Execution, lane("first", 0)),
            track(2, LaneClass::Execution, second),
        ]);
        let top_down = flame_as(&p, FlameLabels::Name);
        assert_eq!(category_at(&top_down, "step", 1), Some("first".to_owned()));
        assert_eq!(category_at(&top_down, "step", 2), Some("second".to_owned()));
        let g = inverted_as(&p, FlameLabels::Name);
        assert_eq!(category_at(&g, "leaf", 1), Some("generic".to_owned()));
        assert_eq!(category_at(&g, "step", 1), None);
        assert_eq!(category_at(&g, "step", 2), None);
        assert_eq!(category_at(&g, "outer", 3), Some("generic".to_owned()));
    }

    #[test]
    fn a_category_past_the_last_an_id_reaches_is_carried_by_no_frame() {
        // Categories are numbered in text order, so each filler takes an id
        // ahead of the one sorting after them all.
        let fillers = i64::from(u16::MAX);
        let mut slices: Vec<TestSlice> = (0..fillers)
            .map(|i| slice(&format!("s{i:05}"), &format!("c{i:05}"), i * 10, 5))
            .collect();
        slices.push(slice("last", "d", fillers * 10, 5));
        let p = profile(vec![track(1, LaneClass::Analysis, slices)]);
        let g = flame(&p);
        assert_eq!(category_at(&g, "s00000", 1), Some("c00000".to_owned()));
        assert_eq!(category_at(&g, "s65534", 1), Some("c65534".to_owned()));
        assert_eq!(category_at(&g, "last", 1), None);
    }

    #[test]
    fn selection_draws_only_the_chosen_lane_class() {
        let p = profile(vec![
            track(7, LaneClass::Analysis, vec![action("CppCompile", 0, 20)]),
            track(8, LaneClass::Execution, vec![action("Rustc", 0, 40)]),
            track(9, LaneClass::RepoFetch, vec![action("Fetch", 0, 5)]),
        ]);
        let t = p.threads();
        let class = |c: LaneClass| {
            TrackFlamer::new(&p).build(FlameLabels::Kind, &TimeSelection::default(), move |i| {
                t[i].lane_class == c
            })
        };
        let analysis = class(LaneClass::Analysis);
        assert_eq!(analysis.root().total_us, 20);
        assert!(
            analysis
                .children(&analysis.root())
                .iter()
                .all(|c| analysis.str(c.name) == "CppCompile")
        );
        let exec = class(LaneClass::Execution);
        assert_eq!(exec.root().total_us, 40);
        assert!(
            exec.children(&exec.root())
                .iter()
                .all(|c| exec.str(c.name) == "Rustc")
        );
        let loading = class(LaneClass::RepoFetch);
        assert_eq!(loading.root().total_us, 5);
        // Every real lane, the three classes unioned.
        assert_eq!(flame(&p).root().total_us, 65);
    }

    /// A profile spanning 0..250 whose shapes a window has to clip: a nested
    /// mnemonic action, a zero-duration span, and a second lane.
    fn clipping_profile() -> Arc<Profile> {
        profile(vec![
            track(
                1,
                LaneClass::Execution,
                vec![
                    action_named("Compiling //x:y", "Rustc", 0, 100),
                    slice("subprocess.run", "subprocess run", 60, 20),
                    slice("checkOutputs", "general information", 90, 0),
                ],
            ),
            track(2, LaneClass::Analysis, vec![action("CppCompile", 200, 50)]),
        ])
    }

    #[test]
    fn a_window_spanning_the_profile_matches_the_unrestricted_graph() {
        let p = clipping_profile();
        for labels in [FlameLabels::Kind, FlameLabels::Name] {
            let whole = flame_within(&p, labels, &window(&[(0, 250)]));
            assert_eq!(
                resolved(&whole),
                resolved(&flame_as(&p, labels)),
                "{labels:?}"
            );
        }
    }

    #[test]
    fn a_window_of_the_whole_profile_span_matches_the_unrestricted_graph() {
        // The lane's last span is zero-duration, so the profile's extent
        // reaches past it only if it covers the microsecond that span sits in.
        let p = profile(vec![track(
            1,
            LaneClass::Execution,
            vec![
                action_named("Compiling //x:y", "Rustc", 0, 100),
                slice("checkOutputs", "general information", 100, 0),
            ],
        )]);
        let span = p.span();
        for labels in [FlameLabels::Kind, FlameLabels::Name] {
            let whole = flame_within(&p, labels, &window(&[(span.start_us, span.end_us)]));
            assert_eq!(
                resolved(&whole),
                resolved(&flame_as(&p, labels)),
                "{labels:?}"
            );
        }
    }

    #[test]
    fn a_window_over_half_a_span_halves_its_total() {
        let p = profile(vec![track(
            1,
            LaneClass::Execution,
            vec![action("Rustc", 0, 100)],
        )]);
        let g = flame_within(&p, FlameLabels::Name, &window(&[(0, 50)]));
        let rustc = child(&g, &g.root(), "Rustc");
        assert_eq!(rustc.total_us, 50);
        assert_eq!(rustc.count, 1);
        assert_eq!(g.root().total_us, 50);
    }

    #[test]
    fn regions_covering_a_span_between_them_count_it_once() {
        let p = profile(vec![track(
            1,
            LaneClass::Execution,
            vec![action("Rustc", 0, 100)],
        )]);
        let total = |regions: &[(i64, i64)]| {
            let g = flame_within(&p, FlameLabels::Name, &window(regions));
            child(&g, &g.root(), "Rustc").total_us
        };
        assert_eq!(total(&[(0, 40)]), 40);
        assert_eq!(total(&[(60, 100)]), 40);
        // The pieces the two regions take are the pieces each takes alone.
        assert_eq!(total(&[(0, 40), (60, 100)]), 80);
        // Two regions meeting at an instant cover the span, and cover it once.
        assert_eq!(total(&[(0, 50), (50, 100)]), 100);
        assert_eq!(total(&[(0, 100)]), 100);
    }

    #[test]
    fn a_span_outside_every_region_contributes_no_frame() {
        let p = profile(vec![track(
            1,
            LaneClass::Execution,
            vec![action("Rustc", 0, 50), action("CppCompile", 200, 50)],
        )]);
        let g = flame_within(&p, FlameLabels::Name, &window(&[(0, 50)]));
        assert_eq!(g.root().total_us, 50);
        let kids: Vec<String> = g
            .children(&g.root())
            .iter()
            .map(|c| g.str(c.name).to_owned())
            .collect();
        assert_eq!(kids, ["Rustc"]);
    }

    #[test]
    fn a_zero_duration_span_needs_a_region_holding_its_instant() {
        let p = profile(vec![track(
            1,
            LaneClass::Execution,
            vec![
                slice("inside", "general information", 25, 0),
                slice("outside", "general information", 75, 0),
            ],
        )]);
        let g = flame_within(&p, FlameLabels::Name, &window(&[(0, 50)]));
        assert_eq!(child(&g, &g.root(), "inside").total_us, 0);
        assert_eq!(g.children(&g.root()).len(), 1);
        // Unrestricted, a zero-duration span frames like any other.
        let all = flame_as(&p, FlameLabels::Name);
        assert_eq!(all.children(&all.root()).len(), 2);
    }

    /// Whether a window of `regions` frames a lone zero-duration span sitting
    /// at `at_us`.
    fn frames_zero_duration_at(regions: &[(i64, i64)], at_us: i64) -> bool {
        let p = profile(vec![track(
            1,
            LaneClass::Execution,
            vec![slice("instant", "general information", at_us, 0)],
        )]);
        let g = flame_within(&p, FlameLabels::Name, &window(regions));
        !g.children(&g.root()).is_empty()
    }

    #[test]
    fn a_zero_duration_span_at_a_regions_start_is_present() {
        assert!(frames_zero_duration_at(&[(100, 200)], 100));
    }

    #[test]
    fn a_zero_duration_span_a_microsecond_before_a_regions_start_is_absent() {
        // The leading region holds no part of the span either; it is there to
        // carry the lane walk far enough to reach it.
        assert!(!frames_zero_duration_at(&[(0, 50), (100, 200)], 99));
    }

    #[test]
    fn a_zero_duration_span_a_microsecond_inside_a_regions_end_is_present() {
        assert!(frames_zero_duration_at(&[(100, 200)], 199));
    }

    #[test]
    fn a_zero_duration_span_at_a_regions_end_is_absent() {
        assert!(!frames_zero_duration_at(&[(100, 200)], 200));
    }

    #[test]
    fn a_window_over_a_lanes_tail_aggregates_what_the_whole_lanes_walk_would() {
        // The lane's slices all sit past 20, so the leading region holds none of
        // them and adds nothing but reach: the two selections aggregate the same
        // spans, one walking from the tail and one from the head of the lane.
        let p = profile(vec![track(
            1,
            LaneClass::Execution,
            vec![
                action_named("Compiling //a:a", "Rustc", 20, 30),
                slice("subprocess.run", "subprocess run", 25, 10),
                action_named("Compiling //b:b", "Rustc", 80, 20),
                slice("checkOutputs", "check outputs", 85, 5),
            ],
        )]);
        for labels in [FlameLabels::Kind, FlameLabels::Name] {
            let tail = flame_within(&p, labels, &window(&[(80, 100)]));
            let reaching_back = flame_within(&p, labels, &window(&[(0, 5), (80, 100)]));
            assert_eq!(resolved(&tail), resolved(&reaching_back), "{labels:?}");
        }
    }

    #[test]
    fn a_callee_nests_under_a_caller_it_overshoots_by_one_microsecond_but_not_by_two() {
        let lane = |callee_dur| {
            profile(vec![track(
                1,
                LaneClass::Execution,
                vec![
                    slice("_cc_library_impl", "general information", 0, 100),
                    slice("_compile", "general information", 10, callee_dur),
                ],
            )])
        };
        let nested = flame(&lane(91));
        let caller = child(&nested, &nested.root(), "_cc_library_impl");
        assert_eq!(child(&nested, &caller, "_compile").total_us, 91);
        assert_eq!(nested.children(&nested.root()).len(), 1);

        let apart = flame(&lane(92));
        assert_eq!(
            child_totals(&apart, &apart.root()),
            [
                ("_cc_library_impl".to_owned(), 100),
                ("_compile".to_owned(), 92)
            ]
        );
    }

    #[test]
    fn spans_running_back_to_back_a_step_apart_stay_siblings() {
        // Bazel's per-module lifecycle callbacks run one after another, each a
        // microsecond long, so every one ends a step past the one before it.
        // Read as nesting they would chain into a staircase as deep as the run
        // is long, and lane heights follow that depth.
        let step = |name: &str, start| slice(name, "general information", start, 1);
        let p = profile(vec![track(
            1,
            LaneClass::Execution,
            vec![step("a", 0), step("b", 1), step("c", 2), step("d", 3)],
        )]);
        let g = flame(&p);
        assert_eq!(g.children(&g.root()).len(), 4);
        assert_eq!(p.threads()[0].max_depth, 1);
    }

    #[test]
    fn slice_depth_matches_the_containment_chain_down_a_staircase_of_overshoots() {
        // Each span here ends one microsecond after the one it nests in, and the
        // last outlives them all by enough to sit beside the first. The parser's
        // depth stack and the containment walk must read the lane alike.
        let p = profile(vec![track(
            1,
            LaneClass::Execution,
            vec![
                slice("a", "general information", 0, 100),
                slice("b", "general information", 1, 100),
                slice("c", "general information", 2, 100),
                slice("d", "general information", 3, 100),
                slice("e", "general information", 4, 102),
            ],
        )]);
        let thread = &p.threads()[0];
        let slices = p.slices(thread);
        let parents = containment_parents(slices);
        let chain_depth = |mut i: usize| {
            let mut depth = 0;
            while let Some(parent) = parents[i] {
                depth += 1;
                i = parent;
            }
            depth
        };
        let from_parse: Vec<u32> = slices.iter().map(|s| s.depth).collect();
        let from_chain: Vec<u32> = (0..slices.len()).map(chain_depth).collect();
        assert_eq!(from_parse, [0, 1, 2, 3, 0]);
        assert_eq!(from_parse, from_chain);
        assert_eq!(thread.max_depth, 4);
    }

    #[test]
    fn a_window_clipping_an_overshooting_pair_floors_the_parents_self_time() {
        // The window holds one microsecond of "caller" and two of "callee",
        // which began inside it and is recorded ending a microsecond past it, so
        // the child's clipped duration exceeds the parent's.
        let p = profile(vec![track(
            1,
            LaneClass::Execution,
            vec![
                slice("caller", "general information", 0, 100),
                slice("callee", "general information", 50, 51),
            ],
        )]);
        let g = flame_within(&p, FlameLabels::Name, &window(&[(99, 150)]));
        let caller = child(&g, &g.root(), "caller");
        assert_eq!((caller.total_us, caller.self_us), (1, 0));
        assert_eq!(child(&g, &caller, "callee").total_us, 2);
    }

    #[test]
    fn a_zero_duration_span_at_the_window_start_keeps_the_ancestor_it_keys_onto() {
        // The lane walk's lower bound at its tightest. A zero-duration slice
        // reaches the window by the microsecond it occupies, so its own end sits
        // at the window's start rather than past it, as does the end of the
        // slice it nests in — and `end_maxes` carries nothing higher. Admitting
        // only slices ending past the window's start would drop the pair.
        let p = profile(vec![track(
            1,
            LaneClass::Execution,
            vec![
                slice("outer", "general information", 10, 0),
                slice("inner", "general information", 10, 0),
            ],
        )]);
        let g = flame_within(&p, FlameLabels::Name, &window(&[(10, 20)]));
        let outer = child(&g, &g.root(), "outer");
        assert_eq!(child(&g, &outer, "inner").total_us, 0);
    }

    #[test]
    fn a_span_starting_before_the_window_still_parents_its_nested_spans() {
        let p = profile(vec![track(
            1,
            LaneClass::Execution,
            vec![
                action_named("Compiling //x:y", "Rustc", 0, 100),
                slice("subprocess.run", "subprocess run", 60, 30),
                slice("checkOutputs", "check outputs", 70, 10),
            ],
        )]);
        let g = flame_within(&p, FlameLabels::Name, &window(&[(75, 95)]));
        let compiling = child(&g, &g.root(), "Compiling //x:y");
        assert_eq!(compiling.total_us, 20);
        let sub = child(&g, &compiling, "subprocess.run");
        assert_eq!(sub.total_us, 15);
        assert_eq!(child(&g, &sub, "checkOutputs").total_us, 5);
    }

    #[test]
    fn a_lane_lying_wholly_outside_the_window_contributes_nothing() {
        let p = profile(vec![
            track(1, LaneClass::Execution, vec![action("Rustc", 0, 50)]),
            track(2, LaneClass::Execution, vec![action("Javac", 200, 50)]),
        ]);
        let g = flame_within(&p, FlameLabels::Kind, &window(&[(200, 250)]));
        assert_eq!(child_totals(&g, &g.root()), [("Javac".to_owned(), 50)]);
    }

    #[test]
    fn a_selection_whose_regions_sit_far_apart_aggregates_both_of_them() {
        let p = profile(vec![track(
            1,
            LaneClass::Execution,
            vec![action("Rustc", 0, 10), action("Javac", 990, 10)],
        )]);
        let g = flame_within(&p, FlameLabels::Kind, &window(&[(0, 10), (990, 1000)]));
        assert_eq!(
            child_totals(&g, &g.root()),
            [("Javac".to_owned(), 10), ("Rustc".to_owned(), 10)]
        );
    }

    #[test]
    fn a_window_cutting_a_parent_leaves_a_nested_child_whole() {
        // Rustc(0..100) contains subprocess.run(60..80); the window opens at 50,
        // so the action loses its first half and the child none of itself.
        let p = profile(vec![track(
            1,
            LaneClass::Execution,
            vec![
                action_named("Compiling //x:y", "Rustc", 0, 100),
                slice("subprocess.run", "subprocess run", 60, 20),
            ],
        )]);
        let g = flame_within(&p, FlameLabels::Kind, &window(&[(50, 100)]));
        let rustc = child(&g, &g.root(), "Rustc");
        assert_eq!(rustc.total_us, 50);
        let sub = child(&g, &rustc, "subprocess.run");
        assert_eq!(sub.total_us, 20);
        // Each frame's self time is what the window leaves it less its children.
        assert_eq!(child(&g, &rustc, "Compiling //x:y").total_us, 30);
        assert_eq!(child(&g, &sub, "Compiling //x:y").total_us, 20);
        assert_eq!(g.root().total_us, 50);
    }

    #[test]
    fn a_track_selection_change_under_one_window_reuses_the_reduced_lanes() {
        let p = clipping_profile();
        let win = window(&[(0, 90)]);
        let scan = TrackFlamer::new(&p);
        let one = scan.build(FlameLabels::Name, &win, |i| i == 0);
        let other = scan.build(FlameLabels::Name, &win, |i| i == 1);
        let both = scan.build(FlameLabels::Name, &win, |_| true);
        assert_eq!(scan.reductions(), 1, "one window, one reduction");
        // Each selection reads as an independently scanned build of it.
        for (graph, lane) in [(&one, 0usize), (&other, 1)] {
            let fresh = TrackFlamer::new(&p).build(FlameLabels::Name, &win, |i| i == lane);
            assert_eq!(resolved(graph), resolved(&fresh));
        }
        // The lanes are disjoint, so their totals add up to the union's.
        assert_eq!(
            both.root().total_us,
            one.root().total_us + other.root().total_us
        );
        let narrower = window(&[(0, 40)]);
        scan.build(FlameLabels::Name, &narrower, |_| true);
        assert_eq!(scan.reductions(), 2);
        scan.build(FlameLabels::Kind, &narrower, |_| true);
        assert_eq!(scan.reductions(), 3);
    }

    #[test]
    fn toggling_the_labelling_under_one_window_reduces_each_labelling_once() {
        let p = clipping_profile();
        let win = window(&[(0, 90)]);
        let scan = TrackFlamer::new(&p);
        let mnemonic = scan.build(FlameLabels::Kind, &win, |_| true);
        let name = scan.build(FlameLabels::Name, &win, |_| true);
        assert_eq!(scan.reductions(), 2, "one reduction per labelling");
        let mnemonic_again = scan.build(FlameLabels::Kind, &win, |_| true);
        let name_again = scan.build(FlameLabels::Name, &win, |_| true);
        assert_eq!(scan.reductions(), 2, "a revisited labelling is a memo hit");
        assert_eq!(resolved(&mnemonic), resolved(&mnemonic_again));
        assert_eq!(resolved(&name), resolved(&name_again));
    }

    #[test]
    fn a_window_reduces_once_for_durations_and_once_more_for_its_wall_column() {
        let p = clipping_profile();
        let win = window(&[(0, 90)]);
        let scan = TrackFlamer::new(&p);
        scan.build(FlameLabels::Name, &win, |_| true);
        assert_eq!(scan.reductions(), 1);
        let filled = scan.build_wall_share(FlameLabels::Name, &win, |_| true);
        assert_eq!(scan.reductions(), 2, "the held lanes gain a wall column");
        scan.build_wall_share(FlameLabels::Name, &win, |i| i == 0);
        scan.build(FlameLabels::Name, &win, |_| true);
        assert_eq!(
            scan.reductions(),
            2,
            "either weight then merges what is held"
        );

        // Asked for wall share first, one reduction serves both weights, and
        // reads as the two reductions do.
        let fresh = TrackFlamer::new(&p);
        let at_once = fresh.build_wall_share(FlameLabels::Name, &win, |_| true);
        let by_duration = fresh.build(FlameLabels::Name, &win, |_| true);
        assert_eq!(fresh.reductions(), 1);
        assert_eq!(resolved(&at_once.flame), resolved(&filled.flame));
        assert_eq!(at_once.duration_us, filled.duration_us);
        let totals: Vec<i64> = (0..by_duration.len())
            .map(|i| by_duration.total_us(i))
            .collect();
        assert_eq!(at_once.duration_us, totals);
        // Carrying a wall column leaves the duration graph as a scan that
        // never weighed by wall share builds it.
        let untouched = TrackFlamer::new(&p).build(FlameLabels::Name, &win, |_| true);
        assert_eq!(by_duration.nodes(), untouched.nodes());
    }

    #[test]
    fn a_flamer_scans_and_splits_its_profile_only_once_a_build_needs_it() {
        let p = clipping_profile();
        let whole = TimeSelection::default();
        let scan = TrackFlamer::new(&p);
        assert!(scan.flamer.get().is_none() && scan.wall_share.get().is_none());
        scan.build(FlameLabels::Name, &whole, |_| true);
        assert!(scan.flamer.get().is_some() && scan.wall_share.get().is_none());
        scan.build_wall_share(FlameLabels::Name, &whole, |_| true);
        assert!(scan.wall_share.get().is_some());
    }

    #[test]
    fn a_memo_poisoned_by_a_panic_still_serves_the_reduced_lanes() {
        let p = clipping_profile();
        let win = window(&[(0, 90)]);
        let scan = TrackFlamer::new(&p);
        let before = scan.build(FlameLabels::Name, &win, |_| true);
        let memo = scan.windowed.of(FlameLabels::Name);
        let panicked = std::panic::catch_unwind(|| {
            let _entry = memo.0.lock().unwrap();
            panic!("a panic while the entry is locked");
        });
        assert!(panicked.is_err());
        assert!(memo.0.is_poisoned());
        let after = scan.build(FlameLabels::Name, &win, |_| true);
        assert_eq!(scan.reductions(), 1, "the poisoned memo still hits");
        assert_eq!(resolved(&before), resolved(&after));
    }

    #[test]
    fn a_build_racing_another_under_a_different_window_gets_its_own_lanes() {
        let p = clipping_profile();
        let scan = TrackFlamer::new(&p);
        let wide = window(&[(0, 90)]);
        let narrow = window(&[(0, 40)]);
        let raced = std::thread::scope(|s| {
            let a = s.spawn(|| scan.build(FlameLabels::Name, &wide, |_| true));
            let b = s.spawn(|| scan.build(FlameLabels::Name, &narrow, |_| true));
            [a.join().unwrap(), b.join().unwrap()]
        });
        for (graph, win) in raced.iter().zip([&wide, &narrow]) {
            let fresh = TrackFlamer::new(&p).build(FlameLabels::Name, win, |_| true);
            assert_eq!(resolved(graph), resolved(&fresh));
        }
    }

    #[test]
    fn overflowing_span_is_a_sibling_not_a_child() {
        // A span that starts inside another but ends AFTER it isn't enclosed, so
        // it is a sibling, not a child. Nesting it (popping only on
        // end-before-start) is what built the runaway false-depth staircase.
        // Labelled by name, so every frame is a span and Outer's childlessness
        // is exactly the containment claim.
        let p = profile(vec![track(
            1,
            LaneClass::Execution,
            vec![
                action("Outer", 0, 50),
                slice("inner", "general information", 10, 100),
            ],
        )]);
        let g = flame_as(&p, FlameLabels::Name);
        let outer = child(&g, &g.root(), "Outer");
        assert_eq!(outer.total_us, 50);
        assert!(
            g.children(&outer).is_empty(),
            "inner must not nest under Outer"
        );
        assert_eq!(child(&g, &g.root(), "inner").total_us, 100);
    }

    #[test]
    fn build_is_independent_of_slice_order() {
        // Disjoint top-level spans, so order only affects the emitted event
        // stream; the parser sorts each lane into lane order and the merge is
        // commutative.
        let ordered = vec![
            action("CppCompile", 0, 100),
            action("Rustc", 100, 40),
            action("CppCompile", 200, 50),
            slice("wait", "thread wait", 300, 90),
        ];
        let mut reversed = ordered.clone();
        reversed.reverse();
        let ga = flame(&profile(vec![track(1, LaneClass::Execution, ordered)]));
        let gb = flame(&profile(vec![track(1, LaneClass::Execution, reversed)]));
        // Compare every node field with the name resolved: the graphs hold two
        // independently interned profiles, so equal names need not share a
        // handle across them, but the flattened structure must be identical.
        let resolved = |g: &FlameGraph| -> Vec<_> {
            g.nodes()
                .iter()
                .map(|n| {
                    (
                        g.str(n.name).to_owned(),
                        n.total_us,
                        n.self_us,
                        n.width_us,
                        n.role,
                        n.count,
                        n.depth,
                        n.children,
                    )
                })
                .collect()
        };
        assert_eq!(resolved(&ga), resolved(&gb));
    }

    #[test]
    fn large_build_is_deterministic_across_the_parallel_threshold() {
        // More distinct disjoint frames than PARALLEL_MIN (1 << 13), so the union
        // `par_sort_by` merge and the `sort_child_ranges` parallel branch both run:
        // the sealed graph must be deterministic and independent of input order.
        const N: i64 = 9000;
        let slices = |reversed: bool| {
            let mut v: Vec<_> = (0..N)
                .map(|i| action(&format!("M{i:05}"), i * 100, (i % 97) + 1))
                .collect();
            if reversed {
                v.reverse();
            }
            v
        };
        let resolved = |g: &FlameGraph| -> Vec<_> {
            g.nodes()
                .iter()
                .map(|n| {
                    (
                        g.str(n.name).to_owned(),
                        n.total_us,
                        n.self_us,
                        n.width_us,
                        n.role,
                        n.count,
                        n.depth,
                        n.children,
                    )
                })
                .collect::<Vec<_>>()
        };
        let a = flame(&profile(vec![track(
            1,
            LaneClass::Execution,
            slices(false),
        )]));
        let b = flame(&profile(vec![track(1, LaneClass::Execution, slices(true))]));
        let c = flame(&profile(vec![track(
            1,
            LaneClass::Execution,
            slices(false),
        )]));
        assert!(
            a.nodes().len() > (1 << 13),
            "profile must cross the parallel threshold"
        );
        assert_eq!(resolved(&a), resolved(&b), "independent of input order");
        assert_eq!(resolved(&a), resolved(&c), "deterministic across builds");
    }

    /// Knuth's multiplicative-hashing multiplier (*The Art of Computer
    /// Programming* vol. 3, §6.4): the odd integer nearest 2^64/φ. Odd makes it
    /// a unit mod 2^64, so multiplying is a bijection and the totals come out
    /// distinct; the golden ratio spreads consecutive multiples most evenly
    /// across the range, so they arrive in an order neither ascending nor
    /// descending.
    const SCRAMBLE: u64 = 0x9E37_79B9_7F4A_7C15;

    /// An arena of one parent per entry of `sizes` under the root, the `k`th
    /// holding `sizes[k]` children whose totals are distinct and in no
    /// particular order, returned with its parent array.
    fn child_range_arena(sizes: &[usize]) -> Arena {
        let node = |total_us: i64| ArenaNode {
            name: ROOT_LABEL,
            rank: 0,
            role: SpanRole {
                workload: Workload::Other,
                payload: false,
            },
            category: None,
            detail: false,
            total_us,
            count: 0,
        };
        let mut arena = Arena::with_capacity(1 + sizes.len() + sizes.iter().sum::<usize>());
        for _ in sizes {
            arena.push(node(0), 0);
        }
        for (k, &size) in sizes.iter().enumerate() {
            for j in 0..size {
                let total_us = (j as u64).wrapping_mul(SCRAMBLE) as i64;
                arena.push(node(total_us), 1 + k as u32);
            }
        }
        arena
    }

    #[test]
    fn a_wide_child_range_sorts_into_the_order_a_narrow_one_does() {
        let wide = wide_range_min();
        let sizes = [wide + 1, wide, wide - 1, 2, 1, 0];
        assert!(
            sizes.iter().any(|&s| s >= wide) && sizes.iter().any(|&s| (2..wide).contains(&s)),
            "the ranges must straddle the width at which a sort spreads over the workers"
        );
        let arena = child_range_arena(&sizes);
        let mut kids = csr_from_parents(&arena.parents);
        let n = arena.len();
        let expect: Vec<Vec<u32>> = (0..n)
            .map(|i| {
                let mut ids = kids.of(i).to_vec();
                ids.sort_by_key(|&c| (child_order(&arena, c as usize), c));
                ids
            })
            .collect();
        sort_child_ranges(&mut kids, n, &arena);
        for (i, want) in expect.iter().enumerate() {
            assert_eq!(kids.of(i), *want, "children of node {i}");
        }
    }

    #[test]
    fn a_sealed_graph_holds_no_capacity_past_its_nodes() {
        // Two lanes walking the same paths, so the merge sorts twice as many
        // elements as there are distinct paths to reconstruct: an arena sized
        // from the element count would leave its graph holding the difference.
        let lane = |tid: i64| {
            track(
                tid,
                LaneClass::Execution,
                vec![
                    action("Rustc", 0, 100),
                    slice("subprocess.run", "subprocess run", 0, 80),
                ],
            )
        };
        let p = profile(vec![lane(1), lane(2)]);
        for g in [flame(&p), invert(flame(&p))] {
            let held = [
                (g.name.len(), g.name.capacity()),
                (g.rank.len(), g.rank.capacity()),
                (g.total_us.len(), g.total_us.capacity()),
                (g.self_us.len(), g.self_us.capacity()),
                (g.width_us.len(), g.width_us.capacity()),
                (g.role.len(), g.role.capacity()),
                (g.detail.len(), g.detail.capacity()),
                (g.count.len(), g.count.capacity()),
                (g.subtree_max_depth.len(), g.subtree_max_depth.capacity()),
                (g.child_off.len(), g.child_off.capacity()),
                (g.child_pool.len(), g.child_pool.capacity()),
            ];
            assert!(
                held.iter().all(|&(len, capacity)| len == capacity),
                "(length, capacity) per vector: {held:?}"
            );
        }
    }

    #[test]
    fn tied_same_text_children_order_by_role() {
        // Two same-named, same-duration top-level slices of different span roles
        // tie on (total, text); the role tie-break keeps the flattened order
        // independent of which lane merges first.
        let build = |cats: [&str; 2]| {
            let p = profile(vec![
                track(1, LaneClass::Execution, vec![slice("x", cats[0], 0, 50)]),
                track(2, LaneClass::Execution, vec![slice("x", cats[1], 0, 50)]),
            ]);
            flame(&p)
        };
        let ga = build(["local action execution", "thread wait"]);
        let gb = build(["thread wait", "local action execution"]);
        let resolved = |g: &FlameGraph| -> Vec<(String, SpanRole, i64)> {
            g.nodes()
                .iter()
                .map(|n| (g.str(n.name).to_owned(), n.role, n.total_us))
                .collect()
        };
        assert_eq!(resolved(&ga), resolved(&gb));
        // Workload order (Subprocess before Scheduling) makes the tie-break total.
        assert_eq!(
            resolved(&ga)[1..],
            [
                (
                    "x".to_owned(),
                    SpanRole {
                        workload: Workload::Subprocess,
                        payload: true
                    },
                    50
                ),
                (
                    "x".to_owned(),
                    SpanRole {
                        workload: Workload::Scheduling,
                        payload: false
                    },
                    50
                )
            ]
        );
    }

    #[test]
    fn layout_widths_are_proportional_and_bounded() {
        // root width 150: CppCompile=100, Rustc=50 -> widths 200, 100 of 300px.
        let p = profile(vec![track(
            1,
            LaneClass::Execution,
            vec![action("CppCompile", 0, 100), action("Rustc", 100, 50)],
        )]);
        let g = flame(&p);
        let boxes = lay_out_flame(
            &g,
            300.0,
            FlamePlacement {
                scroll_x: 0.0,
                view_w: 300.0,
                metrics: &METRICS,
                min_px: 0.0,
            },
        );
        let root_box = &boxes[0];
        assert_eq!(root_box.w, 300.0);
        assert_eq!(root_box.y, 0.0);
        let a = frame_box(&g, &boxes, "CppCompile").unwrap();
        let b = frame_box(&g, &boxes, "Rustc").unwrap();
        assert_eq!(a.w, 200.0);
        assert_eq!(b.w, 100.0);
        assert_eq!(a.x, 0.0);
        assert_eq!(b.x, 200.0);
        assert_eq!(a.y, METRICS.row_height);
        // Children fit within the parent.
        assert!(a.w + b.w <= root_box.w + f32::EPSILON);
    }

    /// `g` laid out over `content_w` pixels, unscrolled, in a viewport that
    /// shows all of it, with a one-pixel minimum box width.
    fn lay_out(g: &FlameGraph, content_w: f64) -> Vec<FlameBox> {
        lay_out_flame(
            g,
            content_w,
            FlamePlacement {
                scroll_x: 0.0,
                view_w: content_w.ceil() as f32,
                metrics: &METRICS,
                min_px: 1.0,
            },
        )
    }

    /// The name of the frame `b` stands for, `None` for a box standing for
    /// none.
    fn frame_name<'a>(g: &'a FlameGraph, b: &FlameBox) -> Option<&'a str> {
        match b.content {
            FlameBoxContent::Frame(frame) => Some(g.str(frame.name)),
            FlameBoxContent::Remainder(_) | FlameBoxContent::RemainderDepth(_) => None,
        }
    }

    /// The box of the frame named `name`, if one was laid out.
    fn frame_box<'a>(g: &FlameGraph, boxes: &'a [FlameBox], name: &str) -> Option<&'a FlameBox> {
        boxes.iter().find(|b| frame_name(g, b) == Some(name))
    }

    /// The remainder a box at `depth` stands for.
    fn remainder_at(
        boxes: &[FlameBox],
        depth: u32,
    ) -> impl Iterator<Item = (&FlameBox, FlameRemainder)> {
        boxes
            .iter()
            .filter(move |b| b.depth == depth)
            .filter_map(|b| match b.content {
                FlameBoxContent::Remainder(r) => Some((b, r)),
                FlameBoxContent::Frame(_) | FlameBoxContent::RemainderDepth(_) => None,
            })
    }

    /// Every remainder box of `g`'s layout at `depth` as `(x, w, frames,
    /// total_us)`, in paint order.
    fn remainders(g: &FlameGraph, boxes: &[FlameBox], depth: u32) -> Vec<(f32, f32, u32, i64)> {
        remainder_at(boxes, depth)
            .map(|(b, r)| (b.x, b.w, r.frames(), g.remainder_total_us(&r)))
            .collect()
    }

    /// Every box of a remainder's depth as `(x, w, first row, rows)`, in paint
    /// order.
    fn remainder_depths(boxes: &[FlameBox]) -> Vec<(f32, f32, u32, f32)> {
        boxes
            .iter()
            .filter(|b| matches!(b.content, FlameBoxContent::RemainderDepth(_)))
            .map(|b| (b.x, b.w, b.depth, b.h / METRICS.row_height))
            .collect()
    }

    #[test]
    fn a_run_of_sub_pixel_siblings_is_one_remainder_of_their_count_and_total() {
        // At 0.125px per microsecond "wide" takes 127.5px; "aaa", "bbb" and
        // "ccc" are each sub-pixel, and half a pixel together.
        let p = profile(vec![track(
            1,
            LaneClass::Execution,
            vec![
                action("wide", 0, 1020),
                action("aaa", 1020, 2),
                action("bbb", 1022, 1),
                action("ccc", 1023, 1),
            ],
        )]);
        let g = flame_as(&p, FlameLabels::Name);
        let boxes = lay_out(&g, 128.0);
        let wide = frame_box(&g, &boxes, "wide").unwrap();
        assert_eq!((wide.x, wide.w), (0.0, 127.5));
        // Half a pixel wide, the run is drawn as a marker of one.
        assert_eq!(remainders(&g, &boxes, 1), [(127.5, 1.0, 3, 4)]);
        for name in ["aaa", "bbb", "ccc"] {
            assert!(frame_box(&g, &boxes, name).is_none(), "{name}");
        }
    }

    #[test]
    fn a_single_sub_pixel_sibling_stays_itself() {
        let p = profile(vec![track(
            1,
            LaneClass::Execution,
            vec![action("wide", 0, 500), action("aaa", 500, 1)],
        )]);
        let g = flame_as(&p, FlameLabels::Name);
        let boxes = lay_out(&g, 100.0);
        assert_eq!(frame_box(&g, &boxes, "aaa").unwrap().w, 1.0);
        assert_eq!(remainders(&g, &boxes, 1), []);
    }

    #[test]
    fn a_remainder_is_as_wide_as_its_members_together() {
        // "pad" takes 5.5px of the 13.75px content; the eleven 3us siblings
        // after it are 0.75px each.
        let mut slices = vec![slice("pad", "general information", 0, 22)];
        slices.extend(
            (0..11).map(|i: i64| slice(&format!("r{i:02}"), "general information", 22 + 3 * i, 3)),
        );
        let p = profile(vec![track(1, LaneClass::Execution, slices)]);
        let g = flame(&p);
        let boxes = lay_out(&g, 13.75);
        assert_eq!(remainders(&g, &boxes, 1), [(5.5, 8.25, 11, 33)]);
        assert_eq!(painted_row(&boxes, 1), [(0.0, 5.5), (5.5, 8.25)]);
    }

    /// Three parents side by side at 0.5px per microsecond: "p" holding sixteen
    /// 1us children, "q" one 12us child, and "r" eight 1us children.
    fn runs_around_a_wide_box() -> FlameGraph {
        let mut slices = vec![slice("p", "general information", 0, 16)];
        slices
            .extend((0..16).map(|i: i64| slice(&format!("p{i:02}"), "general information", i, 1)));
        slices.push(slice("q", "general information", 16, 12));
        slices.push(slice("q00", "general information", 16, 12));
        slices.push(slice("r", "general information", 28, 8));
        slices.extend(
            (0..8).map(|i: i64| slice(&format!("r{i:02}"), "general information", 28 + i, 1)),
        );
        let p = profile(vec![track(1, LaneClass::Execution, slices)]);
        flame_as(&p, FlameLabels::Name)
    }

    #[test]
    fn a_wide_box_between_two_runs_keeps_them_two_remainders() {
        let g = runs_around_a_wide_box();
        let boxes = lay_out(&g, 18.0);
        assert_eq!(
            remainders(&g, &boxes, 2),
            [(0.0, 8.0, 16, 16), (14.0, 4.0, 8, 8)]
        );
        let wide = frame_box(&g, &boxes, "q00").unwrap();
        assert_eq!((wide.x, wide.w), (8.0, 6.0));
    }

    #[test]
    fn runs_under_adjoining_parents_are_a_remainder_each() {
        // "p" and "q" each hold eight 1us children, 0.5px apiece.
        let mut slices = Vec::with_capacity(18);
        for (parent, start) in [("p", 0), ("q", 8)] {
            slices.push(slice(parent, "general information", start, 8));
            slices.extend(
                (0..8).map(|i: i64| {
                    slice(&format!("{parent}{i}"), "general information", start + i, 1)
                }),
            );
        }
        let p = profile(vec![track(1, LaneClass::Execution, slices)]);
        let g = flame_as(&p, FlameLabels::Name);
        assert_eq!(
            remainders(&g, &lay_out(&g, 8.0), 2),
            [(0.0, 4.0, 8, 8), (4.0, 4.0, 8, 8)]
        );
    }

    #[test]
    fn widening_the_view_resolves_a_remainder_into_its_members() {
        let g = runs_around_a_wide_box();
        // At 2px per microsecond every member clears the minimum width.
        let boxes = lay_out(&g, 72.0);
        assert_eq!(remainders(&g, &boxes, 2), []);
        for i in 0..16 {
            let member = frame_box(&g, &boxes, &format!("p{i:02}")).unwrap();
            assert_eq!((member.x, member.w), (2.0 * i as f32, 2.0), "p{i:02}");
        }
        for i in 0..8 {
            let member = frame_box(&g, &boxes, &format!("r{i:02}")).unwrap();
            assert_eq!(
                (member.x, member.w),
                (56.0 + 2.0 * i as f32, 2.0),
                "r{i:02}"
            );
        }
    }

    #[test]
    fn a_remainder_partly_off_the_viewport_stands_for_its_whole_run() {
        let g = runs_around_a_wide_box();
        // The viewport shows content pixels 3 to 7 of 18: half of the first
        // run's eight, and none of the second run.
        let boxes = lay_out_flame(
            &g,
            18.0,
            FlamePlacement {
                scroll_x: 3.0,
                view_w: 4.0,
                metrics: &METRICS,
                min_px: 1.0,
            },
        );
        assert_eq!(remainders(&g, &boxes, 2), [(-3.0, 8.0, 16, 16)]);
    }

    #[test]
    fn a_remainder_says_whether_its_parent_holds_wider_children_beside_it() {
        let siblings = |g: &FlameGraph, content_w, depth| -> Vec<RemainderSiblings> {
            remainder_at(&lay_out(g, content_w), depth)
                .map(|(_, r)| r.siblings())
                .collect()
        };
        // Every child of "p" and of "r" is in its run.
        let all = runs_around_a_wide_box();
        assert_eq!(
            siblings(&all, 18.0, 2),
            [RemainderSiblings::None, RemainderSiblings::None]
        );
        // "pad" is a box of its own beside the run.
        let mut slices = vec![slice("pad", "general information", 0, 22)];
        slices.extend(
            (0..11).map(|i: i64| slice(&format!("r{i:02}"), "general information", 22 + 3 * i, 3)),
        );
        let p = profile(vec![track(1, LaneClass::Execution, slices)]);
        let beside = flame_as(&p, FlameLabels::Name);
        assert_eq!(siblings(&beside, 13.75, 1), [RemainderSiblings::Wider]);
    }

    #[test]
    fn a_remainder_carries_the_role_its_members_share() {
        let role = |last_category| {
            let mut slices = vec![slice("pad", "general information", 0, 990)];
            slices
                .extend((0..4).map(|i: i64| slice(&format!("/f{i}"), "VFS stat", 990 + 2 * i, 2)));
            slices.push(slice("/last", last_category, 998, 2));
            let p = profile(vec![track(1, LaneClass::Execution, slices)]);
            let g = flame_as(&p, FlameLabels::Name);
            let boxes = lay_out(&g, 100.0);
            assert_eq!(remainders(&g, &boxes, 1), [(99.0, 1.0, 5, 10)]);
            let (_, remainder) = remainder_at(&boxes, 1).next().unwrap();
            remainder.role().map(|role| role.workload)
        };
        assert_eq!(role("VFS stat"), Some(Workload::Filesystem));
        assert_eq!(role("gc notification"), None);
    }

    /// "pad" beside eight 4us members named in the order of their boxes. The
    /// first three are leaves, the next two wrap one child apiece, the two
    /// after those a child and a grandchild, and the last is a leaf.
    fn members_of_three_depths() -> FlameGraph {
        const NESTING: [usize; 8] = [0, 0, 0, 1, 1, 2, 2, 0];
        let mut slices = vec![slice("pad", "general information", 0, 80)];
        for (i, nested) in NESTING.into_iter().enumerate() {
            let start = 80 + 4 * i as i64;
            slices.push(slice(&format!("m{i}"), "general information", start, 4));
            for level in 0..nested {
                let name = format!("m{i}-{level}");
                slices.push(slice(&name, "general information", start, 4));
            }
        }
        let p = profile(vec![track(1, LaneClass::Execution, slices)]);
        flame_as(&p, FlameLabels::Name)
    }

    #[test]
    fn beneath_a_remainder_each_column_shows_how_deep_its_members_reach() {
        let g = members_of_three_depths();
        // 0.125px per microsecond: a member is half a pixel, two to a column,
        // and the run four columns from 10px on.
        let boxes = lay_out(&g, 14.0);
        assert_eq!(remainders(&g, &boxes, 1), [(10.0, 4.0, 8, 32)]);
        // The first column holds leaves alone and shows nothing. The second
        // reaches one row down for its deeper member, and the last two reach
        // two rows as one box.
        assert_eq!(
            remainder_depths(&boxes),
            [(11.0, 1.0, 2, 1.0), (12.0, 2.0, 2, 2.0)]
        );
        // No member is a box of its own, nor is anything nested in one.
        let frames: Vec<&str> = boxes.iter().filter_map(|b| frame_name(&g, b)).collect();
        assert_eq!(frames, ["All", "pad"]);
        let (_, remainder) = remainder_at(&boxes, 1).next().unwrap();
        for b in &boxes {
            if let FlameBoxContent::RemainderDepth(of) = b.content {
                assert_eq!(of, remainder);
            }
        }
    }

    #[test]
    fn a_run_narrower_than_a_box_shows_its_depth_under_the_marker_it_is_drawn_as() {
        // "shallow" and "deep" are a tenth of a pixel together, from 99.9px on,
        // and "deep" nests one span.
        let p = profile(vec![track(
            1,
            LaneClass::Execution,
            vec![
                slice("pad", "general information", 0, 9990),
                slice("shallow", "general information", 9990, 6),
                slice("deep", "general information", 9996, 4),
                slice("deepchild", "general information", 9996, 1),
            ],
        )]);
        let g = flame_as(&p, FlameLabels::Name);
        let boxes = lay_out(&g, 100.0);
        assert_eq!(remainders(&g, &boxes, 1), [(99.9, 1.0, 2, 10)]);
        assert_eq!(remainder_depths(&boxes), [(99.9, 1.0, 2, 1.0)]);
        assert!(frame_box(&g, &boxes, "deepchild").is_none());
    }

    /// The node each node of `g` is a child of; the root's entry is its own.
    fn parents(g: &FlameGraph) -> Vec<usize> {
        let mut parents = vec![0; g.len()];
        for i in 0..g.len() {
            for k in 0..g.child_count(i) {
                parents[g.child_id(i, k)] = i;
            }
        }
        parents
    }

    #[test]
    fn a_painted_box_strays_from_its_row_and_its_parent_by_less_than_a_box() {
        /// How many random profiles are laid out.
        const PROFILES: usize = 48;
        /// Every random span begins before this.
        const HORIZON_US: i64 = 80;
        /// What f32 box edges may differ by from the f64 ones they round.
        const ROUNDING: f32 = 1e-3;
        const MIN_PX: f32 = 1.0;
        // Content widths around and far above the minimum box width, each with
        // the scroll offsets it is laid out at as a fraction of itself.
        const VIEWS: [(f64, [f64; 3]); 5] = [
            (3.0, [0.0, 0.37, 0.9]),
            (7.3, [0.0, 0.37, 0.9]),
            (20.0, [0.0, 0.11, 0.5]),
            (333.3, [0.0, 0.25, 0.6]),
            (5000.0, [0.0, 0.31, 0.83]),
        ];
        const VIEW_W: f32 = 40.0;
        const SEED: u64 = 0x1234_5678_9ABC_DEF1;

        let mut rng = Rng::with_seed(SEED);
        for case in 0..PROFILES {
            let p = random_profile(&mut rng, HORIZON_US);
            for labels in [FlameLabels::Kind, FlameLabels::Name] {
                let top_down = flame_as(&p, labels);
                for g in [invert(top_down.clone()), top_down] {
                    let parents = parents(&g);
                    for (content_w, scrolls) in VIEWS {
                        for scroll in scrolls {
                            let boxes = lay_out_flame(
                                &g,
                                content_w,
                                FlamePlacement {
                                    scroll_x: scroll * content_w,
                                    view_w: VIEW_W,
                                    metrics: &METRICS,
                                    min_px: MIN_PX,
                                },
                            );
                            let at = format!("profile {case} {labels:?} {content_w}px at {scroll}");
                            assert_boxes_keep_to_their_places(
                                &parents, &boxes, MIN_PX, ROUNDING, &at,
                            );
                        }
                    }
                }
            }
        }
    }

    /// Holds a layout to what [`painted`] leaves true of it. A box is drawn at
    /// its own span, or as a marker moved and widened by less than `min_px`, so
    /// two boxes sharing a row overlap by less than that, a frame's box starts
    /// before its parent's ends and ends less than a pixel short of where its
    /// parent's starts, and every frame drawn hangs under one that is drawn.
    fn assert_boxes_keep_to_their_places(
        parents: &[usize],
        boxes: &[FlameBox],
        min_px: f32,
        rounding: f32,
        at: &str,
    ) {
        let rows = |b: &FlameBox| b.depth..b.depth + (b.h / METRICS.row_height).round() as u32;
        let frame_of = |node: usize| {
            boxes.iter().find(|b| match b.content {
                FlameBoxContent::Frame(frame) => frame.node.0 as usize == node,
                _ => false,
            })
        };
        for (i, b) in boxes.iter().enumerate() {
            assert!(b.w > 0.0 && b.h > 0.0, "{b:?} {at}");
            for o in &boxes[i + 1..] {
                if rows(b).end <= rows(o).start || rows(o).end <= rows(b).start {
                    continue;
                }
                let overlap = (b.x + b.w).min(o.x + o.w) - b.x.max(o.x);
                assert!(overlap < min_px + rounding, "{b:?} over {o:?} {at}");
            }
            match b.content {
                FlameBoxContent::Frame(frame) if b.depth > 0 => {
                    let parent = frame_of(parents[frame.node.0 as usize])
                        .unwrap_or_else(|| panic!("no parent drawn for {b:?} {at}"));
                    assert_eq!(parent.depth + 1, b.depth, "{b:?} {at}");
                    assert!(
                        b.x < parent.x + parent.w + rounding,
                        "{b:?} under {parent:?} {at}"
                    );
                    assert!(
                        b.x + b.w > parent.x - 1.0 - rounding,
                        "{b:?} under {parent:?} {at}"
                    );
                }
                FlameBoxContent::Frame(_) => {}
                FlameBoxContent::Remainder(_) => {
                    let covered = boxes.iter().any(|o| {
                        o.depth + 1 == b.depth
                            && matches!(o.content, FlameBoxContent::Frame(_))
                            && b.x < o.x + o.w + rounding
                            && b.x + b.w > o.x - 1.0 - rounding
                    });
                    assert!(covered, "nothing above {b:?} {at}");
                }
                FlameBoxContent::RemainderDepth(of) => {
                    let remainder = boxes.iter().find(|o| {
                        o.depth + 1 == b.depth
                            && o.content == FlameBoxContent::Remainder(of)
                            && o.x - rounding <= b.x
                            && b.x + b.w <= o.x + o.w + rounding
                    });
                    assert!(remainder.is_some(), "no remainder above {b:?} {at}");
                }
            }
        }
    }

    /// The painted `(x, w)` of every box at `depth`, in paint order.
    fn painted_row(boxes: &[FlameBox], depth: u32) -> Vec<(f32, f32)> {
        boxes
            .iter()
            .filter(|b| b.depth == depth)
            .map(|b| (b.x, b.w))
            .collect()
    }

    #[test]
    fn contiguous_subpixel_siblings_paint_a_gapless_band() {
        // "pad" takes 5.5px of the 13.75px content; the eleven 3us siblings after
        // it are 0.75px each, so none renders on its own, and the run starts
        // mid-column at 5.5px. Their equal totals order them by name.
        let mut slices = vec![slice("pad", "general information", 0, 22)];
        slices.extend(
            (0..11).map(|i: i64| slice(&format!("r{i:02}"), "general information", 22 + 3 * i, 3)),
        );
        let p = profile(vec![track(1, LaneClass::Execution, slices)]);
        let g = flame(&p);
        let content_w = 13.75;
        let boxes = lay_out_flame(
            &g,
            content_w,
            FlamePlacement {
                scroll_x: 0.0,
                view_w: 100.0,
                metrics: &METRICS,
                min_px: 1.0,
            },
        );
        assert_gapless(&painted_row(&boxes, 1), 0.0, content_w as f32);
    }

    #[test]
    fn a_subpixel_run_joins_the_wide_boxes_around_it_without_a_gap() {
        // Over 6.625px of content, "p" holds a 1px child then three 0.875px ones
        // — each running past the edge the one before it paints to — and "q",
        // the next parent along, opens with a 2.875px child. The run leaves off
        // at 3.625px, whose fractional part exceeds a half, so that child's left
        // edge rounds up to the next device column: a painted span stopping short
        // of it leaves a whole empty column between the two. Siblings are ordered
        // widest-first, so a wide box can only follow a sub-pixel run under a
        // later parent.
        let p = profile(vec![track(
            1,
            LaneClass::Execution,
            vec![
                slice("p", "general information", 0, 29),
                slice("a0", "general information", 0, 8),
                slice("a1", "general information", 8, 7),
                slice("a2", "general information", 15, 7),
                slice("a3", "general information", 22, 7),
                slice("q", "general information", 29, 24),
                slice("b0", "general information", 29, 23),
            ],
        )]);
        let g = flame_as(&p, FlameLabels::Name);
        let boxes = lay_out_flame(
            &g,
            6.625,
            FlamePlacement {
                scroll_x: 0.0,
                view_w: 100.0,
                metrics: &METRICS,
                min_px: 1.0,
            },
        );
        assert_gapless(&painted_row(&boxes, 2), 0.0, 6.5);
    }

    #[test]
    fn lay_out_is_viewport_relative_and_culls_off_screen() {
        // A wide graph scrolled so only its right half shows.
        let p = profile(vec![track(
            1,
            LaneClass::Execution,
            vec![action("left", 0, 100), action("right", 100, 100)],
        )]);
        let g = flame(&p);
        // 1000px content, scrolled 600px into a 400px viewport: the window covers
        // content [600, 1000) — the right half. "left" (content [0, 500)) is fully
        // off-screen; "right" (content [500, 1000)) shows from its middle.
        let boxes = lay_out_flame(
            &g,
            1000.0,
            FlamePlacement {
                scroll_x: 600.0,
                view_w: 400.0,
                metrics: &METRICS,
                min_px: 1.0,
            },
        );
        assert!(frame_box(&g, &boxes, "left").is_none());
        let right = frame_box(&g, &boxes, "right").unwrap();
        // Its content x is 500; viewport-relative that is 500 - 600 = -100 (still
        // within the -view_w margin, so unclamped).
        assert_eq!(right.x, -100.0);
        // The root spans the whole content from far left, clamped to the margin.
        assert_eq!(boxes[0].x, -400.0);
    }

    #[test]
    fn height_counts_one_row_per_depth() {
        // Rustc(0..100) contains Action.execute(0..90): root depth 0, Rustc depth
        // 1, Action.execute depth 2 -> 3 rows. Labelled by name, so the depth is
        // the span nesting alone.
        let p = profile(vec![track(
            1,
            LaneClass::Execution,
            vec![
                action("Rustc", 0, 100),
                slice("Action.execute", "general information", 0, 90),
            ],
        )]);
        let g = flame_as(&p, FlameLabels::Name);
        assert_eq!(flame_height(&g, &METRICS), 30.0);
    }

    #[test]
    fn height_counts_the_row_a_detail_frame_adds() {
        // The same two-deep nesting, but the action resolves a mnemonic, so the
        // innermost frame's self time hangs one row deeper under the target's
        // name: 4 rows to the same profile's 3 when nothing is broken out.
        let p = profile(vec![track(
            1,
            LaneClass::Execution,
            vec![
                action_named("Compiling //x:y", "Rustc", 0, 100),
                slice("Action.execute", "general information", 0, 90),
            ],
        )]);
        let g = flame(&p);
        assert_eq!(
            child(&g, &child(&g, &g.root(), "Rustc"), "Action.execute").depth,
            2
        );
        assert_eq!(flame_height(&g, &METRICS), 40.0);
        assert_eq!(
            flame_height(&flame_as(&p, FlameLabels::Name), &METRICS),
            30.0
        );
    }

    #[test]
    fn inversion_roots_each_frame_and_walks_out_to_its_callers() {
        // Rustc(0..100) contains Action.execute(0..90) contains
        // subprocess.run(0..80), so the three frames spend 10, 10 and 80 of
        // their own. Labelled by name, so each frame is one of those spans.
        let p = profile(vec![track(
            1,
            LaneClass::Execution,
            vec![
                action("Rustc", 0, 100),
                slice("Action.execute", "general information", 0, 90),
                slice("subprocess.run", "subprocess run", 0, 80),
            ],
        )]);
        let g = inverted_as(&p, FlameLabels::Name);
        assert_eq!(g.root().total_us, 100);
        assert_eq!(
            child_totals(&g, &g.root()),
            [
                ("Action.execute".to_owned(), 10),
                ("Rustc".to_owned(), 10),
                ("subprocess.run".to_owned(), 80),
            ]
        );
        // The innermost frame's time reads out through both spans that reached it.
        let sub = child(&g, &g.root(), "subprocess.run");
        let via_sub = child(&g, &sub, "Action.execute");
        assert_eq!(via_sub.total_us, 80);
        let outermost = child(&g, &via_sub, "Rustc");
        assert_eq!(outermost.total_us, 80);
        assert!(g.children(&outermost).is_empty());
        // The middle frame's own time reaches back to its one caller, and the
        // outermost frame's own time has no caller below it at all.
        let exec = child(&g, &g.root(), "Action.execute");
        assert_eq!(child_totals(&g, &exec), [("Rustc".to_owned(), 10)]);
        assert!(g.children(&child(&g, &g.root(), "Rustc")).is_empty());
    }

    #[test]
    fn a_frame_nested_in_one_of_its_own_name_branches_off_the_shared_chain() {
        // Compile(0..10) directly contains Compile(0..4), so the caller chains
        // are [Compile] and [Compile, Compile] and the first is a whole prefix
        // of the second: the second must branch off the node the first ends at
        // rather than open a root child beside it.
        let p = profile(vec![track(
            1,
            LaneClass::Execution,
            vec![
                slice("Compile", "general information", 0, 10),
                slice("Compile", "general information", 0, 4),
            ],
        )]);
        let g = inverted_as(&p, FlameLabels::Name);
        assert_eq!(child_totals(&g, &g.root()), [("Compile".to_owned(), 10)]);
        let inner = child(&g, &g.root(), "Compile");
        assert_eq!(child_totals(&g, &inner), [("Compile".to_owned(), 4)]);
        assert!(g.children(&child(&g, &inner, "Compile")).is_empty());
    }

    #[test]
    fn a_frame_reached_two_ways_merges_at_the_root_and_splits_below() {
        // "upload inputs" runs under two different actions; bottom-up it is one
        // frame carrying both, splitting into its two callers one level out.
        let lane = |mnemonic: &str, upload: i64| {
            vec![
                action(mnemonic, 0, 100),
                slice("upload inputs", "general information", 0, upload),
            ]
        };
        let p = profile(vec![
            track(1, LaneClass::Execution, lane("Rustc", 30)),
            track(2, LaneClass::Execution, lane("CppCompile", 40)),
        ]);
        let g = inverted_as(&p, FlameLabels::Name);
        assert_eq!(g.root().total_us, 200);
        let upload = child(&g, &g.root(), "upload inputs");
        assert_eq!(upload.total_us, 70);
        assert_eq!(
            child_totals(&g, &upload),
            [("CppCompile".to_owned(), 40), ("Rustc".to_owned(), 30)]
        );
    }

    #[test]
    fn inversion_carries_over_the_whole_graphs_self_time() {
        let p = profile(vec![
            track(
                1,
                LaneClass::Execution,
                vec![
                    action_named("Compiling //a:a", "Rustc", 0, 100),
                    slice("subprocess.run", "subprocess run", 0, 80),
                    slice("upload inputs", "general information", 0, 30),
                    action_named("Compiling //b:b", "Rustc", 200, 60),
                ],
            ),
            track(
                2,
                LaneClass::Analysis,
                vec![
                    action("CppCompile", 0, 40),
                    slice("checkOutputs", "general information", 40, 25),
                ],
            ),
            // Two enclosed spans that overlap each other, so together they
            // overrun what encloses them.
            track(
                3,
                LaneClass::Execution,
                vec![
                    action_named("Compiling //c:c", "Rustc", 400, 100),
                    slice("checkOutputs", "general information", 400, 80),
                    slice("checkOutputs", "general information", 420, 80),
                ],
            ),
        ]);
        for labels in [FlameLabels::Kind, FlameLabels::Name] {
            let top_down = flame_as(&p, labels);
            let root_total = top_down.root().total_us;
            let self_sum: i64 = top_down.nodes().iter().map(|n| n.self_us).sum();
            let g = invert(top_down);
            assert_eq!(g.root().total_us, self_sum);
            // And the inverted tree partitions it again: every contribution ends
            // at exactly one leaf-most caller.
            assert_eq!(g.nodes().iter().map(|n| n.self_us).sum::<i64>(), self_sum);
            // The overrunning frame's self time floors at zero instead of
            // subtracting the overrun, so that summed self time is more than the
            // top-down root's total and the two roots read differently.
            assert!(
                self_sum > root_total,
                "{labels:?}: self {self_sum} vs root {root_total}"
            );
        }
    }

    #[test]
    fn inverted_target_details_end_the_path_rather_than_starting_it() {
        // A Rustc action on //x:y containing subprocess.run: labelled by kind,
        // both frames break their self time out under the target.
        let p = profile(vec![track(
            1,
            LaneClass::Execution,
            vec![
                action_named("Compiling //x:y", "Rustc", 0, 100),
                slice("subprocess.run", "subprocess run", 0, 80),
            ],
        )]);
        let g = inverted_as(&p, FlameLabels::Kind);
        assert_eq!(
            child_totals(&g, &g.root()),
            [("Rustc".to_owned(), 20), ("subprocess.run".to_owned(), 80)]
        );
        assert!(
            (0..g.child_count(0)).all(|k| !g.is_detail(g.child_id(0, k))),
            "a target detail must never root a bottom-up path"
        );
        // The leaf frame's time reads out through its caller to the target,
        // which ends the path.
        let sub = child(&g, &g.root(), "subprocess.run");
        let rustc = child(&g, &sub, "Rustc");
        assert_eq!(rustc.total_us, 80);
        let target = child(&g, &rustc, "Compiling //x:y");
        assert_eq!(target.total_us, 80);
        assert!(g.children(&target).is_empty());
        // The action frame's own time ends on the same target one level up.
        let own = child(&g, &g.root(), "Rustc");
        assert_eq!(child_totals(&g, &own), [("Compiling //x:y".to_owned(), 20)]);
    }

    #[test]
    fn no_detail_frame_has_children() {
        // Detail frames at two depths: an action span, the subprocess span it
        // contains, and a second action on another lane all break their self
        // time out under their target. The inversion composes a node's chain key
        // from its parent's, which places a detail frame correctly only because
        // no path runs past one.
        let p = profile(vec![
            track(
                1,
                LaneClass::Execution,
                vec![
                    action_named("Compiling //x:y", "Rustc", 0, 100),
                    slice("subprocess.run", "subprocess run", 0, 80),
                    slice("upload inputs", "general information", 0, 30),
                ],
            ),
            track(
                2,
                LaneClass::Analysis,
                vec![
                    action_named("Compiling //a:b", "CppCompile", 0, 60),
                    slice("checkOutputs", "general information", 0, 25),
                ],
            ),
        ]);
        let g = flame_as(&p, FlameLabels::Kind);
        assert!(
            (0..g.len()).filter(|&i| g.is_detail(i)).count() >= 2,
            "the profile must produce detail frames for the claim to bite"
        );
        for i in 0..g.len() {
            assert!(
                !g.is_detail(i) || g.child_count(i) == 0,
                "detail frame {} has children",
                g.str(g.name(i))
            );
        }
    }

    #[test]
    fn inversion_sums_the_span_counts_of_the_frames_a_path_reaches() {
        // Labelled by name, so every frame is a span and no detail frame splits
        // the counts. Lane 1: Rustc(100) contains two subprocess.run spans of 40
        // that merge into one frame of count 2, leaving Rustc 20 of self time.
        // Lane 2: CppCompile(100) contains one subprocess.run of 60, leaving 40.
        let p = profile(vec![
            track(
                1,
                LaneClass::Execution,
                vec![
                    action("Rustc", 0, 100),
                    slice("subprocess.run", "subprocess run", 0, 40),
                    slice("subprocess.run", "subprocess run", 50, 40),
                ],
            ),
            track(
                2,
                LaneClass::Execution,
                vec![
                    action("CppCompile", 0, 100),
                    slice("subprocess.run", "subprocess run", 0, 60),
                ],
            ),
        ]);
        let g = inverted_as(&p, FlameLabels::Name);
        // The synthetic root is no frame's chain step, so it counts no spans
        // however many its children carry.
        assert_eq!(g.root().count, 0);
        let counted = |node: &FlameNode| -> Vec<(String, i64, u32)> {
            let mut kids: Vec<_> = g
                .children(node)
                .iter()
                .map(|c| (g.str(c.name).to_owned(), c.total_us, c.count))
                .collect();
            kids.sort();
            kids
        };
        // The two subprocess.run frames coalesce at the root of the inverted
        // tree, so its count is the three spans they merged, not the two frames.
        assert_eq!(
            counted(&g.root()),
            [
                ("CppCompile".to_owned(), 40, 1),
                ("Rustc".to_owned(), 20, 1),
                ("subprocess.run".to_owned(), 140, 3),
            ]
        );
        // One level out the callers split those spans again.
        let sub = child(&g, &g.root(), "subprocess.run");
        assert_eq!(
            counted(&sub),
            [
                ("CppCompile".to_owned(), 60, 1),
                ("Rustc".to_owned(), 80, 2),
            ]
        );
    }

    #[test]
    fn large_inversion_is_deterministic_across_the_parallel_threshold() {
        // Enough distinct callers that the inversion's `par_sort_by` splits its
        // elements across workers, with one shared callee whose elements form a
        // single run that the split lands inside: the inverted graph must be
        // deterministic and independent of input order.
        const N: i64 = 9000;
        let slices = |reversed: bool| {
            let mut v: Vec<_> = (0..N)
                .flat_map(|i| {
                    let start = i * 100;
                    [
                        action(&format!("M{i:05}"), start, (i % 97) + 3),
                        slice("work", "general information", start, (i % 97) + 1),
                    ]
                })
                .collect();
            if reversed {
                v.reverse();
            }
            v
        };
        let invert_of = |reversed: bool| {
            inverted_as(
                &profile(vec![track(1, LaneClass::Execution, slices(reversed))]),
                FlameLabels::Name,
            )
        };
        let resolved = |g: &FlameGraph| -> Vec<_> {
            g.nodes()
                .iter()
                .map(|n| {
                    (
                        g.str(n.name).to_owned(),
                        n.total_us,
                        n.self_us,
                        n.width_us,
                        n.role,
                        n.count,
                        n.depth,
                        n.children,
                    )
                })
                .collect::<Vec<_>>()
        };
        let a = invert_of(false);
        let b = invert_of(true);
        let c = invert_of(false);
        assert!(
            a.nodes().len() > (1 << 13),
            "graph must cross the parallel threshold"
        );
        assert_eq!(resolved(&a), resolved(&b), "independent of input order");
        assert_eq!(resolved(&a), resolved(&c), "deterministic across builds");
        // A mis-ordered sort would break the run of equal keys apart, so the one
        // shared callee would reach the root as several frames with a fraction of
        // the callers each.
        let work = child(&a, &a.root(), "work");
        assert_eq!(a.children(&work).len(), N as usize);
        assert_eq!(
            a.nodes().iter().filter(|n| a.str(n.name) == "work").count(),
            1
        );
    }
}
