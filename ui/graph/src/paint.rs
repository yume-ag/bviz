use crate::camera::{Camera, Rect, to_screen, visible_rect};
use crate::drag::track_gesture;
use crate::model::{Drawn, Expandable};
use crate::morph::Frame;
use crate::neighborhood::{Neighborhood, compute_neighborhood};
use crate::scroll::paint_scrollbars;
use crate::{GraphView, Palette};
use bviz_graph_layout::layout::{CARET_RESERVE, LABEL_BAND};
use bviz_graph_layout::{Connector, EdgeKind, Id, Kind};
use gpui::{
    App, BorderStyle, Bounds, Entity, Hsla, PathBuilder, Pixels, Point, Rgba, TextAlign, Window,
    point, px, quad, size,
};

/// Box label font size in layout pixels.
const LABEL_SIZE: f64 = 13.0;

/// Screen-pixel size a label is never drawn below. Zoom scales the label with
/// the box down to this floor and no further, so a box wide enough to hold its
/// name shows it rather than fading to a blank rectangle.
const LABEL_FLOOR: f32 = 11.0;

/// Screen-pixel clearance held between a label and its box's edges, so a label
/// held at the floor is not painted flush against the border.
const LABEL_PAD: f32 = 2.0;

/// Screen width a box needs to spare for its label before one is drawn: any
/// narrower and what survives truncation names nothing, so the box is left bare.
const LABEL_MIN_WIDTH: f32 = 22.0;

/// Arrowhead size along and across the edge direction, in layout pixels.
const ARROW_LENGTH: f64 = 9.0;
const ARROW_HALF_WIDTH: f64 = 4.0;
/// How far the stroke runs past the arrowhead base into the head, closing the
/// seam without letting the line broaden the visible tip.
const ARROW_OVERDRAW: f64 = 1.0;

/// Stroke width (pre-zoom px) of the line a connector's provenance is drawn in.
const CONNECTOR_WIDTH: f32 = 1.5;

/// Stroke width (pre-zoom px) of that line where the hover marks the connector.
/// A doubling is the smallest step that reads as a heavier line rather than as
/// an artifact of antialiasing.
const MARKED_WIDTH: f32 = 2.0 * CONNECTOR_WIDTH;

/// How far a hover mark carries a connector's color in perceptual lightness,
/// away from the canvas. Bounded below by a marked line standing
/// [`bviz_ui_theme::MIN_DISTINCT_COLOR`] off the unmarked line of its own query,
/// and above by the three provenances holding that distance from one another
/// however each of a pair is marked, and from the inks a route is drawn beside.
/// Set where the nearest of those bounds is furthest off.
const MARK_LIFT: f32 = 0.072;

/// Bare ground (pre-zoom px) left between the strokes of two adjacent lanes, so
/// a bundle of them reads as several lines rather than as one broad one. The
/// zoom carries it along with the strokes it separates, so what holds at every
/// zoom is its ratio to them.
const LANE_GAP: f32 = 1.0;

/// Perpendicular gap (pre-zoom px) between the parallel provenance lines drawn
/// along a node pair that several queries connect. Either lane may be marked and
/// so drawn at [`MARKED_WIDTH`], and every lane of an unmarked bundle pays for
/// that too.
const PARALLEL_SPACING: f32 = MARKED_WIDTH + LANE_GAP;

/// How far a kind accent is carried toward the canvas to make a leaf fill.
/// Bounded below by the fills staying clear of the connector colors drawn over
/// and between them, which are taken from the same slots, and above by the kinds
/// staying apart from one another and off the canvas.
const KIND_BLEND: f32 = 0.6;

/// How far a group's fill is tinted toward the accent of the files it holds: far
/// enough to be told from a plain container fill, and not so far that the group
/// passes for one of the files.
const GROUP_TINT: f32 = 0.17;

/// Opacity of the wash a marked box's fill is painted over. Bounded below by the
/// wash carrying the fill a [`bviz_ui_theme::MIN_DISTINCT_COLOR`] off the one the
/// box wears unmarked, so the mark is seen at all, and above by the box's name —
/// inked from the unwashed fill — holding [`bviz_ui_theme::MIN_TEXT_CONTRAST`] on
/// the ground the wash makes of it.
const HIGHLIGHT_WASH: f32 = 0.155;

/// Everything the canvas draws from, owned so the `'static` paint closure holds
/// no borrow of the view.
pub(crate) struct Scene {
    pub(crate) drawn: Drawn,
    pub(crate) expandable: Expandable,
    pub(crate) palette: Palette,
    /// The box whose neighborhood the hover marks; `None` when none is marked.
    pub(crate) neighborhood: Option<Id>,
    /// The emphasized box and the alpha its highlight paints at.
    pub(crate) highlight: Option<(Id, f32)>,
}

/// The canvas paint pass: box fills parent-before-child so children sit over
/// their containers, then the connectors, then every label on top.
///
/// Labels go last, in a pass of their own, so a name wins over the routes
/// crossing its box; at the finest levels the lanes are dense enough to shred
/// one painted under them. That pass keeps the boxes' outermost-first order,
/// which is the order [`paint_box_label`] arbitrates overlapping titles in.
///
/// A morph in flight places each box between the drawing it came from and the
/// one it is going to. The boxes only the departing drawing held come after the
/// rest, so a member fading into the container it collapses into is drawn over
/// it rather than under.
///
/// The camera is read here, once the prepaint has fitted it, so the level a map
/// draws and the part of it the window shows are both the frame's own. Only what
/// falls inside that part is touched, which is what keeps a level of tens of
/// thousands of boxes paintable.
pub(crate) fn canvas_body(
    scene: Scene,
    entity: Entity<GraphView>,
) -> impl FnOnce(Bounds<Pixels>, (), &mut Window, &mut App) {
    move |bounds, _, window, cx| {
        let (camera, morph) = {
            let view = entity.read(cx);
            (view.camera(), view.morph_frame())
        };
        let g = scene.drawn.view(camera.zoom);
        let visible = visible_rect(bounds, camera);
        let neighborhood = compute_neighborhood(g, scene.neighborhood);
        let drawn: Vec<(PaintBox, Bounds<Pixels>)> = g
            .boxes
            .iter()
            .map(|b| {
                let (rect, alpha) = morph.as_ref().map_or((Rect::of(b), 1.0), |m| m.place(b));
                Placed {
                    b,
                    rect,
                    alpha,
                    leaving: false,
                }
            })
            .chain(
                morph
                    .iter()
                    .flat_map(Frame::leaving)
                    .map(|(b, rect, alpha)| Placed {
                        b,
                        rect,
                        alpha,
                        leaving: true,
                    }),
            )
            .filter(|p| visible.overlaps(&p.rect))
            .map(|p| {
                let b = PaintBox::of(&p, &scene, &neighborhood);
                let rect = screen_rect(&b, bounds, camera);
                paint_box(&b, rect, camera.zoom, &scene.palette, window);
                (b, rect)
            })
            .collect();
        let edges = morph.as_ref().map_or(1.0, |m| m.connectors);
        if edges > 0.0 {
            for c in g
                .connectors
                .iter()
                .filter(|c| visible.crossed_by(&c.points))
            {
                let marked = neighborhood.edges.contains(&(c.from, c.to));
                paint_lanes(c, marked, edges, bounds, camera, &scene.palette, window);
            }
        }
        let mut floated: Vec<Bounds<Pixels>> = Vec::new();
        let mut titles: Vec<TitleTarget> = Vec::new();
        for (b, rect) in &drawn {
            let target = paint_box_label(
                b,
                *rect,
                camera.zoom,
                &scene.palette,
                &mut floated,
                window,
                cx,
            );
            titles.extend(target);
        }
        entity.update(cx, |view, _| view.titles = titles);
        paint_scrollbars(
            bounds,
            scene.drawn.content(),
            camera,
            &scene.palette,
            window,
        );
        track_gesture(&entity, window, cx);
    }
}

