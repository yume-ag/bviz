use super::boxes::PaintBox;
use super::status::{STATUS_RIM, StatusBadge, status_half, status_reserve};
use super::{faded, paint_path};
use crate::Palette;
use crate::pan_bench::{self, Tally};
use crate::status::Status;
use bviz_graph_layout::Id;
use bviz_graph_layout::layout::{CARET_RESERVE, LABEL_BAND};
use bviz_graph_layout::shown::fit_label;
use bviz_ui_components::Inked;
use gpui::{
    App, BorderStyle, Bounds, Hsla, PathBuilder, Pixels, Point, Rgba, TextAlign, Window, point, px,
    quad, size,
};

/// Box label font size in layout pixels.
const LABEL_SIZE: f64 = 13.0;

/// Screen-pixel size a label is never drawn below. Zoom scales the label with
/// the box down to this floor and no further, so a box wide enough to hold its
/// name shows it rather than fading to a blank rectangle.
const LABEL_FLOOR: f32 = 11.0;

/// Screen-pixel step a label's font size is snapped to above the floor. Text is
/// shaped and its glyphs rasterized per distinct size, so on the snapped sizes
/// successive zoom frames, and a zoom back to an earlier size, reuse both. A
/// label is drawn within half a step of its zoomed size.
const LABEL_SIZE_STEP: f32 = 0.25;

/// Screen-pixel clearance held between a label and its box's edges, so a label
/// held at the floor is not painted flush against the border.
const LABEL_PAD: f32 = 2.0;

/// Screen width a box needs to spare for its label before one is drawn: any
/// narrower and what survives truncation names nothing, so the box is left bare.
const LABEL_MIN_WIDTH: f32 = 22.0;

