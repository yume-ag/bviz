//! The tables and toolbar controls the spawn time and spawn bytes panels are
//! built from.

use crate::format::duration;
use crate::palette::Palette;
use crate::panel::CycleList;
use crate::phase_bar::phase_bar;
use crate::summary::{GroupRow, SpawnName};
use bviz_execlog::ByteMeasure;
use bviz_label::names::RepoNames;
use bviz_label::shown::Parts;
use bviz_ui_components::amount::{self, bytes};
use bviz_ui_components::gesture::{Gestures, Pointer};
use bviz_ui_components::{
    ByRoom, Dropdown, HintedLabelLine, LabelLine, ListScroll, MenuItem, OverflowRows, Presentation,
    Priority, SCROLLBAR_THICKNESS, Sort, TextTooltip, ToggleGroup, ToggleSegment, ToolbarGroup,
    TruncatingLabel, list_scrollbar, sort_gesture, sort_header, sort_hint,
};
use gpui::{
    AnyElement, App, Context, Div, ElementId, Entity, FocusHandle, IntoElement, MouseButton,
    MouseDownEvent, Pixels, SharedString, Stateful, Window, div, prelude::*, px,
};
use std::cmp::Ordering;
use std::time::Duration;

const ROW_HEIGHT: Pixels = px(24.);
const TWO_LINE_ROW_HEIGHT: Pixels = px(44.);
const CELL_PAD: Pixels = px(6.);
const CARET: Pixels = px(12.);
const NAME_WIDTH: Pixels = px(220.);
/// The width of a spawn's mnemonic and runner in a list of spawns: narrower
/// than a table of sums gives its names, so its target and output keep room.
const SPAWN_NAME_WIDTH: Pixels = px(170.);
const NUMBER_WIDTH: Pixels = px(110.);
const BYTES_WIDTH: Pixels = px(90.);
const NOTE_WIDTH: Pixels = px(110.);

/// What a panel does for a gesture in [`GESTURES`].
#[derive(Clone, Copy)]
pub(crate) enum TableIntent {
    Sort,
}

pub(crate) const GESTURES: Gestures<TableIntent> =
    Gestures::new(&[sort_gesture(TableIntent::Sort)]);

/// What a table of groups is ordered by.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum SortBy {
    Name,
    Spawns,
    Bytes(ByteMeasure),
    Time,
}

/// The sort a click on `by`'s title asks for of a table sorted by `sort`:
/// names first sort from the start of the alphabet and counts from the
/// largest.
fn clicked(sort: Sort<SortBy>, by: SortBy) -> Sort<SortBy> {
    sort.clicked(by, by != SortBy::Name)
}

/// Rows equal in what `sort` is by go in order of name.
fn compare(sort: Sort<SortBy>, a: &GroupRow, b: &GroupRow) -> Ordering {
    let by = match sort.by {
        SortBy::Name => a.name.cmp(&b.name),
        SortBy::Spawns => a.spawns.cmp(&b.spawns),
        SortBy::Bytes(measure) => a.bytes.measure(measure).cmp(&b.bytes.measure(measure)),
        SortBy::Time => a.timing.total().cmp(&b.timing.total()),
    };
    let by = if sort.descending { by.reverse() } else { by };
    by.then_with(|| a.name.cmp(&b.name))
}

pub(crate) fn apply(sort: Sort<SortBy>, rows: &mut [GroupRow]) {
    rows.sort_by(|a, b| compare(sort, a, b));
}

/// Turns `sort` to what a click on `by`'s title asks for, and orders each of
/// `tables` by it.
pub(crate) fn sort_tables<'a>(
    sort: &mut Sort<SortBy>,
    by: SortBy,
    tables: impl IntoIterator<Item = &'a mut Vec<GroupRow>>,
) {
    *sort = clicked(*sort, by);
    for rows in tables {
        apply(*sort, rows);
    }
}

/// How wide a column of a table is.
#[derive(Clone, Copy)]
enum Width {
    Fixed(Pixels),
    /// Takes what the fixed columns leave.
    Rest,
}

#[derive(Clone, Copy)]
pub(crate) struct Column {
    width: Width,
    /// Right-aligned, so the digits of a column line up.
    numeric: bool,
}

const fn text(width: Width) -> Column {
    Column {
        width,
        numeric: false,
    }
}

