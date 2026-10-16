use crate::format::{duration, exact_duration, share};
use crate::model::ExecLogModel;
use crate::model::Source;
use crate::palette::Palette;
use crate::summary::{BigSpawn, GroupRow, LISTED_SPAWNS, Runner, SpawnName, Summary, TopSpawn};
use bviz_execlog::{ByteMeasure, Phase, PhaseTiming};
use bviz_label::names::RepoNames;
use bviz_label::shown::Parts;
use bviz_ui_components::amount::{self, bytes};
use bviz_ui_components::gesture::{GestureRow, Gestures, Pointer};
use bviz_ui_components::{
    Dropdown, DropdownEvent, HintedLabelLine, KeyScope, LabelLine, Legend, LegendEntry,
    LegendState, ListScroll, OneRow, SCROLLBAR_THICKNESS, ShowLegend, Sort, Swatch, TextTooltip,
    TruncatingLabel, cut_off_note, legend_gestures, list_scrollbar, sort_gesture, sort_header,
    sort_hint,
};
use bviz_ui_spawn_phase::phase_name;
use bviz_ui_workspace::{Panel, PanelEvent, PanelKind, Role, ViewKind};
use gpui::{
    AnyElement, App, Context, Div, ElementId, Entity, EventEmitter, FocusHandle, Focusable,
    IntoElement, MouseButton, MouseDownEvent, Pixels, Render, SharedString, Stateful, Subscription,
    Window, div, prelude::*, px, relative,
};
use std::cmp::Ordering;
use std::time::Duration;

/// The key context scoping the panel's own bindings, so a key pressed here
/// reaches this panel and no other.
pub(crate) const KEY_CONTEXT: KeyScope = KeyScope::new("ExecLogSpawnTime", "Spawn time by phase");

const ROW_HEIGHT: Pixels = px(24.);
const TWO_LINE_ROW_HEIGHT: Pixels = px(44.);
const BAR_HEIGHT: Pixels = px(14.);
/// The canvas showing between two segments of a bar, so neighbouring phases
/// part by more than their colours.
const SEGMENT_GAP: Pixels = px(2.);
/// The narrowest a segment is drawn, so even a sliver of a phase can be
/// hovered for its time.
const MIN_SEGMENT_WIDTH: Pixels = px(4.);
const CELL_PAD: Pixels = px(6.);
const CARET: Pixels = px(12.);
const NAME_WIDTH: Pixels = px(220.);
const NUMBER_WIDTH: Pixels = px(110.);
const BYTES_WIDTH: Pixels = px(90.);
const NOTE_WIDTH: Pixels = px(110.);

/// What the panel does for a gesture in [`GESTURES`].
#[derive(Clone, Copy)]
enum SpawnTimeIntent {
    Sort,
}

const GESTURES: Gestures<SpawnTimeIntent> = Gestures::new(&[sort_gesture(SpawnTimeIntent::Sort)]);

/// What a table of groups is ordered by.
#[derive(Clone, Copy, PartialEq)]
enum SortBy {
    Name,
    Spawns,
    Bytes(ByteMeasure),
    Time,
}

/// The order the mnemonic table opens in.
const DEFAULT_SORT: Sort<SortBy> = Sort {
    by: SortBy::Time,
    descending: true,
};

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

fn apply(sort: Sort<SortBy>, rows: &mut [GroupRow]) {
    rows.sort_by(|a, b| compare(sort, a, b));
}

/// The order the list of spawns largest in bytes opens in.
const BIGGEST_SORT: Sort<ByteMeasure> = Sort {
    by: ByteMeasure::Inputs,
    descending: true,
};

/// What the panel shows of a loaded log.
struct Shown {
    summary: Summary,
    /// Picks the runner, of the summary's, whose spawns the mnemonic table
    /// sums: one runner's spawns report one set of phases, where several
    /// runners' might mix sets.
    runner_picker: Entity<Dropdown>,
    _runner_picked: Subscription,
    sort: Sort<SortBy>,
    /// Picks the phase the list of spawns longest in a phase ranks by.
    phase_picker: Entity<Dropdown>,
    _phase_picked: Subscription,
    /// Which list of spawns largest in bytes shows, and which way round.
    biggest_sort: Sort<ByteMeasure>,
}

