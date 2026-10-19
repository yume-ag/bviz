//! What one panel draws: the document's graph as the document's filter and the
//! panel's own layout rules leave it.
//!
//! What the document holds — the base graph, its reduction, the facets, the
//! names and the map's placement — is derived once, unfiltered, and each panel
//! restricts that for itself, since its rules, its expansion and the dependency
//! paths it draws are its own.
//!
//! A restriction keeps every surviving node's id, so the expansion, the
//! selection and the layout's pinning anchors go on naming what they named and
//! carry across a filter change.

use crate::build::{self, TooDense};
use crate::filter::{DocumentFilter, Judgement};
use crate::framing::Frame;
use crate::mode::LayoutMode;
use crate::model::{Loaded, OPENS_REDUCED, Placement, Shared, Source};
use crate::rules::{self, BoxClass};
use crate::{GraphPanel, MARGIN};
use bviz_graph_layout::graph::Graph;
use bviz_graph_layout::lod::{DEFAULT_MAX_EDGES, DEFAULT_MAX_NODES, Overflow};
use bviz_graph_layout::paths::Paths;
use bviz_graph_layout::select::{Filter, Hiding, Pin, Verdict, Verdicts};
use bviz_graph_layout::transition::Transitions;
use bviz_graph_layout::{Id, LaidOut, MapLayout, Names};
use bviz_ui_components::load_in_background;
use gpui::{Context, Task};
use rustc_hash::FxHashSet;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

/// Derives every view of `map` off the UI thread, coarsest first, so a zoom
/// finds the level it calls for ready rather than drawing a neighbour of it
/// while it derives. A view already derived costs nothing to ask for again, so
/// this races [`derive_view`] harmlessly.
pub(crate) fn derive_levels<V: 'static>(map: Arc<MapLayout>, cx: &mut Context<V>) {
    cx.background_executor()
        .spawn(async move {
            for level in 0..map.level_count() {
                map.level(level);
            }
        })
        .detach();
}

/// Where a map's zoom stands in its pyramid.
#[derive(Clone, Copy)]
pub(crate) struct Level {
    pub(crate) index: usize,
    pub(crate) count: usize,
}

/// Identifies the view a [`Drawn`] resolves to, by which work derived from that
/// view is redone exactly when the view moves under it. Every drawing a panel
/// takes up is numbered afresh, so a view is never taken for one of the drawing
/// it replaced.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ViewKey {
    Layered(u64),
    Map { drawing: u64, level: usize },
}

impl ViewKey {
    pub(crate) fn drawing(self) -> u64 {
        match self {
            Self::Layered(drawing) | Self::Map { drawing, .. } => drawing,
        }
    }
}

/// A view a [`Drawn`] resolves to, with the key identifying it. Both come from
/// one reading of which map levels are derived, which a background thread
/// advances, so work cached under the key is always derived from this view.
#[derive(Clone, Copy)]
pub(crate) struct Shown<'a> {
    key: ViewKey,
    view: &'a LaidOut,
}

impl<'a> Shown<'a> {
    pub(crate) fn key(self) -> ViewKey {
        self.key
    }

    pub(crate) fn view(self) -> &'a LaidOut {
        self.view
    }
}

/// A share in the layout a mode draws, cheap to clone into a `'static` paint
/// closure. A map resolves to a view only once the camera is known, so the level
/// a frame draws is picked at the same scale it paints at.
#[derive(Clone)]
pub(crate) enum Drawn {
    Layered { view: Arc<LaidOut>, drawing: u64 },
    Map { map: Arc<MapLayout>, drawing: u64 },
}

impl Drawn {
    /// The view a frame at `zoom` screen pixels per layout unit draws: the map
    /// level that zoom calls for or, while that one is yet to be derived, the
    /// derived level nearest it. Absent while a map has no level derived:
    /// deriving one costs a fold of the whole graph, which [`derive_view`]
    /// leaves to a background thread.
    pub(crate) fn view(&self, zoom: f32) -> Option<&LaidOut> {
        self.shown(zoom).map(Shown::view)
    }

    /// [`view`](Self::view) at `zoom`, identified.
    pub(crate) fn shown(&self, zoom: f32) -> Option<Shown<'_>> {
        match self {
            Self::Layered { view, drawing } => Some(Shown {
                key: ViewKey::Layered(*drawing),
                view,
            }),
            Self::Map { map, drawing } => {
                let level = shown_level(map, zoom);
                map.derived(level).map(|view| Shown {
                    key: ViewKey::Map {
                        drawing: *drawing,
                        level,
                    },
                    view,
                })
            }
        }
    }

    /// The view a frame at `zoom` calls for, deriving a map's level where it has
    /// yet to be.
    fn derive_view(&self, zoom: f32) -> &LaidOut {
        match self {
            Self::Layered { view, .. } => view,
            Self::Map { map, .. } => map.level(map.level_for_scale(zoom)),
        }
    }

    /// Identifies the view a frame at `zoom` calls for, where that is a map's
    /// level yet to be derived.
    pub(crate) fn underived(&self, zoom: f32) -> Option<ViewKey> {
        match self {
            Self::Layered { .. } => None,
            Self::Map { map, drawing } => {
                let level = map.level_for_scale(zoom);
                map.derived(level).is_none().then_some(ViewKey::Map {
                    drawing: *drawing,
                    level,
                })
            }
        }
    }

    /// Whether [`view`](Self::view) at `zoom` is a map level leaving out what
    /// more closed containers hold than it draws boxes.
    pub(crate) fn withholds_most(&self, zoom: f32) -> bool {
        match self {
            Self::Layered { .. } => false,
            Self::Map { map, .. } => {
                let level = shown_level(map, zoom);
                map.withheld(level)
                    .zip(map.derived(level))
                    .is_some_and(|(withheld, view)| withheld > view.boxes.len())
            }
        }
    }

    /// Which of a map's levels [`view`](Self::view) draws at `zoom`; `None` for a
    /// layered layout, which has no pyramid.
    pub(crate) fn level(&self, zoom: f32) -> Option<Level> {
        match self {
            Self::Layered { .. } => None,
            Self::Map { map, .. } => Some(Level {
                index: shown_level(map, zoom),
                count: map.level_count(),
            }),
        }
    }

    /// The extent the camera pans and zooms over: the graph's own size plus
    /// [`MARGIN`] on every side. Every map level spans the same extent, so this
    /// holds still as the zoom changes level.
    pub(crate) fn content(&self) -> (f32, f32) {
        let (width, height) = match self {
            Self::Layered { view, .. } => (view.width, view.height),
            Self::Map { map, .. } => (map.width(), map.height()),
        };
        (
            width as f32 + 2. * MARGIN as f32,
            height as f32 + 2. * MARGIN as f32,
        )
    }
}

/// The level of `map` a frame at `zoom` draws, which [`Drawn::view`] describes.
fn shown_level(map: &MapLayout, zoom: f32) -> usize {
    let wanted = map.level_for_scale(zoom);
    map.nearest_derived(wanted).unwrap_or(wanted)
}

/// The boxes a mode's layout lets the user open or close, cheap to clone into a
/// `'static` paint closure. A box outside it carries no disclosure caret and
/// ignores a click.
#[derive(Clone)]
pub(crate) enum Expandable {
    /// No box at all: a map draws whichever level its camera calls for, so what
    /// a container holds is reached by zooming in on it.
    Nothing,
    /// The multi-child containers of the drawn graph, plus every collapsed group
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

/// What a panel restricts the document's graph by.
#[derive(Clone)]
enum Lens {
    Filter(Filter),
    /// Dependency paths, drawn in place of whatever the panel's filter leaves.
    Paths(Arc<Paths>),
}

/// The lens a slice was derived through, with what it made of the document's
/// nodes where that is more than which of them the slice holds.
#[derive(Clone)]
enum Through {
    Filter {
        filter: Filter,
        verdicts: Arc<Verdicts>,
    },
    Paths {
        paths: Arc<Paths>,
        /// Which ids the paths' subgraph holds, indexed by id.
        held: Arc<Vec<bool>>,
    },
}

/// What keeps a node of the document's graph out of what a panel draws.
pub(crate) enum Hidden {
    /// Dependency paths are drawn in the filter's place, and none passes
    /// through the node.
    OffPaths,
    /// The filter hides the node, by `hiding`.
    Filtered(Hiding),
}

/// The graph one lens leaves of the document's, with everything derived from
/// it. A derivation replaces it whole, so a frame reads one lens's graph
/// throughout.
#[derive(Clone)]
struct Slice {
    /// The lens this was derived through, which a panel drawing through another
    /// must restrict the document's graph afresh to leave behind.
    lens: Through,
    base: Arc<Graph>,
    /// The restriction with its transitively-redundant edges removed, which the
    /// collapsed views are projected through.
    reduced: Arc<Graph>,
    /// The restriction placed where the document's map places it, so filtering
    /// leaves holes rather than moving what survives.
    map: SliceMap,
    containers: Arc<HashSet<Id>>,
    /// The nodes the filter draws as no part of what it selects, which the
    /// drawing fades rather than leaving out.
    faded: Arc<HashSet<Id>>,
    /// The configuration transitions the graph's edges cross, which the drawing
    /// marks; found for dependency paths alone.
    transitions: Arc<Transitions>,
    /// Extended by each layered layout over this graph, whose quotient pass
    /// mints group ids against it.
    names: Names,
}

/// The map a [`Slice`] draws: the document's placement over the slice's graph.
#[derive(Clone)]
enum SliceMap {
    /// The document's own, for a slice holding the whole graph, whose levels
    /// the document derives.
    Whole(Placement),
    Restricted(RestrictedMap),
}

/// A slice's graph placed where the document places it, made once the
/// document's placement lands. Every clone shares the one map, whose levels the
/// panel derives once.
#[derive(Clone)]
struct RestrictedMap {
    placement: Placement,
    base: Arc<Graph>,
    reduced: Arc<Graph>,
    transitions: Arc<Transitions>,
    map: Arc<OnceLock<Arc<MapLayout>>>,
    /// Whether the levels of `map` have been asked for.
    levels_asked: Arc<AtomicBool>,
}

impl RestrictedMap {
    /// The map, absent until the document's placement lands.
    fn placed(&self) -> Option<&Arc<MapLayout>> {
        let whole = self.placement.placed()?;
        Some(self.map.get_or_init(|| {
            Arc::new(whole.restricted(
                Arc::clone(&self.base),
                Arc::clone(&self.reduced),
                Arc::clone(&self.transitions),
            ))
        }))
    }

