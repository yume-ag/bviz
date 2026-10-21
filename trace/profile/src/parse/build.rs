use std::cmp::{Ordering, Reverse};
use std::collections::{BTreeMap, BinaryHeap, HashMap};

use bviz_invocation::EpochMicros;
use bviz_parallel::worker_count;
use rustc_hash::{FxHashMap, FxHashSet};
use serde_json::{Map, Value};

use crate::concurrent_interner::{ConcurrentInterner, PoolOverflow, Writer};
use crate::model::{
    Category, CategoryId, CounterLane, CounterPoint, CounterSeries, Metadata, OptStr, PhaseMarker,
    Profile, ProfileParts, Slice, SliceDetail, Span32, Str, ThreadTrack, TimeSpan, encloses,
    lane_class, occupied_span, span_role,
};

use super::serde_events::{ArgEntry, ArgVal, RawEvent, arg_i64, arg_str};
use super::{BUILD_ID_KEY, timed};

const MAX_SORT_INDEX: i64 = 1_000_000;
const PHASE_MARKER_CATEGORY: &str = "build phase marker";

/// A [`Writer`] paired with the interner it writes into, so [`finish_merge`]'s
/// late interning and its resolution of existing handles share one writer while
/// the owned interner is sealed by the caller after this is dropped.
struct WriterPool<'a> {
    writer: Writer<'a>,
    interner: &'a ConcurrentInterner,
}

fn round_us(v: Option<f64>) -> i64 {
    v.map(|x| x.round() as i64).unwrap_or(0)
}

/// A counter lane during build: owned series with owned point lists, before
/// flattening into the profile's series/point pools.
struct RawLane {
    name: Str,
    series: Vec<RawSeries>,
}

struct RawSeries {
    name: Str,
    points: Vec<CounterPoint>,
}

/// Accumulates the arena pools as `traceEvents` stream in, interning each
/// event's strings as it is parsed. [`Merge::fold`] folds one or more builders,
/// and [`finish_merge`] seals them and `otherData` into the profile's flat
/// pools.
pub(crate) struct Builder {
    pub(crate) args_pool: Vec<(Str, Str)>,
    thread_meta: BTreeMap<i64, (Option<String>, i64)>,
    /// Slices in event order, each tagged with its thread; accumulating a slice
    /// is one push into pre-grown capacity. [`Builder::seal_slices`] builds
    /// `order` and `lanes` over this buffer without relocating its payloads,
    /// which the builder carries into the merge as [`FoldedRun::payloads`].
    pub(crate) staging: Vec<(i64, Slice, SliceDetail)>,
    /// After [`Builder::seal_slices`]: a permutation of `staging` indices,
    /// contiguous per thread and sorted into lane order within each thread's
    /// range.
    order: Vec<u32>,
    /// After [`Builder::seal_slices`]: each thread's range of `order`, in
    /// first-appearance order.
    lanes: Vec<(i64, Span32)>,
    phase_markers: Vec<PhaseMarker>,
    counter_order: Vec<Str>,
    counters: HashMap<Str, RawLane>,
    /// Every distinct category of the staged slices.
    categories: FxHashSet<Str>,
}

impl Builder {
    pub(crate) fn new() -> Builder {
        Builder {
            args_pool: Vec::new(),
            thread_meta: BTreeMap::new(),
            staging: Vec::new(),
            order: Vec::new(),
            lanes: Vec::new(),
            phase_markers: Vec::new(),
            counter_order: Vec::new(),
            counters: HashMap::new(),
            categories: FxHashSet::default(),
        }
    }

    /// Folds one event in, interning its strings into `interner` — the shared
    /// concurrent arena on the parallel paths, so the handles are global. When
    /// `ev.has_args` is set `scratch` holds the event's args entries; they are
    /// consumed here, and deserializing the next event reuses the buffer.
    pub(crate) fn event(&mut self, interner: &mut Writer, ev: RawEvent, scratch: &[ArgEntry]) {
        let args = ev.has_args.then_some(scratch);
        let ph = ev.ph.as_deref().unwrap_or("");
        match ph {
            "M" => apply_metadata_event(&ev, args, &mut self.thread_meta),
            "X" | "i" => {
                let ts = round_us(ev.ts);
                if ev.cat.as_deref() == Some(PHASE_MARKER_CATEGORY) {
                    self.phase_markers.push(PhaseMarker {
                        name: interner.intern(ev.name.as_deref().unwrap_or("")),
                        start_us: ts,
                    });
                } else {
                    let tid = ev.tid.unwrap_or(0);
                    let category = ev.cat.as_deref().unwrap_or("");
                    let name = ev.name.as_deref().unwrap_or("");
                    let role = span_role(category, name);
                    let name = interner.intern(name);
                    let category = interner.intern(category);
                    self.categories.insert(category);
                    let detail = build_slice_detail(
                        interner,
                        &mut self.args_pool,
                        category,
                        ev.out.as_deref(),
                        args,
                    );
                    self.staging.push((
                        tid,
                        Slice {
                            name,
                            start_us: ts,
                            // A malformed dur must not make the slice end
                            // before it starts: every consumer — the end-max
                            // cull bound, the depth stack, the containment
                            // walk — assumes end >= start.
                            dur_us: round_us(ev.dur).max(0),
                            depth: 0,
                            role,
                            // Numbered by the merge, once every category is
                            // known.
                            category: None,
                        },
                        detail,
                    ));
                }
            }
            "C" => apply_counter_event(
                interner,
                &ev,
                args,
                &mut self.counter_order,
                &mut self.counters,
            ),
            _ => {}
        }
    }