const fn number(width: Width) -> Column {
    Column {
        width,
        numeric: true,
    }
}

pub(crate) const TIME_COLUMN: Column = number(Width::Fixed(NUMBER_WIDTH));
pub(crate) const BYTES_COLUMN: Column = number(Width::Fixed(BYTES_WIDTH));

/// One cell, header or body, laid out by its column so the header and every
/// row agree on the column's width at any window width.
pub(crate) fn cell(column: Column, content: impl IntoElement) -> Div {
    let cell = div()
        .flex()
        .items_center()
        .h_full()
        .px(CELL_PAD)
        .overflow_hidden()
        .whitespace_nowrap()
        .when(column.numeric, |cell| cell.justify_end())
        .child(content);
    match column.width {
        Width::Fixed(width) => cell.w(width).flex_shrink_0(),
        Width::Rest => cell.flex_1().min_w_0(),
    }
}

/// A row of a table.
pub(crate) fn row(id: impl Into<ElementId>, cells: impl IntoIterator<Item = Div>) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_row()
        .w_full()
        .h(ROW_HEIGHT)
        .children(cells)
}

pub(crate) fn header_row(
    cells: impl IntoIterator<Item = impl IntoElement>,
    palette: &Palette,
) -> Div {
    div()
        .flex()
        .flex_row()
        .flex_shrink_0()
        .w_full()
        .h(ROW_HEIGHT)
        .text_color(palette.colors.text_muted)
        .children(cells)
}

/// A column of a table of summed spawns.
#[derive(Clone, Copy)]
pub(crate) enum GroupColumn {
    Name,
    Spawns,
    Bytes(ByteMeasure),
    Time,
    /// The bar of the phases of the spawn time.
    Bar,
    /// A note on what the bar can show.
    Note,
}

impl GroupColumn {
    fn layout(self) -> Column {
        match self {
            GroupColumn::Name => text(Width::Fixed(NAME_WIDTH)),
            GroupColumn::Spawns | GroupColumn::Time => TIME_COLUMN,
            GroupColumn::Bytes(_) => BYTES_COLUMN,
            GroupColumn::Bar => text(Width::Rest),
            GroupColumn::Note => text(Width::Fixed(NOTE_WIDTH)),
        }
    }

    /// The column's title, the name's being `name_title`.
    fn title(self, name_title: &'static str) -> &'static str {
        match self {
            GroupColumn::Name => name_title,
            GroupColumn::Spawns => "Spawns",
            GroupColumn::Bytes(measure) => measure_title(measure),
            GroupColumn::Time => "Spawn time",
            GroupColumn::Bar => "By phase",
            GroupColumn::Note => "",
        }
    }

    /// What a click on the column's title sorts a table of mnemonics by, and
    /// what the hint names it.
    fn sorts(self) -> Option<(SortBy, &'static str)> {
        match self {
            GroupColumn::Name => Some((SortBy::Name, "mnemonic")),
            GroupColumn::Spawns => Some((SortBy::Spawns, "spawn count")),
            GroupColumn::Bytes(measure) => {
                Some((SortBy::Bytes(measure), amount::measure_name(measure)))
            }
            GroupColumn::Time => Some((SortBy::Time, "spawn time")),
            GroupColumn::Bar | GroupColumn::Note => None,
        }
    }
}

pub(crate) fn measure_title(measure: ByteMeasure) -> &'static str {
    match measure {
        ByteMeasure::Outputs => "Outputs",
        ByteMeasure::Inputs => "Inputs",
        ByteMeasure::Tools => "Tools",
    }
}

/// The header of a table of `columns` no title of which sorts it.
pub(crate) fn plain_header(
    columns: &[GroupColumn],
    name_title: &'static str,
    palette: &Palette,
) -> Div {
    header_row(
        (columns.iter()).map(|column| cell(column.layout(), column.title(name_title))),
        palette,
    )
}

