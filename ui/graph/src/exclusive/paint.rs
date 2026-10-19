//! Painting the icicle: each cell a box filled for what it holds and named
//! where it is wide enough to read.

use super::ancestry::{self, Spot};
use super::icicle::{self, Cell, Holds, Parent, Run};
use super::transition::{Placed, Transition};
use super::{RootedBy, SEVERAL, Shown, WHOLE_BUILD, what_is_shared};
use crate::Palette;
use crate::hints::hints_where_alike;
use crate::hue::Colouring;
use crate::model::Shared;
use crate::paint::UNMARKED_FADE;
use crate::point::{
    KindNames, configuration_readings, mnemonic, point_label, primary_output, unit_names,
};
use bviz_graph_layout::Id;
use bviz_graph_layout::dominators::{Dominators, ShownTree, Standing};
use bviz_graph_layout::metrics::Unit;
use bviz_graph_layout::select::{Verdict, Verdicts};
use bviz_graph_layout::shown::{ELLIPSIS, fit_label, name_start};
use bviz_time::Instant;
use bviz_ui_assets::shape_line;
use bviz_ui_components::{Inked, LabelBoxStyle, paint_labeled_box};
use bviz_ui_theme::{label_on, mix, over};
use gpui::{
    App, BorderStyle, Bounds, ContentMask, Hsla, PathBuilder, Pixels, Point, Rgba,
    TransformationMatrix, Window, point, px, quad, size,
};
use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

/// Inset of a cell's name from its edges, above and below as at either side.
const TEXT_PAD: f32 = 3.0;

/// Room between one row of cells and the next, so a cell reads apart from the
/// one beneath it where the two are filled alike.
const ROW_GAP: Pixels = px(1.);

/// The narrowest a cell is drawn on its own; narrower siblings share one.
pub(crate) const MIN_CELL_WIDTH: Pixels = px(4.);

/// Only cells at least this wide are named; narrower ones leave too little
/// room for legible text.
const LABEL_MIN_WIDTH: Pixels = px(28.);

/// How far the seam dividing a cell from its left neighbour is mixed from the
/// cell's fill towards the canvas: enough to read as a divider on any fill.
const SEAM_MIX: f32 = 0.35;

/// How far a cell gathering siblings too narrow to draw recedes towards the
/// canvas from the neutral it is filled with, so it reads as no point itself.
const REST_RECEDE: f32 = 0.5;

/// How long the legs of the notch in the corner of a root the build also
/// reaches as a dependency are: twice the pad a cell's name is inset by, the
/// longest whose slanted edge passes the corner of the name's box without
/// entering it.
pub(crate) const NOTCH: Pixels = px(2. * TEXT_PAD);

/// Whether `node` of `tree` is a root the build also reaches through a
/// dependency, which a notch filling the top-left corner of its cell marks.
pub(crate) fn reached(tree: &Dominators, node: u32) -> bool {
    match tree.standing(node) {
        Standing::ReachedRoot(_) => true,
        Standing::Root { .. } => tree.elsewhere(node).is_some(),
        Standing::Below(_) => false,
    }
}

/// The corners of the notch on a cell `width` wide, from the cell's top-left
/// corner, where its right angle lies: the right triangle with legs
/// [`NOTCH`] long, cut off at the cell's right edge, so a cell narrower than
/// the leg carries the part of it within the cell. On a cell as wide as the
/// leg or wider the two corners on the cut coincide at the end of the top leg.
fn notch(width: Pixels) -> [Point<Pixels>; 4] {
    let reach = width.min(NOTCH);
    [
        point(px(0.), px(0.)),
        point(reach, px(0.)),
        point(reach, NOTCH - reach),
        point(px(0.), NOTCH),
    ]
}

/// Inks the notch of the cell `rect` in `ink`. The notch is laid on the cell
/// as its edges round to device pixels, so its legs lie on the cell's edges
/// and it neither bleeds past the cell nor leaves a sliver of the cell's fill
/// beside them.
pub(crate) fn paint_notch(rect: Bounds<Pixels>, ink: Rgba, window: &mut Window) {
    let (origin, far) = (
        window.pixel_snap_point(rect.origin),
        window.pixel_snap_point(rect.bottom_right()),
    );
    let corners = notch(far.x - origin.x).map(|corner| origin + corner);
    let mut path = PathBuilder::fill();
    path.add_polygon(&corners, true);
    if let Ok(path) = path.build() {
        window.paint_path(path, ink);
    }
}

/// How tall a row of the icicle stands: a name at the inherited text size,
/// padded above and below, and the gap to the next row.
pub(crate) fn row_height(window: &Window) -> Pixels {
    bviz_ui_assets::text_metrics(window).font_size + px(2. * TEXT_PAD) + ROW_GAP
}