/// `c` at `alpha` of its own opacity, by which a box a morph is fading in or out
/// takes its whole appearance — fill, border, label — down together.
fn faded(c: Rgba, alpha: f32) -> Rgba {
    Rgba {
        a: c.a * alpha,
        ..c
    }
}

impl Palette {
    /// Accent for a leaf box, keyed by its node kind. Actions, artifacts and
    /// source files each take a slot of their own; every other kind shares the
    /// catch-all.
    fn kind_accent(&self, kind: Kind) -> Rgba {
        match kind {
            Kind::Action => self.data.orange,
            Kind::Artifact => self.data.green,
            Kind::Source => self.data.purple,
            _ => self.data.blue,
        }
        .into()
    }

    /// Fill for a leaf box, its kind's accent carried toward the canvas so the
    /// box reads as ground a name is set on rather than as a mark.
    fn leaf_fill(&self, kind: Kind) -> Rgba {
        bviz_ui_theme::mix(self.kind_accent(kind), self.background, KIND_BLEND)
    }

    /// Fill for a container box, by which nesting reads package > target >
    /// configured-target, and a group of files carries the accent of the files
    /// it holds.
    fn container_fill(&self, kind: Kind) -> Rgba {
        match kind {
            Kind::Package => self.package,
            Kind::Group => {
                bviz_ui_theme::mix(self.container, self.kind_accent(Kind::Artifact), GROUP_TINT)
            }
            _ => self.container,
        }
    }

    /// Connector color, keyed by the query an edge came from, so static (query),
    /// configured (cquery), and execution-phase (aquery) dependencies read
    /// distinctly; a filter's synthetic transitive edges take a fourth hue of
    /// their own. `marked` carries the color by [`MARK_LIFT`], which leaves
    /// untouched the hue that names the kind.
    pub(crate) fn edge_color(&self, kind: EdgeKind, marked: bool) -> Rgba {
        let slot = match kind {
            EdgeKind::Query => self.data.blue,
            EdgeKind::Cquery => self.data.cyan,
            EdgeKind::Aquery => self.data.orange,
            EdgeKind::Transitive => self.data.red,
        };
        match marked {
            false => slot.into(),
            true => bviz_ui_theme::lighten(slot, self.mark_lift()),
        }
    }

    /// [`MARK_LIFT`] signed to point away from the canvas: up on a theme dark
    /// enough to take white text, down on one that takes black.
    fn mark_lift(&self) -> f32 {
        match bviz_ui_theme::label_on(self.background).l > 0.5 {
            true => MARK_LIFT,
            false => -MARK_LIFT,
        }
    }

    /// The wash a marked box's fill is painted over, at `strength` of the full
    /// mark.
    fn highlight_wash(&self, strength: f32) -> Rgba {
        Rgba {
            a: HIGHLIGHT_WASH * strength,
            ..self.highlight
        }
    }
}

/// A disclosure title as a frame painted it, by which a gesture landing on the
/// title reaches the box it names.
pub(crate) struct TitleTarget {
    plate: Bounds<Pixels>,
    id: Id,
}

/// A box the frame draws: the rect it occupies and the opacity it paints at,
/// both in layout pixels.
struct Placed<'a> {
    b: &'a bviz_graph_layout::Box,
    rect: Rect,
    alpha: f32,
    /// On its way out of the drawing, which only the morph still holds it for.
    leaving: bool,
}

impl Placed<'_> {
    /// Whether this box draws a disclosure caret. A box on its way out is absent
    /// from the drawing that replaced it, so there is nothing left there to open
    /// or close and it carries none however the layout it is leaving classified
    /// it.
    fn expandable(&self, expandable: &Expandable) -> bool {
        !self.leaving && expandable.holds(self.b)
    }
}

/// A box resolved against the theme and the hover's marked set, in layout
/// pixels.
struct PaintBox<'a> {
    id: Id,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    compound: bool,
    /// Whether this box can be opened or closed, so the renderer draws a
    /// disclosure caret: down when open (compound), right when collapsed.
    expandable: bool,
    /// The themed fill, which the quad and the plate a label sits on are both
    /// painted in.
    fill: Rgba,
    label: &'a str,
    /// How strongly the box wears the just-toggled emphasis, from 0 for none to
    /// 1, scaling its accent border.
    highlight: f32,
    /// Whether the hover marks this box, which the wash over its fill says.
    marked: bool,
    /// Opacity the whole box paints at, which a morph fades from zero for a box
    /// only the new drawing holds and to zero for one only the old one held.
    alpha: f32,
}

impl<'a> PaintBox<'a> {
    fn of(p: &Placed<'a>, scene: &Scene, neighborhood: &Neighborhood) -> Self {
        let Placed { b, rect, alpha, .. } = *p;
        let palette = &scene.palette;
        let fill = match b.compound {
            true => palette.container_fill(b.kind),
            false => palette.leaf_fill(b.kind),
        };
        let toggled = match scene.highlight {
            Some((id, alpha)) if id == b.id => alpha,
            _ => 0.0,
        };
        PaintBox {
            id: b.id,
            x: rect.x,
            y: rect.y,
            w: rect.w,
            h: rect.h,
            compound: b.compound,
            expandable: p.expandable(&scene.expandable),
            fill: faded(fill, alpha),
            label: &b.label,
            highlight: toggled,
            marked: neighborhood.boxes.contains(&b.id),
            alpha,
        }
    }