/// A header cell of `column` titled `title`, whose click `sort` applies to
/// the view; `sorted` is as [`Sort::of`] gives it.
pub(crate) fn sorting_cell<V: 'static>(
    column: Column,
    title: &'static str,
    hint: SharedString,
    sorted: Option<bool>,
    palette: &Palette,
    cx: &mut Context<V>,
    sort: impl Fn(&mut V, &mut Context<V>) + 'static,
) -> AnyElement {
    let content = sort_header(
        title,
        sorted,
        column.numeric,
        CARET,
        palette.colors.text_muted,
    );
    cell(column, content)
        .id(title)
        .cursor_pointer()
        .hover(|style| style.bg(palette.colors.element_hover))
        .tooltip(TextTooltip::builder(hint, palette.colors.tooltip()))
        .on_click(cx.listener(move |view, e, _, cx| {
            GESTURES.dispatch(Pointer::click(e), |intent| match intent {
                TableIntent::Sort => {
                    sort(view, cx);
                    true
                }
            });
        }))
        .into_any_element()
}

/// The header of a table of mnemonics laid out in `columns`, whose titles but
/// the bar's and the note's ask `on_sort` to sort it.
pub(crate) fn sorting_header<V: 'static>(
    columns: &[GroupColumn],
    sort: Sort<SortBy>,
    palette: &Palette,
    cx: &mut Context<V>,
    on_sort: fn(&mut V, SortBy, &mut Context<V>),
) -> Div {
    let cells = columns.iter().map(|&column| {
        let (layout, title) = (column.layout(), column.title("Mnemonic"));
        match column.sorts() {
            Some((by, what)) => sorting_cell(
                layout,
                title,
                sort_hint(what),
                sort.of(by),
                palette,
                cx,
                move |view, cx| on_sort(view, by, cx),
            ),
            None => cell(layout, title).into_any_element(),
        }
    });
    let cells: Vec<AnyElement> = cells.collect();
    header_row(cells, palette)
}

/// The spawn time of the longest of `rows`, which every bar is drawn against.
pub(crate) fn longest(rows: &[GroupRow]) -> Duration {
    (rows.iter())
        .map(|row| row.timing.total())
        .max()
        .unwrap_or_default()
}

/// `group` as a row of `columns`, its bar as long against `longest` as its
/// spawn time is.
pub(crate) fn group_row(
    id: impl Into<ElementId>,
    columns: &[GroupColumn],
    group: &GroupRow,
    longest: Duration,
    palette: &Palette,
) -> Stateful<Div> {
    let execution_only = group.timing.reports_execution_only();
    let cells = columns.iter().map(|&column| {
        let content = match column {
            GroupColumn::Name => {
                TruncatingLabel::new("name", group.name.clone()).into_any_element()
            }
            GroupColumn::Spawns => group.spawns.to_string().into_any_element(),
            GroupColumn::Bytes(measure) => bytes(group.bytes.measure(measure)).into_any_element(),
            GroupColumn::Time => duration(group.timing.total()).into_any_element(),
            GroupColumn::Bar => phase_bar(&group.timing, longest, palette).into_any_element(),
            GroupColumn::Note => div()
                .text_color(palette.colors.text_muted)
                .when(execution_only, |note| note.child("execution only"))
                .into_any_element(),
        };
        cell(column.layout(), content)
    });
    row(id, cells)
}

/// The runners' spawns in sum, one row each laid out in `columns`; few enough
/// to lay out whole.
pub(crate) fn runner_table(
    columns: &[GroupColumn],
    runners: &[GroupRow],
    palette: &Palette,
) -> Div {
    let longest = longest(runners);
    div()
        .flex()
        .flex_col()
        .flex_shrink_0()
        .pr(px(SCROLLBAR_THICKNESS))
        .child(plain_header(columns, "Runner", palette))
        .children(
            (runners.iter().enumerate()).map(|(index, runner)| {
                group_row(("runner", index), columns, runner, longest, palette)
            }),
        )
}

/// The columns naming a spawn in a list of spawns: what the spawn made, its
/// mnemonic, and its runner.
const NAME_COLUMNS: [Column; 3] = [
    text(Width::Rest),
    text(Width::Fixed(SPAWN_NAME_WIDTH)),
    text(Width::Fixed(SPAWN_NAME_WIDTH)),
];

const NAME_TITLES: [&str; NAME_COLUMNS.len()] = ["Target and output", "Mnemonic", "Runner"];

/// The least width a list of spawns leaves the target and output of each
/// before it gives up its runner column for them.
const MADE_MIN_WIDTH: Pixels = px(320.);

