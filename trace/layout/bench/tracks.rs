//! Benchmarks the UI's real flame-graph path: `ScopeAggregate::build_for_tracks`
//! is rebuilt from scratch on every track-selection change. Times a single-track
//! selection (the largest lane, walked serially by one worker) and the all-track
//! selection (walked in parallel), the two ends the interactive profile exercised,
//! plus the all-track rebuild from a reused whole-profile scan (`TrackFlamer`),
//! which is what the view does for every selection after the first, in both the
//! top-down and the bottom-up presentation and under either weight.

use std::sync::Arc;

use bviz_trace_layout::{
    FlameDirection, FlameMode, FlameWeight, ScopeAggregate, TimeSelection, TrackFlamer,
    span_between,
};
use bviz_trace_profile::{LaneClass, Profile, TimeSpan};

#[global_allocator]
static GLOBAL: bviz_alloc::MiMalloc = bviz_alloc::MiMalloc;

fn min_secs(iters: usize, mut f: impl FnMut()) -> f64 {
    (0..iters)
        .map(|_| {
            let t = bviz_time::now();
            f();
            t.elapsed().as_secs_f64()
        })
        .fold(f64::INFINITY, f64::min)
}

/// A selection of `parts` equal regions spread evenly over `span`, together
/// covering `coverage` of it, each centred in its own even share. One part is
/// the single stretch a drag selects; several stand for regions far enough apart
/// that their bounds reach across the profile. Expressed as a fraction so a case
/// means the same thing on any profile.
fn spread(span: TimeSpan, coverage: f64, parts: usize) -> TimeSelection {
    let stride = (span.end_us - span.start_us) as f64 / parts as f64;
    let width = stride * coverage;
    let mut sel = TimeSelection::default();
    for k in 0..parts {
        let start = span.start_us as f64 + k as f64 * stride + (stride - width) / 2.0;
        let region = span_between(start as i64, (start + width) as i64);
        sel.insert(region.expect("a region of non-zero width"));
    }
    sel
}

/// The same selection with its last region ending a microsecond earlier: all but
/// an instant of the same time, under a key of its own. Building alternately
/// against `sel` and this misses the per-window memo every time, so each build
/// pays a reduction.
fn evicting(sel: &TimeSelection) -> TimeSelection {
    let mut out = TimeSelection::default();
    for (i, region) in sel.spans().iter().enumerate() {
        let shorten = (i + 1 == sel.len()) as i64;
        let region = span_between(region.start_us, region.end_us - shorten);
        out.insert(region.expect("a region of non-zero width"));
    }
    out
}