impl Shown {
    /// The runner the mnemonic table sums the spawns of.
    fn runner(&self, cx: &App) -> &Runner {
        &self.summary.runners[self.runner_picker.read(cx).selected()]
    }

    fn phase(&self, cx: &App) -> Phase {
        Phase::ALL[self.phase_picker.read(cx).selected()]
    }
}

/// The spawn time of an execution log by phase: summed per runner and per
/// mnemonic, and the spawns that spent longest in one phase. Beside it, the
/// spawns that wrote or read the most.
pub struct SpawnTimePanel {
    focus: FocusHandle,
    model: Entity<ExecLogModel>,
    /// `None` until the log is loaded, the model's placeholder showing instead.
    shown: Option<Shown>,
    scroll: ListScroll,
    legend: Entity<LegendState>,
    _model_observer: Subscription,
}

impl SpawnTimePanel {
    pub(crate) fn new(model: Entity<ExecLogModel>, cx: &mut Context<Self>) -> Self {
        let model_observer = cx.observe(&model, |this, _, cx| {
            this.show_loaded(cx);
            cx.notify();
        });
        let focus = cx.focus_handle();
        let legend = cx.new(|cx| LegendState::new(focus.clone(), cx));
        let mut panel = Self {
            focus,
            model,
            shown: None,
            scroll: ListScroll::new(),
            legend,
            _model_observer: model_observer,
        };
        panel.show_loaded(cx);
        panel
    }

    fn show_loaded(&mut self, cx: &mut Context<Self>) {
        if self.shown.is_some() {
            return;
        }
        let Source::Loaded(summary) = self.model.read(cx).source() else {
            return;
        };
        let mut summary = summary.get().clone();
        let sort = DEFAULT_SORT;
        for runner in &mut summary.runners {
            apply(sort, &mut runner.mnemonics);
        }
        let names: Vec<SharedString> = summary
            .runners
            .iter()
            .map(|runner| runner.sum.name.clone())
            .collect();
        let runner_picker =
            cx.new(|cx| Dropdown::new("mnemonic-runner", names, 0, cx).tooltip("Runner shown"));
        let runner_picked = cx.subscribe(&runner_picker, |_, _, _: &DropdownEvent, cx| {
            cx.notify();
        });
        let phase = summary.longest_phase();
        let phase_picker = cx.new(|cx| {
            Dropdown::new("top-phase", Phase::ALL.map(phase_name), phase.index(), cx)
                .tooltip("Rank by phase")
        });
        let phase_picked = cx.subscribe(&phase_picker, |_, _, _: &DropdownEvent, cx| {
            cx.notify();
        });
        self.shown = Some(Shown {
            summary,
            runner_picker,
            _runner_picked: runner_picked,
            sort,
            phase_picker,
            _phase_picked: phase_picked,
            biggest_sort: BIGGEST_SORT,
        });
    }

    fn sort_mnemonics(&mut self, by: SortBy, cx: &mut Context<Self>) {
        if let Some(shown) = &mut self.shown {
            shown.sort = clicked(shown.sort, by);
            for runner in &mut shown.summary.runners {
                apply(shown.sort, &mut runner.mnemonics);
            }
            cx.notify();
        }
    }

    fn sort_biggest(&mut self, by: ByteMeasure, cx: &mut Context<Self>) {
        if let Some(shown) = &mut self.shown {
            shown.biggest_sort = shown.biggest_sort.clicked(by, true);
            cx.notify();
        }
    }