    /// The wash the hover mark lays over this box's fill, transparent where the
    /// box is unmarked.
    fn wash(&self, palette: &Palette) -> Rgba {
        palette.highlight_wash(if self.marked { self.alpha } else { 0.0 })
    }
}

/// Paints a connector's provenances as evenly spaced parallel lanes centered on
/// its route, `marked` where the hover has picked the connector out.
fn paint_lanes(
    c: &Connector,
    marked: bool,
    alpha: f32,
    bounds: Bounds<Pixels>,
    camera: Camera,
    palette: &Palette,
    window: &mut Window,
) {
    let z = camera.zoom;
    let screen: Vec<(f32, f32)> = c
        .points
        .iter()
        .map(|(x, y)| {
            let s = to_screen(*x, *y, bounds, camera);
            (f32::from(s.x), f32::from(s.y))
        })
        .collect();
    let n = c.kinds.len() as f32;
    for (i, kind) in c.kinds.iter().enumerate() {
        let slot = i as f32 - (n - 1.0) / 2.0;
        let lane = lane_polyline(&screen, slot * PARALLEL_SPACING * z);
        paint_connector(&lane, *kind, marked, alpha, z, palette, window);
    }
}

/// Where `b` falls in the window under `camera`.
fn screen_rect(b: &PaintBox, bounds: Bounds<Pixels>, camera: Camera) -> Bounds<Pixels> {
    let origin = to_screen(b.x, b.y, bounds, camera);
    let zoom = camera.zoom;
    Bounds::new(origin, size(px(b.w as f32 * zoom), px(b.h as f32 * zoom)))
}

/// Paints a box's filled quad, over which the hover mark's wash goes.
fn paint_box(
    b: &PaintBox,
    rect: Bounds<Pixels>,
    zoom: f32,
    palette: &Palette,
    window: &mut Window,
) {
    let corner = px(6. * zoom);
    let ((border_w, border_c), mark) = box_emphasis(b, zoom, palette, rect, corner);
    window.paint_quad(quad(
        rect,
        corner,
        b.fill,
        border_w,
        border_c,
        BorderStyle::Solid,
    ));
    mark(window);
}

/// A box's two emphases, told apart by the channel each takes: the quad border
/// to stroke, which the just-toggled emphasis fades from the themed border to
/// the accent and from a normal to a thick stroke, and a deferred paint of the
/// hover mark's wash, which the fill alone carries.
fn box_emphasis(
    b: &PaintBox,
    zoom: f32,
    palette: &Palette,
    rect: Bounds<Pixels>,
    corner: Pixels,
) -> ((Pixels, Hsla), impl FnOnce(&mut Window)) {
    let (border_w, border_c) = if b.highlight > 0.0 {
        let normal = px(1.);
        let thick = px((2. * zoom).max(2.));
        let accent = Rgba {
            a: b.highlight,
            ..palette.highlight
        };
        (
            normal + (thick - normal) * b.highlight,
            Hsla::from(faded(accent, b.alpha)),
        )
    } else {
        (px(1.), Hsla::from(faded(palette.border, b.alpha)))
    };
    let wash = b.wash(palette);
    let mark = move |window: &mut Window| {
        if wash.a > 0.0 {
            window.paint_quad(quad(
                rect,
                corner,
                wash,
                px(0.),
                Hsla::transparent_black(),
                BorderStyle::Solid,
            ));
        }
    };
    ((border_w, border_c), mark)
}

/// Where a box's label falls within it, in screen pixels: centered in the strip
/// its own box keeps for a label — a container's title band, or the whole box
/// for anything else — which no other box's label reaches into.
struct LabelPlacement {
    /// Font size the label is drawn at.
    size: f32,
    /// Distance from the box's top edge to the label's top.
    top: f32,
    /// Width held left of the label for a disclosure caret; `None` for a box that
    /// carries none. The caret keeps the proportion the layout reserved for it
    /// whatever size the label is drawn at.
    caret: Option<f32>,
    /// Width the label text is truncated to.
    avail: f32,
    /// Whether the title is lifted to the top of its box, over ground the boxes
    /// inside it are drawn on, which a container whose band the zoom has shrunk
    /// below the floored label does.
    floats: bool,
}

impl LabelPlacement {
    /// Where `b`'s label falls in the `rect` it is drawn at, or `None` if that
    /// rect is too short or too narrow for the floored label — what survives
    /// truncation in a narrower one names nothing, so the box is left bare.
    fn of(b: &PaintBox, rect: Bounds<Pixels>, zoom: f32) -> Option<Self> {
        let (w, h) = (f32::from(rect.size.width), f32::from(rect.size.height));
        let size = (LABEL_SIZE as f32 * zoom).max(LABEL_FLOOR);
        let needed = size + 2. * LABEL_PAD;
        if h < needed {
            return None;
        }
        let band = LABEL_BAND as f32 * zoom;
        let (top, floats) = match b.compound {
            true if band >= needed => ((band - size) / 2., false),
            true => (LABEL_PAD, true),
            false => ((h - size) / 2., false),
        };
        let caret = b
            .expandable
            .then(|| size * (CARET_RESERVE / LABEL_SIZE) as f32);
        let avail = w - 2. * LABEL_PAD - caret.unwrap_or(0.);
        (avail >= LABEL_MIN_WIDTH).then_some(LabelPlacement {
            size,
            top,
            caret,
            avail,
            floats,
        })
    }

    /// Width the caret takes out of the plate, zero where the box carries none.
    fn reserved(&self) -> f32 {
        self.caret.unwrap_or(0.)
    }

    /// The plate a `text`-wide label fills in `rect`: the caret and the text
    /// centered together as one unit, with the label's clearance around them.
    fn plate(&self, rect: Bounds<Pixels>, text: f32) -> Bounds<Pixels> {
        let unit = self.reserved() + text;
        let left = f32::from(rect.origin.x) + (f32::from(rect.size.width) - unit) / 2.;
        Bounds::new(
            point(
                px(left - LABEL_PAD),
                px(f32::from(rect.origin.y) + self.top - LABEL_PAD),
            ),
            size(px(unit + 2. * LABEL_PAD), px(self.size + 2. * LABEL_PAD)),
        )
    }

    /// Where the caret sits in `plate`: its left edge, at the label's vertical
    /// center.
    fn caret_at(&self, plate: Bounds<Pixels>) -> Point<Pixels> {
        point(
            plate.origin.x + px(LABEL_PAD),
            plate.origin.y + px(LABEL_PAD + self.size / 2.),
        )
    }

