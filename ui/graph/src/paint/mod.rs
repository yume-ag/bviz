mod boxes;
pub(super) mod connector;
mod footprint;
mod label;
mod palette;
mod status;
mod stroke;

use crate::camera::{Rect, visible_rect};
use crate::colour::Colouring;
use crate::derive::{self, Drawn, Expandable};
use crate::morph::Frame;
use crate::neighborhood::Neighborhood;
use crate::pan_bench::{self, Phase, Tally};
use crate::relation::{Direction, Relation};
use crate::scroll::paint_scrollbars;
use crate::{GraphPanel, Palette};
use boxes::{PaintBox, Placed, paint_box, paint_critical_halo, screen_rect};
use bviz_graph_layout::transition::Transition;
use bviz_graph_layout::{Connector, Id};
use bviz_invocation::EpochMicros;
use connector::{Courses, ink_lanes, ink_taper, paint_badge};
use gpui::{App, Background, Bounds, Entity, Path, Pixels, Rgba, Window};
use label::{Caret, paint_box_label};
use status::{BareBadge, StatusBadge, place_bare};
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;
use stroke::ConnectorInk;

pub(crate) use connector::{BADGE_FLOOR, paint_diamond};
pub(crate) use label::{TitleTarget, title_at};
pub(crate) use palette::Fills;
pub(crate) use status::{STATUS_FLOOR, paint_status};

/// The share of its own opacity a box or connector is drawn at where a standing
/// selection leaves it unmarked, so what the selection picks out reads against a
/// graph that has receded rather than against one drawn just as strongly. Low
/// enough that the marked part carries the eye on its own, and above zero so the
/// rest still says where in the graph that part sits.
pub(crate) const UNMARKED_FADE: f32 = 0.2;

/// What a frame of the canvas draws from that is settled as its scene is
/// built, owned so the `'static` paint closure holds no borrow of the view.
pub(crate) struct Scene {
    pub(crate) drawn: Drawn,
    pub(crate) expandable: Expandable,
    pub(crate) palette: Palette,
    /// The boxes the filter draws as no part of what it selects, which it fades
    /// rather than leaving out so the reader sees what hiding them would do.
    pub(crate) faded: Arc<HashSet<Id>>,
    /// The box the pointer rests on, whose direct neighbors it marks for as long
    /// as it does; `None` when the pointer is off the boxes or the marking is
    /// switched off.
    pub(crate) hovered: Option<Id>,
    /// The selected box, which wears the accent border; what it marks besides is
    /// the selection's to say.
    pub(crate) selected: Option<Id>,
    /// The emphasized box and the alpha its highlight paints at.
    pub(crate) highlight: Option<(Id, f32)>,
    pub(crate) colouring: Colouring,
    /// The document's playback time, which a colouring by playback reads;
    /// `None` where the document does not play back.
    pub(crate) at: Option<EpochMicros>,
    /// Whether the boxes standing for the critical path's steps wear its halo.
    pub(crate) critical_path: bool,
}

impl Scene {
    /// The share of its own opacity the filter leaves whatever `id` names: all
    /// of it, and [`UNMARKED_FADE`] of it where the filter draws `id` as no part
    /// of what it selects.
    fn filter_strength(&self, id: Id) -> f32 {
        match self.faded.contains(&id) {
            true => UNMARKED_FADE,
            false => 1.0,
        }
    }

    /// What a connector between `from` and `to` is left at by the filter, which
    /// fades a route into or out of a box it fades along with the box.
    fn edge_strength(&self, from: Id, to: Id) -> f32 {
        self.filter_strength(from).min(self.filter_strength(to))
    }
}

/// Everything a frame draws marked. A box or a connector either marking picks
/// out is drawn marked, so a selection holds its marks under a hover passing
/// over another part of the graph.
struct Marks {
    hover: Rc<Neighborhood>,
    selection: Rc<Neighborhood>,
}

impl Marks {
    fn marks_box(&self, id: Id) -> bool {
        self.hover.boxes.contains(&id) || self.selection.boxes.contains(&id)
    }

    fn marks_edge(&self, edge: (Id, Id)) -> bool {
        self.hover.edges.contains(&edge) || self.selection.edges.contains(&edge)
    }