    /// Derives the map's levels off the UI thread, the first time it is asked
    /// once the document's placement has landed.
    fn derive_levels<V: 'static>(&self, cx: &mut Context<V>) {
        if let Some(map) = self.placed()
            && !self.levels_asked.swap(true, Ordering::Relaxed)
        {
            derive_levels(Arc::clone(map), cx);
        }
    }
}

impl Slice {
    /// The whole graph drawn through no filter, which is also what a filter
    /// hiding nothing leaves; it is already reduced and placed with the
    /// document, so such a filter costs nothing to draw through.
    fn whole(shared: &Shared) -> Self {
        Slice {
            lens: Through::Filter {
                filter: Filter::default(),
                verdicts: Arc::default(),
            },
            base: Arc::clone(&shared.base),
            reduced: Arc::clone(&shared.reduced_base),
            map: SliceMap::Whole(shared.map.clone()),
            containers: Arc::clone(&shared.containers),
            faded: Arc::new(HashSet::new()),
            transitions: Arc::default(),
            names: Names::clone(&shared.names),
        }
    }

    /// The map this draws, absent until the document's placement lands.
    fn map(&self) -> Option<&Arc<MapLayout>> {
        match &self.map {
            SliceMap::Whole(placement) => placement.placed(),
            SliceMap::Restricted(restricted) => restricted.placed(),
        }
    }

    /// The map this draws where it restricts the document's.
    fn restricted_map(&self) -> Option<&RestrictedMap> {
        match &self.map {
            SliceMap::Whole(_) => None,
            SliceMap::Restricted(restricted) => Some(restricted),
        }
    }
}

/// What re-evaluating a filter left of the graph a panel was drawing.
struct Restricted {
    expanded: HashSet<Id>,
    over_budget: Option<Overflow>,
}

/// One derivation's result.
struct Landed {
    slice: Slice,
    /// Absent where the derivation drew on the restriction the panel already
    /// held, which leaves the expansion and the selection as they were.
    restricted: Option<Restricted>,
    /// The expansion the derivation settled on, which the panel opens at from
    /// here on.
    expanded: HashSet<Id>,
    layered: Relaid,
}

/// What a derivation did about the layered drawing.
enum Relaid {
    /// It laid none out: a panel drawing a map holds none to keep current.
    Nothing,
    /// It would have laid out what the panel's drawing is laid out from, so it
    /// keeps that drawing as its own.
    Held,
    Fresh(Layered),
}

/// A layered drawing and what it was laid out from.
#[derive(Clone)]
struct Layered {
    laid: Result<Laid, TooDense>,
    base: Arc<Graph>,
    expanded: HashSet<Id>,
    reduced: bool,
}

/// A view laid out.
#[derive(Clone)]
struct Laid {
    view: Arc<LaidOut>,
    /// The remap of [`bviz_graph_layout::quotient::quotient`] `view` was laid out
    /// through.
    groups: Arc<BTreeMap<Id, Id>>,
    /// The boxes `view` draws.
    drawn: Arc<HashSet<Id>>,
    /// The container holding each node of the graph `view` was laid out
    /// from.
    parents: Arc<HashMap<Id, Id>>,
}

impl Laid {
    /// `view`, laid out from `base` through `groups`.
    fn new(view: Arc<LaidOut>, groups: Arc<BTreeMap<Id, Id>>, base: &Graph) -> Self {
        let drawn = view.boxes.iter().map(|b| b.id).collect();
        let mut parents: HashMap<Id, Id> = HashMap::with_capacity(base.nodes.len());
        parents.extend((base.nodes.iter()).filter_map(|node| Some((node.id, node.parent?))));
        Laid {
            view,
            groups,
            drawn: Arc::new(drawn),
            parents: Arc::new(parents),
        }
    }
}

impl Layered {
    /// What this draws: nothing, where the view was too dense to lay out.
    fn view(&self) -> Arc<LaidOut> {
        match &self.laid {
            Ok(laid) => Arc::clone(&laid.view),
            Err(TooDense { .. }) => Arc::default(),
        }
    }

    /// Whether laying `base` out at `expanded`, over the reduced edge set where
    /// `reduced`, is what drew this.
    fn draws(&self, base: &Arc<Graph>, expanded: &HashSet<Id>, reduced: bool) -> bool {
        Arc::ptr_eq(&self.base, base) && self.expanded == *expanded && self.reduced == reduced
    }

    /// The innermost box of this drawing standing for `id`: its own, else that
    /// of the merge group it is merged into, else that of the innermost
    /// container holding it.
    fn holder(&self, id: Id) -> Option<Id> {
        let laid = self.laid.as_ref().ok()?;
        let parent = |id: &Id| laid.parents.get(id).copied();
        std::iter::once(id)
            .chain(laid.groups.get(&id).copied())
            .chain(std::iter::successors(parent(&id), parent))
            .find(|candidate| laid.drawn.contains(candidate))
    }

    /// A node of the graph the box `id` stands for: the first of those a
    /// merge group's box gathers, which names the group's nodes in every
    /// drawing, and any other box's own node.
    fn member(&self, id: Id) -> Id {
        let gathered = (self.laid.as_ref().ok())
            .and_then(|laid| laid.groups.iter().find(|&(_, &group)| group == id));
        gathered.map_or(id, |(&member, _)| member)
    }

    /// The nodes of the graph the merge group's box `id` gathers, in id
    /// order; none for any other box.
    fn gathered(&self, id: Id) -> Vec<Id> {
        let Ok(laid) = &self.laid else {
            return Vec::new();
        };
        let members = laid.groups.iter().filter(|&(_, &group)| group == id);
        members.map(|(&member, _)| member).collect()
    }
}

/// What the derivation in flight will produce.
#[derive(Clone, Copy)]
struct InFlight {
    /// Whether it lays a layered drawing out, which is settled when the
    /// derivation is requested and so says nothing about the mode the panel has
    /// selected since.
    lays_out: bool,
}

/// A layered drawing that has landed, and the one it takes the place of, which
/// the view eases onto it.
pub(crate) struct LayoutLanded {
    pub(crate) replaced: Option<Arc<LaidOut>>,
    pub(crate) current: Arc<LaidOut>,
}

/// What adopting a derivation's result leaves the panel to follow up on.
struct Adopted {
    /// Absent where the derivation left the restriction the panel held
    /// standing.
    restriction: Option<Restriction>,
    /// Whether the camera is to be moved onto what the derivation left drawn.
    frame: bool,
    /// The layered drawing to settle onto, where this derivation laid one out
    /// or kept the one held.
    landing: Option<LayoutLanded>,
}

/// What a derivation that evaluated the filter afresh leaves for the panel to
/// answer for.
struct Restriction {
    /// The map of the graph that leaves, where that restricts the document's;
    /// no camera has called for a level of it.
    map: Option<RestrictedMap>,
}

/// Dependency paths a panel draws, and the expansion its filter's drawing had,
/// which clearing them returns to.
struct PathView {
    paths: Arc<Paths>,
    expanded_under_filter: HashSet<Id>,
}

/// What a panel draws the document's graph as, and how far the derivation of it
/// has got.
pub(crate) struct Derived {
    /// The document's filter with the boxes `unwrapped` and `revealed` pinned,
    /// as last taken up.
    filter: Filter,
    /// The box classes the panel's layout unwraps, which no other view's
    /// does.
    unwrapped: BTreeSet<BoxClass>,
    /// The nodes revealed in this panel against its own rules, each standing
    /// until it is withdrawn from the pins card.
    revealed: HashSet<Id>,
    /// Whether a change to the document's filter awaits the panel being shown
    /// again, and whether one such change named the part of the graph to look
    /// at.
    unfollowed: Option<bool>,
    /// Drawn in place of what the filter leaves, with every edge: a reduction
    /// drops an edge a longer path implies, which is itself a path.
    paths: Option<PathView>,
    /// Container and group ids expanded in the layered layout last requested;
    /// every other container folds its subtree away so the layered layout only
    /// processes the visible view.
    expanded: HashSet<Id>,
    /// Whether the layered layout last requested draws the transitive reduction
    /// of the edges rather than every edge.
    reduced: bool,
    /// The node the next derivation opens the expansion onto, so that the
    /// layered layout draws a box of its own for it.
    revealing: Option<Id>,
    /// Absent until the document's read lands.
    slice: Option<Slice>,
    layered: Option<Layered>,
    /// The size of the view the panel last restricted to where it is past the
    /// caps that bound it, absent where it fits.
    over_budget: Option<Overflow>,
    /// What the derivation running will produce, absent while none is. A
    /// request cancels the one it replaces, so at most one runs at a time and
    /// its result is the panel's.
    in_flight: Option<InFlight>,
    /// Counts the drawings taken up, numbering each for its [`ViewKey`].
    drawings: u64,
    /// Whether the filter awaiting a restriction was set by a gesture naming
    /// what to look at, which is answered by moving the camera onto what that
    /// restriction leaves drawn.
    frame_on_landing: bool,
    /// The derivation in flight, held so that it is not cancelled.
    _work: Task<()>,
    /// The current drawing's views a frame found underived, each with the
    /// background derivation of it, which redraws the canvas once it lands.
    views_asked: Vec<(ViewKey, Task<()>)>,
}

impl Derived {
    /// A panel's view of a graph that has not been read yet.
    pub(crate) fn new() -> Self {
        Derived {
            filter: Filter::default(),
            unwrapped: BTreeSet::new(),
            revealed: HashSet::new(),
            unfollowed: None,
            paths: None,
            expanded: HashSet::new(),
            reduced: OPENS_REDUCED,
            revealing: None,
            slice: None,
            layered: None,
            over_budget: None,
            in_flight: None,
            drawings: 0,
            frame_on_landing: false,
            _work: Task::ready(()),
            views_asked: Vec::new(),
        }
    }

    /// Draws `loaded`'s graph whole, at the expansion its read sized, through no
    /// filter.
    ///
    /// A filter's scope and pins name nodes of one graph by id, so it does not
    /// carry to the graph that takes its place. Every drawing of the graph left
    /// behind goes with it, and so does the derivation that was producing one.
    fn opened_on(&mut self, loaded: &Loaded) {
        self.filter = Filter::default();
        self.unwrapped = BTreeSet::new();
        self.revealed = HashSet::new();
        self.paths = None;
        self.expanded = loaded.opening.expanded.clone();
        self.revealing = None;
        self.over_budget = loaded.opening.over_budget;
        self.slice = Some(Slice::whole(&loaded.shared));
        self.drawings += 1;
        self.layered = None;
        self.in_flight = None;
        self._work = Task::ready(());
    }