/// What the icicle paints from, owned so the `'static` paint closure holds no
/// borrow of the panel.
pub(crate) struct Scene {
    pub(crate) tree: Arc<Dominators>,
    /// What of `tree` the cells draw.
    pub(crate) shown: Arc<Shown>,
    /// How the roots of `tree` were chosen.
    pub(crate) by: RootedBy,
    pub(crate) colouring: Arc<Colouring>,
    /// What the cell spanning the top row holds.
    pub(crate) top: Holds,
    /// The move to `top` from the focus before it, where it is under way.
    pub(crate) transition: Option<Transition>,
    pub(crate) shared: Shared,
    /// The verdict of the filter the cells are faded by on each node, absent
    /// where none fades them.
    pub(crate) verdicts: Option<Arc<Verdicts>>,
    pub(crate) palette: Palette,
}

/// Paints the icicle of `scene` filling `bounds` as it stands at `now`, under
/// the rows of the ancestors of the cell it is drawn from.
///
/// While the icicle moves between two cells, one over the other, it is drawn
/// from as many rows down as stand over the cell it is drawn from at that
/// moment of the move, and a cell it carries up past that row takes on the
/// look of a row over it as it goes, so that each end of the move is drawn as
/// at rest. The rows under the moving cells are those over the higher cell
/// for the first half of the move and those over the lower for the second.
pub(crate) fn paint_icicle(
    scene: &Scene,
    bounds: Bounds<Pixels>,
    now: Instant,
    window: &mut Window,
    cx: &mut App,
) {
    let row = row_height(window);
    let tree = &scene.shown.tree;
    let lowered = scene
        .transition
        .as_ref()
        .map(|transition| transition.lowered(now));
    let (rows, region, placed) = frame(
        tree,
        scene.top,
        scene.transition.as_ref().zip(lowered),
        bounds,
        row,
    );
    let (above, _) = ancestry::lay_out(&rows, bounds, row);
    let trail: Vec<u32> = above
        .iter()
        .filter_map(|spot| match spot {
            Spot::Cell(cell) => cell.holds.node(),
            Spot::Hidden { .. } => None,
        })
        .collect();
    window.with_content_mask(Some(ContentMask { bounds }), |window| {
        paint_above(scene, &above, &trail, window, cx);
        paint_cells(scene, &placed, &trail, region.top(), row, window, cx);
    });
}

/// The rows over the icicle of `tree` drawn from `top` in `bounds`, a `row`
/// tall each, where the icicle under them starts, and its cells: at rest, or
/// `lowered` of the way through a move.
fn frame(
    tree: &impl icicle::Tree,
    top: Holds,
    moving: Option<(&Transition, f32)>,
    bounds: Bounds<Pixels>,
    row: Pixels,
) -> (Vec<ancestry::Row>, Bounds<Pixels>, Vec<Placed>) {
    match moving {
        None => {
            let rows = ancestry::rows(tree, top);
            let under = ancestry::below(bounds, row * rows.len() as f32);
            let cells = icicle::lay_out(tree, top, under, row, MIN_CELL_WIDTH);
            (rows, under, Placed::still(cells))
        }
        Some((transition, lowered)) => {
            let (higher, lower) = (
                ancestry::rows(tree, transition.higher()),
                ancestry::rows(tree, transition.lower()),
            );
            let (from, to) = (higher.len() as f32, lower.len() as f32);
            let region = ancestry::below(bounds, row * (from + (to - from) * lowered));
            let cells = transition.cells(tree, region, bounds, row, MIN_CELL_WIDTH, lowered);
            let rows = if lowered < HALF_WAY { higher } else { lower };
            (rows, region, cells)
        }
    }
}

/// How far through a move the rows over it turn from those over the higher
/// cell to those over the lower.
const HALF_WAY: f32 = 0.5;

/// The style every box of the icicle is drawn in.
fn box_style(palette: &Palette) -> LabelBoxStyle {
    LabelBoxStyle {
        background: palette.background,
        seam_mix: SEAM_MIX,
        label_min_px: f32::from(LABEL_MIN_WIDTH),
        text_pad: TEXT_PAD,
    }
}

/// Paints `placed`, the cells of the icicle of `scene`, under the ancestors
/// of the nodes `trail` their names are told apart from. A cell lying above
/// `top`, the top of the icicle, a `row` tall each, is filled as a row over
/// the icicle as far as it lies above it.
fn paint_cells(
    scene: &Scene,
    placed: &[Placed],
    trail: &[u32],
    top: Pixels,
    row: Pixels,
    window: &mut Window,
    cx: &mut App,
) {
    let cells: Vec<Cell> = placed.iter().map(|placed| placed.cell).collect();
    let hints = hints_among(&scene.tree, &scene.shared, trail, &cells);
    let hints = noting_spliced(hints, &scene.shown.tree, scene.top.node());
    for &Placed { cell, on_top } in placed {
        let live = over(fill(scene, cell.holds), scene.palette.background);
        let above = ((top - cell.rect.top()) / row).clamp(0., 1.);
        let fill = mix(live, context_fill(live, &scene.palette), above);
        paint_cell(scene, cell, on_top, fill, &hints, window, cx);
    }
}