    fn body(&self, shown: &Shown, palette: &Palette, cx: &mut Context<Self>) -> Div {
        let summary = &shown.summary;
        let content = div()
            .id("spawn-time")
            .flex()
            .flex_col()
            .gap_5()
            .p_3()
            .pr(px(SCROLLBAR_THICKNESS) + CELL_PAD)
            .size_full()
            .overflow_y_scroll()
            .track_scroll(self.scroll.handle())
            .map(|content| {
                if summary.spawns == 0 {
                    return content.child(
                        div()
                            .text_color(palette.colors.text_muted)
                            .child("The log holds no spawn."),
                    );
                }
                let runners: Vec<GroupRow> =
                    summary.runners.iter().map(|r| r.sum.clone()).collect();
                let phase = shown.phase(cx);
                content
                    .child(section(
                        "By runner",
                        None,
                        group_table("runner", plain_header("Runner", palette), &runners, palette),
                        palette,
                    ))
                    .child(section(
                        "By mnemonic",
                        Some(labelled("Runner", shown.runner_picker.clone())),
                        group_table(
                            "mnemonic",
                            sorting_header(shown.sort, palette, cx),
                            &shown.runner(cx).mnemonics,
                            palette,
                        ),
                        palette,
                    ))
                    .child(section(
                        "Longest in a phase",
                        Some(labelled("Phase", shown.phase_picker.clone())),
                        top_table(phase, &summary.top[phase.index()], &summary.repos, palette),
                        palette,
                    ))
                    .child(section(
                        "Largest in bytes",
                        None,
                        biggest_table(shown.biggest_sort, summary, palette, cx),
                        palette,
                    ))
            });
        div()
            .relative()
            .flex()
            .flex_1()
            .min_h_0()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|panel, e: &MouseDownEvent, _, cx| {
                    if panel.scroll.press(e.position) {
                        cx.stop_propagation();
                    }
                    cx.notify();
                }),
            )
            .child(content)
            .child(list_scrollbar(
                cx.entity(),
                |panel: &SpawnTimePanel| &panel.scroll,
                |panel: &mut SpawnTimePanel| &mut panel.scroll,
                palette.scroll,
            ))
    }
}

/// What the log holds in sum, explaining on hover how to read a sum of spawn
/// time, and the legend naming which colour is which phase.
fn header(
    summary: &Summary,
    legend: &Entity<LegendState>,
    palette: &Palette,
    window: &Window,
    cx: &App,
) -> OneRow {
    let caveats = "spawns overlap · not wall time\naction-cache hits not logged";
    let totals = || {
        div()
            .id("spawn-time-summary")
            .min_w_0()
            .overflow_hidden()
            .whitespace_nowrap()
            .text_ellipsis()
            .child(format!(
                "{} spawns, {} of spawn time",
                summary.spawns,
                duration(summary.timing.total())
            ))
            .tooltip(TextTooltip::builder(
                caveats.into(),
                palette.colors.tooltip(),
            ))
    };
    let phases = spent_in(&summary.timing)
        .map(|phase| LegendEntry::new(Swatch::Fill(palette.phase(phase)), phase_name(phase)));
    let legend = Legend::new("spawn-time-legend", legend).entries(phases);
    OneRow::new("spawn-time-header")
        .filling(|| {
            // Laid over its holder, so the totals ask the row for no room and
            // take what the legend toggle leaves, cutting themselves short.
            div().flex_1().h_full().relative().child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .child(totals()),
            )
        })
        .trailing(legend.toolbar_group(window, cx))
}

/// `control` behind a label naming what it picks.
fn labelled(label: &'static str, control: impl IntoElement) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap_2()
        .child(label)
        .child(control)
        .into_any_element()
}

/// A titled part of the panel, with the controls acting on it beside the title.
fn section(
    title: &'static str,
    controls: Option<AnyElement>,
    content: impl IntoElement,
    palette: &Palette,
) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .flex()
                .items_center()
                .gap_3()
                .pb_1()
                .border_b_1()
                .border_color(palette.colors.border)
                .child(div().text_base().child(title))
                .children(controls),
        )
        .child(content)
}

/// How wide a column of a table is.
#[derive(Clone, Copy)]
enum Width {
    Fixed(Pixels),
    /// Takes what the fixed columns leave.
    Rest,
}