    /// The layout `mode` draws, or `None` while the graph or that mode's layout
    /// is still being derived.
    pub(crate) fn drawn(&self, mode: LayoutMode) -> Option<Drawn> {
        let slice = self.slice.as_ref()?;
        match mode {
            LayoutMode::Map => slice.map().map(|map| Drawn::Map {
                map: Arc::clone(map),
                drawing: self.drawings,
            }),
            LayoutMode::Layered => self.layered.as_ref().map(|layered| Drawn::Layered {
                view: layered.view(),
                drawing: self.drawings,
            }),
        }
    }

    /// The boxes `mode`'s layout lets the user open or close.
    pub(crate) fn expandable(&self, mode: LayoutMode) -> Expandable {
        match (mode, &self.slice) {
            (LayoutMode::Layered, Some(slice)) => {
                Expandable::Containers(Arc::clone(&slice.containers))
            }
            _ => Expandable::Nothing,
        }
    }

    /// Whether the layered layout draws the transitive reduction, `None` where
    /// the drawing offers no choice.
    pub(crate) fn reduced(&self) -> Option<bool> {
        self.paths.is_none().then_some(self.reduced)
    }

    /// Whether the layered layout draws the transitive reduction.
    fn draws_reduced(&self) -> bool {
        self.reduced().unwrap_or(false)
    }

    /// Draws `paths` in place of what the filter leaves, or the filter again
    /// where `None`, at the expansion it had; reports whether that changes what
    /// is drawn.
    fn show_paths(&mut self, paths: Option<Arc<Paths>>) -> bool {
        match (paths, self.paths.take()) {
            (Some(paths), held) => {
                let expanded_under_filter = match held {
                    Some(held) => held.expanded_under_filter,
                    None => self.expanded.clone(),
                };
                self.paths = Some(PathView {
                    paths,
                    expanded_under_filter,
                });
                true
            }
            (None, Some(held)) => {
                self.expanded = held.expanded_under_filter;
                true
            }
            (None, None) => false,
        }
    }

    /// The lens the panel draws through.
    fn lens(&self) -> Lens {
        match &self.paths {
            Some(view) => Lens::Paths(Arc::clone(&view.paths)),
            None => Lens::Filter(self.filter.clone()),
        }
    }

    /// Whether `lens` is the one the panel draws through.
    fn draws_through(&self, lens: &Through) -> bool {
        match (lens, &self.paths) {
            (Through::Paths { paths, .. }, Some(view)) => Arc::ptr_eq(paths, &view.paths),
            (Through::Filter { filter, .. }, None) => *filter == self.filter,
            _ => false,
        }
    }

    /// What keeps the node `id` out of the drawing the panel holds, read from
    /// the lens that drawing was derived through; `None` where it holds the
    /// node, and before the graph is read.
    pub(crate) fn hidden(&self, id: Id) -> Option<Hidden> {
        let slice = self.slice.as_ref()?;
        match &slice.lens {
            Through::Filter { verdicts, .. } => verdicts.hiding(id).map(Hidden::Filtered),
            Through::Paths { held, .. } => {
                let held = held.get(id.0 as usize).copied().unwrap_or(false);
                (!held).then_some(Hidden::OffPaths)
            }
        }
    }

    /// Has the next derivation open the layered layout onto `id`, as
    /// [`expand_to`] does, for a request that derivation answers.
    pub(crate) fn open_onto_next(&mut self, id: Id) {
        self.revealing = Some(id);
    }

    pub(crate) fn unwrapped(&self) -> &BTreeSet<BoxClass> {
        &self.unwrapped
    }

    pub(crate) fn revealed(&self) -> &HashSet<Id> {
        &self.revealed
    }

    /// The filter the panel draws `document`'s graph through.
    fn filter_over(&self, document: &DocumentFilter) -> Filter {
        let mut filter = document.with_rules(rules::unwrapping(&self.unwrapped).as_slice());
        for &id in &self.revealed {
            filter.pins.entry(id).or_insert(Pin::Revealed);
        }
        filter
    }

    /// The boxes the filter draws as no part of what it selects.
    pub(crate) fn faded(&self) -> Arc<HashSet<Id>> {
        self.slice.as_ref().map_or_else(
            || Arc::new(HashSet::new()),
            |slice| Arc::clone(&slice.faded),
        )
    }

    /// The configuration transitions the drawing marks.
    pub(crate) fn transitions(&self) -> Arc<Transitions> {
        self.slice
            .as_ref()
            .map_or_else(Arc::default, |slice| Arc::clone(&slice.transitions))
    }

    /// Whether a derivation is running, so a view can show a progress indicator
    /// while the drawing it will replace stays on screen and interactive.
    pub(crate) fn deriving(&self) -> bool {
        self.in_flight.is_some()
    }

    /// Whether what `mode` draws is on its way, so a mode selected meanwhile
    /// waits on it: the derivation in flight will produce it, or it is a map,
    /// which the document's placement lands.
    fn underway(&self, mode: LayoutMode) -> bool {
        self.deriving_for(mode)
            || (mode == LayoutMode::Map
                && self
                    .slice
                    .as_ref()
                    .is_some_and(|slice| slice.map().is_none()))
    }

    /// Whether the derivation in flight will produce what `mode` draws, by which
    /// a mode selected while one runs waits on it only where it will answer for
    /// that mode.
    fn deriving_for(&self, mode: LayoutMode) -> bool {
        self.in_flight.is_some_and(|in_flight| match mode {
            LayoutMode::Map => true,
            LayoutMode::Layered => in_flight.lays_out,
        })
    }

    /// The map of the restriction the panel draws through its filter, where that
    /// restricts the document's; absent too where a derivation restricting
    /// afresh will replace it.
    fn restricted_map_drawn(&self) -> Option<&RestrictedMap> {
        if self.restricting() {
            return None;
        }
        self.slice.as_ref()?.restricted_map()
    }

    /// Whether a derivation has to restrict the document's graph afresh rather
    /// than lay out over the restriction the panel holds, which it must wherever
    /// that restriction was derived through another lens — as it is for every
    /// request made before a filter change has landed.
    pub(crate) fn restricting(&self) -> bool {
        self.slice
            .as_ref()
            .is_some_and(|slice| !self.draws_through(&slice.lens))
    }

    /// Why `mode` draws nothing, where that is the view being too dense to lay
    /// out.
    pub(crate) fn too_dense(&self, mode: LayoutMode) -> Option<TooDense> {
        match mode {
            LayoutMode::Map => None,
            LayoutMode::Layered => self.layered.as_ref()?.laid.as_ref().err().copied(),
        }
    }

    pub(crate) fn over_budget(&self) -> Option<Overflow> {
        self.over_budget
    }

    /// Takes `landed` as what this panel draws.
    fn adopt(&mut self, landed: Landed) -> Adopted {
        self.in_flight = None;
        self.drawings += 1;
        self.expanded = landed.expanded;
        self.revealing = None;
        let restriction = landed.restricted.map(|restricted| {
            self.over_budget = restricted.over_budget;
            Restriction {
                map: landed.slice.restricted_map().cloned(),
            }
        });
        let frame = std::mem::take(&mut self.frame_on_landing) && restriction.is_some();
        self.slice = Some(landed.slice);
        let landing = match landed.layered {
            Relaid::Nothing => None,
            Relaid::Held => self.layered.as_ref().map(|held| LayoutLanded {
                replaced: None,
                current: held.view(),
            }),
            Relaid::Fresh(layered) => {
                let current = layered.view();
                let replaced = self.layered.replace(layered).map(|held| held.view());
                Some(LayoutLanded { replaced, current })
            }
        };
        Adopted {
            restriction,
            frame,
            landing,
        }
    }
}

/// The status line describing what a panel draws: what the read left where it
/// left no graph, and the size of the drawing `drawn` otherwise, followed by
/// `over_budget` where the panel's view is past the caps on an opening view.
/// `drawn` pairs the drawing with the zoom its view is resolved at, where one is.
pub(crate) fn status(
    source: &Source,
    drawn: Option<(Drawn, f32)>,
    over_budget: Option<Overflow>,
) -> String {
    let viewed = drawn
        .as_ref()
        .and_then(|(drawn, zoom)| Some((drawn, *zoom, drawn.view(*zoom)?)));
    source.status("No graph", |_| match viewed {
        None => "Laying out…".into(),
        Some((drawn, zoom, g)) => {
            let level = drawn
                .level(zoom)
                .map(|l| format!("level {}/{} · ", l.index + 1, l.count))
                .unwrap_or_default();
            format!(
                "{level}{} boxes · {} connectors · {}×{}{}",
                g.boxes.len(),
                g.connectors.len(),
                g.width.round() as i64,
                g.height.round() as i64,
                over_budget.map(past_the_caps).unwrap_or_default(),
            )
        }
    })
}

/// What a view no fold brings within the caps is past them by, so that a graph
/// drawn fully collapsed says why it is.
fn past_the_caps(over: Overflow) -> String {
    let past: Vec<String> = [
        (over.nodes, DEFAULT_MAX_NODES, "boxes"),
        (over.edges, DEFAULT_MAX_EDGES, "connectors"),
    ]
    .into_iter()
    .filter(|&(reached, cap, _)| reached > cap)
    .map(|(reached, cap, what)| format!("{reached} {what} over {cap}"))
    .collect();
    format!(" · fully folded and still {}", past.join(" and "))
}

/// Adopts the graph the document has finished reading, opening at the expansion
/// the read sized, and lays out what the selected mode draws.
pub(crate) fn opened(view: &mut GraphPanel, cx: &mut Context<GraphPanel>) {
    let Some(loaded) = view.model.read(cx).loaded() else {
        return;
    };
    view.derived.opened_on(loaded);
    crate::why::reset(view);
    crate::reveal::reset(view);
    crate::selection::remark(view, cx);
    crate::jump::graph::reset(view, cx);
    let scoped = view.model.read(cx).filter().applied().is_some();
    follow(view, scoped, cx);
    ensure_layout(view, cx);
}

/// Derives what `mode` draws where the panel has no drawing of it yet and no
/// derivation that will produce one is already running.
pub(crate) fn ensure_layout(view: &mut GraphPanel, cx: &mut Context<GraphPanel>) {
    if view.derived.drawn(view.mode).is_none() && !view.derived.underway(view.mode) {
        request(view, cx);
    }
}