    /// Puts the staged slices into the form [`Merge::fold`] requires of a
    /// folded run, filling `order` and `lanes`.
    ///
    /// `order` is allocated at its exact final size and filled through
    /// per-thread cursors, then sorted one thread's range at a time. The
    /// payloads never move; the merge gathers them through the permutation.
    ///
    /// On the parallel paths this runs on the parse workers, so the per-chunk
    /// work hides under the decode∥parse phase instead of adding to the final
    /// merge.
    pub(crate) fn seal_slices(&mut self) {
        // The lane bookkeeping (counts, spans, cursors) is u32; wrapping it
        // would break the exactly-once scatter argument below.
        assert!(
            u32::try_from(self.staging.len()).is_ok(),
            "chunk slice count exceeds u32"
        );
        let mut lane_of: rustc_hash::FxHashMap<i64, usize> = Default::default();
        // Per lane in first-appearance order: the slice count, becoming the
        // scatter cursor once the spans are laid out.
        let mut counts: Vec<u32> = Vec::new();
        let mut tids: Vec<i64> = Vec::new();
        for &(tid, ..) in &self.staging {
            let lane = *lane_of.entry(tid).or_insert_with(|| {
                tids.push(tid);
                counts.push(0);
                counts.len() - 1
            });
            counts[lane] += 1;
        }
        let mut off = 0u32;
        self.lanes = tids
            .iter()
            .zip(&counts)
            .map(|(&tid, &count)| {
                let span = Span32::new(off, count);
                off += count;
                (tid, span)
            })
            .collect();
        let mut cursors: Vec<u32> = self
            .lanes
            .iter()
            .map(|&(_, s)| s.range().start as u32)
            .collect();
        let total = self.staging.len();
        let mut order: Vec<u32> = Vec::with_capacity(total);
        let spare = order.spare_capacity_mut();
        for (i, &(tid, ..)) in self.staging.iter().enumerate() {
            let lane = lane_of[&tid];
            spare[cursors[lane] as usize].write(i as u32);
            cursors[lane] += 1;
        }
        // Every slot in [0, total) was written exactly once: the cursors
        // advance through disjoint per-lane ranges whose lengths sum to the
        // staging count.
        unsafe { order.set_len(total) };
        let payloads = &self.staging;
        for &(_, span) in &self.lanes {
            order[span.range()]
                .sort_by(|&a, &b| slice_order(&payloads[a as usize].1, &payloads[b as usize].1));
        }
        self.order = order;
    }
}

/// Lane order for a thread's slices: by start ascending, then longer duration
/// first, so an enclosing span sorts before the spans it contains. Equal keys
/// keep their relative order under a stable sort, which the merge's run-index
/// tie-break preserves across builders.
fn slice_order(a: &Slice, b: &Slice) -> Ordering {
    a.start_us.cmp(&b.start_us).then(b.dur_us.cmp(&a.dur_us))
}

/// The incremental builder merge: [`Merge::fold`] absorbs one builder at a
/// time — in array order — so everything except the final slice copy happens
/// as builders arrive, and the pipelined parse hides that work under the
/// decode∥parse phase. Every builder's handles are already global (all chunk
/// workers interned into one shared arena), so a fold only concatenates the
/// builder's per-worker args pool — rebasing the arg-pool indices its slice
/// details carry — unions its counters by their (global) name handle, and
/// unions thread metadata per tid; slice, phase-marker, and counter/series
/// handles carry straight through. Only the builder's slices survive a fold
/// (as a [`FoldedRun`] with the arg-pool base its detail indices rebase by):
/// their copy into the flat pool needs every thread's total, so it waits for
/// [`finish_merge`].
pub(crate) struct Merge {
    args_pool: Vec<(Str, Str)>,
    phase_markers: Vec<PhaseMarker>,
    thread_meta: BTreeMap<i64, (Option<String>, i64)>,
    counter_order: Vec<Str>,
    counters: HashMap<Str, RawLane>,
    runs: Vec<FoldedRun>,
    categories: FxHashSet<Str>,
}