#[derive(Clone, Copy)]
struct Column {
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

/// A column of a table of summed spawns.
#[derive(Clone, Copy)]
enum GroupColumn {
    Name,
    Spawns,
    Bytes(ByteMeasure),
    Time,
    /// The bar of the phases of the spawn time.
    Bar,
    /// A note on what the bar can show.
    Note,
}

const GROUP_COLUMNS: [GroupColumn; 8] = [
    GroupColumn::Name,
    GroupColumn::Spawns,
    GroupColumn::Bytes(ByteMeasure::Outputs),
    GroupColumn::Bytes(ByteMeasure::Inputs),
    GroupColumn::Bytes(ByteMeasure::Tools),
    GroupColumn::Time,
    GroupColumn::Bar,
    GroupColumn::Note,
];

impl GroupColumn {
    fn layout(self) -> Column {
        match self {
            GroupColumn::Name => text(Width::Fixed(NAME_WIDTH)),
            GroupColumn::Spawns | GroupColumn::Time => number(Width::Fixed(NUMBER_WIDTH)),
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

/// One cell, header or body, laid out by its column so the header and every
/// row agree on the column's width at any window width.
fn cell(column: Column, content: impl IntoElement) -> Div {
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
fn row(id: impl Into<ElementId>, cells: impl IntoIterator<Item = Div>) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_row()
        .w_full()
        .h(ROW_HEIGHT)
        .children(cells)
}

fn header_row(cells: impl IntoIterator<Item = impl IntoElement>, palette: &Palette) -> Div {
    div()
        .flex()
        .flex_row()
        .w_full()
        .h(ROW_HEIGHT)
        .text_color(palette.colors.text_muted)
        .children(cells)
}

/// The header of a table no title of which sorts it.
fn plain_header(name_title: &'static str, palette: &Palette) -> Div {
    header_row(
        GROUP_COLUMNS.map(|column| cell(column.layout(), column.title(name_title))),
        palette,
    )
}

/// A header cell of `column` titled `title`, which `sort` applies to the
/// panel on a click to sort its table by `what`; `sorted` is as [`Sort::of`]
/// gives it.
fn sorting_cell(
    column: Column,
    title: &'static str,
    hint: SharedString,
    sorted: Option<bool>,
    palette: &Palette,
    cx: &mut Context<SpawnTimePanel>,
    sort: impl Fn(&mut SpawnTimePanel, &mut Context<SpawnTimePanel>) + 'static,
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
        .on_click(cx.listener(move |panel, e, _, cx| {
            GESTURES.dispatch(Pointer::click(e), |intent| match intent {
                SpawnTimeIntent::Sort => {
                    sort(panel, cx);
                    true
                }
            });
        }))
        .into_any_element()
}

/// The header of a table of mnemonics, whose titles but the bar's and the
/// note's sort it.
fn sorting_header(sort: Sort<SortBy>, palette: &Palette, cx: &mut Context<SpawnTimePanel>) -> Div {
    let cells = GROUP_COLUMNS.map(|column| {
        let (layout, title) = (column.layout(), column.title("Mnemonic"));
        match column.sorts() {
            Some((by, what)) => sorting_cell(
                layout,
                title,
                sort_hint(what),
                sort.of(by),
                palette,
                cx,
                move |panel, cx| panel.sort_mnemonics(by, cx),
            ),
            None => cell(layout, title).into_any_element(),
        }
    });
    header_row(cells, palette)
}

/// One row per group of `rows` under `header`, each bar as long against the
/// longest as its spawn time is.
fn group_table(
    id: &'static str,
    header: Div,
    rows: &[GroupRow],
    palette: &Palette,
) -> Stateful<Div> {
    let longest = rows
        .iter()
        .map(|row| row.timing.total())
        .max()
        .unwrap_or_default();
    let body = rows.iter().enumerate().map(|(index, group)| {
        let execution_only = group.timing.reports_execution_only();
        let cells = GROUP_COLUMNS.map(|column| {
            let content = match column {
                GroupColumn::Name => {
                    TruncatingLabel::new("name", group.name.clone()).into_any_element()
                }
                GroupColumn::Spawns => group.spawns.to_string().into_any_element(),
                GroupColumn::Bytes(measure) => {
                    bytes(group.bytes.measure(measure)).into_any_element()
                }
                GroupColumn::Time => duration(group.timing.total()).into_any_element(),
                GroupColumn::Bar => phase_bar(&group.timing, longest, palette).into_any_element(),
                GroupColumn::Note => div()
                    .text_color(palette.colors.text_muted)
                    .when(execution_only, |note| note.child("execution only"))
                    .into_any_element(),
            };
            cell(column.layout(), content)
        });
        row((id, index), cells)
    });
    div().id(id).flex().flex_col().child(header).children(body)
}

/// The columns naming a spawn in a list of spawns: its mnemonic, its runner,
/// and what the spawn made.
const NAME_COLUMNS: [Column; 3] = [
    text(Width::Fixed(NAME_WIDTH)),
    text(Width::Fixed(NAME_WIDTH)),
    text(Width::Rest),
];

const NAME_TITLES: [&str; NAME_COLUMNS.len()] = ["Mnemonic", "Runner", "Target and output"];

fn name_header_cells() -> impl Iterator<Item = AnyElement> {
    NAME_COLUMNS
        .into_iter()
        .zip(NAME_TITLES)
        .map(|(column, title)| cell(column, title).into_any_element())
}

/// What names a spawn that names neither its target nor its output.
const NO_TARGET_OR_OUTPUT: &str = "no target or output";

/// The cells of [`NAME_COLUMNS`] naming `spawn`.
fn name_cells(
    spawn: &SpawnName,
    repos: &RepoNames,
    palette: &Palette,
) -> [Div; NAME_COLUMNS.len()] {
    let [mnemonic, runner, made] = NAME_COLUMNS;
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
            mnemonic,
            TruncatingLabel::new("mnemonic", spawn.mnemonic.clone()),
        ),
        cell(runner, TruncatingLabel::new("runner", spawn.runner.clone())),
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
    ]
}

const TIME_COLUMN: Column = number(Width::Fixed(NUMBER_WIDTH));

/// `spawns`, longest in `phase` first, each named by its target and output.
fn top_table(phase: Phase, spawns: &[TopSpawn], repos: &RepoNames, palette: &Palette) -> Div {
    if spawns.is_empty() {
        return div()
            .text_color(palette.colors.text_muted)
            .child(format!("No spawn spent time in {}.", phase_name(phase)));
    }
    let header = header_row(
        std::iter::once(cell(TIME_COLUMN, "Time in phase").into_any_element())
            .chain(name_header_cells()),
        palette,
    );
    let body = spawns.iter().enumerate().map(|(index, spawn)| {
        let time = cell(TIME_COLUMN, duration(spawn.time));
        row(
            ("top", index),
            std::iter::once(time).chain(name_cells(&spawn.name, repos, palette)),
        )
        .h(TWO_LINE_ROW_HEIGHT)
    });
    div().flex().flex_col().child(header).children(body)
}

const BYTES_COLUMN: Column = number(Width::Fixed(BYTES_WIDTH));

fn measure_title(measure: ByteMeasure) -> &'static str {
    match measure {
        ByteMeasure::Outputs => "Outputs",
        ByteMeasure::Inputs => "Inputs",
        ByteMeasure::Tools => "Tools",
    }
}

/// The spawns largest in the measure `sort` is by, in its order, each named
/// by its target and output. A click on a measure's title shows the spawns
/// largest in it.
fn biggest_table(
    sort: Sort<ByteMeasure>,
    summary: &Summary,
    palette: &Palette,
    cx: &mut Context<SpawnTimePanel>,
) -> Stateful<Div> {
    let mut measure_cell = |measure: ByteMeasure| {
        sorting_cell(
            BYTES_COLUMN,
            measure_title(measure),
            format!(
                "Show the {LISTED_SPAWNS} spawns largest in {}",
                amount::measure_name(measure)
            )
            .into(),
            sort.of(measure),
            palette,
            cx,
            move |panel, cx| panel.sort_biggest(measure, cx),
        )
    };
    let header = header_row(
        ByteMeasure::ALL
            .map(&mut measure_cell)
            .into_iter()
            .chain(name_header_cells()),
        palette,
    );
    let spawns = &summary.biggest[sort.by.index()];
    let ordered: Vec<&BigSpawn> = match sort.descending {
        true => spawns.iter().collect(),
        false => spawns.iter().rev().collect(),
    };
    let body = ordered.into_iter().enumerate().map(|(index, spawn)| {
        let amounts =
            ByteMeasure::ALL.map(|measure| cell(BYTES_COLUMN, bytes(spawn.bytes.measure(measure))));
        row(
            ("biggest", index),
            amounts
                .into_iter()
                .chain(name_cells(&spawn.name, &summary.repos, palette)),
        )
        .h(TWO_LINE_ROW_HEIGHT)
    });
    let none = spawns.is_empty().then(|| {
        div().text_color(palette.colors.text_muted).child(format!(
            "No spawn has any {}.",
            amount::measure_name(sort.by)
        ))
    });
    div()
        .id("biggest")
        .flex()
        .flex_col()
        .child(header)
        .children(body)
        .children(none)
}

/// The phases `timing` spent time in, in the order a bar draws them; a sum's
/// holds every phase any of its bars draws.
fn spent_in(timing: &PhaseTiming) -> impl Iterator<Item = Phase> + '_ {
    Phase::ALL
        .into_iter()
        .filter(|&phase| !timing.phase(phase).is_zero())
}