/// Derives the levels of the map the panel's restriction draws, now the
/// document's placement it is made from has landed.
pub(crate) fn placed(view: &mut GraphPanel, cx: &mut Context<GraphPanel>) {
    if let Some(restricted) = view.derived.restricted_map_drawn() {
        restricted.derive_levels(cx);
    }
}

/// Derives the view a frame of `drawn` at `zoom` calls for off the UI thread
/// where it has yet to be, and redraws the canvas once it lands. A view already
/// asked for is left to the derivation under way.
pub(crate) fn derive_view(
    view: &mut GraphPanel,
    drawn: &Drawn,
    zoom: f32,
    cx: &mut Context<GraphPanel>,
) {
    let Some(key) = drawn.underived(zoom) else {
        return;
    };
    let asked = &mut view.derived.views_asked;
    asked.retain(|(asked, _)| asked.drawing() == key.drawing());
    if asked.iter().any(|(asked, _)| *asked == key) {
        return;
    }
    let drawn = drawn.clone();
    let work = load_in_background(
        cx,
        move || {
            drawn.derive_view(zoom);
        },
        |view: &mut GraphPanel, (), cx| view.invalidate_canvas(cx),
    );
    view.derived.views_asked.push((key, work));
}

/// Flips whether `id` is expanded and lays the graph out at the new expansion.
pub(crate) fn toggle_expansion(view: &mut GraphPanel, id: Id, cx: &mut Context<GraphPanel>) {
    if !view.derived.expanded.remove(&id) {
        view.derived.expanded.insert(id);
    }
    request(view, cx);
}

/// Expands every one of `ids`, laying the graph out once for all of them.
pub(crate) fn expand(
    view: &mut GraphPanel,
    ids: impl IntoIterator<Item = Id>,
    cx: &mut Context<GraphPanel>,
) {
    view.derived.expanded.extend(ids);
    request(view, cx);
}

/// Whether the layered layout last requested draws `id` expanded.
pub(crate) fn is_expanded(view: &GraphPanel, id: Id) -> bool {
    view.derived.expanded.contains(&id)
}

/// Opens the layered layout onto `id`: every container holding it, and the
/// merge group it would fold into, laying the graph out afresh where that opens
/// any. Like a container opened by hand, what this opens is not held to the caps
/// sizing the opening view: the node must be drawn to be shown.
pub(crate) fn expand_to(view: &mut GraphPanel, id: Id, cx: &mut Context<GraphPanel>) {
    let derived = &view.derived;
    let drawn = derived
        .layered
        .as_ref()
        .and_then(|layered| layered.laid.as_ref().ok())
        .is_some_and(|laid| laid.view.boxes.iter().any(|b| b.id == id));
    if drawn && !derived.deriving() && !derived.restricting() {
        return;
    }
    view.derived.revealing = Some(id);
    request(view, cx);
}

/// The innermost box of the layered drawing on screen standing for `id`, as
/// [`Layered::holder`] finds it, opening none. `None` where the drawing holds
/// none, or is yet to be derived.
pub(crate) fn drawn_holder(view: &GraphPanel, id: Id) -> Option<Id> {
    view.derived.layered.as_ref()?.holder(id)
}

/// A node of the graph the box `id` of the drawing on screen stands for, as
/// [`Layered::member`] names it.
pub(crate) fn member(view: &GraphPanel, id: Id) -> Id {
    match (view.mode, &view.derived.layered) {
        (LayoutMode::Layered, Some(layered)) => layered.member(id),
        _ => id,
    }
}

/// The nodes of the graph the box `id` of the drawing on screen gathers, as
/// [`Layered::gathered`] names them.
pub(crate) fn gathered(view: &GraphPanel, id: Id) -> Vec<Id> {
    match (view.mode, &view.derived.layered) {
        (LayoutMode::Layered, Some(layered)) => layered.gathered(id),
        _ => Vec::new(),
    }
}

/// Opens `expanded` onto `id` in `base`: every container holding it, then the
/// merge group the quotient of what that leaves visible draws it in, minted
/// into `names` as the layout that follows will mint it. A node `base` does not
/// hold opens nothing.
fn open_onto(base: &Graph, expanded: &mut HashSet<Id>, id: Id, names: &mut Names) {
    let mut parents: HashMap<Id, Option<Id>> = HashMap::with_capacity(base.nodes.len());
    parents.extend(base.nodes.iter().map(|node| (node.id, node.parent)));
    let Some(&first) = parents.get(&id) else {
        return;
    };
    let parent = |id: Id| parents.get(&id).copied().flatten();
    expanded.extend(std::iter::successors(first, |&container| parent(container)));
    let visible = bviz_graph_layout::lod::collapse(base, expanded);
    if let Some(group) = bviz_graph_layout::quotient::group_holding(&visible, id, names) {
        expanded.insert(group);
    }
}

/// Has the layered layout keep only the transitive reduction of the edges where
/// `reduced`, and every edge otherwise, and lays the graph out over the edge set
/// that leaves. A drawing offering no reduction is left as it is.
pub(crate) fn set_reduced(view: &mut GraphPanel, reduced: bool, cx: &mut Context<GraphPanel>) {
    if view
        .derived
        .reduced()
        .is_some_and(|standing| standing != reduced)
    {
        view.derived.reduced = reduced;
        request(view, cx);
    }
}

/// Takes up a change to the document's filter as [`follow`] does, `frame`
/// saying whether it named the part of the graph to look at; a panel out of
/// sight takes it up once it is shown again, laying nothing out meanwhile.
pub(crate) fn filter_changed(view: &mut GraphPanel, frame: bool, cx: &mut Context<GraphPanel>) {
    match view.shown {
        true => follow(view, frame, cx),
        false => {
            let unfollowed = view.derived.unfollowed.get_or_insert(false);
            *unfollowed |= frame;
        }
    }
}

/// Takes up the changes to the document's filter made while the panel was out
/// of sight.
pub(crate) fn catch_up(view: &mut GraphPanel, cx: &mut Context<GraphPanel>) {
    if let Some(frame) = view.derived.unfollowed {
        follow(view, frame, cx);
    }
}

/// Draws the graph through the document's filter as it stands, laid out by
/// the panel's own rules, the camera moving onto what that leaves drawn where
/// `frame` is set, for a change that named the part of the graph to look at.
/// What survives stays where the document places it, so on a graph whose
/// extent the rest of it still spans the named part need not be anywhere near
/// the view.
///
/// The view keeps what it can of where the reader had it: the expansion as far
/// as the surviving nodes and the budget allow, the camera — save where
/// what survives leaves nothing under it ([`crate::framing::Frame::Escaping`]) —
/// the edge set and, the panel's own to keep, the selection. The drawing on
/// screen stays until the one this asks for lands, and dependency paths drawn
/// in the filter's place stay drawn, the filter taking effect once they are
/// cleared.
pub(crate) fn follow(view: &mut GraphPanel, frame: bool, cx: &mut Context<GraphPanel>) {
    let frame = frame | view.derived.unfollowed.take().unwrap_or(false);
    let filter = view.derived.filter_over(view.model.read(cx).filter());
    if view.derived.filter == filter {
        return;
    }
    view.derived.filter = filter;
    crate::boxes::restate(view, cx);
    if view.derived.paths.is_none() {
        view.derived.frame_on_landing = frame;
        request(view, cx);
    }
}

/// Puts away the dependency paths drawn in the filter's place, for a gesture
/// in this panel changing what the filter draws, whose reader means to see
/// that.
pub(crate) fn show_filter(view: &mut GraphPanel, cx: &mut Context<GraphPanel>) {
    crate::why::put_away(view, cx);
    if view.derived.show_paths(None) {
        request(view, cx);
    }
}

/// Lays the graph out with the boxes of the classes `unwrapped` unwrapped
/// from here on, as [`follow`] draws it.
pub(crate) fn set_unwrapped(
    view: &mut GraphPanel,
    unwrapped: BTreeSet<BoxClass>,
    cx: &mut Context<GraphPanel>,
) {
    show_filter(view, cx);
    view.derived.unwrapped = unwrapped;
    follow(view, false, cx);
}

/// Shows `id` in this panel however its layout unwraps boxes.
pub(crate) fn reveal_in_layout(view: &mut GraphPanel, id: Id, cx: &mut Context<GraphPanel>) {
    show_filter(view, cx);
    view.derived.revealed.insert(id);
    follow(view, false, cx);
}

/// Withdraws the reveals in this panel that `withdrawn` keeps, reporting
/// whether it withdrew any.
pub(crate) fn withdraw_reveals(
    view: &mut GraphPanel,
    withdrawn: impl Fn(Id) -> bool,
    cx: &mut Context<GraphPanel>,
) -> bool {
    let before = view.derived.revealed.len();
    view.derived.revealed.retain(|&id| !withdrawn(id));
    let any = view.derived.revealed.len() != before;
    if any {
        follow(view, false, cx);
    }
    any
}

/// Draws `paths` in place of what the filter leaves, or the filter again where
/// `None`, and moves the camera onto the drawing that lands.
pub(crate) fn set_paths(
    view: &mut GraphPanel,
    paths: Option<Arc<Paths>>,
    cx: &mut Context<GraphPanel>,
) {
    if view.derived.show_paths(paths) {
        view.derived.frame_on_landing = true;
        request(view, cx);
    }
}

