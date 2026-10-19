//! Times the layout one expand/collapse toggle costs.
//!
//! A toggle re-derives the whole visible view — folding every unexpanded
//! container away, merging interchangeable leaves, transitively reducing what
//! is left — and lays it out again, so the wait a toggle imposes is that
//! layout. The same view is laid out twice: freely, deriving a configuration of
//! layers and slots from the graph alone, and held to the configuration of the
//! drawing the toggle replaces, which is what keeps the boxes both drawings
//! share reading where they did. Reporting both prices the holding.
//!
//! Each drawing is reported alongside the layered configuration it realizes —
//! how many layers its frames divide their nodes into and how far its connectors
//! reach across them — since the layered algorithm materializes a dummy node per
//! layer an edge spans, making that span the size of the graph it then places.
//!
//! The container opened is, of those the starting view draws collapsed, the one
//! whose expansion unfolds the most nodes, so the layout timed is about the
//! heaviest a single toggle of that view can ask for.
//!
//! Usage: `bench_relayout [--query FILE] [--cquery FILE] [--aquery FILE]
//! [--max-nodes N] [--max-edges N]`, at least one input, each a Bazel
//! `--output=streamed_proto` dump of that query. The caps bound the starting
//! view the toggle happens inside, so raising them times the toggle in a larger
//! view, up to the whole graph; the iteration count comes from
//! `BVIZ_BENCH_ITERS`.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use bviz_graph_layout::graph::{Containment, Graph};
use bviz_graph_layout::lod::{DEFAULT_MAX_EDGES, DEFAULT_MAX_NODES};
use bviz_graph_layout::{Id, LaidOut, Names, layout, lod, quotient, reduce, streamed_proto};

const USAGE: &str = "usage: bench_relayout [--query FILE] [--cquery FILE] [--aquery FILE] \
                     [--max-nodes N] [--max-edges N] (at least one input); \
                     iterations from BVIZ_BENCH_ITERS";

fn secs(mut f: impl FnMut()) -> f64 {
    let t = std::time::Instant::now();
    f();
    t.elapsed().as_secs_f64()
}

/// The smallest, middle and largest of `samples`, which must be non-empty.
fn spread(samples: &[f64]) -> (f64, f64, f64) {
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    (
        sorted[0],
        sorted[sorted.len() / 2],
        sorted[sorted.len() - 1],
    )
}

/// What one frame gives the layered algorithm to place: the nodes it ranks, the
/// layers it ranks them into, and the connectors it routes with the layers they
/// span, one dummy node per layer.
#[derive(Default)]
struct Frame {
    nodes: usize,
    layers: u32,
    connectors: usize,
    span: u64,
}

impl Frame {
    /// Everything this frame's node placement has to position: its own nodes and
    /// the dummy standing in for each layer a connector spans.
    fn placed(&self) -> u64 {
        self.nodes as u64 + self.span
    }
}

/// The layered configuration a drawing realizes: how its frames divide their
/// nodes and how far the connectors each routes reach across that division.
struct Shape {
    frames: usize,
    layers: u64,
    span: u64,
    longest_span: u32,
    /// The frame with the most to place. Frames are laid out one at a time and
    /// vary by orders of magnitude, so the largest sets the pace of the whole.
    largest: Frame,
    /// Containment levels the connectors descend below the frame that routes
    /// them, summed over both ends, each level costing a hierarchical port.
    descent: u64,
    /// Connectors running between a container and its own descendant, which no
    /// one frame's layering ranks against each other.
    nested: usize,
    /// Interior points of every connector drawn.
    bends: usize,
}

fn shape(graph: &Graph, drawing: &LaidOut) -> Shape {
    let containment = Containment::new(graph);
    let mut frames: HashMap<Option<Id>, Frame> = HashMap::new();
    for (&id, anchor) in &drawing.anchors {
        let frame = frames.entry(containment.parent(id)).or_default();
        frame.nodes += 1;
        frame.layers = frame.layers.max(anchor.layer + 1);
    }

    let mut longest_span = 0;
    let mut descent = 0;
    let mut nested = 0;
    let mut bends = 0;
    for connector in &drawing.connectors {
        bends += connector.points.len().saturating_sub(2);
        let ranked = containment.sibling_pair(connector.from, connector.to);
        let placed = ranked.and_then(|(frame, a, b)| {
            Some((
                frame,
                a,
                b,
                drawing.anchors.get(&a)?,
                drawing.anchors.get(&b)?,
            ))
        });
        let Some((routing_frame, a, b, from, to)) = placed else {
            nested += 1;
            continue;
        };
        let span = from.layer.abs_diff(to.layer);
        longest_span = longest_span.max(span);
        descent += u64::from(containment.depth(connector.from) - containment.depth(a))
            + u64::from(containment.depth(connector.to) - containment.depth(b));
        let frame = frames.entry(routing_frame).or_default();
        frame.connectors += 1;
        frame.span += u64::from(span);
    }

    Shape {
        frames: frames.len(),
        layers: frames.values().map(|f| u64::from(f.layers)).sum(),
        span: frames.values().map(|f| f.span).sum(),
        longest_span,
        largest: frames
            .into_values()
            .max_by_key(Frame::placed)
            .unwrap_or_default(),
        descent,
        nested,
        bends,
    }
}