/// Paints `above`, the rows over the cell the icicle of `scene` is drawn from,
/// each filled as context for the icicle. Their names are told apart by the
/// nodes of `trail`, the ancestors drawn, and the cell on top alone.
fn paint_above(scene: &Scene, above: &[Spot], trail: &[u32], window: &mut Window, cx: &mut App) {
    let palette = &scene.palette;
    let named: Vec<u32> = trail.iter().copied().chain(scene.top.node()).collect();
    let hints = hints_among(&scene.tree, &scene.shared, &named, &[]);
    let hints = noting_spliced(hints, &scene.shown.tree, trail.iter().copied());
    for &spot in above {
        match spot {
            Spot::Cell(cell) => {
                let fill = context_fill(fill(scene, cell.holds), palette);
                paint_cell(scene, cell, false, fill, &hints, window, cx);
            }
            Spot::Hidden { rect, count } => paint_hidden(rect, count, palette, window, cx),
        }
    }
}

/// `hints` with what each of `nodes` passes over to stand where `shown`
/// draws it, the hidden dominators spliced out above it, added to its hint,
/// so that the rows over the focus and the cell on top read what the filter
/// leaves out between them.
fn noting_spliced(
    mut hints: HashMap<u32, String>,
    shown: &ShownTree,
    nodes: impl IntoIterator<Item = u32>,
) -> HashMap<u32, String> {
    for node in nodes {
        let placing = shown.placing(node);
        let Some(note) = placing.and_then(|placing| super::via_hidden(placing.spliced)) else {
            continue;
        };
        hints
            .entry(node)
            .and_modify(|hint| *hint = format!("{hint}{PART_SEPARATOR}{note}"))
            .or_insert(note);
    }
    hints
}

/// How far a row over the focus is carried from the hue of the cell it stands
/// for towards the neutral a point without a hue wears: far enough that its
/// hue reads only as a hint of what it is, which no other channel of the
/// icicle draws with.
const CONTEXT_GREY: f32 = 0.7;

/// How far a row over the focus then recedes towards the canvas: little, as
/// the filter's fade is what recedes far, and the two must not read alike.
const CONTEXT_RECEDE: f32 = 0.3;

/// The fill of a row over the focus standing for a cell filled `fill`, which
/// reads as context for the icicle rather than as a cell of it.
fn context_fill(fill: Rgba, palette: &Palette) -> Rgba {
    let opaque = over(fill, palette.background);
    let grey = mix(opaque, palette.data.neutral, CONTEXT_GREY);
    mix(grey, palette.background, CONTEXT_RECEDE)
}

/// Paints `cell` of the icicle of `scene` in `fill`, named as the cell on top
/// where it is `on_top`, by `hints` telling names reading alike apart.
fn paint_cell(
    scene: &Scene,
    cell: Cell,
    on_top: bool,
    fill: Rgba,
    hints: &HashMap<u32, String>,
    window: &mut Window,
    cx: &mut App,
) {
    let palette = &scene.palette;
    let style = box_style(palette);
    let (rect, label) = (drawn_rect(cell.rect), label_on(fill));
    let name = match (on_top, cell.holds) {
        _ if cell.rect.size.width < LABEL_MIN_WIDTH => CellName::default(),
        (true, Holds::Rest(run)) => {
            let names = unit_names(scene.tree.scope().unit());
            CellName::plain(run_on_top(run.count(), run.weight(), &names))
        }
        (_, Holds::Rest(run)) => {
            let (room, font_size) = style.label_room(rect);
            let width = text_width(window, font_size, label);
            CellName::plain(rest_name(run.count(), room, width))
        }
        (_, Holds::Shared) => {
            let (room, font_size) = style.label_room(rect);
            let shown = &scene.shown.tree;
            let whole = cell_name(&scene.tree, shown, &scene.shared, scene.by, Holds::Shared);
            let count = shown.shared_tops().len();
            let width = text_width(window, font_size, label);
            CellName::plain(shared_cell_name(whole, count, room, width))
        }
        (_, Holds::Node(node)) => {
            let tree = &scene.tree;
            let name = PointName::of(tree.id(node), tree.scope().unit(), &scene.shared);
            let (room, font_size) = style.label_room(rect);
            let hint = hints.get(&node).map(String::as_str);
            name.fitted(hint, room, text_width(window, font_size, label))
        }
        (_, holds) => {
            let (tree, shown) = (&scene.tree, &scene.shown.tree);
            CellName::plain(cell_name(tree, shown, &scene.shared, scene.by, holds))
        }
    };
    let strength = match cell.holds {
        Holds::Node(node) if faded(scene, scene.tree.id(node)) => UNMARKED_FADE,
        _ => 1.0,
    };
    let drawn_fill = Rgba {
        a: fill.a * strength,
        ..fill
    };
    paint_labeled_box(
        rect,
        drawn_fill.into(),
        name.inked(),
        Hsla {
            a: label.a * strength,
            ..label
        },
        style,
        window,
        cx,
    );
    if let Holds::Node(node) = cell.holds
        && reached(&scene.tree, node)
    {
        paint_notch(rect, notch_ink(drawn_fill, palette.background), window);
    }
}

