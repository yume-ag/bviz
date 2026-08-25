use crate::build::{self, Input, build_graph};
use crate::emphasis;
use crate::mode::LayoutMode;
use crate::morph;
use crate::{GraphView, MARGIN};
use bviz_graph_layout::graph::Graph;
use bviz_graph_layout::{Id, Kind, LaidOut, MapLayout, Names};
use bviz_ui_components::load_in_background;
use gpui::{Context, Task};
use std::collections::HashSet;
use std::sync::Arc;

/// Caps the nodes the layered mode's first view opens with. A layered drawing
/// spreads sideways faster than it gains boxes, so beyond this size the fitted
/// view is a smear of boxes too small to carry a label; the reader opens the
/// containers that interest them out of a legible starting point instead.
const MAX_VISIBLE_NODES: usize = 600;

/// Caps the edges the layered mode's first view opens with. On a dense graph an
/// expansion multiplies the connectors among the boxes already drawn without
/// adding many, and it is the connectors, not the boxes, that decide how far the
/// drawing spreads — so the first view is held to both counts. The spread is not
/// gradual either: a drawing routing enough edges is placed by the cheaper
/// strategy, which buys its speed in width, so a first view sized past that goes
/// abruptly wide.
const MAX_VISIBLE_EDGES: usize = 6000;

/// A graph that finished loading, in the form every later relayout starts from.
pub(crate) struct Loaded {
    map: Arc<MapLayout>,
    /// The layered layout of the expanded view; absent until that mode is first
    /// selected, and replaced by each relayout.
    layered: Option<Arc<LaidOut>>,
    /// The joint graph exactly as built from the proto, before any kind filter
    /// narrows it. Cached so changing the kind filter re-derives from it without
    /// re-reading or re-parsing the proto.
    raw: Arc<Graph>,
    /// `raw` restricted to the current kind filter (identical to `raw` when
    /// every kind is selected), pre-quotient. Every relayout starts from this.
    base: Arc<Graph>,
    /// The base with its transitively-redundant edges removed, cached so the
    /// reduced view's edges are a stable projection of one fixed skeleton.
    reduced_base: Arc<Graph>,
    /// The interned node-id table the base graph keys on, threaded into each
    /// relayout so the quotient pass can mint group ids.
    names: Names,
}

/// Where a map's zoom stands in its pyramid.
#[derive(Clone, Copy)]
pub(crate) struct Level {
    pub(crate) index: usize,
    pub(crate) count: usize,
}

/// A share in the layout a mode draws, cheap to clone into a `'static` paint
/// closure. A map resolves to a view only once the camera is known, so the level
/// a frame draws is picked at the same scale it paints at.
#[derive(Clone)]
pub(crate) enum Drawn {
    Layered(Arc<LaidOut>),
    Map(Arc<MapLayout>),
}

impl Drawn {
    /// The view drawn at `zoom` screen pixels per layout unit.
    pub(crate) fn view(&self, zoom: f32) -> &LaidOut {
        match self {
            Self::Layered(view) => view,
            Self::Map(map) => map.level(map.level_for_scale(zoom)),
        }
    }

    /// Which of a map's levels [`view`](Self::view) draws at `zoom`; `None` for a
    /// layered layout, which has no pyramid.
    pub(crate) fn level(&self, zoom: f32) -> Option<Level> {
        match self {
            Self::Layered(_) => None,
            Self::Map(map) => Some(Level {
                index: map.level_for_scale(zoom),
                count: map.level_count(),
            }),
        }
    }

    /// The extent the camera pans and zooms over: the graph's own size plus
    /// [`MARGIN`] on every side. Every map level spans the same extent, so this
    /// holds still as the zoom changes level.
    pub(crate) fn content(&self) -> (f32, f32) {
        let (width, height) = match self {
            Self::Layered(view) => (view.width, view.height),
            Self::Map(map) => (map.width(), map.height()),
        };
        (
            width as f32 + 2. * MARGIN as f32,
            height as f32 + 2. * MARGIN as f32,
        )
    }
}

/// The boxes a mode's layout lets the user open or close, cheap to clone into a
/// `'static` paint closure. A box outside it carries no disclosure caret and
/// ignores a click.
#[derive(Clone)]
pub(crate) enum Expandable {
    /// No box at all: a map draws whichever level its camera calls for, so what
    /// a container holds is reached by zooming in on it.
    Nothing,
    /// The multi-child containers of the base graph, plus every collapsed group
    /// — a leaf holds nothing to open, and a singleton container opens itself.
    Containers(Arc<HashSet<Id>>),
}

impl Expandable {
    pub(crate) fn holds(&self, b: &bviz_graph_layout::Box) -> bool {
        match self {
            Self::Nothing => false,
            Self::Containers(ids) => ids.contains(&b.id) || b.group,
        }
    }
}

pub(crate) type Source = bviz_ui_components::Source<Loaded>;