/// `timing` as a bar of its phases, as long against the bar for `longest` as
/// its total is, each phase hovered for its exact duration and its share of
/// the total.
///
/// Where the reported phases overlap to sum past the total, the segments
/// divide the bar by that sum, and the tooltip says so.
///
/// The phases of a runner that reports only execution are drawn as one
/// unbroken bar, apart from a breakdown whose other phases took no time.
fn phase_bar(timing: &PhaseTiming, longest: Duration, palette: &Palette) -> Div {
    let total = timing.total();
    let summed: Duration = Phase::ALL.into_iter().map(|p| timing.phase(p)).sum();
    let span = total.max(summed);
    let execution_only = timing.reports_execution_only();
    let segments = spent_in(timing).map(|phase| {
        let time = timing.phase(phase);
        let mut tip = format!(
            "{} · {} · {} of {}",
            phase_name(phase),
            exact_duration(time),
            share(time, total),
            exact_duration(total),
        );
        if summed > total {
            tip.push_str(&format!(
                "\nphases overlap · sum {}",
                exact_duration(summed)
            ));
        }
        let segment = div()
            .id(("phase", phase.index()))
            .h_full()
            .min_w(MIN_SEGMENT_WIDTH)
            .w(relative(fraction(time, span)));
        let segment = if execution_only && phase == Phase::Execute {
            tip.push_str("\nonly phase reported");
            segment
                .bg(palette.unbroken)
                .border_2()
                .border_dashed()
                .border_color(palette.phase(phase))
        } else {
            segment.bg(palette.phase(phase))
        };
        segment.tooltip(TextTooltip::builder(tip.into(), palette.colors.tooltip()))
    });
    div()
        .flex()
        .flex_row()
        .gap(SEGMENT_GAP)
        .h(BAR_HEIGHT)
        .w(relative(fraction(total, longest)))
        .children(segments)
}

