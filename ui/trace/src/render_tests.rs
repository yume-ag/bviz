//! Every panel of a profile's document drawn in the states a user brings it
//! to, on gpui's headless platform.

use crate::flame_panel::{CycleDirection, CycleWeight, ToggleKinds};
use crate::model::{TraceModel, TracedPath};
use crate::tracks_menu::DrawnRow;
use crate::{
    ChooseNext, ChoosePrevious, ClearTimeSelection, CloseTracksMenu, CriticalPathPanel,
    FilterTracks, FlamePanel, STATS_NUM_COL_WIDTH, STATS_ROW_HEIGHT, SelectAllTracks,
    ShowWholeFlameGraph, ShowWholeTimeline, TRACKS_MENU_KEY_CONTEXT, TracePanel, Unchoose,
};
use bviz_ui_components::text_input::{Backspace, SelectAll};
use bviz_ui_components::{CloseLegend, ShowLegend, ZoomIn, ZoomOut};
use bviz_ui_testing::{Harness, LOAD_DEADLINE, SWEEP_STEP, fixture};
use bviz_ui_workspace::{DIVIDER_GRIP, SplitRight};
use gpui::{Action, App, Bounds, Entity, MouseButton, Pixels, point, px};
use std::collections::BTreeSet;

/// Runs `test` on a window framing the fixture profile's document, once the
/// profile and what is derived from it have landed.
pub(crate) fn with_profile(test: impl AsyncFnOnce(&mut Harness, Entity<TraceModel>) + 'static) {
    bviz_ui_testing::run_alone(
        &[crate::register_keys],
        |session, clock, window, cx| {
            let path = fixture("PROFILE");
            let load = crate::Load::from(bviz_ui_trace_args::TraceSource { path });
            crate::open(load, session, clock, window, cx)
        },
        async move |harness| {
            let timeline = harness.panel::<TracePanel>();
            let model = harness.read(|cx| timeline.read(cx).model.clone());
            harness
                .until("the profile to load", LOAD_DEADLINE, |cx| {
                    loaded(&model, cx)
                })
                .await;
            test(harness, model).await;
        },
    );
}

/// Whether the profile and everything derived from it has landed.
fn loaded(model: &Entity<TraceModel>, cx: &App) -> bool {
    let model = model.read(cx);
    bviz_ui_testing::loaded("profile", model.source())
        && model.working_lanes().is_some()
        && matches!(model.critical_path(), Some(TracedPath::Traced { .. }))
}

/// Whether the flame graph and stats draw an aggregate, none being rebuilt.
fn aggregated(model: &Entity<TraceModel>, cx: &App) -> bool {
    let model = model.read(cx);
    model.aggregates().is_some() && !model.aggregate_building()
}

/// Splits the shown panel `P` off into a pane of its own to the right of its
/// pane, then halves that pane's width with the divider before it until a
/// click along the panel's controls opens a menu, as they fold into menus;
/// returns the room the panel draws in there.
fn folded<P: bviz_ui_workspace::Panel>(harness: &mut Harness) -> Bounds<Pixels> {
    harness.press_keys_for(&SplitRight);
    let mut room = harness.show::<P>().1;
    while harness.toolbar_menus(room) == 0 {
        let divider = point(room.left() - DIVIDER_GRIP / 2., room.center().y);
        harness.drag(divider, point(room.center().x, divider.y));
        let narrowed = harness.show::<P>().1;
        assert!(
            narrowed.size.width < room.size.width,
            "dragging the divider narrows {room:?}, not to {narrowed:?}"
        );
        room = narrowed;
    }
    room
}

/// Clicks a row apart down the table `body` draws, through the key column it
/// leads with and the column it ends with, so that its header and each of its
/// rows are clicked in both. Every table here ends with a column no narrower
/// than a numeric one, and its key column takes the room the others leave.
pub(crate) fn click_down(harness: &mut Harness, body: Bounds<Pixels>) {
    let inset = px(STATS_NUM_COL_WIDTH) / 2.;
    for x in [body.left() + inset, body.right() - inset] {
        let (top, bottom) = (point(x, body.top()), point(x, body.bottom()));
        harness.click_along(top, bottom, px(STATS_ROW_HEIGHT));
    }
}

/// Where the timeline draws its lanes of slices, and the x halfway across
/// the label gutter to their left.
fn lanes(harness: &mut Harness) -> (Bounds<Pixels>, Pixels) {
    let panel = harness.panel::<TracePanel>();
    harness.read(|cx| {
        let timeline = panel.read(cx).timeline.read(cx);
        let lanes = timeline.regions(cx).flame_rect();
        (lanes, (timeline.viewport().left() + lanes.left()) / 2.)
    })
}

/// Where the timeline draws each slice whose middle it shows.
fn drawn_slices(harness: &mut Harness) -> Vec<Bounds<Pixels>> {
    let panel = harness.panel::<TracePanel>();
    harness.read(|cx| panel.read(cx).timeline.read(cx).drawn_slices(cx))
}