struct FoldedRun {
    args_base: u32,
    /// Slice payloads in event order, moved from the builder's `staging`.
    payloads: Vec<(i64, Slice, SliceDetail)>,
    /// Per-lane sorted indices into `payloads`.
    order: Vec<u32>,
    /// Each thread's range of `order`, in first-appearance order.
    lanes: Vec<(i64, Span32)>,
}

impl Merge {
    pub(crate) fn new() -> Merge {
        Merge {
            args_pool: Vec::new(),
            phase_markers: Vec::new(),
            thread_meta: BTreeMap::new(),
            counter_order: Vec::new(),
            counters: HashMap::new(),
            runs: Vec::new(),
            categories: FxHashSet::default(),
        }
    }

    pub(crate) fn with_capacity(runs: usize, args: usize) -> Merge {
        Merge {
            args_pool: Vec::with_capacity(args),
            phase_markers: Vec::new(),
            thread_meta: BTreeMap::new(),
            counter_order: Vec::new(),
            counters: HashMap::new(),
            runs: Vec::with_capacity(runs),
            categories: FxHashSet::default(),
        }
    }

    /// Absorbs one builder. The builder's slices must already be sealed into
    /// per-thread lane order ([`Builder::seal_slices`]): [`finish_merge`]
    /// k-way merges the folded runs rather than re-sorting whole lanes.
    pub(crate) fn fold(&mut self, builder: Builder) {
        let Builder {
            args_pool,
            thread_meta,
            staging,
            order,
            lanes,
            phase_markers,
            counter_order,
            mut counters,
            categories,
        } = builder;
        self.categories.extend(categories);
        // The args pool is per-worker, concatenated here; a detail's `others`
        // span indexes it, so it rebases by the base this builder lands at. The
        // entries themselves are global text handles, unchanged.
        let args_base = self.args_pool.len() as u32;
        self.args_pool.extend(args_pool);
        self.phase_markers.extend(phase_markers);
        // Per thread: the first folded name wins, the last explicit sort index wins.
        for (tid, (name, sort_index)) in thread_meta {
            let entry = self
                .thread_meta
                .entry(tid)
                .or_insert((None, MAX_SORT_INDEX));
            if entry.0.is_none() {
                entry.0 = name;
            }
            if sort_index != MAX_SORT_INDEX {
                entry.1 = sort_index;
            }
        }
        // A counter name is a global handle, so a lane sampled across chunks
        // keys the same map entry in every builder and unifies without a
        // re-intern.
        for lane_key in counter_order {
            let lane = counters.remove(&lane_key).expect("ordered lane exists");
            let dst = match self.counters.entry(lane.name) {
                std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                std::collections::hash_map::Entry::Vacant(e) => {
                    self.counter_order.push(lane.name);
                    e.insert(RawLane {
                        name: lane.name,
                        series: Vec::new(),
                    })
                }
            };
            for series in lane.series {
                match dst.series.iter_mut().find(|s| s.name == series.name) {
                    Some(s) => s.points.extend(series.points),
                    None => dst.series.push(series),
                }
            }
        }
        self.runs.push(FoldedRun {
            args_base,
            payloads: staging,
            order,
            lanes,
        });
    }
}

/// Seals a folded [`Merge`] whose handles are global into the shared interner:
/// interns the late strings ([`finish_merge`]'s thread names, counter labels,
/// and metadata) through one writer into that same arena, then consumes the
/// interner into the profile's text pool.
pub(crate) fn seal(
    interner: ConcurrentInterner,
    merge: Merge,
    other_data: Option<Map<String, Value>>,
) -> Result<Profile, PoolOverflow> {
    let mut parts = {
        let mut pool = WriterPool {
            writer: interner.writer(),
            interner: &interner,
        };
        finish_merge(merge, &mut pool, other_data)
    };
    parts.distinct_strings = interner.distinct();
    parts.text = interner.into_text()?;
    Ok(Profile::from_parts(parts))
}