fn report_shape(held: &str, shape: &Shape) {
    eprintln!(
        "[bench] {held} shape: {} layers over {} frames, connectors spanning {} layers \
         (longest {}) and descending {} levels, {} nested; {} bends; \
         its largest frame ranks {} nodes into {} layers, routing {} connectors over {}",
        shape.layers,
        shape.frames,
        shape.span,
        shape.longest_span,
        shape.descent,
        shape.nested,
        shape.bends,
        shape.largest.nodes,
        shape.largest.layers,
        shape.largest.connectors,
        shape.largest.span,
    );
}

struct Args {
    query: Option<PathBuf>,
    cquery: Option<PathBuf>,
    aquery: Option<PathBuf>,
    max_nodes: usize,
    max_edges: usize,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            query: None,
            cquery: None,
            aquery: None,
            max_nodes: DEFAULT_MAX_NODES,
            max_edges: DEFAULT_MAX_EDGES,
        }
    }
}

impl Args {
    fn from_env() -> Self {
        let mut args = Self::default();
        let mut argv = std::env::args().skip(1);
        while let Some(flag) = argv.next() {
            let Some(value) = argv.next() else { usage() };
            match flag.as_str() {
                "--query" => args.query = Some(PathBuf::from(&value)),
                "--cquery" => args.cquery = Some(PathBuf::from(&value)),
                "--aquery" => args.aquery = Some(PathBuf::from(&value)),
                "--max-nodes" => args.max_nodes = count(&value),
                "--max-edges" => args.max_edges = count(&value),
                _ => usage(),
            }
        }
        if args.query.is_none() && args.cquery.is_none() && args.aquery.is_none() {
            usage();
        }
        args
    }

    /// Reads every given dump and merges them into one joint graph, as the
    /// application does before any level-of-detail folding.
    fn build_joint(&self) -> (Graph, Names) {
        let aquery = self
            .aquery
            .as_deref()
            .map(|p| decode(p, streamed_proto::read_action_graph_containers));
        let cquery = self
            .cquery
            .as_deref()
            .map(|p| decode(p, streamed_proto::read_cquery_results));
        let query = self
            .query
            .as_deref()
            .map(|p| decode(p, streamed_proto::read_query_targets));
        let joint = bviz_graph_layout::build_joint(
            aquery.as_deref(),
            cquery.as_deref(),
            query.as_deref(),
            false,
        );
        (joint.graph, joint.names)
    }
}

fn usage() -> ! {
    eprintln!("{USAGE}");
    std::process::exit(2)
}

fn count(value: &str) -> usize {
    value.parse().unwrap_or_else(|_| usage())
}

fn decode<T>(
    path: &Path,
    read: impl FnOnce(std::fs::File, Option<u64>) -> Result<Vec<T>, streamed_proto::ReadError>,
) -> Vec<T> {
    let fail = |e: &dyn std::fmt::Display| -> ! { panic!("{}: {e}", path.display()) };
    let file = std::fs::File::open(path).unwrap_or_else(|e| fail(&e));
    let size = file.metadata().map(|metadata| metadata.len()).ok();
    read(file, size).unwrap_or_else(|e| fail(&e))
}