/// Paints the row at `rect` gathering `count` ancestors left out, which
/// reads how many and bears the caret of what opens a list.
fn paint_hidden(
    rect: Bounds<Pixels>,
    count: u32,
    palette: &Palette,
    window: &mut Window,
    cx: &mut App,
) {
    let style = box_style(palette);
    let (rect, fill) = (drawn_rect(rect), context_fill(rest_fill(palette), palette));
    let label = label_on(fill);
    let name = hidden_name(count);
    paint_labeled_box(rect, fill.into(), name.as_str(), label, style, window, cx);
    let (room, font_size) = style.label_room(rect);
    let width = text_width(window, font_size, label)(&name);
    let left = rect.left() + px(TEXT_PAD + width + CARET_GAP);
    if rect.size.width < LABEL_MIN_WIDTH || width + CARET_GAP + f32::from(font_size) > room {
        return;
    }
    let caret = Bounds::new(
        point(left, rect.center().y - font_size / 2.),
        size(font_size, font_size),
    );
    window
        .paint_svg(
            caret,
            bviz_ui_assets::icon_path(CARET),
            None,
            TransformationMatrix::default(),
            label,
            cx,
        )
        .ok();
}

/// The icon closing the name of the row of more ancestors, which opens the
/// list of them.
const CARET: &str = "caret-down";

/// Room between the name of the row of more ancestors and its caret.
const CARET_GAP: f32 = 2.0;

/// What the row gathering `count` more ancestors reads, ahead of its caret.
pub(crate) fn hidden_name(count: u32) -> String {
    format!("{ELLIPSIS} {}", more(count))
}

/// The ink of a notch on a cell drawn in `fill` over `background`: the ink a
/// name takes on the cell as it is seen, laid as opaque as the cell, so a
/// cell the filter fades carries it faded but still seen. It is returned
/// already composited over the cell, opaque, so the notch hides whatever the
/// cell's own paint lays beneath it.
pub(crate) fn notch_ink(fill: Rgba, background: Rgba) -> Rgba {
    let cell = over(fill, background);
    let ink = Rgba::from(label_on(cell));
    over(Rgba { a: fill.a, ..ink }, cell)
}

/// The part of the row at `rect` a box covers, clear of the gap to the next
/// row.
pub(crate) fn drawn_rect(rect: Bounds<Pixels>) -> Bounds<Pixels> {
    Bounds::new(
        rect.origin,
        size(rect.size.width, rect.size.height - ROW_GAP),
    )
}

const RING_WIDTH: Pixels = px(2.);

/// Rings the box drawn in the row at `rect`.
pub(crate) fn paint_ring(rect: Bounds<Pixels>, palette: &Palette, window: &mut Window) {
    window.paint_quad(quad(
        drawn_rect(rect),
        px(0.),
        Hsla::transparent_black(),
        RING_WIDTH,
        palette.label,
        BorderStyle::Solid,
    ));
}

/// Whether the filter fading the cells of `scene` fades the node `id`.
fn faded(scene: &Scene, id: Id) -> bool {
    (scene.verdicts.as_ref()).is_some_and(|verdicts| verdicts.get(id) == Verdict::Faded)
}

fn fill(scene: &Scene, holds: Holds) -> Rgba {
    let palette = &scene.palette;
    match holds {
        Holds::Build => build_fill(palette),
        Holds::Shared => shared_fill(palette),
        Holds::Node(node) => {
            let (tree, facets) = (&scene.tree, &scene.shared.facets);
            let hue = scene.colouring.hue(tree.id(node), tree.kind(node), facets);
            hue.color(palette)
        }
        Holds::Rest(_) => rest_fill(palette),
    }
}

fn build_fill(palette: &Palette) -> Rgba {
    palette.package
}

/// The fill of the cell of the shared tops, which is no point and stands for
/// more than a rest cell does: the neutral carried towards the ink the panel
/// sets text in, where a rest cell's is carried towards the canvas.
fn shared_fill(palette: &Palette) -> Rgba {
    mix(palette.data.neutral, palette.label, SHARED_STAND_OUT)
}

/// How far the fill of the cell of the shared tops is mixed from the neutral
/// towards the text ink: far enough to read apart from a point drawn in the
/// neutral, and short of where it nears a hue.
const SHARED_STAND_OUT: f32 = 0.35;

fn rest_fill(palette: &Palette) -> Rgba {
    mix(palette.data.neutral, palette.background, REST_RECEDE)
}