/// Whether a list of spawns has the room for its last column, the runner's.
#[derive(Clone, Copy)]
pub(crate) enum RunnerColumn {
    Shown,
    Left,
}

impl RunnerColumn {
    /// How many of [`NAME_COLUMNS`] a list shows.
    fn names(self) -> usize {
        match self {
            RunnerColumn::Shown => NAME_COLUMNS.len(),
            RunnerColumn::Left => NAME_COLUMNS.len() - 1,
        }
    }
}

/// The list of spawns `list` builds, with the runner column where the room
/// it is given fits that column beside `figures`, the columns of the
/// figures each spawn is listed with, and without it elsewhere.
pub(crate) fn by_room(
    figures: &[Column],
    mut list: impl FnMut(RunnerColumn) -> AnyElement,
) -> ByRoom {
    let fixed = |column: &Column| match column.width {
        Width::Fixed(width) => width,
        Width::Rest => MADE_MIN_WIDTH,
    };
    let roomy = (figures.iter().chain(&NAME_COLUMNS))
        .map(fixed)
        .fold(px(SCROLLBAR_THICKNESS), |sum, width| sum + width);
    let mut room = |runner| div().size_full().flex().flex_col().child(list(runner));
    ByRoom::width(
        roomy,
        room(RunnerColumn::Shown).into_any_element(),
        room(RunnerColumn::Left).into_any_element(),
    )
}

pub(crate) fn name_header_cells(runner: RunnerColumn) -> impl Iterator<Item = AnyElement> {
    (NAME_COLUMNS.into_iter().zip(NAME_TITLES))
        .take(runner.names())
        .map(|(column, title)| cell(column, title).into_any_element())
}

/// What names a spawn that names neither its target nor its output.
const NO_TARGET_OR_OUTPUT: &str = "no target or output";

/// The cells of [`NAME_COLUMNS`] naming `spawn`, as many as `runner` leaves.
pub(crate) fn name_cells(
    spawn: &SpawnName,
    repos: &RepoNames,
    palette: &Palette,
    runner: RunnerColumn,
) -> impl Iterator<Item = Div> {
    let [made, mnemonic, runner_column] = NAME_COLUMNS;
    let muted = palette.colors.text_muted;
    let label = |label: &str| {
        let written = repos.written_label(label);
        LabelLine::new(repos.show(&written).into_text(), muted)
            .hinted("target", written.into_owned())
    };
    let path = |id: &'static str, path: &SharedString| -> HintedLabelLine {
        LabelLine::parted(path.clone(), Parts::of_path(path), muted).hinted(id, path.clone())
    };
    let (first, second) = match (&spawn.target_label, &spawn.primary_output) {
        (Some(target), output) => (label(target), output.as_ref().map(|o| path("output", o))),
        (None, Some(output)) => (path("output", output), None),
        (None, None) => {
            let none = LabelLine::new(NO_TARGET_OR_OUTPUT, muted);
            (none.hinted("none", NO_TARGET_OR_OUTPUT), None)
        }
    };
    [
        cell(
            made,
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .child(first)
                .children(second.map(|output| div().text_xs().text_color(muted).child(output))),
        ),
        cell(
            mnemonic,
            TruncatingLabel::new("mnemonic", spawn.mnemonic.clone()),
        ),
        cell(
            runner_column,
            TruncatingLabel::new("runner", spawn.runner.clone()),
        ),
    ]
    .into_iter()
    .take(runner.names())
}

/// A row of a list of spawns.
pub(crate) fn spawn_row(
    id: impl Into<ElementId>,
    cells: impl IntoIterator<Item = Div>,
) -> Stateful<Div> {
    row(id, cells).h(TWO_LINE_ROW_HEIGHT)
}

/// A table taking the room left to it: `header`, then `rows` scrolling
/// beneath it through the [`ListScroll`] `scroll` reads and `scroll_mut`
/// moves in the view `entity`.
pub(crate) fn scrolling_table<V: 'static>(
    header: Div,
    rows: impl IntoElement,
    entity: Entity<V>,
    scroll: fn(&V) -> &ListScroll,
    scroll_mut: fn(&mut V) -> &mut ListScroll,
    palette: &Palette,
    cx: &Context<V>,
) -> Div {
    let pane = div()
        .relative()
        .flex()
        .flex_col()
        .flex_1()
        .min_h(ROW_HEIGHT)
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |view, e: &MouseDownEvent, _, cx| {
                if scroll_mut(view).press(e.position) {
                    cx.stop_propagation();
                }
                cx.notify();
            }),
        )
        .child(rows)
        .child(list_scrollbar(entity, scroll, scroll_mut, palette.scroll));
    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .child(header.pr(px(SCROLLBAR_THICKNESS)))
        .child(pane)
}