/// A disclosure title as a frame painted it, by which a gesture landing on the
/// title reaches the box it names.
pub(crate) struct TitleTarget {
    plate: Bounds<Pixels>,
    id: Id,
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
    /// The status whose badge the label holds room for at its left end, ahead
    /// of the caret.
    status: Option<Status>,
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
        let size = ((LABEL_SIZE as f32 * zoom / LABEL_SIZE_STEP).round() * LABEL_SIZE_STEP)
            .max(LABEL_FLOOR);
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
        let placement = LabelPlacement {
            size,
            top,
            caret,
            status: b.status,
            avail: 0.,
            floats,
        };
        let avail = w - 2. * LABEL_PAD - placement.reserved();
        (avail >= LABEL_MIN_WIDTH).then_some(LabelPlacement { avail, ..placement })
    }

    /// What the label is drawn at of its layout size.
    fn scale(&self) -> f32 {
        self.size / LABEL_SIZE as f32
    }

    /// Width the badge takes out of the plate, zero where the box carries none.
    fn badge_reserved(&self) -> f32 {
        self.status.map_or(0., |_| status_reserve(self.scale()))
    }

    /// Width the badge and the caret take out of the plate ahead of the text.
    fn reserved(&self) -> f32 {
        self.badge_reserved() + self.caret.unwrap_or(0.)
    }

    /// The plate a `text`-wide label fills in `rect`: the badge, the caret and
    /// the text centered together as one unit, with the label's clearance
    /// around them.
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
            plate.origin.x + px(LABEL_PAD + self.badge_reserved()),
            plate.origin.y + px(LABEL_PAD + self.size / 2.),
        )
    }

    /// The badge set at the left end of `plate`, at the label's vertical
    /// center, to be drawn at `alpha`; `None` for a box the build reports
    /// nothing of.
    fn badge(&self, plate: Bounds<Pixels>, alpha: f32) -> Option<StatusBadge> {
        let status = self.status?;
        let half = status_half(self.scale());
        Some(StatusBadge {
            center: point(
                plate.origin.x + px(LABEL_PAD + STATUS_RIM + half),
                plate.origin.y + px(LABEL_PAD + self.size / 2.),
            ),
            half,
            status,
            alpha,
        })
    }

    /// Where the text begins in `plate`: its top left corner, past the badge
    /// and the caret.
    fn text_at(&self, plate: Bounds<Pixels>) -> Point<Pixels> {
        plate.origin + point(px(LABEL_PAD + self.reserved()), px(LABEL_PAD))
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

/// `text`, a caption, fitted to `avail` as [`fit_label`] fits it and shaped in
/// `font` at `size`, set in the first of `inks` and where a label's
/// repository and package are, in the second.
fn shape_fitted(
    text: &str,
    avail: f32,
    font: gpui::Font,
    size: Pixels,
    [ink, muted]: [Hsla; 2],
    window: &mut Window,
) -> gpui::ShapedLine {
    let mut width = |text: &str| {
        let line = bviz_ui_assets::shape_line(window, text.to_owned(), font.clone(), size, ink);
        f32::from(line.width())
    };
    let fitted = fit_label(text, avail, &mut width);
    let inked = Inked {
        text: fitted.text(),
        muted: fitted.muted(),
    };
    let runs = inked.runs(fitted.text().len(), ink, muted);
    bviz_ui_assets::shape_runs(window, fitted.text().to_owned(), font, size, &runs)
}

/// What a painted label leaves for the passes after it.
pub(super) struct PaintedLabel {
    pub(super) title: Option<TitleTarget>,
    pub(super) caret: Option<Caret>,
    pub(super) badge: Option<StatusBadge>,
}

/// Paints a box's label, truncated to the width there is room for, and reports
/// the title a click on it toggles, the disclosure caret of an expandable box,
/// and the status badge the label holds room for; `None` where no label is
/// painted. The font is the window's inherited UI text style so the
/// views stay visually coherent, and a container's title is bold.
///
/// `floated` collects what the floating titles have taken, and a label that
/// would land on any of it is dropped rather than printed over it. Since the
/// caller walks the boxes outermost first, that leaves the coarser, more telling
/// name standing.
pub(super) fn paint_box_label(
    b: &PaintBox,
    rect: Bounds<Pixels>,
    zoom: f32,
    palette: &Palette,
    floated: &mut Vec<Bounds<Pixels>>,
    window: &mut Window,
    cx: &mut App,
) -> Option<PaintedLabel> {
    let placement = LabelPlacement::of(b, rect, zoom)?;
    let fs = placement.size;
    let label_color = faded(bviz_ui_theme::label_on(b.fill).into(), b.alpha);
    let base = bviz_ui_assets::text_metrics(window).font;
    let font = if b.compound { base.bold() } else { base };
    pan_bench::tally(Tally::ShapedLabels, 1);
    let muted = faded(bviz_ui_theme::muted_label_on(b.fill).into(), b.alpha);
    let line = shape_fitted(
        b.label,
        placement.avail,
        font,
        px(fs),
        [label_color.into(), muted.into()],
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
        placement.text_at(plate),
        px(fs),
        TextAlign::Left,
        None,
        window,
        cx,
    );

    Some(PaintedLabel {
        title: placement.title(plate, b.id),
        caret: placement.caret.map(|footprint| Caret {
            at: placement.caret_at(plate),
            footprint: px(footprint),
            fs: px(fs),
            down: b.compound,
            color: label_color,
        }),
        badge: placement.badge(plate, b.alpha),
    })
}

/// The disclosure caret a label holds room for, a small filled triangle.
/// Phosphor icons render as `svg()` elements, which cannot be painted inside
/// this canvas, so the caret is a path.
pub(super) struct Caret {
    /// The left edge of the footprint, at the caret's vertical centre.
    at: Point<Pixels>,
    /// The width held for the caret, which the triangle fills less a trailing
    /// gap before the label.
    footprint: Pixels,
    /// The label's font size, which the triangle's height scales with.
    fs: Pixels,
    /// Points the caret downward, for an expanded box; otherwise rightward.
    down: bool,
    color: Rgba,
}

impl Caret {
    pub(super) fn paint(self, window: &mut Window) {
        let Caret {
            at,
            footprint,
            fs,
            down,
            color,
        } = self;
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
            paint_path(path, color, window);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::camera::{Camera, Rect, from_screen};
    use crate::derive::Expandable;
    use crate::morph::{Frame, Transition};
    use crate::paint::boxes::{Placed, screen_rect};
    use crate::paint::status::{Glyph, STATUS_FLOOR, TRIANGLE, TRIANGLE_DOWN, cross, outset};
    use crate::spatial::Indexed;
    use bviz_graph_layout::{Kind, LaidOut};
    use gpui::rgb;
    use std::collections::HashSet;
    use std::sync::Arc;

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
            status: None,
            critical: false,
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
                let resolved =
                    crate::box_at(&titles, &Indexed::of(&g), None, viewport(), camera, at);
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
                covered += (crate::innermost(crate::boxes_over(&Indexed::of(&g), None, gx, gy))
                    .map(|b| b.id)
                    != Some(id)) as u32;
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
        let frame = Transition::at(&t, bviz_time::now()).expect("the transition was already over");

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
                    crate::box_at(
                        &titles,
                        &Indexed::of(&g),
                        Some(&frame),
                        viewport(),
                        camera,
                        at
                    )
                    .map(|b| b.id),
                    Some(id),
                    "zoom {zoom}: the caret a morph drew for {id:?} resolves to another box"
                );
                let (gx, gy) = from_screen(at, viewport().origin, camera);
                covered +=
                    (crate::innermost(crate::boxes_over(&Indexed::of(&g), Some(&frame), gx, gy))
                        .map(|b| b.id)
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

    /// The box at `rect` as a frame draws it standing at `status`, where what
    /// the theme and the marked set resolve has no bearing on what is measured.
    fn badged(
        b: &bviz_graph_layout::Box,
        expandable: bool,
        status: Option<Status>,
    ) -> PaintBox<'_> {
        PaintBox {
            expandable,
            status,
            ..paint_box_of(
                &Placed {
                    b,
                    rect: Rect::of(b),
                    alpha: 1.0,
                    leaving: false,
                },
                &Expandable::Containers(Arc::new(HashSet::new())),
            )
        }
    }

    /// How far two window coordinates may part and still count as one,
    /// covering the rounding of the f32 arithmetic that places a label.
    const PLACING_SLACK: Pixels = px(0.01);

    /// How far to either side of its middle, and how far above and below it,
    /// the badge of `status` is painted at `half`: for a glyph drawn as a path,
    /// out to the corners the sides of its rim meet at.
    fn painted_reach(status: Status, half: f32) -> (f32, f32) {
        fn farthest<const N: usize>(unit: [(f32, f32); N], half: f32) -> (f32, f32) {
            outset(unit.map(|(x, y)| (x * half, y * half)), STATUS_RIM)
                .iter()
                .fold((0.0, 0.0), |(across, along), (x, y)| {
                    (x.abs().max(across), y.abs().max(along))
                })
        }
        match Glyph::of(status) {
            Glyph::TriangleUp => farthest(TRIANGLE, half),
            Glyph::TriangleDown => farthest(TRIANGLE_DOWN, half),
            Glyph::Cross => farthest(cross(), half),
            Glyph::Disc | Glyph::Bar | Glyph::Ring | Glyph::Square => {
                (half + STATUS_RIM, half + STATUS_RIM)
            }
        }
    }

    /// A badge is laid out with the label it goes before: wherever a label is
    /// drawn, what is painted of the badge lies on the label's plate and ends
    /// before the caret begins, the caret ends before the name begins, and the
    /// name ends within the plate.
    #[test]
    fn a_badge_the_caret_and_the_name_follow_one_another_on_the_plate() {
        let boxes = [
            lbox(0, "container", 0.0, 0.0, 4000.0, 3000.0, true),
            lbox(1, "leaf", 0.0, 0.0, 400.0, 30.0, false),
        ];
        let mut labelled = 0;
        for zoom in ZOOMS {
            let camera = Camera {
                zoom,
                pan: (0.0, 0.0),
            };
            for (b, expandable) in boxes.iter().flat_map(|b| [(b, false), (b, true)]) {
                for status in Status::ALL {
                    let pb = badged(b, expandable, Some(status));
                    let rect = screen_rect(&pb, viewport(), camera);
                    let Some(placement) = LabelPlacement::of(&pb, rect, zoom) else {
                        continue;
                    };
                    labelled += 1;
                    let case = format!("zoom {zoom} {:?} {status:?}", b.id);
                    let plate = placement.plate(rect, placement.avail);
                    let badge = placement.badge(plate, 1.0).expect("the box has a status");
                    let (across, along) = painted_reach(status, badge.half);
                    let follows = |before: Pixels, after: Pixels| {
                        assert!(before <= after + PLACING_SLACK, "{case}: {before} {after}");
                    };
                    follows(plate.left(), badge.center.x - px(across));
                    follows(plate.top(), badge.center.y - px(along));
                    follows(badge.center.y + px(along), plate.bottom());
                    let caret = placement.caret_at(plate).x;
                    follows(badge.center.x + px(across), caret);
                    let text = placement.text_at(plate).x;
                    follows(caret + px(placement.caret.unwrap_or(0.)), text);
                    follows(text + px(placement.avail), plate.right());
                    follows(plate.right(), rect.right());
                }
            }
        }
        assert!(labelled > 0, "no zoom drew a label");
    }

    #[test]
    fn a_box_the_build_reports_nothing_of_holds_no_room_for_a_badge() {
        let b = lbox(0, "leaf", 0.0, 0.0, 400.0, 30.0, false);
        let camera = Camera {
            zoom: 1.0,
            pan: (0.0, 0.0),
        };
        let rect = |pb: &PaintBox| screen_rect(pb, viewport(), camera);
        let unreported = badged(&b, true, None);
        let placement = LabelPlacement::of(&unreported, rect(&unreported), 1.0).expect("a label");
        let plate = placement.plate(rect(&unreported), placement.avail);
        assert!(placement.badge(plate, 1.0).is_none());
        assert_eq!(placement.caret_at(plate).x, plate.left() + px(LABEL_PAD));

        let reported = badged(&b, true, Some(Status::Succeeded));
        let badged = LabelPlacement::of(&reported, rect(&reported), 1.0).expect("a label");
        assert!(badged.avail < placement.avail);
    }

    /// A box drawn too small for a label still says how the build ended for
    /// it: a container in the corner its title band starts at, and a leaf at
    /// its middle or, where it has the room, in the corner its bottom edge
    /// starts at.
    #[test]
    fn a_box_too_small_for_its_label_carries_its_badge_bare() {
        let zoom = 0.05;
        let camera = Camera {
            zoom,
            pan: (0.0, 0.0),
        };
        let bare = |b: &bviz_graph_layout::Box, status| {
            let pb = badged(b, false, status);
            let rect = screen_rect(&pb, viewport(), camera);
            assert!(LabelPlacement::of(&pb, rect, zoom).is_none());
            (rect, pb.bare_badge(rect, zoom))
        };
        let within = |rect: Bounds<Pixels>, bounds: Bounds<Pixels>| {
            bounds.left() >= rect.left() - PLACING_SLACK
                && bounds.top() >= rect.top() - PLACING_SLACK
                && bounds.right() <= rect.right() + PLACING_SLACK
                && bounds.bottom() <= rect.bottom() + PLACING_SLACK
        };

        let container = lbox(0, "box", 0.0, 0.0, 400.0, 280.0, true);
        let (rect, found) = bare(&container, Some(Status::RootCause));
        let found = found.expect("the box has a status");
        assert_eq!(found.badge.half, STATUS_FLOOR);
        let off = found.badge.bounds().origin - rect.origin;
        assert!(off.x.abs() < PLACING_SLACK && off.y.abs() < PLACING_SLACK);
        assert!(found.otherwise.is_none());

        let leaf = lbox(0, "box", 0.0, 0.0, 400.0, 280.0, false);
        let (rect, found) = bare(&leaf, Some(Status::RootCause));
        let found = found.expect("the box has a status");
        assert_eq!(found.badge.center, rect.center());
        let corner = StatusBadge {
            center: found.otherwise.expect("room for the badge in a corner"),
            ..found.badge
        };
        assert!(within(rect, corner.bounds()));
        assert!(corner.bounds().left() < rect.left() + PLACING_SLACK);
        assert!(corner.bounds().bottom() > rect.bottom() - PLACING_SLACK);

        let cramped = lbox(0, "box", 0.0, 0.0, 400.0, 200.0, false);
        let (rect, found) = bare(&cramped, Some(Status::RootCause));
        let found = found.expect("the box has a status");
        assert_eq!(found.badge.center, rect.center());
        assert!(found.otherwise.is_none());

        assert!(bare(&leaf, None).1.is_none());
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
            crate::box_at(&titles, &Indexed::of(&g), None, viewport(), camera, at).map(|b| b.id),
            Some(Id(0))
        );

        // A relayout that keeps both ids and slides the right container onto the
        // ground the left one's title was painted over. The recorded plate still
        // covers the cursor and still names a box the layout holds; what it no
        // longer names is the box drawn there.
        let moved = slid(&g, -CONTAINER_WIDTH);
        let (gx, gy) = from_screen(at, viewport().origin, camera);
        assert_eq!(
            crate::innermost(crate::boxes_over(&Indexed::of(&moved), None, gx, gy)).map(|b| b.id),
            Some(Id(1)),
            "the slide left the caret over no other container, so nothing was displaced"
        );
        assert_eq!(
            crate::box_at(&titles, &Indexed::of(&moved), None, viewport(), camera, at)
                .map(|b| b.id),
            Some(Id(1)),
            "a title from a frame the layout has moved on from still answered for it"
        );
    }
}