/// Writes every folded slice exactly once, straight into its final
/// `slice_pool` position: each thread's sorted runs are k-way merged into lane
/// order, with handle rebasing, depth assignment, and span tracking fused into
/// that single write pass, which runs per thread across worker threads. Then
/// seals the merged pools into a [`Profile`].
fn finish_merge(
    merge: Merge,
    pool: &mut WriterPool,
    other_data: Option<Map<String, Value>>,
) -> ProfileParts {
    let Merge {
        args_pool,
        phase_markers,
        thread_meta,
        counter_order,
        counters,
        runs,
        categories,
    } = merge;
    let categories = Categories::numbering(categories, pool.interner);
    let slice_total: usize = runs.iter().map(|r| r.order.len()).sum();
    let mut slice_pools = SlicePools::with_capacity(slice_total);
    let (segments, mut tasks) = carve_segments(
        &mut slice_pools,
        group_runs_by_tid(&runs),
        slice_total,
        &categories,
    );
    merge_lanes(&mut tasks);
    debug_assert_eq!(
        tasks.iter().map(|t| t.emitted).sum::<usize>(),
        slice_total,
        "the merges filled every carved slot"
    );
    let results: Vec<(u32, i64, i64)> = tasks
        .into_iter()
        .map(|t| (t.max_depth, t.min_start_us, t.max_occupied_us))
        .collect();
    // SAFETY: the segments partition the first `slice_total` slots of each
    // pool's spare capacity, each merge wrote its whole segment in all three
    // pools (the runs' lengths sum to the segment length, and every emit
    // writes one slot of each), and the merge scope joined all their writers.
    unsafe { slice_pools.slice_pool.set_len(slice_total) };
    unsafe { slice_pools.detail_pool.set_len(slice_total) };
    unsafe { slice_pools.end_max_pool.set_len(slice_total) };

    let parts = build_parts(
        slice_pools,
        ThreadMerge {
            segments: &segments,
            results: &results,
        },
        categories.ordered,
        args_pool,
        phase_markers,
        thread_meta,
        counter_order,
        counters,
        pool,
        other_data,
    );

    // The folded runs' teardown (per-chunk payload and index buffers) is pure
    // page-return work; where the build has threads, detached so the load
    // doesn't wait on it.
    #[cfg(any(not(target_family = "wasm"), target_feature = "atomics"))]
    bviz_parallel::spawn(move || drop(runs));
    parts
}

/// A profile's categories in the order of their text, which numbers them the
/// same however the events were split among builders.
struct Categories {
    ordered: Vec<Str>,
    ids: FxHashMap<Str, Option<CategoryId>>,
}

impl Categories {
    fn numbering(distinct: FxHashSet<Str>, interner: &ConcurrentInterner) -> Categories {
        let mut ordered: Vec<Str> = distinct.into_iter().collect();
        ordered.sort_unstable_by_key(|&category| interner.resolve(category));
        let ids = ordered
            .iter()
            .enumerate()
            .map(|(position, &category)| (category, CategoryId::at(position)))
            .collect();
        Categories { ordered, ids }
    }
}

struct SlicePools {
    slice_pool: Vec<Slice>,
    detail_pool: Vec<SliceDetail>,
    end_max_pool: Vec<i64>,
}

impl SlicePools {
    /// Allocates room for `slices` slices, left as uninitialized capacity for
    /// the merge phase, so a slot's page is first touched by the worker that
    /// merges it.
    fn with_capacity(slices: usize) -> SlicePools {
        SlicePools {
            slice_pool: Vec::with_capacity(slices),
            detail_pool: Vec::with_capacity(slices),
            end_max_pool: Vec::with_capacity(slices),
        }
    }
}

/// One source run per (thread, builder): the thread's lane-ordered indices
/// into that builder's event-order payloads, with the arg-pool base its
/// detail spans rebase by.
struct Run<'a> {
    payloads: &'a [(i64, Slice, SliceDetail)],
    order: &'a [u32],
    args_base: u32,
}

/// One thread's share of the merge phase. Its destination segments cover the
/// same index range in each pool and start uninitialized; the runs' lengths
/// sum to exactly that range, so the merge writes every slot of each exactly
/// once.
struct TidTask<'a> {
    dest: &'a mut [std::mem::MaybeUninit<Slice>],
    dest_detail: &'a mut [std::mem::MaybeUninit<SliceDetail>],
    dest_end_max: &'a mut [std::mem::MaybeUninit<i64>],
    runs: Vec<Run<'a>>,
    categories: &'a Categories,
    max_depth: u32,
    min_start_us: i64,
    /// The furthest any of the lane's slices reaches by
    /// [`occupied_span`], which is the lane's contribution to
    /// [`compute_span`].
    max_occupied_us: i64,
    /// Slots the merge has written, each one slot of every destination.
    emitted: usize,
}