/// What the cell holding `holds` of `shown`, drawn from `tree`, whose roots
/// were chosen `by`, reads.
fn cell_name(
    tree: &Dominators,
    shown: &ShownTree,
    shared: &Shared,
    by: RootedBy,
    holds: Holds,
) -> String {
    let many = unit_names(tree.scope().unit()).many;
    match holds {
        Holds::Build if super::hides_any(tree, shown) => {
            format!(
                "whole build · {} of {} {many} shown",
                shown.total(),
                tree.total()
            )
        }
        Holds::Build => format!("whole build · {} {many}", tree.total()),
        Holds::Shared => {
            let (name, count) = (shared_name(tree, by), shown.shared_tops().len());
            format!("{name} · {count}")
        }
        Holds::Node(node) => point_name(tree.id(node), tree.scope().unit(), shared),
        Holds::Rest(run) => more(run.count()),
    }
}

/// How a reader names the shared tops of `tree`, its roots being chosen
/// `by`.
pub(crate) fn shared_name(tree: &Dominators, by: RootedBy) -> String {
    super::shared_by(SEVERAL, by.word(), unit_names(tree.scope().unit()).many)
}

/// Measures text as `window` shapes it at `font_size` in `ink`, in the font
/// the window inherits.
fn text_width(window: &mut Window, font_size: Pixels, ink: Hsla) -> impl FnMut(&str) -> f32 {
    let font = bviz_ui_assets::text_metrics(window).font;
    move |text| {
        let line = shape_line(window, text.to_owned(), font.clone(), font_size, ink);
        f32::from(line.width())
    }
}

/// What tells apart, by node, the names that read alike among the nodes
/// `named` of `tree` and the cells `cells` wide enough to name. Each
/// such name is told apart by its configuration, as
/// [`configuration_readings`] reads it, and an action's also by the file it
/// writes first. A node whose name no other reads alike has none.
pub(crate) fn hints_among(
    tree: &Dominators,
    shared: &Shared,
    named: &[u32],
    cells: &[Cell],
) -> HashMap<u32, String> {
    let unit = tree.scope().unit();
    let wide = cells.iter().filter_map(|cell| match cell.holds {
        Holds::Node(node) if cell.rect.size.width >= LABEL_MIN_WIDTH => Some(node),
        _ => None,
    });
    let nodes: Vec<u32> = named.iter().copied().chain(wide).collect();
    let names: Vec<String> = nodes
        .iter()
        .map(|&node| point_name(tree.id(node), unit, shared))
        .collect();
    let hints = hints_where_alike(&names, |row| {
        let id = tree.id(nodes[row]);
        let [short, whole] = configuration_readings(id, shared);
        match unit {
            Unit::Action => {
                let output = primary_output(id, shared).map(str::to_owned);
                vec![short, whole, output]
            }
            Unit::ConfiguredTarget | Unit::Target => vec![short, whole],
        }
    });
    let told = nodes.into_iter().zip(hints);
    told.filter_map(|(node, hint)| Some((node, hint?)))
        .collect()
}

/// Between the parts a cell's name reads: a name and what tells it apart
/// from others reading alike, or a count and what it weighs.
pub(crate) const PART_SEPARATOR: &str = " · ";

/// How a name counts `count` siblings or ancestors left out of the cells.
fn more(count: u32) -> String {
    format!("{count} more")
}

/// How a tooltip names a run of `tree`, whose roots were chosen `by`,
/// naming what its siblings stand under.
pub(crate) fn run_name(tree: &Dominators, shared: &Shared, by: RootedBy, run: Run) -> String {
    let count = run.count();
    match run.parent() {
        Parent::Node(parent) => {
            let parent = point_name(tree.id(parent), tree.scope().unit(), shared);
            format!("{} under {parent}", more(count))
        }
        Parent::Shared => {
            let shared = what_is_shared(by.word(), unit_names(tree.scope().unit()).many);
            format!("{} of {shared}", more(count))
        }
        Parent::Build => format!("{} under {WHOLE_BUILD}", more(count)),
    }
}

/// What a run of `count` siblings weighing `weight` points, named `names`,
/// reads drawn on top, where the cell over it just above names what they
/// stand under.
fn run_on_top(count: u32, weight: u32, names: &KindNames) -> String {
    format!("{}{PART_SEPARATOR}{}", more(count), names.counted(weight))
}

/// What a cell gathering `count` siblings reads within `room` as `width`
/// measures text: a count cut short says nothing, so its name is left whole,
/// or failing that the bare count, or failing that nothing.
fn rest_name(count: u32, room: f32, width: impl FnMut(&str) -> f32) -> String {
    first_fitting([more(count), count.to_string()], room, width)
}

/// What the cell gathering `count` shared tops, named `whole`, reads within
/// `room` as `width` measures text: its name whole, or failing that one word
/// and the count, the word alone, or nothing.
fn shared_cell_name(
    whole: String,
    count: usize,
    room: f32,
    width: impl FnMut(&str) -> f32,
) -> String {
    let names = [whole, format!("{SHARED} · {count}"), SHARED.to_owned()];
    first_fitting(names, room, width)
}