    /// The handle box `id`'s label is, once drawn on `plate`; `None` for a box
    /// that carries no disclosure caret.
    fn title(&self, plate: Bounds<Pixels>, id: Id) -> Option<TitleTarget> {
        self.caret.map(|_| TitleTarget { plate, id })
    }
}

/// The box a title painted over `pos` (window px) opens or closes.
pub(crate) fn title_at(titles: &[TitleTarget], pos: Point<Pixels>) -> Option<Id> {
    titles.iter().find(|t| t.plate.contains(&pos)).map(|t| t.id)
}

/// Paints a box's label, truncated to the width there is room for, with a
/// disclosure caret for an expandable box, and reports the title a click on it
/// toggles. The font is the window's inherited UI text style so the views stay
/// visually coherent, and a container's title is bold.
///
/// `floated` collects what the floating titles have taken, and a label that
/// would land on any of it is dropped rather than printed over it. Since the
/// caller walks the boxes outermost first, that leaves the coarser, more telling
/// name standing.
fn paint_box_label(
    b: &PaintBox,
    rect: Bounds<Pixels>,
    zoom: f32,
    palette: &Palette,
    floated: &mut Vec<Bounds<Pixels>>,
    window: &mut Window,
    cx: &mut App,
) -> Option<TitleTarget> {
    let placement = LabelPlacement::of(b, rect, zoom)?;
    let fs = placement.size;
    let label_color = faded(bviz_ui_theme::label_on(b.fill).into(), b.alpha);
    let base = bviz_ui_assets::text_metrics(window).font;
    let font = if b.compound { base.bold() } else { base };
    let line = bviz_ui_components::shape_truncated(
        b.label,
        placement.avail,
        font,
        px(fs),
        label_color.into(),
        window,
    );
    let plate = placement.plate(rect, f32::from(line.width()));
    if floated.iter().any(|taken| taken.intersects(&plate)) {
        return None;
    }
    if placement.floats {
        floated.push(plate);
    }
    // The plate is painted in the box's own ground, wash and all, so a name reads
    // against its box rather than against the routes crossing it and leaves no
    // rectangle of its own behind.
    let ground = bviz_ui_theme::over(b.wash(palette), b.fill);
    window.paint_quad(quad(
        plate,
        px(0.),
        ground,
        px(0.),
        Hsla::transparent_black(),
        BorderStyle::Solid,
    ));
    let _ = line.paint(
        plate.origin + point(px(LABEL_PAD + placement.reserved()), px(LABEL_PAD)),
        px(fs),
        TextAlign::Left,
        None,
        window,
        cx,
    );

    if let Some(caret) = placement.caret {
        // Phosphor icons render as `svg()` elements, which cannot be painted
        // inside this canvas, so the caret is a small filled triangle.
        paint_caret(
            placement.caret_at(plate),
            px(caret),
            px(fs),
            b.compound,
            label_color,
            window,
        );
    }
    placement.title(plate, b.id)
}

/// Paints a small filled-triangle disclosure caret inside the reserved
/// `footprint` whose left edge is `at.x`, centered vertically at `at.y`. The
/// triangle width fills the footprint less a trailing gap before the label; its
/// height scales with the font size `fs`. `down` points it downward (expanded),
/// otherwise rightward (collapsed).
fn paint_caret(
    at: Point<Pixels>,
    footprint: Pixels,
    fs: Pixels,
    down: bool,
    color: Rgba,
    window: &mut Window,
) {
    let s = f32::from(footprint) * 0.6; // triangle width within the footprint
    let height = f32::from(fs) * 0.5; // triangle height, scaling with the label
    let left = at.x;
    let right = left + px(s);
    let (top, bottom) = (at.y - px(height / 2.), at.y + px(height / 2.));
    let mut tri = PathBuilder::fill();
    if down {
        // Apex down, base along the top.
        tri.add_polygon(
            &[
                point(left, top),
                point(right, top),
                point(left + px(s / 2.), bottom),
            ],
            true,
        );
    } else {
        // Apex right, base along the left.
        tri.add_polygon(
            &[point(left, top), point(right, at.y), point(left, bottom)],
            true,
        );
    }
    if let Ok(path) = tri.build() {
        window.paint_path(path, color);
    }
}

/// A polyline parallel to `pts` at signed perpendicular distance `d` (screen px),
/// for drawing a node pair's several provenances as evenly spaced parallel lanes.
/// Every vertex is pushed off its segments' normals — the endpoints perpendicular
/// to their one segment, the interior mitered at the corners — so a lane stays
/// parallel to the route along its whole length and meets each node offset
/// alongside its siblings rather than converging to a single point.
pub(crate) fn lane_polyline(pts: &[(f32, f32)], d: f32) -> Vec<(f32, f32)> {
    if d == 0.0 || pts.len() < 2 {
        return pts.to_vec();
    }
    let left_normal = |from: (f32, f32), to: (f32, f32)| {
        let (dx, dy) = (to.0 - from.0, to.1 - from.1);
        let l = (dx * dx + dy * dy).sqrt().max(1e-3);
        (-dy / l, dx / l)
    };
    let n = pts.len();
    let mut out = Vec::with_capacity(n);
    let ns = left_normal(pts[0], pts[1]);
    out.push((pts[0].0 + ns.0 * d, pts[0].1 + ns.1 * d));
    for i in 1..n - 1 {
        out.push(miter_vertex(pts[i - 1], pts[i], pts[i + 1], d));
    }
    let ne = left_normal(pts[n - 2], pts[n - 1]);
    out.push((pts[n - 1].0 + ne.0 * d, pts[n - 1].1 + ne.1 * d));
    out
}

/// Offsets the corner `b` (between segments `a->b` and `b->c`) by `d` along the
/// miter direction, keeping it distance `d` from both segments. A near-reversal,
/// or a miter that would shoot far past a sharp corner, falls back to one
/// segment's normal.
fn miter_vertex(a: (f32, f32), b: (f32, f32), c: (f32, f32), d: f32) -> (f32, f32) {
    let unit = |(x, y): (f32, f32)| {
        let l = (x * x + y * y).sqrt().max(1e-3);
        (x / l, y / l)
    };
    let d1 = unit((b.0 - a.0, b.1 - a.1));
    let d2 = unit((c.0 - b.0, c.1 - b.1));
    let (n1, n2) = ((-d1.1, d1.0), (-d2.1, d2.0)); // left normals
    let m = (n1.0 + n2.0, n1.1 + n2.1);
    let ml = (m.0 * m.0 + m.1 * m.1).sqrt();
    if ml < 1e-3 {
        return (b.0 + n1.0 * d, b.1 + n1.1 * d);
    }
    let mdir = (m.0 / ml, m.1 / ml);
    let cos_half = (mdir.0 * n1.0 + mdir.1 * n1.1).max(0.35); // miter limit
    let scale = d / cos_half;
    (b.0 + mdir.0 * scale, b.1 + mdir.1 * scale)
}

