//! The spawn bytes panel: the bytes a log's spawns wrote and read, summed per
//! runner and per mnemonic, and the spawns that wrote or read the most.

use crate::format::duration;
use crate::palette::Palette;
use crate::panel::{SpawnPanel, Tables};
use crate::summary::{GroupRow, LISTED_SPAWNS, Summary};
use crate::table::{
    BYTES_COLUMN, Column, GroupColumn, List, RunnerColumn, SortBy, SpawnsToggle, TIME_COLUMN,
    apply, by_room, cell, group_row, header_row, list_group, list_rows, longest, measure_title,
    name_cells, name_header_cells, none_note, picker_group, runner_table, scrolling_table,
    sort_tables, sorting_cell, sorting_header, spawn_row, totals,
};
use bviz_execlog::ByteMeasure;
use bviz_ui_components::amount::{self, bytes, moved};
use bviz_ui_components::{Dropdown, DropdownEvent, KeyScope, ListScroll, OneRow, Sort};
use bviz_ui_workspace::{PanelKind, Role, ViewKind};
use gpui::{
    App, Context, Div, Entity, FocusHandle, IntoElement, Render, SharedString, Subscription,
    Window, div, prelude::*,
};

/// The key context scoping the panel's own bindings, so a key pressed here
/// reaches this panel and no other.
pub(crate) const KEY_CONTEXT: KeyScope = KeyScope::new("ExecLogBytes", "Spawn bytes");

pub type BytesPanel = SpawnPanel<BytesTables>;

const COLUMNS: [GroupColumn; 6] = [
    GroupColumn::Name,
    GroupColumn::Spawns,
    GroupColumn::Bytes(ByteMeasure::Outputs),
    GroupColumn::Bytes(ByteMeasure::Inputs),
    GroupColumn::Bytes(ByteMeasure::Tools),
    GroupColumn::Time,
];

/// The columns of the figures each spawn of the list of the largest is listed
/// with: its bytes in each measure, then its spawn time.
const FIGURES: [Column; ByteMeasure::ALL.len() + 1] =
    [BYTES_COLUMN, BYTES_COLUMN, BYTES_COLUMN, TIME_COLUMN];

/// The order the mnemonic table opens in.
const DEFAULT_SORT: Sort<SortBy> = Sort {
    by: SortBy::Bytes(ByteMeasure::Inputs),
    descending: true,
};

/// The order the list of spawns largest in bytes opens in.
const LARGEST_SORT: Sort<ByteMeasure> = Sort {
    by: ByteMeasure::Inputs,
    descending: true,
};

const LARGEST: SpawnsToggle = SpawnsToggle {
    label: "largest",
    tip: "Spawns largest in bytes",
};

/// The runner picker's first option, summing every runner's spawns; the
/// summary's runners follow it in order.
const ALL_RUNNERS: &str = "All runners";

pub struct BytesTables {
    summary: Summary,
    list: List,
    /// Picks whose spawns the mnemonic table sums: every runner's, or one
    /// runner's.
    runner_picker: Entity<Dropdown>,
    _runner_picked: Subscription,
    sort: Sort<SortBy>,
    /// Which list of spawns largest in bytes shows, and which way round.
    largest_sort: Sort<ByteMeasure>,
    mnemonic_scroll: ListScroll,
    largest_scroll: ListScroll,
}

impl BytesTables {
    /// The spawns of the runner picked, or of every runner, by mnemonic.
    fn mnemonics(&self, cx: &App) -> &[GroupRow] {
        match self.runner_picker.read(cx).selected().checked_sub(1) {
            None => &self.summary.mnemonics,
            Some(runner) => &self.summary.runners[runner].mnemonics,
        }
    }

    fn sort_mnemonics(&mut self, by: SortBy, cx: &mut Context<Self>) {
        let summary = &mut self.summary;
        let tables = (summary.runners.iter_mut())
            .map(|runner| &mut runner.mnemonics)
            .chain([&mut summary.mnemonics]);
        sort_tables(&mut self.sort, by, tables);
        cx.notify();
    }

    fn sort_largest(&mut self, by: ByteMeasure, cx: &mut Context<Self>) {
        self.largest_sort = self.largest_sort.clicked(by, true);
        cx.notify();
    }

    fn show(&mut self, list: List, cx: &mut Context<Self>) {
        self.list = list;
        cx.notify();
    }

    /// The mnemonics of the runners picked, under the runners.
    fn by_mnemonic(&self, palette: &Palette, cx: &mut Context<Self>) -> Div {
        let runners: Vec<_> = (self.summary.runners.iter())
            .map(|r| r.sum.clone())
            .collect();
        let count = self.mnemonics(cx).len();
        let header = sorting_header(&COLUMNS, self.sort, palette, cx, Self::sort_mnemonics);
        let rows = list_rows("mnemonics", count, &self.mnemonic_scroll, {
            let (entity, palette) = (cx.entity(), *palette);
            move |shown, _, cx| {
                let mnemonics = entity.read(cx).mnemonics(cx);
                let longest = longest(mnemonics);
                (shown.map(|index| {
                    group_row(
                        ("mnemonic", index),
                        &COLUMNS,
                        &mnemonics[index],
                        longest,
                        &palette,
                    )
                }))
                .collect()
            }
        });
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .gap_3()
            .child(runner_table(&COLUMNS, &runners, palette))
            .child(scrolling_table(
                header,
                rows,
                cx.entity(),
                |tables: &BytesTables| &tables.mnemonic_scroll,
                |tables: &mut BytesTables| &mut tables.mnemonic_scroll,
                palette,
                cx,
            ))
    }