/// What a cell too narrow to say what shares its tops reads of them.
const SHARED: &str = "shared";

/// The first of `names` no wider than `room` as `width` measures text, or
/// nothing where none fits.
fn first_fitting<const N: usize>(
    names: [String; N],
    room: f32,
    mut width: impl FnMut(&str) -> f32,
) -> String {
    let mut names = names.into_iter();
    names.find(|name| width(name) <= room).unwrap_or_default()
}

/// How a cell names the point `id` of `unit`.
pub(crate) fn point_name(id: Id, unit: Unit, shared: &Shared) -> String {
    let PointName { lead, label } = PointName::of(id, unit, shared);
    lead + &label
}

/// How a cell names a point: by the label it carries, or failing that its
/// node's name, led for an action by its mnemonic.
struct PointName {
    lead: String,
    label: String,
}

impl PointName {
    fn of(id: Id, unit: Unit, shared: &Shared) -> Self {
        let lead = match unit {
            Unit::Action => format!("{} ", mnemonic(id, shared)),
            Unit::ConfiguredTarget | Unit::Target => String::new(),
        };
        let label = point_label(id, shared);
        Self { lead, label }
    }

    /// What a cell reads of this followed by `hint` within `room` as `width`
    /// measures text. Whole where it fits; else the label is cut short as
    /// [`fit_label`] cuts it so the hint still reads beside it, unless that
    /// leaves the label less than [`NAME_SHARE`] of the room, where it is cut
    /// short alone.
    fn fitted(
        &self,
        hint: Option<&str>,
        room: f32,
        mut width: impl FnMut(&str) -> f32,
    ) -> CellName {
        let tail = hint.map_or_else(String::new, |hint| format!("{PART_SEPARATOR}{hint}"));
        let whole = format!("{}{}{tail}", self.lead, self.label);
        if width(&whole) <= room {
            let place = name_start(&self.label).unwrap_or(0);
            return self.inked(whole, 0..place);
        }
        let lead = width(&self.lead);
        let beside = room - lead - width(&tail);
        let (room, tail) = match beside >= room * NAME_SHARE {
            true => (beside, tail),
            false => (room - lead, String::new()),
        };
        let label = fit_label(&self.label, room, &mut width);
        self.inked(
            format!("{}{}{tail}", self.lead, label.text()),
            label.muted(),
        )
    }

    /// `text`, led by this's lead, with the `place` bytes of the label
    /// following the lead muted.
    fn inked(&self, text: String, place: Range<usize>) -> CellName {
        let lead = self.lead.len();
        CellName {
            text,
            muted: lead + place.start..lead + place.end,
        }
    }
}

/// The least of a cell's room a label keeps beside the hint telling it
/// apart, so a long hint never reads in place of the label.
const NAME_SHARE: f32 = 0.5;

/// What a cell reads, the part of it saying where a label's target lives
/// muted.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CellName {
    text: String,
    /// The bytes of `text` set in the muted ink.
    muted: Range<usize>,
}

impl CellName {
    fn plain(text: String) -> Self {
        Self { text, muted: 0..0 }
    }

