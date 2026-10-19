//! The spawn time panel: the time of a log's spawns by phase, summed per
//! runner and per mnemonic, and the spawns that spent longest in one phase.

use crate::format::duration;
use crate::palette::Palette;
use crate::panel::{SpawnPanel, Tables};
use crate::phase_bar::spent_in;
use crate::summary::{Runner, Summary};
use crate::table::{
    GroupColumn, List, RunnerColumn, SortBy, SpawnsToggle, TIME_COLUMN, apply, by_room, cell,
    group_row, header_row, list_group, list_rows, longest, name_cells, name_header_cells,
    none_note, picker_group, runner_table, scrolling_table, sort_tables, sorting_header, spawn_row,
    totals,
};
use bviz_execlog::Phase;
use bviz_ui_components::gesture::GestureRow;
use bviz_ui_components::{
    Dropdown, DropdownEvent, KeyScope, Legend, LegendEntry, LegendState, ListScroll, OneRow,
    ShowLegend, Sort, Swatch, legend_gestures,
};
use bviz_ui_spawn_phase::phase_name;
use bviz_ui_workspace::{PanelKind, Role, ViewKind};
use gpui::{
    App, Context, Div, Entity, FocusHandle, IntoElement, Render, SharedString, Subscription,
    Window, div, prelude::*,
};

/// The key context scoping the panel's own bindings, so a key pressed here
/// reaches this panel and no other.
pub(crate) const KEY_CONTEXT: KeyScope = KeyScope::new("ExecLogTime", "Spawn time");

pub type TimePanel = SpawnPanel<TimeTables>;

const COLUMNS: [GroupColumn; 5] = [
    GroupColumn::Name,
    GroupColumn::Spawns,
    GroupColumn::Time,
    GroupColumn::Bar,
    GroupColumn::Note,
];

/// The order the mnemonic table opens in.
const DEFAULT_SORT: Sort<SortBy> = Sort {
    by: SortBy::Time,
    descending: true,
};

const LONGEST: SpawnsToggle = SpawnsToggle {
    label: "longest",
    tip: "Spawns longest in a phase",
};

pub struct TimeTables {
    summary: Summary,
    list: List,
    /// Picks the runner, of the summary's, whose spawns the mnemonic table
    /// sums: one runner's spawns report one set of phases, where several
    /// runners' might mix sets.
    runner_picker: Entity<Dropdown>,
    _runner_picked: Subscription,
    sort: Sort<SortBy>,
    /// Picks the phase the list of spawns longest in a phase ranks by.
    phase_picker: Entity<Dropdown>,
    _phase_picked: Subscription,
    /// Drawn in the panel's toolbar, outside the cached layer, so that
    /// opening it repaints no table.
    legend: Entity<LegendState>,
    mnemonic_scroll: ListScroll,
    longest_scroll: ListScroll,
}

impl TimeTables {
    /// The runner the mnemonic table sums the spawns of.
    fn runner(&self, cx: &App) -> &Runner {
        &self.summary.runners[self.runner_picker.read(cx).selected()]
    }

    fn phase(&self, cx: &App) -> Phase {
        Phase::ALL[self.phase_picker.read(cx).selected()]
    }

    fn sort_mnemonics(&mut self, by: SortBy, cx: &mut Context<Self>) {
        let tables = (self.summary.runners.iter_mut()).map(|runner| &mut runner.mnemonics);
        sort_tables(&mut self.sort, by, tables);
        cx.notify();
    }

    fn show(&mut self, list: List, cx: &mut Context<Self>) {
        self.list = list;
        cx.notify();
    }

    /// The mnemonics of the runner picked, under the runners.
    fn by_mnemonic(&self, palette: &Palette, cx: &mut Context<Self>) -> Div {
        let runners: Vec<_> = (self.summary.runners.iter())
            .map(|r| r.sum.clone())
            .collect();
        let mnemonics = &self.runner(cx).mnemonics;
        let longest = longest(mnemonics);
        let header = sorting_header(&COLUMNS, self.sort, palette, cx, Self::sort_mnemonics);
        let rows = list_rows("mnemonics", mnemonics.len(), &self.mnemonic_scroll, {
            let (entity, palette) = (cx.entity(), *palette);
            move |shown, _, cx| {
                let mnemonics = &entity.read(cx).runner(cx).mnemonics;
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
                |tables: &TimeTables| &tables.mnemonic_scroll,
                |tables: &mut TimeTables| &mut tables.mnemonic_scroll,
                palette,
                cx,
            ))
    }

    /// The spawns longest in the phase picked, longest first, each named by
    /// its target and output.
    fn longest(&self, runner: RunnerColumn, palette: &Palette, cx: &mut Context<Self>) -> Div {
        let phase = self.phase(cx);
        let spawns = &self.summary.top[phase.index()];
        let header = header_row(
            std::iter::once(cell(TIME_COLUMN, "Time in phase").into_any_element())
                .chain(name_header_cells(runner)),
            palette,
        );
        if spawns.is_empty() {
            let none = format!("No spawn spent time in {}.", phase_name(phase));
            return div().child(header).child(none_note(none, palette));
        }
        let rows = list_rows("longest", spawns.len(), &self.longest_scroll, {
            let (entity, palette) = (cx.entity(), *palette);
            move |shown, _, cx| {
                let tables = entity.read(cx);
                let spawns = &tables.summary.top[tables.phase(cx).index()];
                (shown.map(|index| {
                    let spawn = &spawns[index];
                    let time = cell(TIME_COLUMN, duration(spawn.time));
                    let names = name_cells(&spawn.name, &tables.summary.repos, &palette, runner);
                    spawn_row(("longest", index), std::iter::once(time).chain(names))
                }))
                .collect()
            }
        });
        scrolling_table(
            header,
            rows,
            cx.entity(),
            |tables: &TimeTables| &tables.longest_scroll,
            |tables: &mut TimeTables| &mut tables.longest_scroll,
            palette,
            cx,
        )
    }
}

impl Tables for TimeTables {
    fn kind() -> PanelKind {
        PanelKind {
            view: ViewKind::new("execlog-spawn-time"),
            key_context: KEY_CONTEXT,
            role: Role::Analysis,
        }
    }

