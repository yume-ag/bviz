//! The graph document's views drawn in the states a user brings them to, on
//! gpui's headless platform.

use crate::camera::{Rect, to_screen, visible_rect};
use crate::mode::LayoutMode;
use crate::{
    CycleColouring, Dismiss, GraphPanel, JumpBack, JumpForward, ShowWholeGraph, ToggleCriticalPath,
    ToggleMap, ToggleNeighborhood, ToggleOutcomes, ToggleReduce, ToggleTiming, chips, jump,
};
use bviz_graph_layout::layout::LABEL_BAND;
use bviz_graph_layout::{Id, Kind};
use bviz_input::Named;
use bviz_ui_components::ZoomIn;
use bviz_ui_graph_args::{GraphFiles, InputKind};
use bviz_ui_session::Sharing;
use bviz_ui_testing::{Harness, LOAD_DEADLINE, SWEEP_STEP, fixture};
use gpui::{Action, App, Entity, MouseButton, Pixels, Point, point};

/// Runs `test` on a window framing the joint graph of the fixture build's
/// query, cquery and aquery, once the graph view has laid it out.
pub(crate) fn with_graph(test: impl AsyncFnOnce(&mut Harness) + 'static) {
    bviz_ui_testing::run_alone(
        &[crate::register_keys],
        |session, clock, window, cx| {
            let mut files = GraphFiles::default();
            for (kind, var) in [
                (InputKind::Query, "QUERY"),
                (InputKind::Cquery, "CQUERY"),
                (InputKind::Aquery, "AQUERY"),
            ] {
                *files.file_mut(kind) = Some(Named::Path(fixture(var)));
            }
            let source = files.source().expect("the fixture names every file");
            crate::open(source, session, clock, Sharing::Alone, window, cx)
        },
        async move |harness| {
            let graph = harness.panel::<GraphPanel>();
            harness
                .until("the graph to lay out", LOAD_DEADLINE, |cx| {
                    laid_out(&graph, cx)
                })
                .await;
            test(harness).await;
        },
    );
}

/// A box a view draws.
#[derive(Clone)]
struct DrawnBox {
    /// Where a click lands on the box rather than on one it holds: the middle
    /// of a container's title band, and of any other box.
    aim: Point<Pixels>,
    id: Id,
    kind: Kind,
    container: bool,
    /// Whether a click opens or closes the box.
    opens: bool,
    label: String,
}

/// Each box `graph` draws whose aim it shows, aimed nearest the middle of its
/// view first.
fn drawn_boxes(graph: &GraphPanel) -> Vec<DrawnBox> {
    let camera = graph.camera();
    let viewport = graph.framing.viewport();
    let visible = visible_rect(viewport, camera);
    let Some(laid_out) = graph.drawn() else {
        return Vec::new();
    };
    let Some(view) = laid_out.view(camera.zoom) else {
        return Vec::new();
    };
    let middle = viewport.center();
    let expandable = graph.expandable();
    let mut boxes: Vec<DrawnBox> = view
        .boxes
        .iter()
        .filter_map(|b| {
            let r = Rect::of(b);
            let x = r.x + r.w / 2.;
            let y = match b.compound {
                true => r.y + LABEL_BAND / 2.,
                false => r.y + r.h / 2.,
            };
            visible.covers(x as f32, y as f32).then(|| DrawnBox {
                aim: to_screen(x, y, viewport, camera),
                id: b.id,
                kind: b.kind,
                container: b.compound,
                opens: expandable.holds(b),
                label: b.label.clone(),
            })
        })
        .collect();
    let from_middle = |b: &DrawnBox| {
        let to = b.aim - middle;
        f32::from(to.x).hypot(f32::from(to.y))
    };
    boxes.sort_by(|a, b| from_middle(a).total_cmp(&from_middle(b)));
    boxes
}

/// Whether `graph` draws the box `id` open, holding what it contains; `None`
/// where it draws no such box.
fn drawn_open(graph: &GraphPanel, id: Id) -> Option<bool> {
    let view = graph.drawn()?;
    let view = view.view(graph.camera().zoom)?;
    let drawn = view.boxes.iter().find(|b| b.id == id)?;
    Some(drawn.compound)
}