/// The runs feeding each thread's merge, in fold order within a thread.
fn group_runs_by_tid(runs: &[FoldedRun]) -> BTreeMap<i64, Vec<Run<'_>>> {
    let mut tid_runs: BTreeMap<i64, Vec<Run>> = BTreeMap::new();
    for folded in runs {
        for &(tid, span) in &folded.lanes {
            tid_runs.entry(tid).or_default().push(Run {
                payloads: &folded.payloads,
                order: &folded.order[span.range()],
                args_base: folded.args_base,
            });
        }
    }
    tid_runs
}

/// Carves each thread a destination segment out of the pools' spare capacity,
/// sized to its runs and paired with them into a merge task. The returned
/// spans locate the threads in the sealed pools.
fn carve_segments<'a>(
    pools: &'a mut SlicePools,
    tid_runs: BTreeMap<i64, Vec<Run<'a>>>,
    slice_total: usize,
    categories: &'a Categories,
) -> (Vec<(i64, Span32)>, Vec<TidTask<'a>>) {
    let mut segments: Vec<(i64, Span32)> = Vec::with_capacity(tid_runs.len());
    let mut tasks: Vec<TidTask> = Vec::with_capacity(tid_runs.len());
    let mut rest = pools.slice_pool.spare_capacity_mut();
    let mut rest_detail = pools.detail_pool.spare_capacity_mut();
    let mut rest_end_max = pools.end_max_pool.spare_capacity_mut();
    let mut off = 0u32;
    for (tid, runs) in tid_runs {
        let len: usize = runs.iter().map(|r| r.order.len()).sum();
        let (dest, tail) = std::mem::take(&mut rest).split_at_mut(len);
        rest = tail;
        let (dest_detail, tail) = std::mem::take(&mut rest_detail).split_at_mut(len);
        rest_detail = tail;
        let (dest_end_max, tail) = std::mem::take(&mut rest_end_max).split_at_mut(len);
        rest_end_max = tail;
        segments.push((tid, Span32::new(off, len as u32)));
        off += len as u32;
        tasks.push(TidTask {
            dest,
            dest_detail,
            dest_end_max,
            runs,
            categories,
            max_depth: 0,
            min_start_us: i64::MAX,
            max_occupied_us: i64::MIN,
            emitted: 0,
        });
    }
    debug_assert_eq!(off as usize, slice_total);
    (segments, tasks)
}

fn merge_lanes(tasks: &mut [TidTask]) {
    timed("    slices", || {
        // Thread sizes are heavily skewed toward a few big evaluator lanes, so
        // workers steal tasks one thread at a time.
        let workers = worker_count().min(tasks.len());
        if workers <= 1 {
            tasks.iter_mut().for_each(merge_lane);
            return;
        }
        let stealer = std::sync::Mutex::new(tasks.iter_mut());
        bviz_parallel::fan_out(workers, |_| {
            loop {
                let Some(task) = stealer.lock().unwrap().next() else {
                    break;
                };
                merge_lane(task);
            }
        });
    });
}

/// K-way merges a thread's runs into its destination segments, in whole-lane
/// [`slice_order`]; an equal key takes the lowest run index, which reproduces a
/// stable sort of the runs concatenated in fold order.
///
/// Filling a slot carries the rest of the per-slice work in the same pass: it
/// rebases the detail's arg-pool span onto its run's base, numbers the slice's
/// category, assigns the depth the ancestor stack gives, folds the slice into
/// the lane's time extent, and hands the end-max slot the running end its
/// slices have reached, feeding [`Profile::end_maxes`].
///
/// Slice, detail, and end max travel as one triple, so the three pools stay
/// index-parallel by construction.
fn merge_lane(task: &mut TidTask) {
    let TidTask {
        dest,
        dest_detail,
        dest_end_max,
        runs,
        categories,
        max_depth,
        min_start_us,
        max_occupied_us,
        emitted,
    } = task;
    let mut slots = dest.iter_mut();
    let mut detail_slots = dest_detail.iter_mut();
    let mut end_max_slots = dest_end_max.iter_mut();
    // End times of the ancestors still open.
    let mut stack: Vec<i64> = Vec::new();
    let mut end_max_us = i64::MIN;
    // A lane runs many slices of one category in a row, each but the first
    // numbered without hashing it.
    let mut numbered: Option<(Str, Option<CategoryId>)> = None;
    let mut emit = |run: &Run, src: &(i64, Slice, SliceDetail)| {
        let mut s = src.1;
        let mut d = src.2;
        d.others = d.others.shifted(run.args_base);
        s.category = match numbered {
            Some((category, id)) if category == d.category => id,
            _ => {
                let id = categories.ids[&d.category];
                numbered = Some((d.category, id));
                id
            }
        };
        let end = s.end_us();
        while stack
            .last()
            .is_some_and(|&open| !encloses(open, s.start_us, end))
        {
            stack.pop();
        }
        s.depth = stack.len() as u32;
        *max_depth = (*max_depth).max(s.depth + 1);
        stack.push(end);
        *min_start_us = (*min_start_us).min(s.start_us);
        *max_occupied_us = (*max_occupied_us).max(occupied_span(s.start_us, s.dur_us).end_us);
        end_max_us = end_max_us.max(end);
        slots
            .next()
            .expect("segment sized to the runs' total")
            .write(s);
        detail_slots
            .next()
            .expect("segment sized to the runs' total")
            .write(d);
        end_max_slots
            .next()
            .expect("segment sized to the runs' total")
            .write(end_max_us);
        *emitted += 1;
    };
    if let [run] = &runs[..] {
        for &idx in run.order {
            emit(run, &run.payloads[idx as usize]);
        }
    } else {
        let mut cursors = vec![0usize; runs.len()];
        let mut heap: BinaryHeap<Reverse<(i64, Reverse<i64>, usize)>> =
            BinaryHeap::with_capacity(runs.len());
        for (i, run) in runs.iter().enumerate() {
            if let Some(&idx) = run.order.first() {
                let s = &run.payloads[idx as usize].1;
                heap.push(Reverse((s.start_us, Reverse(s.dur_us), i)));
            }
        }
        while let Some(Reverse((.., i))) = heap.pop() {
            let run = &runs[i];
            let idx = run.order[cursors[i]] as usize;
            cursors[i] += 1;
            emit(run, &run.payloads[idx]);
            if let Some(&next) = run.order.get(cursors[i]) {
                let s = &run.payloads[next as usize].1;
                heap.push(Reverse((s.start_us, Reverse(s.dur_us), i)));
            }
        }
    }
}