/// The rows of a list of `count` rows, of which `rows` builds only those in
/// view, kept clear of the scroll bar.
pub(crate) fn list_rows<R: IntoElement + 'static>(
    id: &'static str,
    count: usize,
    scroll: &ListScroll,
    rows: impl Fn(std::ops::Range<usize>, &mut Window, &mut App) -> Vec<R> + 'static,
) -> impl IntoElement {
    gpui::uniform_list(id, count, rows)
        .flex_1()
        .w_full()
        .pr(px(SCROLLBAR_THICKNESS))
        .track_scroll(scroll.uniform_handle())
}

/// Which list a panel shows under its runners, or in their place.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum List {
    /// The spawns of each mnemonic in sum, under the runners'.
    ByMnemonic,
    /// The spawns largest in a measure, one by one.
    Spawns,
}

const LISTS: [List; 2] = [List::ByMnemonic, List::Spawns];

impl List {
    /// The list after this one, round to the first.
    pub(crate) fn next(self) -> List {
        match self {
            List::ByMnemonic => List::Spawns,
            List::Spawns => List::ByMnemonic,
        }
    }
}

/// How long each group of a panel's toolbar holds out against the panel
/// narrowing, the choice of list longest.
const LIST: Priority = 3;
const PICKER: Priority = 2;

/// The word naming the choice of list, beside it, on its collapsed trigger
/// and on its row of the overflow menu.
const LIST_NAME: &str = "list";

/// What a panel's toggle for [`List::Spawns`] reads, and its tooltip.
pub(crate) struct SpawnsToggle {
    pub(crate) label: &'static str,
    pub(crate) tip: &'static str,
}

/// The toolbar's choice of list, standing at `standing`, a click asking
/// `choose` for the list picked, naming the keys that cycle it where `focus`
/// stands.
pub(crate) fn list_group(
    standing: List,
    spawns: SpawnsToggle,
    choose: impl Fn(List, &mut App) + Clone + 'static,
    focus: &FocusHandle,
    window: &Window,
) -> ToolbarGroup {
    let segment = move |list: List| {
        match list {
            List::ByMnemonic => ToggleSegment::new("by mnemonic").tooltip("Spawns by mnemonic"),
            List::Spawns => ToggleSegment::new(spawns.label).tooltip(spawns.tip),
        }
        .keys_for(&CycleList, focus)
    };
    let toggles = || {
        let choose = choose.clone();
        ToggleGroup::one_of(
            "spawn-list",
            LIST_NAME,
            LISTS,
            standing,
            segment,
            move |list, _, cx| choose(list, cx),
        )
        .tooltip("List shown")
    };
    let row = MenuItem::submenu(LIST_NAME, toggles().submenu(window));
    let set = standing != List::ByMnemonic;
    ToolbarGroup::named(LIST, LIST_NAME, OverflowRows::row(row, set), toggles)
        .or(toggles().presentation(Presentation::Collapsed))
}

/// `picker` as a group of the toolbar beside the word `name`, `set` where it
/// stands other than at rest.
pub(crate) fn picker_group(
    name: &'static str,
    picker: &Entity<Dropdown>,
    set: bool,
    cx: &App,
) -> ToolbarGroup {
    let row = MenuItem::submenu(name, Dropdown::submenu(picker, None, cx));
    ToolbarGroup::named(PICKER, name, OverflowRows::row(row, set), || picker.clone())
}

/// What a panel's log holds in sum, `text`, explaining on hover how to read
/// it by `caveats`. Laid over its holder, so the totals ask the toolbar for
/// no room and take what its groups leave, cutting themselves short.
pub(crate) fn totals(
    text: SharedString,
    caveats: &'static str,
    palette: &Palette,
) -> impl FnMut() -> Div {
    let tooltip = palette.colors.tooltip();
    move || {
        let totals = div()
            .id("log-totals")
            .min_w_0()
            .overflow_hidden()
            .whitespace_nowrap()
            .text_ellipsis()
            .child(text.clone())
            .tooltip(TextTooltip::builder(caveats.into(), tooltip));
        div().flex_1().h_full().relative().child(
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .child(totals),
        )
    }
}