    fn inked(&self) -> Inked<'_> {
        Inked {
            text: &self.text,
            muted: self.muted.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Text as wide as it has characters.
    fn chars(text: &str) -> f32 {
        text.chars().count() as f32
    }

    /// On whatever fill a root wears, and whether or not the filter fades
    /// it, the ink of its notch stands off the cell it lies on.
    #[test]
    fn the_ink_of_a_notch_is_seen_on_every_fill_faded_or_not() {
        use bviz_ui_theme::{
            MIN_DISTINCT_COLOR, MIN_SECONDARY_CONTRAST, built_in_themes, contrast_ratio, distance,
        };
        for theme in built_in_themes().iter() {
            let palette = Palette::from_theme(theme);
            let fills = palette.data.chromatic().into_iter().map(Rgba::from);
            for fill in fills.chain([palette.data.neutral.into()]) {
                let ink = notch_ink(fill, palette.background);
                let contrast = contrast_ratio(ink, fill);
                assert!(
                    contrast >= MIN_SECONDARY_CONTRAST,
                    "{}: {contrast}",
                    theme.name
                );
                for strength in [1.0, UNMARKED_FADE] {
                    let drawn = Rgba {
                        a: strength,
                        ..fill
                    };
                    let cell = over(drawn, palette.background);
                    let inked = over(notch_ink(drawn, palette.background), cell);
                    let apart = distance(inked, cell);
                    assert!(
                        apart >= MIN_DISTINCT_COLOR,
                        "{} at {strength}: {apart}",
                        theme.name
                    );
                }
            }
        }
    }

    #[test]
    fn a_notch_is_a_right_triangle_in_the_cell_s_top_left_corner() {
        let at = |x: f32, y: f32| point(px(x), px(y));
        let leg = f32::from(NOTCH);
        assert_eq!(
            notch(px(60.)),
            [at(0., 0.), at(leg, 0.), at(leg, 0.), at(0., leg)]
        );
    }

    /// A cell narrower than the notch's leg carries the triangle cut off at
    /// its right edge: as tall as the leg at its left edge and as tall as the
    /// slant leaves at its right.
    #[test]
    fn a_notch_on_a_narrow_cell_is_cut_off_at_its_right_edge() {
        let at = |x: f32, y: f32| point(px(x), px(y));
        let (width, leg) = (f32::from(MIN_CELL_WIDTH), f32::from(NOTCH));
        assert_eq!(
            notch(MIN_CELL_WIDTH),
            [
                at(0., 0.),
                at(width, 0.),
                at(width, leg - width),
                at(0., leg)
            ]
        );
    }

    /// The ink of a faded cell's notch is opaque, so the cell's own paint
    /// does not show through it.
    #[test]
    fn the_ink_of_a_notch_is_opaque_on_a_faded_cell() {
        let palette = Palette::from_theme(&bviz_ui_theme::built_in_themes()[0]);
        let fill = Rgba {
            a: UNMARKED_FADE,
            ..Rgba::from(palette.data.neutral)
        };
        let opacity = notch_ink(fill, palette.background).a;
        assert!((opacity - 1.0).abs() < f32::EPSILON, "{opacity}");
    }

    /// A notch stays out of the box a cell's name is set in, which starts a
    /// pad in from the cell's top-left corner: every corner of the notch lies
    /// on or within the slant `x + y = NOTCH`, and the box's nearest corner on
    /// or beyond it.
    #[test]
    fn a_notch_stays_clear_of_the_name() {
        let name = Bounds::new(point(px(TEXT_PAD), px(TEXT_PAD)), size(px(40.), px(12.)));
        let along = |corner: Point<Pixels>| corner.x + corner.y;
        for width in [MIN_CELL_WIDTH, px(60.)] {
            for corner in notch(width) {
                assert!(along(corner) <= NOTCH, "{corner:?}");
            }
        }
        assert!(along(name.origin) >= NOTCH);
    }

    fn named(label: &str) -> PointName {
        PointName {
            lead: String::new(),
            label: label.to_owned(),
        }
    }

    #[test]
    fn a_label_cut_short_keeps_the_hint_while_it_keeps_the_larger_part() {
        let label = "@llvm//toolchain:linux_x86_64_cc_toolchain";
        let fit = |room| named(label).fitted(Some("k8"), room, chars);
        assert_eq!(fit(100.0).text, format!("{label} · k8"));
        assert_eq!(fit(100.0).muted, 0..16);
        let cut = fit(40.0);
        assert_eq!(cut.text, "@llvm//t…:linux_x86_64_cc_toolchain · k8");
        assert_eq!(cut.muted, 0..11);
        let long = "a-long-configuration-name (exec, 3f2a)";
        let alone = named(label).fitted(Some(long), 40.0, chars);
        assert_eq!(alone.text, "@llvm//too…ain:linux_x86_64_cc_toolchain");
    }

    #[test]
    fn an_action_keeps_its_mnemonic_ahead_of_its_label_cut_short() {
        let name = PointName {
            lead: "CppCompile ".to_owned(),
            label: "@r//a/b/c:lib".to_owned(),
        };
        let cut = name.fitted(None, 21.0, chars);
        assert_eq!(cut.text, "CppCompile @r//a…:lib");
        assert_eq!(cut.muted, 11..19);
    }

    #[test]
    fn a_name_that_is_no_label_is_cut_short_at_its_end() {
        let cut = named("action:bazel-out/k8/bin/a.o").fitted(None, 10.0, chars);
        assert_eq!((cut.text.as_str(), cut.muted), ("action:ba…", 0..0));
    }

    #[test]
    fn a_run_on_top_reads_its_count_and_weight() {
        let names = unit_names(Unit::Target);
        assert_eq!(run_on_top(663, 1204, &names), "663 more · 1204 targets");
        assert_eq!(run_on_top(2, 1, &names), "2 more · 1 target");
    }

    #[test]
    fn a_rest_cell_names_its_count_whole_or_not_at_all() {
        assert_eq!(rest_name(22, 7.0, chars), "22 more");
        assert_eq!(rest_name(22, 6.0, chars), "22");
        assert_eq!(rest_name(22, 2.0, chars), "22");
        assert_eq!(rest_name(22, 1.0, chars), "");
    }

    /// Each end of a move between two cells deep enough that the rows over
    /// them gather some ancestors is drawn as the icicle is at rest there:
    /// the same rows, the same cells under them, and over them only the cell
    /// carried up, standing where the row for it stands.
    #[test]
    fn a_deep_move_starts_and_ends_as_drawn_at_rest() {
        use super::super::icicle::tests::{Laid, ROW, Spec, area, leaf};
        let chain = Spec(
            9,
            vec![Spec(
                8,
                vec![Spec(
                    7,
                    vec![Spec(6, vec![Spec(5, vec![leaf(2), leaf(2)])])],
                )],
            )],
        );
        let tree = Laid::of(&[chain]);
        let (higher, lower) = (Holds::Node(4), Holds::Node(5));
        let bounds = area(100., 12.);
        let down = Transition::between(&tree, higher, lower, bviz_time::now()).unwrap();
        let at_rest = |top| frame(&tree, top, None, bounds, ROW);
        let cells = |placed: &[Placed], within: Bounds<Pixels>| -> Vec<Cell> {
            let inside = placed
                .iter()
                .filter(|placed| placed.cell.rect.top() >= within.top());
            let mut cells: Vec<Cell> = inside.map(|placed| placed.cell).collect();
            let at = |cell: &Cell| (f32::from(cell.rect.top()), f32::from(cell.rect.left()));
            cells.sort_by(|a, b| {
                let ((a_top, a_left), (b_top, b_left)) = (at(a), at(b));
                a_top.total_cmp(&b_top).then(a_left.total_cmp(&b_left))
            });
            cells
        };
        let (rows, region, placed) = frame(&tree, higher, Some((&down, 0.)), bounds, ROW);
        let (rest_rows, rest_region, rest) = at_rest(higher);
        assert_eq!((rows, region), (rest_rows, rest_region));
        assert_eq!(cells(&placed, region), cells(&rest, rest_region));
        let (rows, region, placed) = frame(&tree, higher, Some((&down, 1.)), bounds, ROW);
        let (rest_rows, rest_region, rest) = at_rest(lower);
        assert_eq!((&rows, region), (&rest_rows, rest_region));
        assert_eq!(cells(&placed, region), cells(&rest, rest_region));
        let carried: Vec<Cell> = placed
            .iter()
            .map(|placed| placed.cell)
            .filter(|cell| cell.rect.top() < region.top())
            .collect();
        let (above, _) = ancestry::lay_out(&rows, bounds, ROW);
        assert_eq!(carried.len(), 1, "{carried:#?}");
        assert_eq!(above.last().map(Spot::rect), Some(carried[0].rect));
    }

    /// A row over the focus reads apart from the live cell it stands for and
    /// from that cell as the filter fades it, and its name still reads on it.
    #[test]
    fn a_row_over_the_focus_reads_as_neither_a_live_nor_a_faded_cell() {
        use bviz_ui_theme::{
            MIN_DISTINCT_COLOR, MIN_TEXT_CONTRAST, built_in_themes, contrast_ratio, distance,
        };
        for theme in built_in_themes().iter() {
            let palette = Palette::from_theme(theme);
            let fills = palette.data.chromatic().into_iter().map(Rgba::from);
            for fill in fills.chain([palette.data.neutral.into()]) {
                let context = context_fill(fill, &palette);
                let faded = over(
                    Rgba {
                        a: UNMARKED_FADE,
                        ..fill
                    },
                    palette.background,
                );
                for (other, what) in [(over(fill, palette.background), "live"), (faded, "faded")] {
                    let apart = distance(context, other);
                    assert!(
                        apart >= MIN_DISTINCT_COLOR,
                        "{} {what}: {apart}",
                        theme.name
                    );
                }
                let contrast = contrast_ratio(label_on(context), context);
                assert!(contrast >= MIN_TEXT_CONTRAST, "{}: {contrast}", theme.name);
            }
        }
    }

    /// The cell of the shared tops reads apart from every hue a point is
    /// drawn in, from a rest cell and from the whole build.
    #[test]
    fn the_cell_of_the_shared_tops_is_filled_unlike_any_other_cell() {
        use bviz_ui_theme::{MIN_DISTINCT_COLOR, built_in_themes, distance};
        for theme in built_in_themes().iter() {
            let palette = Palette::from_theme(theme);
            let shared = shared_fill(&palette);
            let hues = palette.data.chromatic().into_iter().map(Rgba::from);
            let others = hues.chain([
                palette.data.neutral.into(),
                rest_fill(&palette),
                build_fill(&palette),
            ]);
            for other in others {
                let apart = distance(shared, other);
                assert!(
                    apart >= MIN_DISTINCT_COLOR,
                    "{}: {apart} from {other:?}",
                    theme.name
                );
            }
        }
    }

    #[test]
    fn the_cell_of_the_shared_tops_reads_as_much_of_its_name_as_fits_whole() {
        let whole = "shared by several top-level targets · 12";
        let read = |room| shared_cell_name(whole.to_owned(), 12, room, chars);
        assert_eq!(read(40.0), whole);
        assert_eq!(read(39.0), "shared · 12");
        assert_eq!(read(10.0), "shared");
        assert_eq!(read(5.0), "");
    }
}