/// Assembles the merged slice pools and the folded counters, phase markers,
/// and thread metadata into the profile's flat parts. `text` and
/// `distinct_strings` are filled by [`seal`].
#[allow(clippy::too_many_arguments)]
fn build_parts(
    pools: SlicePools,
    merged: ThreadMerge,
    categories: Vec<Str>,
    args_pool: Vec<(Str, Str)>,
    phase_markers: Vec<PhaseMarker>,
    thread_meta: BTreeMap<i64, (Option<String>, i64)>,
    counter_order: Vec<Str>,
    counters: HashMap<Str, RawLane>,
    pool: &mut WriterPool,
    other_data: Option<Map<String, Value>>,
) -> ProfileParts {
    timed("    tail", || {
        let SlicePools {
            slice_pool,
            detail_pool,
            end_max_pool,
        } = pools;
        let metadata = build_metadata(&mut pool.writer, other_data.as_ref());
        let span = compute_span(merged, &counters, &phase_markers);
        let threads = build_thread_tracks(merged, &thread_meta, &mut pool.writer);
        let (series_pool, point_pool, counters) =
            build_counter_lanes(counter_order, counters, pool);
        let categories = categories
            .into_iter()
            .map(|name| {
                let text = pool.interner.resolve(name);
                Category::new(name, text, |kind| pool.writer.intern(kind))
            })
            .collect();

        ProfileParts {
            text: String::new(),
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
            distinct_strings: 0,
            categories,
        }
    })
}

/// The per-thread merge output: `segments[i]` (thread id and its slice span)
/// paired index-wise with `results[i]` (max depth, min start, max occupied
/// end).
#[derive(Clone, Copy)]
struct ThreadMerge<'a> {
    segments: &'a [(i64, Span32)],
    results: &'a [(u32, i64, i64)],
}

fn compute_span(
    merge: ThreadMerge,
    counters: &HashMap<Str, RawLane>,
    phase_markers: &[PhaseMarker],
) -> TimeSpan {
    let ThreadMerge { segments, results } = merge;
    let mut start: Option<i64> = None;
    let mut end: Option<i64> = None;
    let mut note = |occupied: TimeSpan| {
        start = Some(start.map_or(occupied.start_us, |cur| cur.min(occupied.start_us)));
        end = Some(end.map_or(occupied.end_us, |cur| cur.max(occupied.end_us)));
    };
    for (&(_, span), &(_, min_start, max_occupied)) in segments.iter().zip(results) {
        if !span.is_empty() {
            note(TimeSpan {
                start_us: min_start,
                end_us: max_occupied,
            });
        }
    }
    for lane in counters.values() {
        for series in &lane.series {
            for p in &series.points {
                note(occupied_span(p.ts_us, 0));
            }
        }
    }
    for m in phase_markers {
        note(occupied_span(m.start_us, 0));
    }
    TimeSpan {
        start_us: start.unwrap_or(0),
        end_us: end.unwrap_or(0),
    }
}