    fn gestures() -> Vec<GestureRow> {
        legend_gestures().collect()
    }

    fn cycle_list(&mut self, cx: &mut Context<Self>) {
        self.show(self.list.next(), cx);
    }

    fn new(mut summary: Summary, focus: &FocusHandle, cx: &mut Context<Self>) -> Self {
        let sort = DEFAULT_SORT;
        for runner in &mut summary.runners {
            apply(sort, &mut runner.mnemonics);
        }
        let names: Vec<SharedString> = (summary.runners.iter())
            .map(|runner| runner.sum.name.clone())
            .collect();
        let runner_picker =
            cx.new(|cx| Dropdown::new("time-runner", names, 0, cx).tooltip("Runner shown"));
        let runner_picked = cx.subscribe(&runner_picker, |_, _, _: &DropdownEvent, cx| {
            cx.notify();
        });
        let phase = summary.longest_phase();
        let phase_picker = cx.new(|cx| {
            Dropdown::new("time-phase", Phase::ALL.map(phase_name), phase.index(), cx)
                .tooltip("Rank by phase")
        });
        let phase_picked = cx.subscribe(&phase_picker, |_, _, _: &DropdownEvent, cx| {
            cx.notify();
        });
        Self {
            summary,
            list: List::ByMnemonic,
            runner_picker,
            _runner_picked: runner_picked,
            sort,
            phase_picker,
            _phase_picked: phase_picked,
            legend: cx.new(|cx| LegendState::new(focus.clone(), cx)),
            mnemonic_scroll: ListScroll::new(),
            longest_scroll: ListScroll::new(),
        }
    }

    fn toolbar(tables: &Entity<Self>, focus: &FocusHandle, window: &Window, cx: &App) -> OneRow {
        let this = tables.read(cx);
        let palette = Palette::from_theme(&bviz_ui_theme::theme(window, cx));
        let summary = &this.summary;
        let text = format!(
            "{} spawns, {} of spawn time",
            summary.spawns,
            duration(summary.timing.total())
        );
        let caveats = "spawns overlap · not wall time\naction-cache hits not logged";
        let choose = {
            let tables = tables.clone();
            move |list, cx: &mut App| tables.update(cx, |tables, cx| tables.show(list, cx))
        };
        let control = match this.list {
            List::ByMnemonic => {
                let set = this.runner_picker.read(cx).selected() != 0;
                picker_group("runner", &this.runner_picker, set, cx)
            }
            List::Spawns => {
                let longest = summary.longest_phase();
                let set = this.phase(cx) != longest;
                picker_group("phase", &this.phase_picker, set, cx)
            }
        };
        let phases = spent_in(&summary.timing)
            .map(|phase| LegendEntry::new(Swatch::Fill(palette.phase(phase)), phase_name(phase)));
        let legend = Legend::new("spawn-time-legend", &this.legend).entries(phases);
        OneRow::new("spawn-time-toolbar")
            .filling(totals(text.into(), caveats, &palette))
            .group(list_group(this.list, LONGEST, choose, focus, window))
            .group(control)
            .trailing(legend.toolbar_group(window, cx))
    }

    fn on_actions(panel: Div, tables: &Entity<Self>) -> Div {
        let tables = tables.clone();
        panel.on_action(move |_: &ShowLegend, window, cx| {
            let legend = tables.read(cx).legend.clone();
            legend.update(cx, |legend, cx| legend.show(window, cx));
        })
    }
}

impl Render for TimeTables {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::from_theme(&bviz_ui_theme::theme(window, cx));
        let body = div().size_full().flex().flex_col().pt_2().pl_3();
        if self.summary.spawns == 0 {
            return body.child(none_note("The log holds no spawn.".into(), &palette));
        }
        match self.list {
            List::ByMnemonic => body.child(self.by_mnemonic(&palette, cx)),
            List::Spawns => body.child(by_room(&[TIME_COLUMN], |runner| {
                self.longest(runner, &palette, cx).into_any_element()
            })),
        }
    }
}