    /// The share of its own opacity something standing at `marked` is drawn at:
    /// all of it wherever the selection marks nothing in this drawing — because
    /// none is made, or because the view folds the selected box away — and
    /// [`UNMARKED_FADE`] of it for whatever a selection that does mark something
    /// here leaves out.
    fn strength(&self, marked: bool) -> f32 {
        match marked || self.selection.boxes.is_empty() {
            true => 1.0,
            false => UNMARKED_FADE,
        }
    }

    /// What a connector standing at `marked` and left at `filtered` by the
    /// filter is drawn at, `edges` being the share a morph in flight leaves its
    /// routes at. A mark buys no exemption from [`ink_taper`]: how much of a
    /// drawn band is the line's own is the camera's to answer, and a marked line
    /// meets the camera on the same terms as the rest, so a selection stands its
    /// routes off the graph by as much at every zoom.
    fn connector_alpha(&self, marked: bool, filtered: f32, edges: f32, zoom: f32) -> f32 {
        self.badge_alpha(marked, filtered, edges) * ink_taper(zoom)
    }

    /// What a mark set on a connector standing at `marked`, `filtered` and
    /// `edges` is drawn at. A badge is held to a screen size, so it keeps its
    /// ink at every zoom.
    fn badge_alpha(&self, marked: bool, filtered: f32, edges: f32) -> f32 {
        edges * receded(self.strength(marked), filtered)
    }
}

/// The share of its own opacity something the selection leaves at `marked` and
/// the filter at `filtered` is drawn at. Both say the same thing — this is no
/// part of what the reader picked out — so the fainter of the two decides;
/// multiplied together they would take a box both leave out past the point of
/// being seen at all.
fn receded(marked: f32, filtered: f32) -> f32 {
    marked.min(filtered)
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
    entity: Entity<GraphPanel>,
) -> impl FnOnce(Bounds<Pixels>, (), &mut Window, &mut App) {
    move |bounds, _, window, cx| {
        let _span = pan_bench::span(Phase::CanvasPaint);
        let (camera, morph) = {
            let view = entity.read(cx);
            (view.camera(), view.morph_frame())
        };
        let Some((indexed, marks)) = entity.update(cx, |view, cx| {
            derive::derive_view(view, &scene.drawn, camera.zoom, cx);
            let (shown, indexed) = view.spatial.indexed(&scene.drawn, camera.zoom, cx)?;
            let marks = Marks {
                hover: view.hover_marking.marking(
                    shown,
                    scene.hovered,
                    Relation::Direct(Direction::Both),
                ),
                selection: view.selection.marking(shown),
            };
            Some((indexed, marks))
        }) else {
            return;
        };
        let visible = visible_rect(bounds, camera);
        let Some(loaded) = entity.read(cx).model.read(cx).loaded() else {
            return;
        };
        let statuses = loaded.statuses();
        let fills = Fills::of(
            scene.colouring,
            loaded.times().ok(),
            loaded.bytes().ok(),
            loaded.states(),
        );
        let critical = scene.critical_path.then(|| loaded.critical());
        let drawn: Vec<(PaintBox, Bounds<Pixels>)> = indexed
            .boxes_near(&visible, morph.as_ref())
            .map(|(b, rect, alpha)| Placed {
                b,
                rect,
                alpha,
                leaving: false,
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
                let b = PaintBox::of(&p, &scene, &marks, statuses, &fills, critical);
                let rect = screen_rect(&b, bounds, camera);
                paint_box(&b, rect, camera.zoom, &scene.palette, window);
                (b, rect)
            })
            .collect();
        pan_bench::tally(Tally::Boxes, drawn.len());
        let edges = morph.as_ref().map_or(1.0, |m| m.connectors);
        let (mut candidates, mut connectors) = (0, 0);
        // Painted over the labels, which a badge at a route's middle can fall
        // under where the route crosses a container's title band.
        let mut badges: Vec<(&Connector, Transition, f32)> = Vec::new();
        if edges > 0.0 {
            order_fence(bounds, window);
            let on_screen: HashSet<Id> = drawn.iter().map(|(b, _)| b.id).collect();
            let mut ink = ConnectorInk::new(camera.zoom, window.viewport_size());
            let mut courses = Courses::default();
            for c in indexed.routes_near(&visible, on_screen.iter().copied()) {
                candidates += 1;
                if drawn_route(c, &visible, &on_screen) {
                    let marked = marks.marks_edge((c.from, c.to));
                    let filtered = scene.edge_strength(c.from, c.to);
                    let alpha = marks.connector_alpha(marked, filtered, edges, camera.zoom);
                    ink_lanes(c, marked, alpha, bounds, camera, &mut courses, &mut ink);
                    connectors += 1;
                    if let Some(transition) = c.transition {
                        badges.push((c, transition, marks.badge_alpha(marked, filtered, edges)));
                    }
                }
            }
            ink.paint(&scene.palette, window);
            order_fence(bounds, window);
        }
        pan_bench::tally(Tally::RouteCandidates, candidates);
        pan_bench::tally(Tally::Connectors, connectors);
        for (b, rect) in drawn.iter().filter(|(b, _)| b.critical) {
            paint_critical_halo(b, *rect, camera.zoom, &scene.palette, window);
        }
        let mut floated: Vec<Bounds<Pixels>> = Vec::new();
        let mut titles: Vec<TitleTarget> = Vec::new();
        let mut carets: Vec<Caret> = Vec::with_capacity(drawn.len());
        // Painted once every label is, so that no plate lands on a badge.
        let mut statuses: Vec<StatusBadge> = Vec::with_capacity(drawn.len());
        let mut bare: Vec<BareBadge> = Vec::with_capacity(drawn.len());
        for (b, rect) in &drawn {
            let label = paint_box_label(
                b,
                *rect,
                camera.zoom,
                &scene.palette,
                &mut floated,
                window,
                cx,
            );
            match label {
                Some(label) => {
                    titles.extend(label.title);
                    carets.extend(label.caret);
                    statuses.extend(label.badge);
                }
                None => bare.extend(b.bare_badge(*rect, camera.zoom)),
            }
        }
        place_bare(bare, &floated, &mut statuses);
        entity.update(cx, |view, _| view.titles = titles);
        // The marks over the labels: those drawn as paths in one fenced run,
        // then the statuses drawn in quads, which would split the run's batch.
        order_fence(bounds, window);
        for caret in carets {
            caret.paint(window);
        }
        for (c, transition, alpha) in badges {
            paint_badge(c, transition, alpha, bounds, camera, &scene.palette, window);
        }
        for badge in statuses.iter().filter(|b| b.traced()) {
            badge.paint(&scene.palette, window);
        }
        order_fence(bounds, window);
        for badge in statuses.iter().filter(|b| !b.traced()) {
            badge.paint(&scene.palette, window);
        }
        paint_scrollbars(
            bounds,
            scene.drawn.content(),
            camera,
            &scene.palette,
            window,
        );
    }
}