/// The zoom and pan of the camera `graph` looks through.
fn framed(graph: &GraphPanel) -> (f32, (f32, f32)) {
    let camera = graph.camera();
    (camera.zoom, camera.pan)
}

/// The place in `graph`'s view nearest its middle, of those a sweep stops at,
/// where no box is drawn.
fn empty_place(harness: &mut Harness, graph: &Entity<GraphPanel>) -> Option<Point<Pixels>> {
    let viewport = harness.read(|cx| graph.read(cx).framing.viewport());
    let across = (viewport.size.width / SWEEP_STEP.width).ceil();
    let down = (viewport.size.height / SWEEP_STEP.height).ceil();
    let mut places = Vec::with_capacity((across * down) as usize);
    let mut y = viewport.top() + SWEEP_STEP.height / 2.;
    while y < viewport.bottom() {
        let mut x = viewport.left() + SWEEP_STEP.width / 2.;
        while x < viewport.right() {
            places.push(point(x, y));
            x += SWEEP_STEP.width;
        }
        y += SWEEP_STEP.height;
    }
    let middle = viewport.center();
    let from_middle = |at: &Point<Pixels>| {
        let to = *at - middle;
        f32::from(to.x).hypot(f32::from(to.y))
    };
    places.sort_by(|a, b| from_middle(a).total_cmp(&from_middle(b)));
    harness.update(|_, _, cx| {
        places.into_iter().find(|&at| {
            graph
                .update(cx, |graph, cx| graph.box_id_at(at, cx))
                .is_none()
        })
    })
}

/// Of `boxes`, the first of each kind, containers apart from the rest.
fn one_of_each_kind(boxes: Vec<DrawnBox>) -> Vec<DrawnBox> {
    let mut kinds = Vec::with_capacity(boxes.len());
    let mut each = Vec::with_capacity(boxes.len());
    for drawn in boxes {
        let kind = (drawn.kind, drawn.container);
        if !kinds.contains(&kind) {
            kinds.push(kind);
            each.push(drawn);
        }
    }
    each
}

/// The longest run of word characters in `label`, a name to search by free
/// of the punctuation of labels.
fn longest_word(label: &str) -> Option<&str> {
    label
        .split(|character: char| !character.is_alphanumeric())
        .max_by_key(|word| word.len())
        .filter(|word| !word.is_empty())
}

/// Whether `graph` draws a layout of its mode with no derivation running.
fn laid_out(graph: &Entity<GraphPanel>, cx: &App) -> bool {
    let graph = graph.read(cx);
    bviz_ui_testing::loaded("graph", graph.model.read(cx).source())
        && graph.derived.drawn(graph.mode).is_some()
        && !graph.derived.deriving()
}