/// Derives what the panel draws off the UI thread and adopts the result.
fn request(view: &mut GraphPanel, cx: &mut Context<GraphPanel>) {
    let model = view.model.read(cx);
    let Some(shared) = model.loaded().map(|l| l.shared.clone()) else {
        return;
    };
    // The document's own verdicts serve a panel whose rules add nothing to
    // its filter.
    let judged = match view.derived.lens() {
        Lens::Filter(filter) => (model.judgement())
            .filter(|judgement| judgement.judges(&filter, &shared.base))
            .cloned(),
        Lens::Paths(_) => None,
    };
    let Some(slice) = view.derived.slice.clone() else {
        return;
    };
    let restricting = view.derived.restricting();
    // A derivation that leaves the restriction standing was asked for to
    // redraw. One that restricts afresh lays a drawing out for the panel showing
    // one, and for the panel holding one, which would otherwise go on drawing
    // over a graph the filter has moved out from under it; a map needs none.
    let lays_out =
        !restricting || view.mode == LayoutMode::Layered || view.derived.layered.is_some();
    // A restriction's map derives the level on screen with the rest of the
    // restriction, so adopting and painting it on the UI thread find it ready.
    let map_zoom = view
        .drawn()
        .filter(|drawn| matches!(drawn, Drawn::Map { .. }))
        .and_then(|drawn| view.view_zoom(&drawn));
    let derived = &mut view.derived;
    derived.in_flight = Some(InFlight { lays_out });
    let (lens, reduced) = (derived.lens(), derived.draws_reduced());
    let (expanded, revealing) = (derived.expanded.clone(), derived.revealing);
    let held = derived.layered.clone();
    cx.notify();
    derived._work = load_in_background(
        cx,
        move || {
            let keeps_held = |base: &Arc<Graph>, expanded: &HashSet<Id>| {
                held.as_ref()
                    .is_some_and(|held| held.draws(base, expanded, reduced))
            };
            let (mut slice, restricted) = match restricting {
                true => {
                    let (mut next, restricted) = match &lens {
                        Lens::Filter(filter) => {
                            let judgement = judged.unwrap_or_else(|| {
                                Arc::new(Judgement::new(filter.clone(), Arc::clone(&shared.base)))
                            });
                            restrict(&shared, &judgement, &expanded, reduced)
                        }
                        Lens::Paths(paths) => restrict_to_paths(&shared, paths),
                    };
                    if let (Some(zoom), Some(map)) = (map_zoom, next.map()) {
                        map.level(map.level_for_scale(zoom));
                    }
                    // The drawing kept names the groups its layout minted.
                    if keeps_held(&next.base, &restricted.expanded) {
                        next.names = slice.names;
                    }
                    (next, Some(restricted))
                }
                false => (slice, None),
            };
            let mut expanded = match &restricted {
                Some(restricted) => restricted.expanded.clone(),
                None => expanded,
            };
            if let Some(id) = revealing {
                open_onto(&slice.base, &mut expanded, id, &mut slice.names);
            }
            let layered = match lays_out {
                false => Relaid::Nothing,
                true if keeps_held(&slice.base, &expanded) => Relaid::Held,
                true => {
                    let laid = build::relayout(
                        &slice.base,
                        &slice.reduced,
                        &expanded,
                        &mut slice.names,
                        reduced,
                        &slice.transitions,
                        held.as_ref()
                            .and_then(|held| held.laid.as_ref().ok())
                            .map(|laid| &laid.view.anchors),
                    );
                    Relaid::Fresh(Layered {
                        laid: laid.map(|(view, groups)| {
                            Laid::new(Arc::new(view), Arc::new(groups), &slice.base)
                        }),
                        base: Arc::clone(&slice.base),
                        expanded: expanded.clone(),
                        reduced,
                    })
                }
            };
            Landed {
                slice,
                restricted,
                expanded,
                layered,
            }
        },
        |view, landed, cx| {
            let Adopted {
                restriction,
                frame,
                landing,
            } = view.derived.adopt(landed);
            let restricted = restriction.is_some();
            if let Some(map) = restriction.and_then(|restriction| restriction.map) {
                map.derive_levels(cx);
            }
            if frame {
                view.framing.request_frame(Frame::Drawing);
            } else if restricted {
                view.framing.request_frame(Frame::Escaping);
            }
            if let Some(landing) = landing {
                view.settle_onto(&landing, cx);
            }
            crate::selection::remark(view, cx);
            crate::reveal::landed(view, cx);
            view.drawing_moved(cx);
            view.invalidate_canvas(cx);
        },
    );
}

/// The graph `judgement`'s filter leaves of `shared`'s, with `expanded` held to
/// what survives it and to the caps that bound a legible drawing.
///
/// A filter that hides nothing leaves the shared graph itself, with the
/// reduction, containers and placement the document holds for it.
fn restrict(
    shared: &Shared,
    judgement: &Judgement,
    expanded: &HashSet<Id>,
    reduced: bool,
) -> (Slice, Restricted) {
    let verdicts = judgement.verdicts();
    // Every node lands in `keep` or in `dropped`, and `faded` is a part of what
    // `keep` holds, so the node count bounds all three.
    let nodes = shared.base.nodes.len();
    let mut keep: FxHashSet<Id> = FxHashSet::with_capacity_and_hasher(nodes, Default::default());
    let mut dropped: HashSet<Id> = HashSet::with_capacity(nodes);
    let mut faded: HashSet<Id> = HashSet::with_capacity(nodes);
    for node in &shared.base.nodes {
        match verdicts.get(node.id) {
            Verdict::Hidden => {
                dropped.insert(node.id);
            }
            verdict => {
                keep.insert(node.id);
                if verdict == Verdict::Faded {
                    faded.insert(node.id);
                }
            }
        }
    }
    let faded = Arc::new(faded);
    let lens = Through::Filter {
        filter: judgement.filter().clone(),
        verdicts,
    };
    let slice = match dropped.is_empty() {
        true => Slice {
            lens,
            faded,
            ..Slice::whole(shared)
        },
        false => {
            let base = bviz_graph_layout::filter::filter(&shared.base, &shared.reduced_base, &keep);
            restricted_slice(shared, lens, base, faded, Arc::default())
        }
    };
    let restricted = keeping(&slice, expanded, dropped, reduced);
    (slice, restricted)
}

/// The graph `paths` leaves of `shared`'s, opened as far as the caps that bound
/// a legible drawing allow: the reader named exactly what to look at.
fn restrict_to_paths(shared: &Shared, paths: &Arc<Paths>) -> (Slice, Restricted) {
    let base = paths.subgraph(&shared.base);
    let mut held = vec![false; bviz_graph_layout::graph::id_span(&shared.base)];
    for node in &base.nodes {
        held[node.id.0 as usize] = true;
    }
    let opened = bviz_graph_layout::lod::default_expansion(
        &base,
        None,
        DEFAULT_MAX_NODES,
        DEFAULT_MAX_EDGES,
    );
    let (expanded, over_budget) = match opened {
        Ok(expanded) => (expanded, None),
        Err(overflow) => (HashSet::new(), Some(overflow)),
    };
    let lens = Through::Paths {
        paths: Arc::clone(paths),
        held: Arc::new(held),
    };
    let transitions = Arc::new(Transitions::of(&base, &shared.facets));
    let slice = restricted_slice(shared, lens, base, Arc::new(HashSet::new()), transitions);
    let restricted = Restricted {
        expanded,
        over_budget,
    };
    (slice, restricted)
}

/// `base`, a restriction of `shared`'s graph through `lens`, with its reduction,
/// its containers, and its map placed where the document places it; `faded` and
/// `transitions` are what the drawing marks of it.
fn restricted_slice(
    shared: &Shared,
    lens: Through,
    base: Graph,
    faded: Arc<HashSet<Id>>,
    transitions: Arc<Transitions>,
) -> Slice {
    let reduction = bviz_graph_layout::reduce::transitive_reduction(&base);
    let containers = Arc::new(bviz_graph_layout::lod::expandable_containers(&base));
    let (base, reduction) = (Arc::new(base), Arc::new(reduction));
    Slice {
        lens,
        map: SliceMap::Restricted(RestrictedMap {
            placement: shared.map.clone(),
            base: Arc::clone(&base),
            reduced: Arc::clone(&reduction),
            transitions: Arc::clone(&transitions),
            map: Arc::default(),
            levels_asked: Arc::default(),
        }),
        base,
        reduced: reduction,
        containers,
        faded,
        transitions,
        names: Names::clone(&shared.names),
    }
}