fn main() {
    // SAFETY: the first statement of main, before any other thread starts.
    unsafe { bviz_alloc::retain_freed_memory_across_load() };
    let path = std::env::args()
        .nth(1)
        .expect("usage: bench_tracks <profile>");
    let iters = std::env::var("BVIZ_BENCH_ITERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5);

    let profile = Arc::new(
        Profile::from_path(std::path::Path::new(&path))
            .expect("load profile")
            .into_inner(),
    );
    let threads = profile.threads();

    // The largest non-critical-path lane by slice count — a single-track
    // selection lands its whole merge on one worker.
    let (biggest, big_slices) = threads
        .iter()
        .enumerate()
        .filter(|(_, t)| t.lane_class != LaneClass::CriticalPath)
        .map(|(i, t)| (i, profile.slices(t).len()))
        .max_by_key(|&(_, n)| n)
        .expect("a real lane");
    let real_lanes = threads
        .iter()
        .filter(|t| t.lane_class != LaneClass::CriticalPath)
        .count();
    let total_slices: usize = threads
        .iter()
        .filter(|t| t.lane_class != LaneClass::CriticalPath)
        .map(|t| profile.slices(t).len())
        .sum();

    // The presentation the view opens on, over the whole timeline.
    let mode = FlameMode::default();
    let window = TimeSelection::default();
    let single = min_secs(iters, || {
        std::hint::black_box(ScopeAggregate::build_for_tracks(
            &profile,
            mode,
            &window,
            |i| i == biggest,
        ));
    });
    let all = min_secs(iters, || {
        std::hint::black_box(ScopeAggregate::build_for_tracks(
            &profile,
            mode,
            &window,
            |i| threads[i].lane_class != LaneClass::CriticalPath,
        ));
    });
    // The whole-profile scan the view holds after the first build; every later
    // selection rebuilds from it rather than rescanning.
    let scan_cold = min_secs(iters, || {
        std::hint::black_box(TrackFlamer::scanned(&profile));
    });
    // The first rebuild off a scan pays that labelling's per-lane reduction, so
    // this timing is warm: the pure merge every selection change after it costs.
    let scan = TrackFlamer::new(&profile);
    let all_reused = min_secs(iters, || {
        std::hint::black_box(ScopeAggregate::build_for_tracks_reusing(
            &scan,
            mode,
            &window,
            |i| threads[i].lane_class != LaneClass::CriticalPath,
        ));
    });

    // The same rebuild presented bottom-up, so the gap against `all_reused` is
    // what inverting a built graph costs.
    let all_inverted = min_secs(iters, || {
        std::hint::black_box(ScopeAggregate::build_for_tracks_reusing(
            &scan,
            FlameMode {
                direction: FlameDirection::BottomUp,
                ..mode
            },
            &window,
            |i| threads[i].lane_class != LaneClass::CriticalPath,
        ));
    });

    // The first wall-share build off a scan that has only weighed durations is
    // timed apart from the ones after it, which merge what it left held.
    let wall_share = FlameMode {
        weight: FlameWeight::WallShare,
        ..mode
    };
    let first = bviz_time::now();
    let wall_agg = ScopeAggregate::build_for_tracks_reusing(&scan, wall_share, &window, |i| {
        threads[i].lane_class != LaneClass::CriticalPath
    });
    let wall_first = first.elapsed().as_secs_f64();
    let wall_reused = min_secs(iters, || {
        std::hint::black_box(ScopeAggregate::build_for_tracks_reusing(
            &scan,
            wall_share,
            &window,
            |i| threads[i].lane_class != LaneClass::CriticalPath,
        ));
    });

    let bottom_up_agg = ScopeAggregate::build_for_tracks_reusing(
        &scan,
        FlameMode {
            direction: FlameDirection::BottomUp,
            ..mode
        },
        &window,
        |i| threads[i].lane_class != LaneClass::CriticalPath,
    );
    let single_agg = ScopeAggregate::build_for_tracks(&profile, mode, &window, |i| i == biggest);
    let all_agg = ScopeAggregate::build_for_tracks(&profile, mode, &window, |i| {
        threads[i].lane_class != LaneClass::CriticalPath
    });
    eprintln!(
        "[bench] lanes: {real_lanes} real, {total_slices} slices; \
         biggest lane #{biggest} = {big_slices} slices"
    );
    eprintln!(
        "[bench] build_for_tracks(1 biggest): {single:.3}s  \
         build_for_tracks(all {real_lanes}): {all:.3}s  (min of {iters})"
    );
    eprintln!(
        "[bench] scan once: {scan_cold:.3}s  \
         build_for_tracks_reusing(all): {all_reused:.3}s  \
         (rebuild saves {:.3}s vs one-shot {all:.3}s)",
        all - all_reused,
    );
    eprintln!(
        "[bench] build_for_tracks_reusing(all, bottom-up): {all_inverted:.3}s  \
         (inversion adds {:.3}s)",
        all_inverted - all_reused,
    );
    eprintln!(
        "[bench] build_for_tracks_reusing(all, wall share): first {wall_first:.3}s  \
         then {wall_reused:.3}s  (root {:.3}s of {:.3}s summed duration)",
        wall_agg.flame.root_total_us() as f64 / 1e6,
        all_agg.flame.root_total_us() as f64 / 1e6,
    );
    eprintln!(
        "[bench] single: {} nodes / {} rows;  all: {} nodes / {} rows;  \
         all bottom-up: {} nodes",
        single_agg.flame.nodes().len(),
        single_agg.stats.len(),
        all_agg.flame.nodes().len(),
        all_agg.stats.len(),
        bottom_up_agg.flame.nodes().len(),
    );

    // What a window costs the view: dragging a region out reduces the lanes
    // afresh under it, and every track-selection change under that same region
    // then merges from the memo. The reduce walks the slices the regions' bounds
    // reach, so it falls away with a narrowing single region but not with a set
    // spread across the profile. Spans sit unevenly in time, so what a case
    // aggregates is its node and row counts rather than its coverage.
    for (coverage, parts) in [(0.01, 1), (0.10, 1), (1.00, 1), (0.10, 8)] {
        let win = spread(profile.span(), coverage, parts);
        let other = evicting(&win);
        let mut builds = 0usize;
        let reduced = min_secs(iters, || {
            builds += 1;
            let w = if builds.is_multiple_of(2) {
                &other
            } else {
                &win
            };
            std::hint::black_box(ScopeAggregate::build_for_tracks_reusing(
                &scan,
                mode,
                w,
                |i| threads[i].lane_class != LaneClass::CriticalPath,
            ));
        });
        let merged = min_secs(iters, || {
            std::hint::black_box(ScopeAggregate::build_for_tracks_reusing(
                &scan,
                mode,
                &win,
                |i| threads[i].lane_class != LaneClass::CriticalPath,
            ));
        });
        // Dragging a region out while weighing by wall share.
        let reduced_wall = min_secs(iters, || {
            builds += 1;
            let w = if builds.is_multiple_of(2) {
                &other
            } else {
                &win
            };
            std::hint::black_box(ScopeAggregate::build_for_tracks_reusing(
                &scan,
                wall_share,
                w,
                |i| threads[i].lane_class != LaneClass::CriticalPath,
            ));
        });
        let agg = ScopeAggregate::build_for_tracks_reusing(&scan, mode, &win, |i| {
            threads[i].lane_class != LaneClass::CriticalPath
        });
        eprintln!(
            "[bench] window {:.0}% in {parts} region(s): reduce+merge {reduced:.3}s  \
             memo-hit merge {merged:.3}s  wall-share reduce+merge {reduced_wall:.3}s  \
             ({} nodes / {} rows)",
            coverage * 100.0,
            agg.flame.nodes().len(),
            agg.stats.len(),
        );
    }
}