/// One [`ThreadTrack`] per merged thread segment, named from `thread_meta`,
/// falling back to `thread {tid}` when the trace named no thread.
fn build_thread_tracks(
    merge: ThreadMerge,
    thread_meta: &BTreeMap<i64, (Option<String>, i64)>,
    writer: &mut Writer,
) -> Vec<ThreadTrack> {
    let ThreadMerge { segments, results } = merge;
    let mut threads: Vec<ThreadTrack> = segments
        .iter()
        .zip(results)
        .map(|(&(tid, slices), &(max_depth, ..))| {
            let (name, sort_index) = match thread_meta.get(&tid) {
                Some((Some(n), s)) => ((*n).to_owned(), *s),
                Some((None, s)) => (format!("thread {tid}"), *s),
                None => (format!("thread {tid}"), MAX_SORT_INDEX),
            };
            let lane_class = lane_class(tid, &name);
            ThreadTrack {
                tid,
                name: writer.intern(&name),
                sort_index,
                slices,
                max_depth,
                lane_class,
            }
        })
        .collect();
    threads.sort_by_key(|t| (t.sort_index, t.tid));
    threads
}

/// Splits each counter into one lane per numeric series: overlaid series are
/// hard to read, so each becomes its own graph, labelled with its arg key when
/// a counter carries more than one. A counter that carried no numeric
/// series (e.g. worker memory on a build with no persistent workers) keeps an
/// empty lane so its absence stays visible rather than being silently dropped.
/// Returns the flattened `(series_pool, point_pool, lanes)` pools that
/// [`CounterLane`] and [`CounterSeries`] index into.
fn build_counter_lanes(
    counter_order: Vec<Str>,
    mut counters: HashMap<Str, RawLane>,
    pool: &mut WriterPool,
) -> (Vec<CounterSeries>, Vec<CounterPoint>, Vec<CounterLane>) {
    struct SplitLane {
        label: String,
        rank: (u8, u8),
        series: Option<RawSeries>,
    }
    let mut split: Vec<SplitLane> = Vec::new();
    for name in counter_order {
        let Some(lane) = counters.remove(&name) else {
            continue;
        };
        let counter_name = pool.interner.resolve(lane.name).to_string();
        if lane.series.is_empty() {
            let rank = counter_rank(&counter_name, "");
            split.push(SplitLane {
                label: counter_name,
                rank,
                series: None,
            });
            continue;
        }
        let multi = lane.series.len() > 1;
        for s in lane.series {
            let key = pool.interner.resolve(s.name).to_string();
            let rank = counter_rank(&counter_name, &key);
            let label = if multi {
                format!("{counter_name} · {key}")
            } else {
                counter_name.clone()
            };
            split.push(SplitLane {
                label,
                rank,
                series: Some(s),
            });
        }
    }
    split.sort_by_key(|l| l.rank);

    let split_series = || split.iter().filter_map(|l| l.series.as_ref());
    let mut series_pool: Vec<CounterSeries> = Vec::with_capacity(split_series().count());
    let mut point_pool: Vec<CounterPoint> =
        Vec::with_capacity(split_series().map(|s| s.points.len()).sum());
    let counters: Vec<CounterLane> = split
        .into_iter()
        .map(|l| {
            let name = pool.writer.intern(&l.label);
            let series_off = series_pool.len() as u32;
            if let Some(mut s) = l.series {
                let points_off = point_pool.len() as u32;
                let points_len = s.points.len() as u32;
                // Consumers binary-search points by time, so the sealed series
                // must be sorted; the trace format does not promise that counter
                // samples arrive in time order.
                if !s.points.is_sorted_by_key(|p| p.ts_us) {
                    s.points.sort_by_key(|p| p.ts_us);
                }
                let max_value = s
                    .points
                    .iter()
                    .map(|p| p.value)
                    .fold(f64::NEG_INFINITY, f64::max);
                point_pool.extend(s.points);
                series_pool.push(CounterSeries {
                    name: s.name,
                    points: Span32::new(points_off, points_len),
                    max_value,
                });
            }
            let series_len = series_pool.len() as u32 - series_off;
            CounterLane {
                name,
                series: Span32::new(series_off, series_len),
            }
        })
        .collect();
    (series_pool, point_pool, counters)
}

fn build_metadata(interner: &mut Writer, other: Option<&Map<String, Value>>) -> Metadata {
    let value = |k: &str| other.and_then(|m| m.get(k));
    let mut get = |k: &str| -> OptStr {
        let v = value(k).and_then(Value::as_str);
        v.map(|v| interner.intern(v)).into()
    };
    Metadata {
        bazel_version: get("bazel_version"),
        build_id: get(BUILD_ID_KEY),
        output_base: get("output_base"),
        date: get("date"),
        epoch_origin: value("profile_start_ts")
            .and_then(Value::as_i64)
            .map(EpochMicros::from_millis),
    }
}