/// Draws everything painted after it within `bounds` over everything painted
/// before it. gpui's wgpu renderer draws a run of paths as one batch, a single
/// offscreen multisampled pass, only while no other primitive's draw order
/// falls among theirs, and a primitive otherwise takes its order from whatever
/// it overlaps; fencing a run of paths on both sides keeps it one batch however
/// its members interleave on screen with the primitives around them.
fn order_fence(bounds: Bounds<Pixels>, window: &mut Window) {
    window.paint_layer(bounds, |_| {});
}

/// Whether a frame draws the route `c`: it crosses the view, and either the
/// view could hold the whole of it or `on_screen` — the boxes this frame drew —
/// holds a box at one of its ends. A route anchored to a box in sight is worth
/// following wherever its far end lies, and how far it reaches says nothing
/// about that: a connector meets its boxes at their borders, and a box wider
/// than the view is ordinary at a deep zoom. Left out is the route with neither
/// end in sight, which only crosses the view on its way between two places the
/// reader cannot see.
///
/// What a marking picks out has no bearing on it. A mark reaches routes whose
/// boxes are both off screen, and exempting those would lay back over the view
/// exactly the grid the reach test is here to take off it.
pub(crate) fn drawn_route(c: &Connector, visible: &Rect, on_screen: &HashSet<Id>) -> bool {
    visible.crossed_by(&c.points)
        && (visible.could_hold(&c.points)
            || on_screen.contains(&c.from)
            || on_screen.contains(&c.to))
}