/// The graph one expansion of `base` draws: only the descendants of expanded
/// containers, interchangeable leaves merged, and of the edges that projection
/// leaves only those the transitive reduction keeps. `reduced_base` is `base`
/// already reduced, so a collapsed box draws the reduction of its hidden
/// subtree's edges.
fn view_at(base: &Graph, reduced_base: &Graph, expanded: &HashSet<Id>, names: &mut Names) -> Graph {
    let visible = lod::collapse(base, expanded);
    let (mut view, remap) = quotient::quotient(&visible, expanded, names);
    let skeleton = lod::collapse(reduced_base, expanded);
    let box_of = |id: Id| remap.get(&id).copied().unwrap_or(id);
    let kept: HashSet<(Id, Id)> = skeleton
        .edges
        .iter()
        .map(|e| (box_of(e.from), box_of(e.to)))
        .filter(|(a, b)| a != b)
        .collect();
    view.edges.retain(|e| kept.contains(&(e.from, e.to)));
    reduce::transitive_reduction(&view)
}

/// Of the containers the view at `expanded` draws collapsed, the one whose
/// expansion unfolds the most nodes, and how many that is — counted before
/// interchangeable leaves merge, so the boxes the view gains are fewer. Ties go
/// to the lower id, so a graph always picks the same container.
fn densest_collapsed_container(base: &Graph, expanded: &HashSet<Id>) -> Option<(Id, usize)> {
    let drawn: HashSet<Id> = lod::collapse(base, expanded)
        .nodes
        .iter()
        .map(|n| n.id)
        .collect();
    let forest = lod::Forest::new(base);
    lod::expandable_containers(base)
        .into_iter()
        .filter(|id| drawn.contains(id) && !expanded.contains(id))
        .map(|id| (id, forest.reveal_count(id)))
        .max_by_key(|&(id, revealed)| (revealed, Reverse(id)))
}

fn main() {
    let args = Args::from_env();
    let iters: usize = std::env::var("BVIZ_BENCH_ITERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5);

    let (base, mut names) = args.build_joint();
    let reduced_base = reduce::transitive_reduction(&base);

    let opening = lod::default_expansion(
        &base,
        Some(&reduced_base),
        args.max_nodes,
        args.max_edges,
    )
    .unwrap_or_else(|over| {
        eprintln!(
            "[bench] collapsed to its roots the graph is still {} nodes / {} edges, past the \
                 caps {}/{}: the view the toggle happens inside is the whole of it",
            over.nodes, over.edges, args.max_nodes, args.max_edges,
        );
        HashSet::new()
    });
    let start_view = view_at(&base, &reduced_base, &opening, &mut names);
    let start = layout::layout(&start_view);

    let (container, revealed) = densest_collapsed_container(&base, &opening)
        .expect("the starting view draws a container it can open");
    let mut opened = opening.clone();
    opened.insert(container);
    let toggled = view_at(&base, &reduced_base, &opened, &mut names);

    // Alternating the two within each iteration pairs them against the same
    // machine load, which over a run of minutes is not otherwise steady.
    let mut free_secs = Vec::with_capacity(iters);
    let mut pinned_secs = Vec::with_capacity(iters);
    let mut free_drawing = LaidOut::default();
    let mut pinned_drawing = LaidOut::default();
    for _ in 0..iters {
        free_secs.push(secs(|| free_drawing = layout::layout(&toggled)));
        pinned_secs.push(secs(|| {
            pinned_drawing = layout::layout_pinned(&toggled, &start.anchors)
        }));
    }
    let (free, free_median, free_max) = spread(&free_secs);
    let (pinned, pinned_median, pinned_max) = spread(&pinned_secs);

    eprintln!(
        "[bench] base: {} nodes / {} edges; under caps {}/{} the starting view is \
         {} boxes / {} edges, {} of them anchored by its drawing",
        base.nodes.len(),
        base.edges.len(),
        args.max_nodes,
        args.max_edges,
        start_view.nodes.len(),
        start_view.edges.len(),
        start.anchors.len(),
    );
    eprintln!(
        "[bench] opening {} unfolds {revealed} nodes: the view laid out grows to \
         {} boxes / {} edges",
        names.resolve(container),
        toggled.nodes.len(),
        toggled.edges.len(),
    );
    report_shape("free", &shape(&toggled, &free_drawing));
    report_shape("pinned", &shape(&toggled, &pinned_drawing));
    eprintln!(
        "[bench] layout free: {free:.3}s min / {free_median:.3}s median / {free_max:.3}s max  \
         pinned: {pinned:.3}s / {pinned_median:.3}s / {pinned_max:.3}s  (of {iters})"
    );
    let paired: Vec<String> = free_secs
        .iter()
        .zip(&pinned_secs)
        .map(|(free, pinned)| format!("{:.3}", pinned / free))
        .collect();
    eprintln!(
        "[bench] pinned/free: {:.3} on the minima, {} paired",
        pinned / free,
        paired.join(" "),
    );
}