/// The graph being viewed: the layouts on screen and the expansion the next
/// relayout starts from, every layout produced off the UI thread.
pub(crate) struct Model {
    source: Source,
    /// Container and group ids currently expanded; every other container folds
    /// its subtree away so the layered layout only processes the visible view.
    expanded: HashSet<Id>,
    /// The set a layered layout's [`Expandable`] is built from.
    containers: Arc<HashSet<Id>>,
    /// The raw ids [`filter`](bviz_graph_layout::filter::filter) draws from,
    /// written by the kind picker (every node of a toggled kind, at once) and
    /// the label search (one node at a time); empty until the raw graph is
    /// loaded, when it starts out holding every node.
    visible: HashSet<Id>,
    /// Whether a layered layout is running, so the view shows a progress
    /// indicator while the prior layout stays interactive.
    relaying: bool,
    /// Holds the background load/relayout task so it isn't cancelled on drop.
    _task: Task<()>,
}

impl Model {
    /// Reads `inputs`, builds the graph and derives the map pyramid on the
    /// background executor, so window appearance and interactivity are never
    /// blocked.
    pub(crate) fn loading(inputs: Vec<Input>, cx: &mut Context<GraphView>) -> Self {
        let task = load_in_background(
            cx,
            move || {
                let (graph, reduced_base, names) = build_graph(&inputs)?;
                let map = bviz_graph_layout::map_layout(&graph, &reduced_base);
                let expanded = bviz_graph_layout::lod::default_expansion(
                    &graph,
                    MAX_VISIBLE_NODES,
                    MAX_VISIBLE_EDGES,
                );
                Ok::<_, String>((graph, reduced_base, names, expanded, map))
            },
            |view, result, _| {
                view.model.source = match result {
                    Ok((graph, reduced_base, names, expanded, map)) => {
                        view.model.containers =
                            Arc::new(bviz_graph_layout::lod::expandable_containers(&graph));
                        view.model.expanded = expanded;
                        view.model.visible = graph.nodes.iter().map(|n| n.id).collect();
                        let graph = Arc::new(graph);
                        Source::Loaded(Loaded {
                            map: Arc::new(map),
                            layered: None,
                            raw: Arc::clone(&graph),
                            base: graph,
                            reduced_base: Arc::new(reduced_base),
                            names,
                        })
                    }
                    Err(err) => Source::Failed(err),
                };
            },
        );
        Self {
            source: Source::Loading,
            expanded: HashSet::new(),
            containers: Arc::new(HashSet::new()),
            visible: HashSet::new(),
            relaying: false,
            _task: task,
        }
    }

    pub(crate) fn source(&self) -> &Source {
        &self.source
    }

    /// The unfiltered joint graph search reads labels from, or `None` before it
    /// has loaded.
    pub(crate) fn raw(&self) -> Option<&Graph> {
        match &self.source {
            Source::Loaded(loaded) => Some(&loaded.raw),
            _ => None,
        }
    }

    /// Whether `id` is in the current visible set, for a search row's checkmark.
    pub(crate) fn is_visible(&self, id: Id) -> bool {
        self.visible.contains(&id)
    }

    /// The layout `mode` draws, or `None` while the graph or that mode's layout
    /// is still being computed.
    pub(crate) fn drawn(&self, mode: LayoutMode) -> Option<Drawn> {
        let Source::Loaded(loaded) = &self.source else {
            return None;
        };
        match mode {
            LayoutMode::Map => Some(Drawn::Map(Arc::clone(&loaded.map))),
            LayoutMode::Layered => loaded.layered.clone().map(Drawn::Layered),
        }
    }

    /// The boxes `mode`'s layout lets the user open or close.
    pub(crate) fn expandable(&self, mode: LayoutMode) -> Expandable {
        match mode {
            LayoutMode::Map => Expandable::Nothing,
            LayoutMode::Layered => Expandable::Containers(Arc::clone(&self.containers)),
        }
    }

    pub(crate) fn relaying(&self) -> bool {
        self.relaying
    }

    pub(crate) fn status(&self, mode: LayoutMode, zoom: f32) -> String {
        self.source.status("No graph", |_| match self.drawn(mode) {
            None => "Laying out…".into(),
            Some(drawn) => {
                let g = drawn.view(zoom);
                let level = drawn
                    .level(zoom)
                    .map(|l| format!("level {}/{} · ", l.index + 1, l.count))
                    .unwrap_or_default();
                format!(
                    "{level}{} boxes · {} connectors · {}×{}",
                    g.boxes.len(),
                    g.connectors.len(),
                    g.width.round() as i64,
                    g.height.round() as i64,
                )
            }
        })
    }
}

/// Flips whether `id` is expanded and lays the graph out at the new expansion.
pub(crate) fn toggle_expansion(view: &mut GraphView, id: Id, cx: &mut Context<GraphView>) {
    if !view.model.expanded.remove(&id) {
        view.model.expanded.insert(id);
    }
    relayout(view, cx);
}