/// `c` at `alpha` of its own opacity, by which a box takes its whole appearance
/// — fill, border, label — down together.
fn faded(c: Rgba, alpha: f32) -> Rgba {
    Rgba {
        a: c.a * alpha,
        ..c
    }
}

fn paint_path(path: Path<Pixels>, color: impl Into<Background>, window: &mut Window) {
    pan_bench::tally(Tally::PaintPaths, 1);
    window.paint_path(path, color);
}

#[cfg(test)]
mod tests {
    use super::*;
    use bviz_graph_layout::EdgeKind;

    /// The view the route tests cull against.
    const VIEW: Rect = Rect {
        x: 0.0,
        y: 0.0,
        w: 100.0,
        h: 100.0,
    };

    /// A route along `points`, running between the boxes `Id(0)` and `Id(1)`.
    fn route(points: Vec<(f64, f64)>) -> Connector {
        Connector {
            points,
            kinds: vec![EdgeKind::Query],
            from: Id(0),
            to: Id(1),
            transition: None,
        }
    }

    /// A route the view could not show whole is left out, so a zoom deep into a
    /// dense graph is not laid over with routes leaving for somewhere the reader
    /// cannot see.
    #[test]
    fn a_route_too_long_to_follow_is_left_out() {
        let none_drawn = HashSet::new();

        let local = route(vec![(10.0, 10.0), (40.0, 40.0)]);
        assert!(drawn_route(&local, &VIEW, &none_drawn));

        // Crosses the view, but runs far past what it can show.
        let far = route(vec![(-5000.0, 50.0), (5000.0, 50.0)]);
        assert!(!drawn_route(&far, &VIEW, &none_drawn));

        // Short enough to follow, but nowhere near the view.
        let elsewhere = route(vec![(9000.0, 9000.0), (9030.0, 9030.0)]);
        assert!(!drawn_route(&elsewhere, &VIEW, &none_drawn));
    }

    /// A box on screen at either end carries a route past the reach test, which
    /// alone would leave it out. A zoom that shrinks the view below the route's
    /// reach must not take the route with it while the box it leaves is still
    /// there to be read.
    #[test]
    fn a_route_is_drawn_from_a_box_on_screen_however_far_it_reaches() {
        let far = route(vec![(-5000.0, 50.0), (5000.0, 50.0)]);
        for anchored in [
            HashSet::from([Id(0)]),
            HashSet::from([Id(1)]),
            HashSet::from([Id(0), Id(1)]),
        ] {
            assert!(drawn_route(&far, &VIEW, &anchored));
        }
        assert!(!drawn_route(&far, &VIEW, &HashSet::new()));
    }

    /// The exemption spares a route the reach test, not the view: one that never
    /// touches the view has nothing to draw there whoever its boxes are.
    #[test]
    fn a_route_missing_the_view_is_left_out_though_its_boxes_are_on_screen() {
        let both = HashSet::from([Id(0), Id(1)]);
        let past = route(vec![(-5000.0, 9000.0), (5000.0, 9000.0)]);
        assert!(!drawn_route(&past, &VIEW, &both));
    }

    /// An orthogonal route can detour well outside the boxes it joins, so its
    /// extent overruns the view even with both ends inside it.
    #[test]
    fn a_route_that_bends_outside_the_view_is_drawn_from_a_box_on_screen() {
        let both = HashSet::from([Id(0), Id(1)]);
        let detour = route(vec![
            (10.0, 10.0),
            (10.0, 5000.0),
            (90.0, 5000.0),
            (90.0, 90.0),
        ]);
        assert!(drawn_route(&detour, &VIEW, &both));
    }

    /// A box the selection leaves out and the filter fades is left out by both
    /// for the same reason, so it recedes once: taking both fades would leave it
    /// too faint to say where in the graph the marked part sits.
    #[test]
    fn a_faded_box_the_selection_also_leaves_out_recedes_no_further() {
        assert_eq!(receded(1.0, 1.0), 1.0);
        assert_eq!(receded(UNMARKED_FADE, 1.0), UNMARKED_FADE);
        assert_eq!(receded(1.0, UNMARKED_FADE), UNMARKED_FADE);
        assert_eq!(receded(UNMARKED_FADE, UNMARKED_FADE), UNMARKED_FADE);
    }
}