fn apply_metadata_event(
    ev: &RawEvent,
    args: Option<&[ArgEntry]>,
    thread_meta: &mut BTreeMap<i64, (Option<String>, i64)>,
) {
    let tid = ev.tid.unwrap_or(0);
    let entry = thread_meta.entry(tid).or_insert((None, MAX_SORT_INDEX));
    match ev.name.as_deref() {
        Some("thread_name") => {
            if let Some(name) = args.and_then(|a| arg_str(a, "name")) {
                entry.0 = Some(name.to_owned());
            }
        }
        Some("thread_sort_index") => {
            if let Some(idx) = args.and_then(|a| arg_i64(a, "sort_index")) {
                entry.1 = idx;
            }
        }
        _ => {}
    }
}

fn build_slice_detail(
    interner: &mut Writer,
    args_pool: &mut Vec<(Str, Str)>,
    category: Str,
    primary_output: Option<&str>,
    args: Option<&[ArgEntry]>,
) -> SliceDetail {
    let mut out = SliceDetail {
        category,
        out: primary_output.map(|s| interner.intern(s)).into(),
        ..SliceDetail::default()
    };
    let Some(args) = args else {
        return out;
    };
    let others_off = args_pool.len() as u32;
    for (k, v) in args {
        // The typed fields are strings; a non-string value under one of those
        // keys is dropped, everything else is kept for display.
        match k.as_ref() {
            "target" => {
                if let ArgVal::Str(s) = v {
                    out.target = Some(interner.intern(s)).into();
                }
            }
            "mnemonic" => {
                if let ArgVal::Str(s) = v {
                    out.mnemonic = Some(interner.intern(s)).into();
                }
            }
            "configuration" => {
                if let ArgVal::Str(s) = v {
                    out.configuration = Some(interner.intern(s)).into();
                }
            }
            _ => {
                let key = interner.intern(k);
                // A string value interns straight from the borrowed input; a
                // numeric one formats to its display string first.
                let val = match v {
                    ArgVal::Str(s) => interner.intern(s),
                    ArgVal::Int(n) => interner.intern(&n.to_string()),
                    ArgVal::Float(n) => interner.intern(&n.to_string()),
                };
                args_pool.push((key, val));
            }
        }
    }
    out.others = Span32::new(others_off, args_pool.len() as u32 - others_off);
    out
}

fn apply_counter_event(
    interner: &mut Writer,
    ev: &RawEvent,
    args: Option<&[ArgEntry]>,
    order: &mut Vec<Str>,
    counters: &mut HashMap<Str, RawLane>,
) {
    let lane_name = interner.intern(ev.name.as_deref().unwrap_or(""));
    let ts = round_us(ev.ts);
    let Some(args) = args else {
        return;
    };
    let lane = counters.entry(lane_name).or_insert_with(|| {
        order.push(lane_name);
        RawLane {
            name: lane_name,
            series: Vec::new(),
        }
    });
    for (key, value) in args {
        let Some(value) = value.as_f64() else {
            continue;
        };
        let key = interner.intern(key);
        let series = match lane.series.iter_mut().find(|s| s.name == key) {
            Some(s) => s,
            None => {
                lane.series.push(RawSeries {
                    name: key,
                    points: Vec::new(),
                });
                lane.series.last_mut().unwrap()
            }
        };
        series.points.push(CounterPoint { ts_us: ts, value });
    }
}

/// Sort key `(cluster, within)` placing a counter series in the order
/// [`Profile::counters`] states, from the counter event's `name` and its arg
/// key `series`. Every unrecognised counter ranks alike, so the stable sort
/// leaves those in first-seen order.
fn counter_rank(name: &str, series: &str) -> (u8, u8) {
    let name = name.to_lowercase();
    let series = series.to_lowercase();
    if name.contains("action count") {
        // Total actions, then local execution, then local cache.
        let within = if series == "action" {
            0
        } else if series.contains("cache") {
            2
        } else {
            1
        };
        (0, within)
    } else if name.contains("load average") {
        (1, 0)
    } else if name.contains("cpu usage") {
        (2, if name.contains("total") { 0 } else { 1 })
    } else if name.contains("memory") {
        // System total, then Bazel, then worker memory.
        let within = if name.contains("worker") {
            2
        } else if name.contains("total") {
            0
        } else {
            1
        };
        (3, within)
    } else if name.contains("network") {
        (4, if name.contains("down") { 0 } else { 1 })
    } else {
        (5, 0)
    }
}