/// Lays the cached base graph out at the current expansion off the UI thread,
/// adopts the result as the layered layout, and asks the framing to settle onto
/// it.
pub(crate) fn relayout(view: &mut GraphView, cx: &mut Context<GraphView>) {
    let Source::Loaded(loaded) = &view.model.source else {
        return;
    };
    let base = Arc::clone(&loaded.base);
    let reduced_base = Arc::clone(&loaded.reduced_base);
    // The relayout mints group ids against the interned-name table, so it
    // extends a clone and hands it back for the view to adopt.
    let mut names = loaded.names.clone();
    let previous = loaded.layered.clone();
    let expanded = view.model.expanded.clone();
    let reduced = view.reduced;
    view.model.relaying = true;
    view.model._task = load_in_background(
        cx,
        move || {
            let laid_out = build::relayout(
                &base,
                &reduced_base,
                &expanded,
                &mut names,
                reduced,
                previous.as_deref().map(|previous| &previous.anchors),
            );
            (laid_out, names)
        },
        |view, (laid_out, names), cx| {
            let laid_out = Arc::new(laid_out);
            let replaced = match &mut view.model.source {
                Source::Loaded(loaded) => {
                    loaded.names = names;
                    loaded.layered.replace(Arc::clone(&laid_out))
                }
                _ => None,
            };
            // A morph eases one layered drawing onto its successor, so it says
            // nothing about a map's levels, which the camera alone selects
            // between: armed while the map is on screen it would pump repaints
            // of a canvas that draws none of it.
            if let Some(replaced) = replaced.filter(|_| view.mode == LayoutMode::Layered) {
                morph::begin(view, replaced, &laid_out, cx);
            }
            view.framing.request_recenter();
            view.model.relaying = false;
            emphasis::begin_fade(view, cx);
        },
    );
}

/// Bulk-adds or removes every node of the given kinds to or from the visible
/// set, matching the kind picker's checked state, and re-derives the graph.
pub(crate) fn set_kind_selection(
    view: &mut GraphView,
    kinds: HashSet<Kind>,
    cx: &mut Context<GraphView>,
) {
    let Source::Loaded(loaded) = &view.model.source else {
        return;
    };
    let raw = Arc::clone(&loaded.raw);
    // A node not present in `kinds` but individually re-shown by search stays
    // shown: this only touches the kinds the picker actually reports on.
    for node in &raw.nodes {
        if kinds.contains(&node.kind) {
            view.model.visible.insert(node.id);
        } else {
            view.model.visible.remove(&node.id);
        }
    }
    rederive(view, cx);
}

/// Flips whether `id` is in the visible set, matching a search row's click,
/// and re-derives the graph.
pub(crate) fn toggle_node_visible(view: &mut GraphView, id: Id, cx: &mut Context<GraphView>) {
    if !view.model.visible.remove(&id) {
        view.model.visible.insert(id);
    }
    rederive(view, cx);
}

/// Restricts the cached raw graph to the current visible set and adopts the
/// result as the base every later relayout starts from, off the UI thread. The
/// map pyramid, the expandable containers, and the default expansion are all
/// derived from node identity, so a change in which nodes exist re-derives
/// every one of them rather than patching them in place; the prior layered
/// layout is dropped for the same reason, and recomputed only if that mode is
/// on screen.
fn rederive(view: &mut GraphView, cx: &mut Context<GraphView>) {
    let Source::Loaded(loaded) = &view.model.source else {
        return;
    };
    let raw = Arc::clone(&loaded.raw);
    let visible = view.model.visible.clone();
    view.model.relaying = true;
    view.model._task = load_in_background(
        cx,
        move || {
            let base = bviz_graph_layout::filter::filter(&raw, &visible);
            let reduced_base = bviz_graph_layout::reduce::transitive_reduction(&base);
            let map = bviz_graph_layout::map_layout(&base, &reduced_base);
            let expanded = bviz_graph_layout::lod::default_expansion(
                &base,
                MAX_VISIBLE_NODES,
                MAX_VISIBLE_EDGES,
            );
            (base, reduced_base, map, expanded)
        },
        |view, (base, reduced_base, map, expanded), cx| {
            view.model.containers = Arc::new(bviz_graph_layout::lod::expandable_containers(&base));
            view.model.expanded = expanded;
            if let Source::Loaded(loaded) = &mut view.model.source {
                loaded.base = Arc::new(base);
                loaded.reduced_base = Arc::new(reduced_base);
                loaded.map = Arc::new(map);
                loaded.layered = None;
            }
            view.hovered = None;
            view.emphasis.clear();
            view.morph.clear();
            view.framing.refit();
            view.model.relaying = false;
            if view.mode == LayoutMode::Layered {
                relayout(view, cx);
            }
        },
    );
}