    /// The spawns largest in the measure the list is sorted by, in its order,
    /// each named by its target and output. A click on a measure's title
    /// shows the spawns largest in it.
    fn largest(&self, runner: RunnerColumn, palette: &Palette, cx: &mut Context<Self>) -> Div {
        let sort = self.largest_sort;
        let measure_cell = |measure: ByteMeasure, cx: &mut Context<Self>| {
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
                move |tables: &mut Self, cx| tables.sort_largest(measure, cx),
            )
        };
        let measures: Vec<_> = (ByteMeasure::ALL.into_iter())
            .map(|measure| measure_cell(measure, cx))
            .collect();
        let header = header_row(
            (measures.into_iter())
                .chain([cell(TIME_COLUMN, "Spawn time").into_any_element()])
                .chain(name_header_cells(runner)),
            palette,
        );
        let spawns = &self.summary.biggest[sort.by.index()];
        if spawns.is_empty() {
            let none = format!("No spawn has any {}.", amount::measure_name(sort.by));
            return div().child(header).child(none_note(none, palette));
        }
        let rows = list_rows("largest", spawns.len(), &self.largest_scroll, {
            let (entity, palette) = (cx.entity(), *palette);
            move |shown, _, cx| {
                let tables = entity.read(cx);
                let spawns = &tables.summary.biggest[sort.by.index()];
                (shown.map(|index| {
                    let spawn = match sort.descending {
                        true => &spawns[index],
                        false => &spawns[spawns.len() - 1 - index],
                    };
                    let amounts = ByteMeasure::ALL
                        .map(|measure| cell(BYTES_COLUMN, bytes(spawn.bytes.measure(measure))));
                    let time = cell(TIME_COLUMN, duration(spawn.time));
                    let names = name_cells(&spawn.name, &tables.summary.repos, &palette, runner);
                    spawn_row(
                        ("largest", index),
                        amounts.into_iter().chain([time]).chain(names),
                    )
                }))
                .collect()
            }
        });
        scrolling_table(
            header,
            rows,
            cx.entity(),
            |tables: &BytesTables| &tables.largest_scroll,
            |tables: &mut BytesTables| &mut tables.largest_scroll,
            palette,
            cx,
        )
    }
}

impl Tables for BytesTables {
    fn kind() -> PanelKind {
        PanelKind {
            view: ViewKind::new("execlog-spawn-bytes"),
            key_context: KEY_CONTEXT,
            role: Role::Analysis,
        }
    }

    fn cycle_list(&mut self, cx: &mut Context<Self>) {
        self.show(self.list.next(), cx);
    }

    fn new(mut summary: Summary, _focus: &FocusHandle, cx: &mut Context<Self>) -> Self {
        let sort = DEFAULT_SORT;
        for runner in &mut summary.runners {
            apply(sort, &mut runner.mnemonics);
        }
        apply(sort, &mut summary.mnemonics);
        let names: Vec<SharedString> = std::iter::once(ALL_RUNNERS.into())
            .chain((summary.runners.iter()).map(|runner| runner.sum.name.clone()))
            .collect();
        let runner_picker =
            cx.new(|cx| Dropdown::new("bytes-runner", names, 0, cx).tooltip("Runner shown"));
        let runner_picked = cx.subscribe(&runner_picker, |_, _, _: &DropdownEvent, cx| {
            cx.notify();
        });
        Self {
            summary,
            list: List::ByMnemonic,
            runner_picker,
            _runner_picked: runner_picked,
            sort,
            largest_sort: LARGEST_SORT,
            mnemonic_scroll: ListScroll::new(),
            largest_scroll: ListScroll::new(),
        }
    }

    fn toolbar(tables: &Entity<Self>, focus: &FocusHandle, window: &Window, cx: &App) -> OneRow {
        let this = tables.read(cx);
        let palette = Palette::from_theme(&bviz_ui_theme::theme(window, cx));
        let summary = &this.summary;
        let text = format!("{} spawns: {}", summary.spawns, moved(summary.bytes()));
        let caveats = "action-cache hits not logged";
        let choose = {
            let tables = tables.clone();
            move |list, cx: &mut App| tables.update(cx, |tables, cx| tables.show(list, cx))
        };
        let row = OneRow::new("spawn-bytes-toolbar")
            .filling(totals(text.into(), caveats, &palette))
            .group(list_group(this.list, LARGEST, choose, focus, window));
        match this.list {
            List::ByMnemonic => {
                let set = this.runner_picker.read(cx).selected() != 0;
                row.group(picker_group("runner", &this.runner_picker, set, cx))
            }
            List::Spawns => row,
        }
    }
}

impl Render for BytesTables {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::from_theme(&bviz_ui_theme::theme(window, cx));
        let body = div().size_full().flex().flex_col().pt_2().pl_3();
        if self.summary.spawns == 0 {
            return body.child(none_note("The log holds no spawn.".into(), &palette));
        }
        match self.list {
            List::ByMnemonic => body.child(self.by_mnemonic(&palette, cx)),
            List::Spawns => body.child(by_room(&FIGURES, |runner| {
                self.largest(runner, &palette, cx).into_any_element()
            })),
        }
    }
}