/// Strokes a connector polyline (screen coordinates) and caps its end with a
/// filled arrowhead pointing along the final segment.
fn paint_connector(
    points: &[(f32, f32)],
    kind: EdgeKind,
    marked: bool,
    alpha: f32,
    zoom: f32,
    palette: &Palette,
    window: &mut Window,
) {
    if points.len() < 2 {
        return;
    }
    let color = faded(palette.edge_color(kind, marked), alpha);
    let pts: Vec<Point<Pixels>> = points.iter().map(|(x, y)| point(px(*x), px(*y))).collect();

    let n = pts.len();
    let tip = pts[n - 1];
    let (dir, seg) = direction(pts[n - 2], tip);
    let trim = stem_trim(zoom).min(seg);
    let line_end = point(tip.x - px(dir.0 * trim), tip.y - px(dir.1 * trim));

    let width = match marked {
        true => MARKED_WIDTH,
        false => CONNECTOR_WIDTH,
    };
    let mut line = PathBuilder::stroke(px(width * zoom));
    line.move_to(pts[0]);
    for p in &pts[1..n - 1] {
        line.line_to(*p);
    }
    line.line_to(line_end);
    if let Ok(path) = line.build() {
        window.paint_path(path, color);
    }
    paint_arrowhead(tip, dir, zoom, color, window);
}

/// The unit vector from `a` to `b`, and how far apart they are.
fn direction(a: Point<Pixels>, b: Point<Pixels>) -> ((f32, f32), f32) {
    let (dx, dy) = (f32::from(b.x - a.x), f32::from(b.y - a.y));
    let len = (dx * dx + dy * dy).sqrt().max(1e-3);
    ((dx / len, dy / len), len)
}

/// How far back from the tip a connector's line stops.
fn stem_trim(zoom: f32) -> f32 {
    (ARROW_LENGTH - ARROW_OVERDRAW) as f32 * zoom
}

/// Paints the triangle [`arrowhead`] describes.
fn paint_arrowhead(
    tip: Point<Pixels>,
    dir: (f32, f32),
    zoom: f32,
    color: Rgba,
    window: &mut Window,
) {
    let mut head = PathBuilder::fill();
    head.add_polygon(&arrowhead(tip, dir, zoom), true);
    if let Ok(path) = head.build() {
        window.paint_path(path, color);
    }
}