/// A note in place of rows a table has none of.
pub(crate) fn none_note(text: String, palette: &Palette) -> Div {
    div()
        .px(CELL_PAD)
        .py_2()
        .text_color(palette.colors.text_muted)
        .child(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bviz_execlog::{Bytes, PhaseTiming, SpawnBytes};

    #[test]
    fn every_gesture_is_reachable() {
        assert_eq!(GESTURES.unreachable(), []);
    }

    #[test]
    fn every_gesture_fits_the_keys_help() {
        assert_eq!(GESTURES.overlong(), []);
    }

    const BY_TIME: Sort<SortBy> = Sort {
        by: SortBy::Time,
        descending: true,
    };

    fn rows(named: &[(&str, usize)]) -> Vec<GroupRow> {
        named
            .iter()
            .map(|&(name, spawns)| GroupRow {
                name: name.to_owned().into(),
                spawns,
                timing: PhaseTiming::default(),
                bytes: SpawnBytes::default(),
            })
            .collect()
    }

    fn names(rows: &[GroupRow]) -> Vec<&str> {
        rows.iter().map(|row| row.name.as_ref()).collect()
    }

    #[test]
    fn a_click_on_a_new_title_sorts_names_up_and_counts_down() {
        let by_name = clicked(BY_TIME, SortBy::Name);
        assert!(!by_name.descending);
        assert!(clicked(by_name, SortBy::Spawns).descending);
    }

    #[test]
    fn a_click_on_the_sorting_title_turns_the_order_round() {
        let by_spawns = clicked(BY_TIME, SortBy::Spawns);
        assert!(!clicked(by_spawns, SortBy::Spawns).descending);
        assert!(!clicked(BY_TIME, SortBy::Time).descending);
    }

    #[test]
    fn rows_take_the_order_of_their_sort() {
        let mut table = rows(&[("CppLink", 2), ("CppCompile", 9), ("Genrule", 4)]);
        apply(clicked(BY_TIME, SortBy::Name), &mut table);
        assert_eq!(names(&table), ["CppCompile", "CppLink", "Genrule"]);
        apply(clicked(BY_TIME, SortBy::Spawns), &mut table);
        assert_eq!(names(&table), ["CppCompile", "Genrule", "CppLink"]);
    }

    #[test]
    fn ties_go_in_order_of_name_either_way() {
        let mut table = rows(&[("b", 1), ("c", 2), ("a", 1)]);
        let ascending = clicked(clicked(BY_TIME, SortBy::Spawns), SortBy::Spawns);
        apply(ascending, &mut table);
        assert_eq!(names(&table), ["a", "b", "c"]);
        apply(clicked(ascending, SortBy::Spawns), &mut table);
        assert_eq!(names(&table), ["c", "a", "b"]);
    }

    #[test]
    fn rows_sort_by_the_amount_a_byte_title_names() {
        let mut table = rows(&[("a", 1), ("b", 1), ("c", 1)]);
        let tools = [5, 9, 0];
        for (row, tools) in table.iter_mut().zip(tools) {
            row.bytes.inputs.tools = Bytes::new(tools);
        }
        let by_tools = clicked(BY_TIME, SortBy::Bytes(ByteMeasure::Tools));
        apply(by_tools, &mut table);
        assert_eq!(names(&table), ["b", "a", "c"]);
        apply(
            clicked(by_tools, SortBy::Bytes(ByteMeasure::Outputs)),
            &mut table,
        );
        assert_eq!(names(&table), ["a", "b", "c"]);
    }

    #[test]
    fn sorting_tables_sorts_each_of_them_alike() {
        let mut first = rows(&[("b", 1), ("a", 2)]);
        let mut second = rows(&[("d", 3), ("c", 1)]);
        let mut sort = BY_TIME;
        sort_tables(&mut sort, SortBy::Name, [&mut first, &mut second]);
        assert_eq!(sort, clicked(BY_TIME, SortBy::Name));
        assert_eq!(names(&first), ["a", "b"]);
        assert_eq!(names(&second), ["c", "d"]);
    }
}