/// The widest of `boxes`.
fn widest(boxes: impl IntoIterator<Item = Bounds<Pixels>>) -> Option<Bounds<Pixels>> {
    let width = |b: &Bounds<Pixels>| f32::from(b.size.width);
    boxes
        .into_iter()
        .max_by(|a, b| width(a).total_cmp(&width(b)))
}

/// The widest of the slices `drawn` on each row, top row first.
fn widest_of_each_row(drawn: &[Bounds<Pixels>]) -> Vec<Bounds<Pixels>> {
    let mut rows: Vec<Bounds<Pixels>> = Vec::with_capacity(drawn.len());
    for &slice in drawn {
        match rows.iter_mut().find(|row| row.top() == slice.top()) {
            Some(row) if row.size.width < slice.size.width => *row = slice,
            Some(_) => {}
            None => rows.push(slice),
        }
    }
    rows.sort_by(|a, b| f32::from(a.top()).total_cmp(&f32::from(b.top())));
    rows
}

#[test]
fn the_timeline_draws_hovered_selected_and_zoomed() {
    with_profile(async |harness, model| {
        let (_, body) = harness.show::<TracePanel>();
        let (lanes, gutter) = lanes(harness);
        harness.sweep(body, SWEEP_STEP);
        harness.dispatch(&ZoomIn);
        let zoomed = drawn_slices(harness);
        let from = widest(zoomed.iter().copied())
            .expect("the timeline draws slices")
            .center();
        harness.drag(from, point(from.x - lanes.size.width / 2., from.y));
        assert_ne!(
            drawn_slices(harness),
            zoomed,
            "a drag along the lanes pans them"
        );
        harness.sweep(body, SWEEP_STEP);
        let rows = widest_of_each_row(&drawn_slices(harness));
        let top = rows.first().expect("the timeline draws slices");
        harness.click(point(gutter, top.center().y));
        for slice in &rows {
            harness.click(slice.center());
        }
        harness.click_with(MouseButton::Left, top.center(), 2);
        harness.dispatch(&SelectAllTracks);
        harness.dispatch(&ZoomIn);
        harness.dispatch(&ZoomIn);
        harness.sweep(body, SWEEP_STEP);
        harness.dispatch(&ZoomOut);
        harness.dispatch(&ShowWholeTimeline);
        harness.dispatch(&ClearTimeSelection);
        harness.scroll_wheel(lanes.center(), point(0., -1.));
        let (_, tracks) = harness.read(|cx| model.read(cx).visible_count());
        assert_ne!(tracks, 0, "the profile has tracks to filter");
        harness.dispatch(&FilterTracks);
        assert!(
            harness.focused_within(TRACKS_MENU_KEY_CONTEXT.id()),
            "the tracks menu opens to filter"
        );
        harness.sweep(body, SWEEP_STEP);
        let all = listed(&menu_rows(harness));
        let word = rarest_word(harness, &model);
        harness.type_text(&word);
        let filtered = menu_rows(harness);
        assert!(
            (1..all).contains(&listed(&filtered)),
            "a word of a track's name lists some of the {all} tracks, not {}",
            listed(&filtered)
        );
        let (track, row) = filtered
            .iter()
            .find_map(|&(drawn, at)| match drawn {
                DrawnRow::Track(track) => Some((track, at)),
                _ => None,
            })
            .expect("the filtered menu lists a track unfolded");
        let shown =
            |harness: &mut Harness| harness.read(|cx| model.read(cx).visible_tracks()[track]);
        for _ in 0..2 {
            let before = shown(harness);
            harness.click(row.center());
            assert_ne!(
                shown(harness),
                before,
                "a click on a track's row toggles it"
            );
        }
        harness.press_keys_for(&SelectAll);
        harness.press_keys_for(&Backspace);
        assert_eq!(
            listed(&menu_rows(harness)),
            all,
            "an empty filter lists every track"
        );
        let control = menu_rows(harness)
            .into_iter()
            .find_map(|(drawn, at)| matches!(drawn, DrawnRow::GroupVisibility { .. }).then_some(at))
            .expect("the menu draws a group's visibility control");
        let count = |harness: &mut Harness| harness.read(|cx| model.read(cx).visible_count().0);
        let before = count(harness);
        harness.click(control.center());
        assert!(count(harness) < before, "hiding a group hides its tracks");
        harness.click(control.center());
        assert_eq!(
            count(harness),
            before,
            "showing a group shows its tracks again"
        );
        let (group, header) = menu_rows(harness)
            .into_iter()
            .find_map(|(drawn, at)| match drawn {
                DrawnRow::Header { group, .. } => Some((group, at)),
                _ => None,
            })
            .expect("the menu draws a group's header");
        let folded = |harness: &mut Harness| {
            menu_rows(harness)
                .into_iter()
                .find_map(|(drawn, _)| match drawn {
                    DrawnRow::Header {
                        group: at, folded, ..
                    } if at == group => Some(folded),
                    _ => None,
                })
        };
        for _ in 0..2 {
            let before = folded(harness).expect("the menu draws the group's header");
            harness.click(header.center());
            assert_eq!(
                folded(harness),
                Some(!before),
                "a click on a group's header folds or unfolds it"
            );
        }
        harness.press_keys_for(&CloseTracksMenu);
        assert!(
            !harness.focused_within(TRACKS_MENU_KEY_CONTEXT.id()),
            "the tracks menu closes on its key"
        );
        assert!(
            menu_rows(harness).is_empty(),
            "the closed tracks menu draws no rows"
        );
    });
}