/// What `slice` leaves of an expansion built up over the graph it restricts:
/// those of `expanded` it still holds boxes for, as far as the caps that bound a
/// legible view allow.
///
/// `dropped` names the nodes the filter took out. An expanded id beside those
/// and beside `slice`'s own containers is a group the quotient pass minted,
/// which the next layout mints afresh, so it is left alone.
fn keeping(
    slice: &Slice,
    expanded: &HashSet<Id>,
    dropped: HashSet<Id>,
    reduced: bool,
) -> Restricted {
    let mut surviving: HashSet<Id> = HashSet::with_capacity(expanded.len());
    surviving.extend(expanded.iter().copied().filter(|id| !dropped.contains(id)));
    match bviz_graph_layout::lod::fitting_expansion(
        &slice.base,
        reduced.then_some(slice.reduced.as_ref()),
        &surviving,
        DEFAULT_MAX_NODES,
        DEFAULT_MAX_EDGES,
    ) {
        Ok(expanded) => Restricted {
            expanded,
            over_budget: None,
        },
        // No expansion of this graph fits, and the unexpanded view is the
        // smallest drawing of it there is.
        Err(overflow) => Restricted {
            expanded: HashSet::new(),
            over_budget: Some(overflow),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Opening;
    use bviz_graph_layout::graph::Node;
    use bviz_graph_layout::select::labels::LabelIndex;
    use bviz_graph_layout::select::{Pin, Predicate, Rule, Values, Visibility};
    use bviz_graph_layout::{EdgeKind, Facets, Kind};

    /// [`document_placing`] with its map landed.
    fn document() -> Loaded {
        let (loaded, land) = document_placing();
        land();
        loaded
    }

    /// Two packages, each holding a target with two actions under it, the
    /// actions wired in a chain, prepared the way a read prepares a document's
    /// graph, and what lands the map placing it, which has yet to land.
    fn document_placing() -> (Loaded, impl FnOnce()) {
        let mut names = Names::default();
        let mut base = Graph::new();
        let mut actions: Vec<Id> = Vec::new();
        for package in ["a", "b"] {
            let package_id = names.intern(&format!("//{package}"));
            base.add_node(Node::new(package_id, package, Kind::Package, None));
            let target = names.intern(&format!("//{package}:t"));
            base.add_node(Node::new(target, "t", Kind::Target, Some(package_id)));
            for action in ["compile", "link"] {
                let id = names.intern(&format!("//{package}:t#{action}"));
                base.add_node(Node::new(id, action, Kind::Action, Some(target)));
                actions.push(id);
            }
        }
        for pair in actions.windows(2) {
            base.add_edge(pair[0], pair[1], EdgeKind::Aquery);
        }
        let reduced_base = bviz_graph_layout::reduce::transitive_reduction(&base);
        let containers = Arc::new(bviz_graph_layout::lod::expandable_containers(&base));
        let (base, reduced_base) = (Arc::new(base), Arc::new(reduced_base));
        let facets = Arc::new(Facets::default());
        let labels = Arc::new(LabelIndex::of(&base, &names, &facets));
        let map = bviz_graph_layout::map_layout(Arc::clone(&base), Arc::clone(&reduced_base));
        let (placement, landing) = Placement::pending();
        let loaded = Loaded::new(
            Shared {
                base,
                reduced_base,
                map: placement,
                names: Arc::new(names),
                facets,
                labels,
                containers,
                capabilities: bviz_graph_layout::capability::CapabilitiesBuilder::default().seal(),
                provenance: Arc::default(),
                inputs: bviz_graph_layout::Input::Aquery.into(),
                config_names: Arc::default(),
            },
            Opening {
                expanded: HashSet::new(),
                over_budget: None,
            },
        );
        (loaded, move || landing.land(Arc::new(map)))
    }

    fn id(loaded: &Loaded, name: &str) -> Id {
        loaded.shared.names.get(name).expect("an interned node")
    }

    fn kind_of(loaded: &Loaded, id: Id) -> Kind {
        loaded
            .shared
            .base
            .nodes
            .iter()
            .find(|node| node.id == id)
            .expect("a node of the document's graph")
            .kind
    }

    /// The nodes of `shared`'s graph `slice` leaves out.
    fn dropped(shared: &Shared, slice: &Slice) -> HashSet<Id> {
        let kept: HashSet<Id> = slice.base.nodes.iter().map(|node| node.id).collect();
        let nodes = shared.base.nodes.iter().map(|node| node.id);
        nodes.filter(|id| !kept.contains(id)).collect()
    }

    /// The graph `filter` leaves of `shared`'s, as [`restrict`] leaves it.
    fn restrict_through(
        shared: &Shared,
        filter: &Filter,
        expanded: &HashSet<Id>,
        reduced: bool,
    ) -> (Slice, Restricted) {
        let judgement = Judgement::new(filter.clone(), Arc::clone(&shared.base));
        restrict(shared, &judgement, expanded, reduced)
    }

    /// A filter that draws everything but the nodes of `kind`.
    fn hiding(kind: Kind) -> Filter {
        Filter {
            rules: vec![Rule {
                predicate: Predicate {
                    kinds: Values::new([kind]),
                    ..Predicate::default()
                },
                visibility: Visibility::Hidden,
            }],
            ..Filter::default()
        }
    }

    fn finest(map: Option<&Arc<MapLayout>>) -> &LaidOut {
        let map = map.expect("a placed map");
        map.level(map.level_count() - 1)
    }

    /// The panel's own view of a graph opened with no filter set.
    fn opened(loaded: &Loaded) -> Derived {
        let mut derived = Derived::new();
        derived.opened_on(loaded);
        derived
    }

    /// A filter drawing every node leaves the document's own graph, placement
    /// and all, so a panel that sets no filter draws exactly what the document
    /// derived and pays nothing for the filter.
    #[test]
    fn a_filter_that_hides_nothing_draws_the_document_graph_itself() {
        let loaded = document();
        let expanded: HashSet<Id> = [id(&loaded, "//a:t")].into_iter().collect();

        let (slice, restricted) =
            restrict_through(&loaded.shared, &Filter::default(), &expanded, true);

        assert!(Arc::ptr_eq(&slice.base, &loaded.shared.base));
        assert!(Arc::ptr_eq(&slice.reduced, &loaded.shared.reduced_base));
        assert!(Arc::ptr_eq(
            slice.map().expect("a placed map"),
            loaded.shared.map.placed().expect("a placed map"),
        ));
        assert!(dropped(&loaded.shared, &slice).is_empty());
        assert_eq!(restricted.expanded, expanded);
        assert_eq!(restricted.over_budget, None);
    }

    #[test]
    fn a_filter_hiding_a_kind_leaves_no_box_for_those_nodes() {
        let loaded = document();

        let (slice, _) =
            restrict_through(&loaded.shared, &hiding(Kind::Action), &HashSet::new(), true);

        let dropped = dropped(&loaded.shared, &slice);
        assert!(
            dropped
                .iter()
                .all(|&id| kind_of(&loaded, id) == Kind::Action)
        );
        assert!(!dropped.is_empty());
        assert!(
            slice
                .base
                .nodes
                .iter()
                .all(|node| node.kind != Kind::Action)
        );
        assert!(
            finest(slice.map())
                .boxes
                .iter()
                .all(|b| kind_of(&loaded, b.id) != Kind::Action),
        );
        assert!(
            finest(loaded.shared.map.placed())
                .boxes
                .iter()
                .any(|b| kind_of(&loaded, b.id) == Kind::Action),
            "the document's own drawing holds none of the boxes the filter hides",
        );
    }

    /// The level-of-detail fold collapses a container's descendants into it,
    /// which leaves the container standing for them rather than standing
    /// empty, so a coarse view draws every container the filter keeps.
    #[test]
    fn a_container_whose_descendants_are_folded_is_still_drawn() {
        let loaded = document();
        let (slice, _) =
            restrict_through(&loaded.shared, &hiding(Kind::Target), &HashSet::new(), true);

        let coarsest = slice.map().expect("a placed map").level(0);
        assert!(
            coarsest.boxes.iter().any(|b| b.id == id(&loaded, "//a")),
            "the container the fold collapses into",
        );
        assert!(
            !coarsest
                .boxes
                .iter()
                .any(|b| b.id == id(&loaded, "//a:t#compile")),
            "the coarsest view folded nothing into that container",
        );
    }

    /// The document, and the slice hiding its actions leaves of it.
    fn actions_hidden() -> (Loaded, Slice) {
        let loaded = document();
        let (slice, _) =
            restrict_through(&loaded.shared, &hiding(Kind::Action), &HashSet::new(), true);
        (loaded, slice)
    }

    #[test]
    fn a_target_whose_children_a_filter_hides_shrinks_in_place() {
        let (loaded, slice) = actions_hidden();
        let target = id(&loaded, "//a:t");

        let whole = finest(loaded.shared.map.placed())
            .boxes
            .iter()
            .find(|b| b.id == target)
            .expect("the document draws the target");
        let shrunk = finest(slice.map())
            .boxes
            .iter()
            .find(|b| b.id == target)
            .expect("the filter keeps the target");
        assert_eq!((shrunk.x, shrunk.y), (whole.x, whole.y));
        assert!(shrunk.width < whole.width && shrunk.height < whole.height);
    }

    /// A restriction places what survives where the whole graph places it, so a
    /// filter change leaves the view looking at the same place rather than at a
    /// graph redrawn under it.
    #[test]
    fn a_restricted_map_draws_a_survivor_where_the_document_draws_it() {
        let (loaded, slice) = actions_hidden();

        let whole = finest(loaded.shared.map.placed());
        for b in &finest(slice.map()).boxes {
            let at = whole
                .boxes
                .iter()
                .find(|whole| whole.id == b.id)
                .expect("a surviving box the document draws");
            assert_eq!((b.x, b.y), (at.x, at.y));
        }
    }

    /// A filter set before the map lands is drawn in the map once it does, placed
    /// where the document places it and shared by every clone of the
    /// restriction, so its levels are derived once.
    #[test]
    fn a_restriction_made_before_the_map_lands_is_placed_where_the_document_places_it() {
        let (loaded, land) = document_placing();
        let (slice, _) =
            restrict_through(&loaded.shared, &hiding(Kind::Action), &HashSet::new(), true);
        assert!(slice.map().is_none());

        land();

        let placed = slice
            .map()
            .expect("the restriction placed once the map lands");
        assert!(!Arc::ptr_eq(
            placed,
            loaded.shared.map.placed().expect("a placed map")
        ));
        assert!(Arc::ptr_eq(
            placed,
            slice.clone().map().expect("a placed map")
        ));
        let whole = finest(loaded.shared.map.placed());
        for b in &finest(slice.map()).boxes {
            let at = whole
                .boxes
                .iter()
                .find(|whole| whole.id == b.id)
                .expect("a surviving box the document draws");
            assert_eq!((b.x, b.y), (at.x, at.y));
        }
    }

    /// Ids outlive a restriction, so what the reader had opened goes on naming
    /// the same boxes; only what the filter took out gives way.
    #[test]
    fn an_expansion_outlives_a_filter_change() {
        let loaded = document();
        let (kept, hidden) = (id(&loaded, "//a:t"), id(&loaded, "//b:t"));
        let expanded: HashSet<Id> = [kept, hidden].into_iter().collect();
        let filter = Filter {
            pins: [(hidden, Pin::Set(Visibility::Hidden))]
                .into_iter()
                .collect(),
            ..Filter::default()
        };

        let (_, restricted) = restrict_through(&loaded.shared, &filter, &expanded, true);

        assert_eq!(restricted.expanded, [kept].into_iter().collect());
    }

    /// A derivation laying no layered drawing out settles nothing for a panel
    /// that switches to the layered mode while it runs, which must therefore
    /// ask for a drawing of its own rather than wait on that derivation.
    #[test]
    fn a_derivation_laying_out_no_drawing_answers_for_the_map_alone() {
        let loaded = document();
        let mut derived = opened(&loaded);
        derived.in_flight = Some(InFlight { lays_out: false });

        assert!(derived.deriving_for(LayoutMode::Map));
        assert!(!derived.deriving_for(LayoutMode::Layered));

        derived.adopt(Landed {
            slice: Slice::whole(&loaded.shared),
            expanded: HashSet::new(),
            restricted: None,
            layered: Relaid::Nothing,
        });

        assert!(derived.drawn(LayoutMode::Layered).is_none());
        assert!(!derived.deriving_for(LayoutMode::Layered));
    }

    /// A filter naming what to look at that lands before the map is placed
    /// still asks for the camera to frame what it leaves, and hands the panel
    /// the restriction's map, which is placed once the document's lands.
    #[test]
    fn a_framed_filter_landing_before_the_map_frames_the_map_once_placed() {
        let (loaded, land) = document_placing();
        let mut derived = opened(&loaded);
        derived.filter = hiding(Kind::Action);
        derived.frame_on_landing = true;
        let (slice, restricted) =
            restrict_through(&loaded.shared, &derived.filter, &HashSet::new(), true);

        let adopted = derived.adopt(Landed {
            slice,
            expanded: restricted.expanded.clone(),
            restricted: Some(restricted),
            layered: Relaid::Nothing,
        });
        assert!(adopted.frame);
        let map = adopted
            .restriction
            .and_then(|restriction| restriction.map)
            .expect("the restriction's own map");
        assert!(map.placed().is_none());
        assert!(derived.drawn(LayoutMode::Map).is_none());

        land();

        let Some(Drawn::Map { map: drawn, .. }) = derived.drawn(LayoutMode::Map) else {
            panic!("no map drawn once the document's lands");
        };
        assert!(Arc::ptr_eq(map.placed().expect("a placed map"), &drawn));
    }

    /// The map landing derives the levels of the restriction a panel draws, and
    /// none of one a derivation in flight is to replace.
    #[test]
    fn the_map_landing_derives_only_a_restriction_that_stays() {
        let (loaded, land) = document_placing();
        let mut derived = opened(&loaded);
        assert!(derived.restricted_map_drawn().is_none(), "the whole graph");

        derived.filter = hiding(Kind::Action);
        let (slice, restricted) =
            restrict_through(&loaded.shared, &derived.filter, &HashSet::new(), true);
        derived.adopt(Landed {
            slice,
            expanded: restricted.expanded.clone(),
            restricted: Some(restricted),
            layered: Relaid::Nothing,
        });
        land();
        let drawn = derived
            .restricted_map_drawn()
            .expect("the restriction the panel draws");
        assert!(drawn.placed().is_some());

        derived.filter = hiding(Kind::Target);
        assert!(derived.restricted_map_drawn().is_none());
    }

    #[test]
    fn a_request_made_before_a_filter_change_lands_restricts_afresh() {
        let loaded = document();
        let mut derived = opened(&loaded);
        assert!(!derived.restricting());

        derived.filter = hiding(Kind::Action);
        assert!(derived.restricting());

        let (slice, restricted) = restrict_through(
            &loaded.shared,
            &derived.filter,
            &derived.expanded,
            derived.reduced,
        );
        derived.adopt(Landed {
            slice,
            expanded: restricted.expanded.clone(),
            restricted: Some(restricted),
            layered: Relaid::Nothing,
        });

        assert!(!derived.restricting());
    }

    /// The controls read the filter and the drawing is derived from it, so a
    /// filter the panel held over a graph that has been replaced would have the
    /// two describing different graphs — and a drawing already in hand answers
    /// the request for one, so nothing would re-derive it.
    #[test]
    fn a_document_opened_into_a_panel_is_drawn_through_no_filter() {
        let loaded = document();
        let mut derived = opened(&loaded);
        derived.filter = hiding(Kind::Action);
        derived.layered = Some(layered(&loaded, 10.0));

        derived.opened_on(&document());

        assert_eq!(derived.filter, Filter::default());
        assert!(
            !derived.restricting(),
            "the drawing was derived from another filter than the controls read"
        );
        assert!(derived.drawn(LayoutMode::Layered).is_none());
    }

    /// The paths from the second action of `//a:t` to the first of `//b:t`,
    /// which the chain joins by one edge.
    fn across(loaded: &Loaded) -> Arc<Paths> {
        let found = bviz_graph_layout::paths::between(
            &loaded.shared.base,
            id(loaded, "//a:t#link"),
            id(loaded, "//b:t#compile"),
        )
        .expect("a path");
        Arc::new(found.paths)
    }

    /// Dependency paths draw the nodes on them in the packages and targets
    /// holding them, and no edge but theirs.
    #[test]
    fn paths_draw_their_nodes_their_frames_and_their_edges_alone() {
        let loaded = document();
        let (slice, _) = restrict_to_paths(&loaded.shared, &across(&loaded));

        let mut drawn: Vec<&str> = slice
            .base
            .nodes
            .iter()
            .map(|node| loaded.shared.names.resolve(node.id))
            .collect();
        drawn.sort_unstable();
        assert_eq!(
            drawn,
            [
                "//a",
                "//a:t",
                "//a:t#link",
                "//b",
                "//b:t",
                "//b:t#compile"
            ]
        );
        let edges: Vec<(Id, Id)> = slice.base.edges.iter().map(|e| (e.from, e.to)).collect();
        assert_eq!(
            edges,
            [(id(&loaded, "//a:t#link"), id(&loaded, "//b:t#compile"))]
        );
        let dropped = dropped(&loaded.shared, &slice);
        let mut dropped: Vec<&str> = (dropped.iter())
            .map(|&id| loaded.shared.names.resolve(id))
            .collect();
        dropped.sort_unstable();
        assert_eq!(dropped, ["//a:t#compile", "//b:t#link"]);
    }

    /// Paths take the filter's place without overwriting it, so clearing them
    /// draws through the filter again at the expansion it had, and the
    /// reduction the reader chose is offered again.
    #[test]
    fn clearing_paths_returns_to_the_filter_and_its_expansion() {
        let loaded = document();
        let mut derived = opened(&loaded);
        derived.filter = hiding(Kind::Source);
        let expanded: HashSet<Id> = [id(&loaded, "//a:t")].into_iter().collect();
        derived.expanded = expanded.clone();
        let (slice, restricted) =
            restrict_through(&loaded.shared, &derived.filter, &derived.expanded, true);
        derived.adopt(Landed {
            slice,
            expanded: restricted.expanded.clone(),
            restricted: Some(restricted),
            layered: Relaid::Nothing,
        });
        assert!(!derived.restricting());

        assert!(derived.show_paths(Some(across(&loaded))));
        assert!(derived.restricting());
        assert_eq!(derived.reduced(), None);
        let (slice, restricted) = restrict_to_paths(&loaded.shared, &across(&loaded));
        assert!(
            !derived.draws_through(&slice.lens),
            "paths found afresh are told from the ones asked for"
        );
        let Through::Paths { held, .. } = &slice.lens else {
            panic!("a slice drawn through paths");
        };
        let lens = Through::Paths {
            paths: Arc::clone(&derived.paths.as_ref().expect("the paths asked for").paths),
            held: Arc::clone(held),
        };
        derived.adopt(Landed {
            slice: Slice { lens, ..slice },
            expanded: restricted.expanded.clone(),
            restricted: Some(restricted),
            layered: Relaid::Nothing,
        });
        assert!(!derived.restricting());

        assert!(derived.show_paths(None));
        assert!(!derived.show_paths(None), "nothing left to clear");
        assert_eq!(derived.filter, hiding(Kind::Source));
        assert_eq!(derived.expanded, expanded);
        assert_eq!(derived.reduced(), Some(derived.reduced));
        assert!(derived.restricting());
    }

    /// A package holding a target whose action reads two sources, which the
    /// quotient merges into one group.
    struct MergedSources {
        base: Graph,
        names: Names,
        package: Id,
        target: Id,
        sources: [Id; 2],
    }

    impl MergedSources {
        fn new() -> Self {
            let mut names = Names::default();
            let mut base = Graph::new();
            let package = names.intern("//p");
            base.add_node(Node::new(package, "p", Kind::Package, None));
            let target = names.intern("//p:t");
            base.add_node(Node::new(target, "t", Kind::Target, Some(package)));
            let action = names.intern("//p:t#compile");
            base.add_node(Node::new(action, "compile", Kind::Action, Some(target)));
            let sources = ["//p:a.c", "//p:b.c"].map(|name| {
                let source = names.intern(name);
                base.add_node(Node::new(source, name, Kind::Source, Some(target)));
                base.add_edge(source, action, EdgeKind::Aquery);
                source
            });
            Self {
                base,
                names,
                package,
                target,
                sources,
            }
        }

        /// The drawing laid out at `expanded`, over every edge.
        fn layered(&mut self, expanded: HashSet<Id>) -> Layered {
            let reduced_base = bviz_graph_layout::reduce::transitive_reduction(&self.base);
            let (view, groups) = build::relayout(
                &self.base,
                &reduced_base,
                &expanded,
                &mut self.names,
                false,
                &Transitions::default(),
                None,
            )
            .expect("the fixture is sparse enough to lay out");
            Layered {
                laid: Ok(Laid::new(Arc::new(view), Arc::new(groups), &self.base)),
                base: Arc::new(self.base.clone()),
                expanded,
                reduced: false,
            }
        }
    }

    /// Opening onto a source a target's action reads with another opens the
    /// package and target holding it and the group the two merge into, so the
    /// quotient the layout draws keeps a box of the source's own.
    #[test]
    fn opening_onto_a_merged_source_opens_its_containers_and_its_group() {
        let MergedSources {
            base,
            mut names,
            package,
            target,
            sources,
        } = MergedSources::new();
        let revealed = sources[1];
        let mut expanded = HashSet::new();

        open_onto(&base, &mut expanded, revealed, &mut names);

        assert!(expanded.contains(&package) && expanded.contains(&target));
        let visible = bviz_graph_layout::lod::collapse(&base, &expanded);
        let (drawn, _) = bviz_graph_layout::quotient::quotient(&visible, &expanded, &mut names);
        assert!(drawn.nodes.iter().any(|node| node.id == revealed));
    }

    /// A source merged into a collapsed group is stood for by the group's box,
    /// not the target holding the group; once the group opens, by its own.
    #[test]
    fn a_merged_source_is_held_by_its_group_box_until_the_group_opens() {
        let mut fixture = MergedSources::new();
        let [first, second] = fixture.sources;
        let folded = fixture.layered([fixture.package, fixture.target].into());

        let group = folded.holder(first).expect("a box standing for the source");
        assert_ne!(group, fixture.target);
        assert_ne!(group, first);
        assert_eq!(folded.holder(second), Some(group));

        let opened = fixture.layered([fixture.package, fixture.target, group].into());
        assert_eq!(opened.holder(first), Some(first));
    }

    /// A group's box is minted afresh by each layout, so what it stands for
    /// outside the drawing is one of the sources it gathers, the same one
    /// every time.
    #[test]
    fn a_group_box_stands_for_the_first_source_it_gathers() {
        let mut fixture = MergedSources::new();
        let [first, second] = fixture.sources;
        let folded = fixture.layered([fixture.package, fixture.target].into());

        let group = folded.holder(first).expect("a box standing for the source");
        assert_eq!(folded.member(group), first.min(second));
        assert_eq!(folded.member(fixture.target), fixture.target);
        assert_eq!(
            folded.gathered(group),
            [first.min(second), first.max(second)]
        );
        assert!(folded.gathered(fixture.target).is_empty());
    }

    #[test]
    fn opening_onto_a_node_the_graph_does_not_hold_opens_nothing() {
        let loaded = document();
        let mut names = Names::clone(&loaded.shared.names);
        let mut expanded = HashSet::new();

        open_onto(&loaded.shared.base, &mut expanded, Id(9_999), &mut names);

        assert!(expanded.is_empty());
    }

    /// What the panel draws through decides what keeps a node out of the
    /// drawing: a filter names the hiding its verdicts reached, and paths drawn
    /// in its place leave out every node off them.
    #[test]
    fn a_node_the_drawing_leaves_out_is_told_by_the_lens_it_was_drawn_through() {
        let loaded = document();
        let mut derived = opened(&loaded);
        let action = id(&loaded, "//a:t#compile");
        let target = id(&loaded, "//a:t");
        assert!(derived.hidden(action).is_none());

        derived.filter = hiding(Kind::Action);
        let (slice, restricted) =
            restrict_through(&loaded.shared, &derived.filter, &HashSet::new(), true);
        derived.adopt(Landed {
            slice,
            expanded: restricted.expanded.clone(),
            restricted: Some(restricted),
            layered: Relaid::Nothing,
        });
        assert!(matches!(
            derived.hidden(action),
            Some(Hidden::Filtered(Hiding::Rule(0)))
        ));
        assert!(derived.hidden(target).is_none());

        derived.show_paths(Some(across(&loaded)));
        let (slice, restricted) = restrict_to_paths(&loaded.shared, &across(&loaded));
        derived.adopt(Landed {
            slice,
            expanded: restricted.expanded.clone(),
            restricted: Some(restricted),
            layered: Relaid::Nothing,
        });
        assert!(matches!(derived.hidden(action), Some(Hidden::OffPaths)));
        assert!(derived.hidden(id(&loaded, "//a:t#link")).is_none());
    }

    /// A drawing whose extent says which derivation produced it.
    fn drawing(width: f64) -> Arc<LaidOut> {
        Arc::new(LaidOut {
            width,
            ..LaidOut::default()
        })
    }

    /// [`drawing`] as laid out from `loaded`'s whole graph, folded, over the
    /// reduced edge set.
    fn layered(loaded: &Loaded, width: f64) -> Layered {
        Layered {
            laid: Ok(Laid::new(
                drawing(width),
                Arc::default(),
                &loaded.shared.base,
            )),
            base: Arc::clone(&loaded.shared.base),
            expanded: HashSet::new(),
            reduced: true,
        }
    }

    fn landed(loaded: &Loaded, width: f64) -> Landed {
        Landed {
            slice: Slice::whole(&loaded.shared),
            expanded: HashSet::new(),
            restricted: None,
            layered: Relaid::Fresh(layered(loaded, width)),
        }
    }

    /// A layered drawing is laid out again only where the graph, the expansion
    /// or the edge set it would be laid out from differs from what drew it.
    #[test]
    fn a_drawing_answers_for_the_graph_expansion_and_edge_set_it_was_laid_out_from() {
        let loaded = document();
        let held = layered(&loaded, 10.0);
        let base = &loaded.shared.base;
        let folded = HashSet::new();
        let package = loaded.shared.names.get("//a").expect("a package");

        assert!(held.draws(base, &folded, true));
        assert!(!held.draws(base, &folded, false));
        assert!(!held.draws(base, &[package].into_iter().collect(), true));
        assert!(!held.draws(&Arc::new(Graph::clone(base)), &folded, true));
    }

    /// A derivation that keeps the drawing held lands it as a relayout would,
    /// so a toggle undone while its relayout ran still settles and fades, but
    /// with no drawing to ease from.
    #[test]
    fn a_derivation_keeping_the_drawing_held_lands_it_with_nothing_to_ease_from() {
        let loaded = document();
        let mut derived = opened(&loaded);
        derived.adopt(landed(&loaded, 10.0));
        let held = derived.layered.as_ref().expect("a drawing held").view();

        let adopted = derived.adopt(Landed {
            slice: Slice::whole(&loaded.shared),
            expanded: HashSet::new(),
            restricted: None,
            layered: Relaid::Held,
        });

        let landing = adopted.landing.expect("the held drawing lands");
        assert!(landing.replaced.is_none());
        assert!(Arc::ptr_eq(&landing.current, &held));
        let drawn = derived
            .drawn(LayoutMode::Layered)
            .expect("a layered drawing");
        assert_eq!(drawn.view(1.0).map(|g| g.width), Some(10.0));
    }

    /// Identifies the view `derived`'s map draws, deriving it first.
    fn map_key(derived: &Derived) -> ViewKey {
        let drawn = derived.drawn(LayoutMode::Map).expect("a map");
        drawn.derive_view(1.0);
        drawn.shown(1.0).expect("a derived view").key()
    }

    /// A graph opened in place of another is told from it even where no
    /// derivation has landed on either, so work keyed on the old graph's view
    /// is not taken for the new one's.
    #[test]
    fn a_document_opened_into_a_panel_identifies_the_drawing_it_leaves() {
        let mut derived = opened(&document());
        let first = map_key(&derived);

        derived.opened_on(&document());

        let second = map_key(&derived);
        assert_ne!(first, second);
    }

    /// A derivation adopted is told from the drawing it replaced, so work keyed
    /// on the view is redone when it lands.
    #[test]
    fn each_derivation_adopted_identifies_the_drawing_it_leaves() {
        let loaded = document();
        let mut derived = opened(&loaded);
        let first = map_key(&derived);

        derived.adopt(landed(&loaded, 10.0));

        let second = map_key(&derived);
        assert_ne!(first, second);
    }

    /// The drawing on screen is the panel's until a derivation lands, so a
    /// filter change never blanks the view.
    #[test]
    fn a_running_derivation_leaves_the_drawing_it_will_replace_on_screen() {
        let loaded = document();
        let mut derived = opened(&loaded);
        derived.adopt(landed(&loaded, 10.0));

        derived.in_flight = Some(InFlight { lays_out: true });

        assert!(derived.deriving());
        let drawn = derived
            .drawn(LayoutMode::Layered)
            .expect("a layered drawing");
        assert_eq!(drawn.view(1.0).map(|g| g.width), Some(10.0));
    }

    /// The status line is drawn from the read and the panel's own drawing, so it
    /// reads the same whether or not a panel is showing one.
    #[test]
    fn a_graph_still_being_read_says_so() {
        assert_eq!(status(&Source::Loading, None, None), "Loading…");
    }

    #[test]
    fn a_graph_that_could_not_be_read_names_what_is_absent() {
        let failed = Source::Failed("aquery.pb: no such file".into());
        assert_eq!(status(&failed, None, None), "No graph");
    }

    /// Describing a map's view on the UI thread derives none: with no level
    /// derived the status line reads as still laying out, and once one is, a
    /// zoom calling for another is described by the one drawn in its place.
    #[test]
    fn a_map_describes_the_derived_level_it_draws() {
        let loaded = document();
        let drawn = opened(&loaded).drawn(LayoutMode::Map).expect("a map");
        let Drawn::Map { map, .. } = &drawn else {
            panic!("a map draws a map");
        };
        let finest = map.level_count() - 1;
        assert!(finest > 0, "a level finer than the coarsest");
        let described = || {
            status(
                &Source::Loaded(document()),
                Some((drawn.clone(), f32::MAX)),
                None,
            )
        };

        assert_eq!(described(), "Laying out…");

        map.level(0);
        assert!(
            described().starts_with(&format!("level 1/{} · ", finest + 1)),
            "{}",
            described()
        );
        assert!(map.derived(finest).is_none());
        assert_eq!(
            drawn.shown(f32::MAX).map(Shown::key),
            drawn.shown(0.0).map(Shown::key)
        );
        assert!(drawn.underived(f32::MAX).is_some());
    }

    #[test]
    fn a_drawn_graph_describes_what_it_draws() {
        let loaded = document();
        let derived = opened(&loaded);
        let drawn = derived.drawn(LayoutMode::Map).expect("a map");
        let level = drawn.level(1.0).expect("a map draws one of its levels");
        let boxes = drawn.derive_view(1.0).boxes.len();
        let status = status(&Source::Loaded(document()), Some((drawn, 1.0)), None);
        assert!(
            status.starts_with(&format!(
                "level {}/{} · {boxes} boxes · ",
                level.index + 1,
                level.count
            )),
            "{status}",
        );
    }

    /// A graph too large to draw within the caps opens fully collapsed, which
    /// looks like a graph that needed no folding, so the status line says which
    /// count the view is past its cap on.
    #[test]
    fn a_view_no_fold_brings_within_the_caps_says_what_it_is_past() {
        let loaded = document();
        let derived = opened(&loaded);
        let drawn = derived.drawn(LayoutMode::Map).expect("a map");
        drawn.derive_view(1.0);
        let over = Overflow {
            nodes: DEFAULT_MAX_NODES,
            edges: DEFAULT_MAX_EDGES + 5,
        };

        let status = status(&Source::Loaded(document()), Some((drawn, 1.0)), Some(over));

        assert!(
            status.ends_with(&format!(
                " · fully folded and still {} connectors over {DEFAULT_MAX_EDGES}",
                over.edges
            )),
            "{status}",
        );
    }

    /// A panel drawing the map while it is being placed waits on the placement,
    /// which no derivation produces, and draws the map as soon as it lands; the
    /// layered mode, which needs no map, is its own to ask for.
    #[test]
    fn a_panel_opened_before_the_map_lands_draws_it_once_it_lands() {
        let (loaded, land) = document_placing();
        let derived = opened(&loaded);
        assert!(derived.drawn(LayoutMode::Map).is_none());
        assert!(derived.underway(LayoutMode::Map));
        assert!(!derived.underway(LayoutMode::Layered));

        land();

        assert!(derived.drawn(LayoutMode::Map).is_some());
        assert!(!derived.underway(LayoutMode::Map));
    }

    /// The map is placed as the graph is read and the layered layout only on
    /// demand, so a panel holds one and not the other until it asks for the
    /// layered mode.
    #[test]
    fn a_panel_holds_the_map_before_it_holds_a_layered_layout() {
        let loaded = document();
        let derived = opened(&loaded);
        assert!(derived.drawn(LayoutMode::Map).is_some());
        assert!(derived.drawn(LayoutMode::Layered).is_none());
        assert_eq!(
            status(&Source::Loaded(document()), None, None),
            "Laying out…"
        );
    }

    /// A map is opened by zooming rather than by clicking a box, so it offers
    /// nothing to open; the layered layout offers the containers of the graph
    /// the panel draws.
    #[test]
    fn only_the_layered_layout_offers_boxes_to_open() {
        let loaded = document();
        let derived = opened(&loaded);
        assert!(matches!(
            derived.expandable(LayoutMode::Map),
            Expandable::Nothing
        ));
        assert!(matches!(
            derived.expandable(LayoutMode::Layered),
            Expandable::Containers(_)
        ));
    }

    /// Nothing is expandable until there is a graph to expand, whichever mode a
    /// panel has selected.
    #[test]
    fn a_graph_still_being_read_offers_nothing_to_open() {
        for mode in LayoutMode::ALL {
            assert!(matches!(
                Derived::new().expandable(mode),
                Expandable::Nothing
            ));
        }
    }
}