/// `part` of `whole`, as a length relative to the length `whole` takes.
fn fraction(part: Duration, whole: Duration) -> f32 {
    if whole.is_zero() {
        0.0
    } else {
        (part.as_secs_f64() / whole.as_secs_f64()) as f32
    }
}

impl EventEmitter<PanelEvent> for SpawnTimePanel {}

impl Focusable for SpawnTimePanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Panel for SpawnTimePanel {
    fn kind() -> PanelKind {
        PanelKind {
            view: ViewKind::new("execlog-spawn-time"),
            key_context: KEY_CONTEXT,
            role: Role::Analysis,
        }
    }

    fn gestures() -> Vec<GestureRow> {
        GESTURES.rows().chain(legend_gestures()).collect()
    }

    fn status(&self, cx: &App) -> Option<SharedString> {
        let status = self
            .model
            .read(cx)
            .source()
            .status("No execution log", |summary| {
                format!("{} spawns", summary.get().spawns)
            });
        Some(status.into())
    }

    fn set_active(&mut self, active: bool, window: &mut Window, cx: &mut Context<Self>) {
        if active {
            self.focus.focus(window, cx);
        }
    }
}

impl Render for SpawnTimePanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::from_theme(&bviz_ui_theme::theme(window, cx));
        let panel = div()
            .key_context(Self::kind().key_context)
            .track_focus(&self.focus)
            .on_action(cx.listener(|panel, _: &ShowLegend, window, cx| {
                panel
                    .legend
                    .update(cx, |legend, cx| legend.show(window, cx));
            }))
            .flex()
            .flex_col()
            .size_full()
            .overflow_hidden()
            .bg(palette.colors.background)
            .text_color(palette.colors.text)
            .text_sm();
        match &self.shown {
            Some(shown) => panel
                .children(
                    self.model
                        .read(cx)
                        .cut_off()
                        .then(|| cut_off_note("execution log", "spawns", &palette.colors)),
                )
                .child(header(&shown.summary, &self.legend, &palette, window, cx))
                .child(self.body(shown, &palette, cx)),
            None => panel.children(self.model.read(cx).source().placeholder(&palette.colors)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bviz_execlog::{Bytes, SpawnBytes};

    #[test]
    fn every_gesture_is_reachable() {
        assert_eq!(GESTURES.unreachable(), []);
    }

    #[test]
    fn every_gesture_fits_the_keys_help() {
        assert_eq!(GESTURES.overlong(), []);
    }

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
    fn only_the_phases_spent_in_are_listed_in_bar_order() {
        let mut reported = [Duration::ZERO; bviz_invocation::spawn::REPORTED_PHASES];
        reported[Phase::Fetch.index()] = Duration::from_millis(3);
        reported[Phase::Queue.index()] = Duration::from_millis(2);
        let timing = PhaseTiming::new(Duration::from_millis(5), reported);
        let phases: Vec<Phase> = spent_in(&timing).collect();
        assert_eq!(phases, [Phase::Queue, Phase::Fetch]);
    }

    #[test]
    fn a_click_on_a_new_title_sorts_names_up_and_counts_down() {
        let by_name = clicked(DEFAULT_SORT, SortBy::Name);
        assert!(!by_name.descending);
        assert!(clicked(by_name, SortBy::Spawns).descending);
    }

    #[test]
    fn a_click_on_the_sorting_title_turns_the_order_round() {
        let by_spawns = clicked(DEFAULT_SORT, SortBy::Spawns);
        assert!(!clicked(by_spawns, SortBy::Spawns).descending);
        assert!(!clicked(DEFAULT_SORT, SortBy::Time).descending);
    }

    #[test]
    fn rows_take_the_order_of_their_sort() {
        let mut table = rows(&[("CppLink", 2), ("CppCompile", 9), ("Genrule", 4)]);
        apply(clicked(DEFAULT_SORT, SortBy::Name), &mut table);
        assert_eq!(names(&table), ["CppCompile", "CppLink", "Genrule"]);
        apply(clicked(DEFAULT_SORT, SortBy::Spawns), &mut table);
        assert_eq!(names(&table), ["CppCompile", "Genrule", "CppLink"]);
    }

    #[test]
    fn ties_go_in_order_of_name_either_way() {
        let mut table = rows(&[("b", 1), ("c", 2), ("a", 1)]);
        let ascending = clicked(clicked(DEFAULT_SORT, SortBy::Spawns), SortBy::Spawns);
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
        let by_tools = clicked(DEFAULT_SORT, SortBy::Bytes(ByteMeasure::Tools));
        apply(by_tools, &mut table);
        assert_eq!(names(&table), ["b", "a", "c"]);
        apply(
            clicked(by_tools, SortBy::Bytes(ByteMeasure::Outputs)),
            &mut table,
        );
        assert_eq!(names(&table), ["a", "b", "c"]);
    }
}