/// Where the open tracks menu draws each row of its list, and what it is.
fn menu_rows(harness: &mut Harness) -> Vec<(DrawnRow, Bounds<Pixels>)> {
    let panel = harness.panel::<TracePanel>();
    harness.read(|cx| panel.read(cx).tracks_menu.read(cx).drawn_rows(cx))
}

/// How many tracks the menu lists under the group headers among `rows`,
/// folded or not.
fn listed(rows: &[(DrawnRow, Bounds<Pixels>)]) -> usize {
    rows.iter()
        .map(|(drawn, _)| match drawn {
            DrawnRow::Header { listed, .. } => *listed,
            _ => 0,
        })
        .sum()
}

/// The word of the profile's track names that the fewest of them hold, as a
/// filter narrowing the tracks the most.
fn rarest_word(harness: &Harness, model: &Entity<TraceModel>) -> String {
    harness.read(|cx| {
        let profile = model
            .read(cx)
            .profile()
            .expect("the profile loaded")
            .clone();
        let names: Vec<String> = profile
            .threads()
            .iter()
            .map(|thread| profile.str(thread.name).to_lowercase())
            .collect();
        let holding = |word: &str| names.iter().filter(|name| name.contains(word)).count();
        names
            .iter()
            .flat_map(|name| name.split(|c: char| !c.is_alphabetic()))
            .filter(|word| !word.is_empty())
            .min_by_key(|word| (holding(word), *word))
            .expect("the profile names its tracks")
            .to_string()
    })
}

#[test]
fn the_timeline_raises_its_tooltips() {
    with_profile(async |harness, _| {
        let (_, body) = harness.show::<TracePanel>();
        harness.toolbar_hints(body).await;
    });
}

#[test]
fn the_flame_graph_draws_hovered_picked_and_restyled() {
    with_profile(async |harness, model| {
        let (flame, body) = harness.show::<FlamePanel>();
        let built = |cx: &App| aggregated(&model, cx);
        harness
            .until("the flame graph to aggregate", LOAD_DEADLINE, built)
            .await;
        harness.sweep(body, SWEEP_STEP);
        let drawn = |harness: &mut Harness| harness.read(|cx| flame.read(cx).drawn_boxes(cx));
        let depths: BTreeSet<u32> = drawn(harness).into_iter().map(|(depth, _)| depth).collect();
        assert!(!depths.is_empty(), "the flame graph draws boxes");
        for depth in depths {
            let at_depth = drawn(harness).into_iter().filter(|&(at, _)| at == depth);
            let widest = widest(at_depth.map(|(_, drawn)| drawn))
                .unwrap_or_else(|| panic!("the flame graph draws row {depth}"));
            harness.click(widest.center());
        }
        harness.sweep(body, SWEEP_STEP);
        harness.dispatch(&ZoomIn);
        harness.dispatch(&ShowWholeFlameGraph);
        let restyles: [&dyn Action; 3] = [&CycleDirection, &CycleWeight, &ToggleKinds];
        for restyle in restyles {
            let before = harness.read(|cx| model.read(cx).flame_mode());
            harness.dispatch(restyle);
            assert!(
                harness.read(|cx| model.read(cx).flame_mode()) != before,
                "{} changes the flame graph's mode",
                restyle.name()
            );
            harness
                .until(
                    "the flame graph to aggregate as restyled",
                    LOAD_DEADLINE,
                    built,
                )
                .await;
            harness.sweep(body, SWEEP_STEP);
        }
        let narrow = folded::<FlamePanel>(harness);
        harness
            .until(
                "the flame graph to aggregate as clicked",
                LOAD_DEADLINE,
                built,
            )
            .await;
        harness.dispatch(&ShowLegend);
        harness.sweep(narrow, SWEEP_STEP);
        harness.dispatch(&CloseLegend);
        harness.toolbar_hints(narrow).await;
    });
}

#[test]
fn the_critical_path_draws_chosen_sorted_and_hovered() {
    with_profile(async |harness, model| {
        let (_, body) = harness.show::<CriticalPathPanel>();
        harness.sweep(body, SWEEP_STEP);
        harness.dispatch(&ChooseNext);
        assert!(
            harness.read(|cx| model.read(cx).chosen(cx).is_some()),
            "choosing the next step chooses one"
        );
        harness.sweep(body, SWEEP_STEP);
        harness.dispatch(&ChooseNext);
        harness.dispatch(&ChoosePrevious);
        click_down(harness, body);
        harness.sweep(body, SWEEP_STEP);
        harness.dispatch(&Unchoose);
    });
}