#[test]
fn the_graph_draws_hovered_opened_marked_and_queried() {
    with_graph(async |harness| {
        let (graph, body) = harness.show::<GraphPanel>();
        let settled = |cx: &App| laid_out(&graph, cx);
        harness.sweep(body, SWEEP_STEP);
        let menus = harness.toolbar_menus(body);
        assert_ne!(menus, 0, "the graph's toolbar opens a menu");
        harness
            .until("the graph to lay out as picked", LOAD_DEADLINE, settled)
            .await;
        let boxes = |harness: &mut Harness| harness.read(|cx| drawn_boxes(graph.read(cx)));
        for drawn in one_of_each_kind(boxes(harness)) {
            harness.click(drawn.aim);
        }
        harness
            .until("the graph to lay out as clicked", LOAD_DEADLINE, settled)
            .await;
        harness.sweep(body, SWEEP_STEP);
        let mode = |harness: &mut Harness| harness.read(|cx| graph.read(cx).mode);
        let map = mode(harness);
        harness.dispatch(&ToggleMap);
        harness
            .until("the graph to lay out in layers", LOAD_DEADLINE, settled)
            .await;
        assert_eq!(
            mode(harness),
            LayoutMode::Layered,
            "the map toggles to layers"
        );
        let opened = boxes(harness)
            .into_iter()
            .find(|drawn| drawn.opens)
            .expect("the graph draws a box that opens or closes");
        let open = |harness: &mut Harness| harness.read(|cx| drawn_open(graph.read(cx), opened.id));
        for _ in 0..2 {
            let before = open(harness).expect("the graph draws the box it opens");
            harness.click(opened.aim);
            harness
                .until("the graph to lay out as opened", LOAD_DEADLINE, settled)
                .await;
            assert_eq!(
                open(harness),
                Some(!before),
                "a click on {} opens or closes it",
                opened.label
            );
        }
        let at_rest = |cx: &App| graph.read(cx).framing.at_rest();
        harness.dispatch(&ShowWholeGraph);
        harness
            .until("the view to frame the whole graph", LOAD_DEADLINE, at_rest)
            .await;
        let whole = harness.read(|cx| framed(graph.read(cx)));
        harness.dispatch(&ZoomIn);
        harness
            .until("the view to zoom in", LOAD_DEADLINE, at_rest)
            .await;
        assert_ne!(
            harness.read(|cx| framed(graph.read(cx))),
            whole,
            "zooming in moves the view off the whole graph"
        );
        let empty = empty_place(harness, &graph).expect("the zoomed view shows empty canvas");
        harness.click_with(MouseButton::Left, empty, 2);
        harness
            .until("the view to frame the whole graph", LOAD_DEADLINE, at_rest)
            .await;
        assert_eq!(
            harness.read(|cx| framed(graph.read(cx))),
            whole,
            "a double click on empty canvas frames the whole graph"
        );
        harness.dispatch(&ToggleMap);
        harness
            .until("the graph to lay out as a map", LOAD_DEADLINE, settled)
            .await;
        assert_eq!(mode(harness), map, "the layers toggle back to the map");
        let mut box_menus = 0;
        for drawn in one_of_each_kind(boxes(harness)) {
            harness.click_with(MouseButton::Right, drawn.aim, 1);
            if harness.read(|cx| graph.read(cx).box_menu.is_some()) {
                box_menus += 1;
                harness.sweep(body, SWEEP_STEP);
                harness.press("escape");
            }
        }
        assert_ne!(box_menus, 0, "a secondary click on a box opens its menu");
        let toggles: [&dyn Action; 7] = [
            &ToggleNeighborhood,
            &ToggleReduce,
            &CycleColouring,
            &ToggleCriticalPath,
            &ToggleOutcomes,
            &ToggleTiming,
            &ToggleMap,
        ];
        for toggle in toggles {
            harness.dispatch(toggle);
            harness
                .until("the graph to lay out as toggled", LOAD_DEADLINE, settled)
                .await;
            harness.sweep(body, SWEEP_STEP);
        }
        harness.dispatch(&ShowWholeGraph);
        harness.dispatch(&JumpBack);
        harness.dispatch(&JumpForward);
        let selected =
            |harness: &mut Harness| harness.read(|cx| graph.read(cx).model.read(cx).selected());
        while selected(harness).is_some() {
            harness.dispatch(&Dismiss);
        }
        let target = boxes(harness)
            .into_iter()
            .find(|drawn| drawn.kind == Kind::Target)
            .expect("the graph draws a target");
        let name = longest_word(&target.label).expect("a target's label names it");
        harness.dispatch(&jump::Open);
        assert!(
            harness.read(|cx| graph.read(cx).jump.read(cx).is_open()),
            "the jump palette opens"
        );
        harness.type_text(name);
        harness
            .until("the jump palette to list a match", LOAD_DEADLINE, |cx| {
                graph.read(cx).jump.read(cx).lists_a_match()
            })
            .await;
        harness.press_keys_for(&jump::Next);
        harness.press_keys_for(&jump::Go);
        assert!(selected(harness).is_some(), "the jump lands on a node");
        harness
            .until("the graph to lay out as jumped", LOAD_DEADLINE, settled)
            .await;
        harness.sweep(body, SWEEP_STEP);
        harness.press_keys_for(&chips::Focus);
        assert!(
            harness.focused_within(chips::FIELD_KEY_CONTEXT.id()),
            "the query field takes the keys"
        );
        harness.type_text(name);
        harness.press_keys_for(&chips::Commit);
        harness.press_keys_for(&chips::Apply);
        assert!(
            harness.read(|cx| !graph.read(cx).model.read(cx).query().is_empty()),
            "the query applies"
        );
        harness
            .until("the graph to lay out as queried", LOAD_DEADLINE, settled)
            .await;
        harness.sweep(body, SWEEP_STEP);
        harness.press_keys_for(&chips::Leave);
        harness.toolbar_hints(body).await;
    });
}