/// The triangle capping a connector at `tip` and pointing along the unit vector
/// `dir`, sized as [`ARROW_LENGTH`] and [`ARROW_HALF_WIDTH`] give it at `zoom`.
fn arrowhead(tip: Point<Pixels>, dir: (f32, f32), zoom: f32) -> [Point<Pixels>; 3] {
    let (length, half_width) = (ARROW_LENGTH as f32 * zoom, ARROW_HALF_WIDTH as f32 * zoom);
    let (ux, uy) = dir;
    let (nx, ny) = (-uy, ux);
    let base = point(tip.x - px(ux * length), tip.y - px(uy * length));
    [
        tip,
        point(base.x + px(nx * half_width), base.y + px(ny * half_width)),
        point(base.x - px(nx * half_width), base.y - px(ny * half_width)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::camera::from_screen;
    use crate::morph::Transition;
    use bviz_graph_layout::LaidOut;
    use gpui::rgb;
    use std::collections::HashSet;
    use std::sync::Arc;
    use std::time::Instant;

    /// The zooms the caret invariant is checked over, spanning both sides of the
    /// band width at which a container's title stops fitting in it.
    const ZOOMS: [f32; 9] = [0.05, 0.1, 0.2, 0.4, 0.5, 0.6, 0.8, 1.0, 2.0];

    fn lbox(
        id: u32,
        label: &str,
        x: f64,
        y: f64,
        w: f64,
        h: f64,
        compound: bool,
    ) -> bviz_graph_layout::Box {
        bviz_graph_layout::Box {
            id: Id(id),
            label: label.into(),
            kind: Kind::Source,
            x,
            y,
            width: w,
            height: h,
            compound,
            group: false,
        }
    }

    /// A container large enough to carry a title at every zoom in [`ZOOMS`],
    /// holding one child over all of it but the title band — so a title floated
    /// out of that band lands wholly on the child, and the container's only
    /// ground of its own is the band itself.
    fn covered_container() -> LaidOut {
        let (w, h) = (4000.0, 3000.0);
        LaidOut {
            boxes: vec![
                lbox(0, "container", 0.0, 0.0, w, h, true),
                lbox(1, "member", 0.0, LABEL_BAND, w, h - LABEL_BAND, false),
            ],
            ..Default::default()
        }
    }

    /// A box as a frame draws it, where the theme and the marked set a full
    /// [`PaintBox::of`] resolves have no bearing on what is measured.
    fn paint_box_of<'a>(p: &Placed<'a>, expandable: &Expandable) -> PaintBox<'a> {
        PaintBox {
            id: p.b.id,
            x: p.rect.x,
            y: p.rect.y,
            w: p.rect.w,
            h: p.rect.h,
            compound: p.b.compound,
            expandable: p.expandable(expandable),
            fill: rgb(0),
            label: &p.b.label,
            highlight: 0.0,
            marked: false,
            alpha: p.alpha,
        }
    }

    /// The boxes of `g` as a frame under `morph` draws them, placed as
    /// [`canvas_body`] places them.
    fn placed<'a>(g: &'a LaidOut, morph: Option<&Frame>) -> Vec<Placed<'a>> {
        g.boxes
            .iter()
            .map(|b| {
                let (rect, alpha) = morph.map_or((Rect::of(b), 1.0), |m| m.place(b));
                Placed {
                    b,
                    rect,
                    alpha,
                    leaving: false,
                }
            })
            .collect()
    }

    /// Which boxes of [`covered_container`] open and close: both, as a container
    /// and the collapsed group filling it do, so a click landing on the wrong one
    /// still toggles something; and the container alone, as one holding a plain
    /// leaf does.
    fn expansion_sets() -> [Expandable; 2] {
        [
            Expandable::Containers(Arc::new(HashSet::from([Id(0), Id(1)]))),
            Expandable::Containers(Arc::new(HashSet::from([Id(0)]))),
        ]
    }

    /// A caret a frame painted: the box it belongs to, the point at the middle of
    /// its footprint, and whether the title carrying it floated out of its band.
    struct DrawnCaret {
        id: Id,
        at: Point<Pixels>,
        floats: bool,
    }

    /// The titles a frame drawing `boxes` records under `camera`, and a caret for each,
    /// following the same order and overlap arbitration [`canvas_body`] paints
    /// in. Every label is measured at the widest its box has room for, which puts
    /// the plate at its widest and the caret furthest from the box's own center.
    fn drawn_titles(
        boxes: &[Placed],
        expandable: &Expandable,
        viewport: Bounds<Pixels>,
        camera: Camera,
    ) -> (Vec<TitleTarget>, Vec<DrawnCaret>) {
        let mut floated: Vec<Bounds<Pixels>> = Vec::new();
        let mut titles: Vec<TitleTarget> = Vec::new();
        let mut carets: Vec<DrawnCaret> = Vec::new();
        for p in boxes {
            let pb = paint_box_of(p, expandable);
            let rect = screen_rect(&pb, viewport, camera);
            let Some(placement) = LabelPlacement::of(&pb, rect, camera.zoom) else {
                continue;
            };
            let plate = placement.plate(rect, placement.avail);
            if floated.iter().any(|taken| taken.intersects(&plate)) {
                continue;
            }
            if placement.floats {
                floated.push(plate);
            }
            let Some(title) = placement.title(plate, p.b.id) else {
                continue;
            };
            carets.push(DrawnCaret {
                id: p.b.id,
                at: placement.caret_at(plate) + point(px(placement.reserved() / 2.), px(0.)),
                floats: placement.floats,
            });
            titles.push(title);
        }
        (titles, carets)
    }

    /// A viewport laid out away from the window origin, so a resolution that
    /// neglects to take the canvas offset off the cursor lands visibly wrong.
    fn viewport() -> Bounds<Pixels> {
        Bounds::new(point(px(10.), px(20.)), size(px(800.), px(600.)))
    }

    #[test]
    fn a_drawn_title_resolves_to_the_box_it_names() {
        for expandable in expansion_sets() {
            check_titles_resolve(&expandable);
        }
    }

    /// Asserts that every title drawn over [`covered_container`] both toggles and
    /// hovers the box it names, at every zoom in [`ZOOMS`].
    fn check_titles_resolve(expandable: &Expandable) {
        let g = covered_container();
        let (mut floating, mut banded, mut covered) = (0, 0, 0);
        for zoom in ZOOMS {
            let camera = Camera {
                zoom,
                pan: (0.0, 0.0),
            };
            let (titles, carets) = drawn_titles(&placed(&g, None), expandable, viewport(), camera);
            for caret in carets {
                let DrawnCaret { id, at, floats } = caret;
                match floats {
                    true => floating += 1,
                    false => banded += 1,
                }
                let resolved = crate::box_at(&titles, &g, None, viewport(), camera, at);
                assert_eq!(
                    resolved.map(|b| b.id),
                    Some(id),
                    "zoom {zoom}: the caret drawn for {id:?} resolves to another box"
                );
                // What the click keeps of that: a box it holds no handle on is
                // left alone, and every title target names one it does.
                assert_eq!(
                    resolved.filter(|b| expandable.holds(b)).map(|b| b.id),
                    Some(id),
                    "zoom {zoom}: the caret drawn for {id:?} toggles nothing"
                );
                let (gx, gy) = from_screen(at, viewport().origin, camera);
                covered +=
                    (crate::hit_box(&g.boxes, None, gx, gy).map(|b| b.id) != Some(id)) as u32;
            }
        }
        assert!(floating > 0, "no zoom floated a title out of its band");
        assert!(banded > 0, "no zoom left a title in its band");
        assert!(
            covered > 0,
            "no caret fell on a box other than its own, so the boxes alone \
            would have resolved every gesture and the case this pins never arose"
        );
    }

    /// How far a relayout slides [`covered_container`] for the mid-morph frame:
    /// wide enough that a box the morph places falls clear of every rect the
    /// layout laid out, so resolving a gesture against the layout alone lands on
    /// a different box or on none.
    const SLIDE: f64 = 6000.0;

    #[test]
    fn a_title_drawn_mid_morph_resolves_to_the_box_it_names() {
        let g = covered_container();
        let expandable = Expandable::Containers(Arc::new(HashSet::from([Id(0), Id(1)])));
        let t = Arc::new(
            Transition::between(Arc::new(slid(&g, SLIDE)), &g, None).expect("nothing to animate"),
        );
        // The morph's opening frame, which draws every box a whole slide away
        // from where the layout put it.
        let frame = Transition::at(&t, Instant::now()).expect("the transition was already over");

        let (mut floating, mut covered) = (0, 0);
        for zoom in ZOOMS {
            let camera = Camera {
                zoom,
                pan: (0.0, 0.0),
            };
            let boxes = placed(&g, Some(&frame));
            let (titles, carets) = drawn_titles(&boxes, &expandable, viewport(), camera);
            for DrawnCaret { id, at, floats } in carets {
                floating += floats as u32;
                assert_eq!(
                    crate::box_at(&titles, &g, Some(&frame), viewport(), camera, at).map(|b| b.id),
                    Some(id),
                    "zoom {zoom}: the caret a morph drew for {id:?} resolves to another box"
                );
                let (gx, gy) = from_screen(at, viewport().origin, camera);
                covered += (crate::hit_box(&g.boxes, Some(&frame), gx, gy).map(|b| b.id)
                    != Some(id)) as u32;
            }
        }
        assert!(floating > 0, "no zoom floated a title out of its band");
        assert!(
            covered > 0,
            "no caret fell on a box other than its own, so the boxes alone \
            would have resolved every gesture and the case this pins never arose"
        );
    }

    #[test]
    fn a_box_on_its_way_out_carries_no_disclosure_caret() {
        let g = covered_container();
        let expandable = Expandable::Containers(Arc::new(HashSet::from([Id(0)])));
        let b = &g.boxes[0];
        let held = |leaving| Placed {
            b,
            rect: Rect::of(b),
            alpha: 1.0,
            leaving,
        };
        assert!(
            held(false).expandable(&expandable),
            "the container the layout opens and closes carries no caret",
        );
        assert!(
            !held(true).expandable(&expandable),
            "a container the relayout dropped still carries a caret it cannot answer",
        );
    }

    /// The width of each container in [`side_by_side`], also the distance a
    /// relayout has to slide them for one to take the other's ground.
    const CONTAINER_WIDTH: f64 = 2000.0;

    /// Two containers abreast, each carrying a title of its own.
    fn side_by_side() -> LaidOut {
        let (w, h) = (CONTAINER_WIDTH, 1200.0);
        LaidOut {
            boxes: vec![
                lbox(0, "left", 0.0, 0.0, w, h, true),
                lbox(1, "right", w, 0.0, w, h, true),
            ],
            ..Default::default()
        }
    }

    /// The same boxes laid out `dx` units along, keeping every id.
    fn slid(g: &LaidOut, dx: f64) -> LaidOut {
        LaidOut {
            boxes: g
                .boxes
                .iter()
                .map(|b| {
                    lbox(
                        b.id.0,
                        &b.label,
                        b.x + dx,
                        b.y,
                        b.width,
                        b.height,
                        b.compound,
                    )
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn a_title_the_layout_has_moved_out_from_under_resolves_to_the_box_now_there() {
        let g = side_by_side();
        let expandable = Expandable::Containers(Arc::new(HashSet::from([Id(0), Id(1)])));
        let camera = Camera {
            zoom: 0.2,
            pan: (0.0, 0.0),
        };
        let (titles, carets) = drawn_titles(&placed(&g, None), &expandable, viewport(), camera);
        let left = carets.iter().find(|c| c.id == Id(0));
        let at = left.expect("the left container drew no title").at;
        assert_eq!(
            crate::box_at(&titles, &g, None, viewport(), camera, at).map(|b| b.id),
            Some(Id(0))
        );

        // A relayout that keeps both ids and slides the right container onto the
        // ground the left one's title was painted over. The recorded plate still
        // covers the cursor and still names a box the layout holds; what it no
        // longer names is the box drawn there.
        let moved = slid(&g, -CONTAINER_WIDTH);
        let (gx, gy) = from_screen(at, viewport().origin, camera);
        assert_eq!(
            crate::hit_box(&moved.boxes, None, gx, gy).map(|b| b.id),
            Some(Id(1)),
            "the slide left the caret over no other container, so nothing was displaced"
        );
        assert_eq!(
            crate::box_at(&titles, &moved, None, viewport(), camera, at).map(|b| b.id),
            Some(Id(1)),
            "a title from a frame the layout has moved on from still answered for it"
        );
    }
}

#[cfg(test)]
mod colors {
    use super::*;
    use bviz_ui_theme::{
        MIN_DISTINCT_COLOR, MIN_EDGED_STEP, MIN_TEXT_CONTRAST, built_in_themes, contrast_ratio,
        distance, label_on, level_step, luminance, oklch, over,
    };
    use gpui::SharedString;

    /// One node kind per accent slot: the three that carry a slot of their own,
    /// and one standing for every kind that takes the catch-all.
    const ACCENTED_KINDS: [Kind; 4] = [Kind::Action, Kind::Artifact, Kind::Source, Kind::Target];

    /// One node kind per container fill: the two that carry a fill of their own,
    /// and one standing for every kind that takes the plain one.
    const CONTAINER_KINDS: [Kind; 3] = [Kind::Package, Kind::Group, Kind::Target];

    /// Every provenance a connector is drawn in.
    const EDGE_KINDS: [EdgeKind; 4] = [
        EdgeKind::Query,
        EdgeKind::Cquery,
        EdgeKind::Aquery,
        EdgeKind::Transitive,
    ];

    /// Every theme's palette, under the name it answers to, for a rule that has
    /// to hold whichever one a window paints with.
    fn palettes() -> Vec<(SharedString, Palette)> {
        built_in_themes()
            .iter()
            .map(|theme| (theme.name.clone(), Palette::from_theme(theme)))
            .collect()
    }

    /// Every fill a box is painted in, each once.
    fn box_fills(p: &Palette) -> Vec<Rgba> {
        ACCENTED_KINDS
            .iter()
            .map(|kind| p.leaf_fill(*kind))
            .chain(CONTAINER_KINDS.iter().map(|kind| p.container_fill(*kind)))
            .collect()
    }

    /// Every ground a connector is drawn over, each once. A marked connector runs
    /// between marked boxes, and the boxes are painted before the connectors, so
    /// each fill counts both bare and under the hover mark's wash.
    fn edge_grounds(p: &Palette) -> Vec<Rgba> {
        box_fills(p)
            .into_iter()
            .flat_map(|fill| [fill, over(p.highlight_wash(1.0), fill)])
            .chain([p.background])
            .collect()
    }

    /// Every ink a route is drawn beside, each once: the text the controls
    /// framing the canvas are set in, and the name a box carries over the routes
    /// crossing it.
    fn edge_inks(p: &Palette) -> Vec<Rgba> {
        let mut inks = vec![p.label];
        for fill in box_fills(p) {
            let ink = label_on(fill).into();
            if !inks.contains(&ink) {
                inks.push(ink);
            }
        }
        inks
    }

    /// A leaf's kind is read off the color of the box, so no two kinds may be
    /// taken for each other and none may sink into the canvas they are drawn on.
    #[test]
    fn every_node_kind_is_told_from_the_others_and_from_the_canvas() {
        for (theme, p) in palettes() {
            for (i, kind) in ACCENTED_KINDS.iter().enumerate() {
                let fill = p.leaf_fill(*kind);
                let over_canvas = distance(fill, p.background);
                assert!(
                    over_canvas >= MIN_DISTINCT_COLOR,
                    "{theme} {kind:?}: {over_canvas}"
                );
                for other in &ACCENTED_KINDS[i + 1..] {
                    let apart = distance(fill, p.leaf_fill(*other));
                    assert!(
                        apart >= MIN_DISTINCT_COLOR,
                        "{theme} {kind:?}/{other:?}: {apart}"
                    );
                }
            }
        }
    }

    /// A connector is followed across the canvas and over the boxes it runs
    /// between, so its provenance has to be told from every ground it crosses,
    /// wherever the hover leaves it.
    #[test]
    fn every_edge_provenance_is_told_from_what_it_crosses() {
        for (theme, p) in palettes() {
            let grounds = edge_grounds(&p);
            for kind in EDGE_KINDS {
                for marked in [false, true] {
                    let color = p.edge_color(kind, marked);
                    for ground in &grounds {
                        let apart = distance(color, *ground);
                        assert!(
                            apart >= MIN_DISTINCT_COLOR,
                            "{theme} {kind:?} marked {marked} on a ground: {apart}"
                        );
                    }
                }
            }
        }
    }

    /// A route is followed past writing as well as over fills — a box's own name
    /// over the fill it crosses, the legend and toolbar text beyond the canvas.
    /// A line that meets one of those has to go on reading as a line rather than
    /// as more of the writing, wherever the hover leaves it.
    #[test]
    fn every_edge_provenance_is_told_from_the_inks_it_runs_beside() {
        for (theme, p) in palettes() {
            let inks = edge_inks(&p);
            for kind in EDGE_KINDS {
                for marked in [false, true] {
                    let color = p.edge_color(kind, marked);
                    for ink in &inks {
                        let apart = distance(color, *ink);
                        assert!(
                            apart >= MIN_DISTINCT_COLOR,
                            "{theme} {kind:?} marked {marked} beside an ink: {apart}"
                        );
                    }
                }
            }
        }
    }

    /// The color of a connector names the query it came from, and a lane runs
    /// beside lanes of every other provenance, each of the pair left as the
    /// hover found it or carried by [`MARK_LIFT`]. No two of those meet.
    #[test]
    fn no_two_provenances_meet_however_the_hover_marks_them() {
        for (theme, p) in palettes() {
            for (i, kind) in EDGE_KINDS.iter().enumerate() {
                for other in &EDGE_KINDS[i + 1..] {
                    for marked in [false, true] {
                        for other_marked in [false, true] {
                            let apart = distance(
                                p.edge_color(*kind, marked),
                                p.edge_color(*other, other_marked),
                            );
                            assert!(
                                apart >= MIN_DISTINCT_COLOR,
                                "{theme} {kind:?} marked {marked} / {other:?} marked \
                                 {other_marked}: {apart}"
                            );
                        }
                    }
                }
            }
        }
    }

    /// A marked lane runs beside unmarked lanes of its own query, so the mark
    /// has to carry the line clear of the one it is drawn from.
    #[test]
    fn a_marked_connector_is_told_from_the_unmarked_line_of_its_own_query() {
        for (theme, p) in palettes() {
            for kind in EDGE_KINDS {
                let seen = distance(p.edge_color(kind, true), p.edge_color(kind, false));
                assert!(seen >= MIN_DISTINCT_COLOR, "{theme} {kind:?}: {seen}");
            }
        }
    }

    /// Slack allowed on a hue compared across the lift, as a turn of the wheel,
    /// covering the rounding of the f32 arithmetic through it.
    const HUE_SLACK: f32 = 1e-3;

    /// A connector's hue is what names the query it came from, so a mark leaves
    /// the connector saying what it said and only turns the light up on it.
    #[test]
    fn a_mark_holds_the_hue_that_names_the_query() {
        for (theme, p) in palettes() {
            for kind in EDGE_KINDS {
                let bare = oklch(p.edge_color(kind, false)).hue;
                let marked = oklch(p.edge_color(kind, true)).hue;
                let turned = (bare - marked).abs();
                assert!(
                    turned.min(1.0 - turned) < HUE_SLACK,
                    "{theme} {kind:?}: hue {bare} -> {marked}"
                );
            }
        }
    }

    /// Which way a mark carries a connector is the theme's to say: a marked line
    /// stands further off the canvas in lightness than the line it marks,
    /// whether the canvas is the dark end of the range or the light one.
    #[test]
    fn a_mark_carries_a_connector_away_from_the_canvas() {
        for (theme, p) in palettes() {
            let canvas = luminance(p.background);
            for kind in EDGE_KINDS {
                let bare = luminance(p.edge_color(kind, false)) - canvas;
                let marked = luminance(p.edge_color(kind, true)) - canvas;
                assert!(
                    marked.abs() > bare.abs() && marked.signum() == bare.signum(),
                    "{theme} {kind:?}: {bare} -> {marked} off the canvas"
                );
            }
        }
    }

    /// The accent border is what draws the eye back to the box just toggled, so
    /// it has to stand off the border it replaces, the fill it is drawn around,
    /// the canvas beyond it and the connectors meeting it.
    #[test]
    fn the_highlight_is_told_from_everything_it_is_drawn_against() {
        for (theme, p) in palettes() {
            let against = box_fills(&p)
                .into_iter()
                .chain([p.background, p.border])
                .chain(
                    EDGE_KINDS
                        .iter()
                        .flat_map(|kind| [p.edge_color(*kind, false), p.edge_color(*kind, true)]),
                );
            for color in against {
                let apart = distance(p.highlight, color);
                assert!(apart >= MIN_DISTINCT_COLOR, "{theme}: {apart}");
            }
        }
    }

    /// A box carries its name on its own fill, whichever kind picked that fill,
    /// and on the ground the hover mark's wash makes of it — the one wash a fill
    /// ever wears. The ink is picked from the fill alone, so that one choice has
    /// to hold on both.
    #[test]
    fn a_name_is_legible_on_the_box_it_names() {
        for (theme, p) in palettes() {
            for fill in box_fills(&p) {
                let ink = label_on(fill);
                for ground in [fill, over(p.highlight_wash(1.0), fill)] {
                    let held = contrast_ratio(ink, ground);
                    assert!(held >= MIN_TEXT_CONTRAST, "{theme}: {held}");
                }
            }
        }
    }

    /// Nesting is read off the fills: a container stands off the canvas it is
    /// laid on, and the package holding it stands off the container in turn.
    /// Each pair meets at a drawn border.
    #[test]
    fn a_container_and_the_package_holding_it_step_off_the_canvas_in_turn() {
        for (theme, p) in palettes() {
            let plain = p.container_fill(Kind::Target);
            let off_canvas = level_step(plain, p.background);
            assert!(off_canvas >= MIN_EDGED_STEP, "{theme}: {off_canvas} levels");
            let off_container = level_step(p.container_fill(Kind::Package), plain);
            assert!(
                off_container >= MIN_EDGED_STEP,
                "{theme}: {off_container} levels"
            );
        }
    }

    /// A group is a container of files, which its fill says by carrying their
    /// accent — far enough from a plain container to be seen carrying it, and
    /// not so far that the group passes for one of the files inside it.
    #[test]
    fn a_group_reads_as_a_container_of_the_files_it_holds() {
        for (theme, p) in palettes() {
            let group = p.container_fill(Kind::Group);
            let off_plain = distance(group, p.container_fill(Kind::Target));
            assert!(off_plain >= MIN_DISTINCT_COLOR, "{theme}: {off_plain}");
            let off_file = distance(group, p.leaf_fill(Kind::Artifact));
            assert!(off_file >= MIN_DISTINCT_COLOR, "{theme}: {off_file}");
        }
    }

    /// The wash marks the box the hover picked out: whatever fill that box wears,
    /// the wash carries it far enough off the one the box wears unmarked to be
    /// seen beside the boxes still wearing it.
    #[test]
    fn a_marked_box_is_seen_whatever_fill_it_wears() {
        for (theme, p) in palettes() {
            for fill in box_fills(&p) {
                let seen = distance(over(p.highlight_wash(1.0), fill), fill);
                assert!(seen >= MIN_DISTINCT_COLOR, "{theme}: {seen}");
            }
        }
    }
}
