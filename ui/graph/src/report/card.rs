use super::{Badged, Report, Row};
use crate::column::{Folding, line, scrolling};
use crate::legend;
use crate::model::Loaded;
use crate::reveal::reveal;
use crate::status::{Status, Statuses};
use crate::{GraphPanel, Palette, ToggleOutcomes, why};
use bviz_graph_layout::Id;
use bviz_invocation::Configurations;
use bviz_label::names::RepoNames;
use bviz_ui_components::LabelLine;
use bviz_ui_components::gesture::{
    Button as PointerButton, Entry, GestureRow, Gestures, Input, Place, Pointer,
};
use bviz_ui_components::{Button, ListScroll, MenuItem, TextTooltip, TruncatingLabel};
use gpui::{
    App, ClickEvent, Context, Div, Entity, ListSizingBehavior, Stateful, Window, div, prelude::*,
    uniform_list,
};

/// What the card does for a gesture in [`GESTURES`].
#[derive(Clone, Copy)]
enum ReportIntent {
    JumpTo,
}

const GESTURES: Gestures<ReportIntent> = Gestures::new(&[Entry::new(
    Input::click(PointerButton::Left),
    Place::On("an outcome"),
    "Jump to node",
    ReportIntent::JumpTo,
)]);

/// The gestures the card answers to.
pub(crate) fn gestures() -> impl Iterator<Item = GestureRow> {
    GESTURES.rows()
}

/// What the panel holds of the card reading what the build reported.
pub(crate) struct Card {
    /// Where the reader put the card, up or away; `None` until they do
    /// either, while it is up for a build that left something to look into.
    chosen: Option<bool>,
    scroll: ListScroll,
}

impl Card {
    pub(crate) fn new() -> Self {
        Card {
            chosen: None,
            scroll: ListScroll::new(),
        }
    }

    fn is_up(&self, report: &Report) -> bool {
        self.chosen.unwrap_or_else(|| report.troubled())
    }

    /// Puts the card reading `report` away where it is up, and up where it
    /// is away.
    fn toggle(&mut self, report: &Report) {
        self.chosen = Some(!self.is_up(report));
    }
}

/// What the card reads of a graph whose build reported anything.
#[derive(Clone, Copy)]
struct Reported<'a> {
    loaded: &'a Loaded,
    /// The configurations of the invocation the build is of.
    configurations: &'a Configurations,
}

impl<'a> Reported<'a> {
    /// `None` while no graph is loaded, and for one whose build reported
    /// nothing: that of a graph opened as part of no invocation did not.
    fn of(view: &GraphPanel, cx: &'a App) -> Option<Self> {
        let model = view.model.read(cx);
        let loaded = model.loaded()?;
        let configurations = model.configurations(cx);
        (!loaded.reported().is_empty()).then_some(Reported {
            loaded,
            configurations,
        })
    }

    fn report(self) -> &'a Report {
        self.loaded.reported()
    }

    /// The names the graph shows the build's labels by.
    fn names(self) -> &'a RepoNames {
        self.loaded.shared.labels.repos().names()
    }
}

/// Puts the card up or away, where the build reported anything for it to read.
pub(crate) fn toggle(view: &mut GraphPanel, cx: &mut Context<GraphPanel>) {
    if let Some(reported) = Reported::of(view, cx) {
        view.report.toggle(reported.report());
        cx.notify();
    }
}

/// The toolbar's control putting the card up and away; `None` where the build
/// reported nothing for the card to read. It reads its name where `labelled`,
/// and is icon-only otherwise.
pub(crate) fn control(
    view: &GraphPanel,
    labelled: bool,
    cx: &Context<GraphPanel>,
) -> Option<Button> {
    let report = Reported::of(view, cx)?.report();
    Some(
        Button::new(
            "report-toggle",
            cx.listener(|view, _: &ClickEvent, _, cx| toggle(view, cx)),
        )
        .when(labelled, |toggle| toggle.label(NAME))
        .leading_icon("list-checks")
        .keys_for(&ToggleOutcomes, &view.focus_handle)
        .selected(view.report.is_up(report)),
    )
}

/// The overflow menu's row putting the card up and away; `None` where the build
/// reported nothing for the card to read.
pub(crate) fn menu_row(
    view: &GraphPanel,
    window: &Window,
    cx: &Context<GraphPanel>,
) -> Option<MenuItem> {
    Reported::of(view, cx)?;
    let panel = cx.weak_entity();
    let row = MenuItem::new(NAME, move |_, cx| {
        panel.update(cx, toggle).ok();
    });
    Some(row.keys_for(&ToggleOutcomes, &view.focus_handle, window))
}

/// What the card's toolbar control and its overflow menu row read.
const NAME: &str = "outcomes";

/// The row `index` of the card's list, which a click shows the node of where
/// the graph holds one to show. A configuration is named as `configurations`
/// names it.
fn row(
    index: usize,
    row: &Row,
    configurations: &Configurations,
    names: &RepoNames,
    view: &Entity<GraphPanel>,
    palette: &Palette,
) -> Stateful<Div> {
    let colors = &palette.colors;
    let name = div()
        .flex()
        .gap_1()
        .min_w_0()
        .children(
            row.lead()
                .map(|lead| div().flex_none().whitespace_nowrap().child(lead)),
        )
        .child({
            let subject = row.name().subject(names);
            let parts = row.name().parts(&subject);
            let label = LabelLine::parted(subject.into_owned(), parts, colors.text_muted);
            div().min_w_0().child(label)
        })
        .children(
            row.configuration(configurations)
                .map(|configuration| div().flex_none().whitespace_nowrap().child(configuration)),
        );
    let listed = div()
        .id(("report-row", index))
        .w_full()
        .flex()
        .items_center()
        .gap_1()
        .px_1()
        .rounded_sm()
        .tooltip(TextTooltip::builder(
            row.hover(configurations, names).into(),
            colors.tooltip(),
        ))
        .child(legend::status(row.listed.status(), palette))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(name)
                .child(line(row.detail()).text_color(colors.text_muted)),
        );
    match row.shown.node() {
        Some(node) => {
            let (hover, view) = (colors.element_hover, view.clone());
            listed
                .cursor_pointer()
                .hover(move |row| row.bg(hover))
                .on_click(move |event: &ClickEvent, _, cx| {
                    GESTURES.dispatch(Pointer::click(event), |intent| match intent {
                        ReportIntent::JumpTo => {
                            view.update(cx, |view, cx| reveal(view, node, cx));
                            true
                        }
                    });
                })
        }
        None => listed.text_color(colors.text_muted),
    }
}

/// What the card says of the selected box.
struct Selected<'a> {
    node: Id,
    /// The status the box is badged with.
    status: Status,
    badged: Badged<'a>,
}

impl<'a> Selected<'a> {
    /// `None` for a node the build reports nothing of or in.
    fn of(node: Id, report: &'a Report, statuses: &Statuses) -> Option<Self> {
        let (status, badged) = report.badged(node, statuses)?;
        Some(Selected {
            node,
            status,
            badged,
        })
    }

    /// The root cause the card names, which the paths to the box can be
    /// asked for from.
    fn cause(&self) -> Option<Id> {
        match self.badged {
            Badged::Reported(said) | Badged::ReportedAndHeld(said) => said.cause,
            Badged::Held => None,
        }
    }
}

/// What the build reports of the selected box, over the control asking for
/// the dependency paths its root cause reaches it by, which is offered until
/// those are what is asked.
fn selected(
    selected: Selected,
    names: &RepoNames,
    view: &GraphPanel,
    palette: &Palette,
    cx: &Context<GraphPanel>,
) -> Div {
    let node = selected.node;
    let cause = selected
        .cause()
        .filter(|&cause| !why::asking_between(view, cause, node));
    let words = selected.badged.words(selected.status, names);
    let said = div()
        .flex()
        .items_center()
        .gap_1()
        .child(legend::status(selected.status, palette))
        .child(div().flex_1().min_w_0().child(words));
    div()
        .flex()
        .flex_col()
        .items_end()
        .gap_1()
        .child(said.w_full())
        .children(cause.map(|cause| {
            Button::new(
                "report-why",
                cx.listener(move |view, _: &ClickEvent, _, cx| {
                    why::ask_between(view, cause, node, cx)
                }),
            )
            .label("paths from its root cause")
            .tooltip("Show paths from root cause")
            .compact()
        }))
}

/// The `rows` rows of the card.
fn listed(view: &GraphPanel, rows: usize, palette: &Palette, cx: &Context<GraphPanel>) -> Div {
    let (entity, palette) = (cx.entity(), *palette);
    let rows = uniform_list("report-rows", rows, move |range, _, cx| {
        let reported = Reported::of(entity.read(cx), cx);
        let rows = reported.into_iter().flat_map(|reported| {
            let listed = reported.report().listed(range.clone());
            listed.map(move |listed| (listed, reported))
        });
        range
            .clone()
            .zip(rows)
            .map(|(index, (listed, reported))| {
                let (configurations, names) = (reported.configurations, reported.names());
                row(index, listed, configurations, names, &entity, &palette)
            })
            .collect::<Vec<_>>()
    })
    .with_sizing_behavior(ListSizingBehavior::Infer)
    .track_scroll(view.report.scroll.uniform_handle());
    scrolling(
        rows,
        |view| &view.report.scroll,
        |view| &mut view.report.scroll,
        &palette,
        cx,
    )
}

/// What the build reported, for the caller to place over the drawing; `None`
/// while the card is away, and where the build reported nothing.
pub(crate) fn card(
    view: &GraphPanel,
    palette: &Palette,
    cx: &Context<GraphPanel>,
) -> Option<Folding> {
    let reported = Reported::of(view, cx)?;
    let report = reported.report();
    if !view.report.is_up(report) {
        return None;
    }
    let statuses = reported.loaded.statuses();
    let chosen = view
        .selection
        .node()
        .and_then(|node| Selected::of(node, report, statuses));
    let colors = &palette.colors;
    let head = div()
        .flex()
        .flex_col()
        .gap_1()
        .text_xs()
        .text_color(palette.label)
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap_2()
                .child(TruncatingLabel::new("report-builds", report.builds.line()))
                .child(
                    Button::new(
                        "report-close",
                        cx.listener(|view, _: &ClickEvent, _, cx| toggle(view, cx)),
                    )
                    .label("close")
                    .tooltip("Close card")
                    .compact(),
                ),
        )
        .children((report.tests.line()).map(|tests| TruncatingLabel::new("report-tests", tests)))
        .children(
            report
                .mismatches
                .lines()
                .map(|mismatch| div().min_w_0().text_color(colors.warning).child(mismatch)),
        )
        .children(chosen.map(|chosen| selected(chosen, reported.names(), view, palette, cx)));
    let rows = report.rows();
    let rows = (rows > 0).then(|| listed(view, rows, palette, cx));
    Some(Folding::new(head, rows, palette))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_gesture_is_reachable_and_fits_the_help() {
        assert_eq!(GESTURES.unreachable(), []);
        assert_eq!(GESTURES.overlong(), []);
    }

    use super::super::tests::{node, resolved};
    use bviz_invocation::{BuildOutcome, Cause, ConfiguredTarget, FailedAction, Outcomes};

    const CONFIG: &str = "a7a71f";
    const EXEC_CONFIG: &str = "960ec4";
    const GEN_OUTPUT: &str = "bazel-out/k8/bin/gen/gen.out";

    fn target(label: &str, configuration: &str) -> ConfiguredTarget {
        ConfiguredTarget::new(label.into(), Some(configuration.into()))
    }

    /// A build in which the action of `//gen:gen` writing [`GEN_OUTPUT`]
    /// failed, and with it `//app:bin`, the build's one target.
    fn failed_through_a_generator() -> Outcomes {
        let mut outcomes = Outcomes::default();
        outcomes.report_failed_action(
            GEN_OUTPUT.into(),
            FailedAction {
                owner: Some(target("//gen:gen", CONFIG)),
                mnemonic: "Genrule".into(),
                exit_code: None,
                message: String::new(),
            },
        );
        let causes = vec![Cause::Action {
            primary_output: GEN_OUTPUT.into(),
        }];
        outcomes.report_build(target("//app:bin", CONFIG), BuildOutcome::Failed { causes });
        outcomes
    }

    fn selected(outcomes: &Outcomes, name: &str) -> Option<(Status, String, Option<Id>)> {
        let (report, statuses) = resolved(outcomes);
        let selected = Selected::of(node(name), &report, &statuses)?;
        let words = selected
            .badged
            .words(selected.status, &RepoNames::default());
        Some((selected.status, words, selected.cause()))
    }

    #[test]
    fn a_box_is_said_to_have_ended_as_the_build_reports_of_it() {
        let outcomes = failed_through_a_generator();
        let action = node(&format!("action:{GEN_OUTPUT}"));
        assert_eq!(
            selected(&outcomes, &format!("ct://app:bin#{CONFIG}")),
            Some((
                Status::Failed,
                "//app:bin failed: //gen (Genrule) failed".into(),
                Some(action)
            ))
        );
        assert_eq!(
            selected(&outcomes, &format!("action:{GEN_OUTPUT}")),
            Some((
                Status::RootCause,
                "Genrule //gen failed · 1 target failed".into(),
                None
            ))
        );
        assert_eq!(selected(&outcomes, "target://lib:broken"), None);
    }

    #[test]
    fn a_box_the_build_reports_nothing_of_is_said_to_hold_what_it_stands_at() {
        let outcomes = failed_through_a_generator();
        for (name, held) in [
            ("pkg:gen", "this box holds the root cause of a failure"),
            (
                "pkg:app",
                "this box holds a target that failed through something it depends on",
            ),
        ] {
            let (_, text, cause) = selected(&outcomes, name).expect("a badged box");
            assert_eq!((text.as_str(), cause), (held, None), "{name}");
        }
        // The build reports nothing of the target owning the failed action.
        let (status, text, _) =
            selected(&outcomes, &format!("ct://gen:gen#{CONFIG}")).expect("a badged box");
        assert_eq!(status, Status::RootCause);
        assert_eq!(text, "this box holds the root cause of a failure");
    }

    /// The badge is the worst of what a box holds, so a sentence saying less
    /// of the box itself says so.
    #[test]
    fn a_box_standing_worse_for_what_it_holds_is_said_to_hold_it() {
        let mut outcomes = failed_through_a_generator();
        outcomes.report_build(target("//gen:gen", CONFIG), BuildOutcome::Built);
        assert_eq!(
            selected(&outcomes, &format!("ct://gen:gen#{CONFIG}")),
            Some((
                Status::RootCause,
                "//gen built; this box holds the root cause of a failure".into(),
                None
            ))
        );
        assert_eq!(
            selected(&outcomes, "target://gen:gen"),
            Some((
                Status::RootCause,
                "//gen built; this box holds the root cause of a failure".into(),
                None
            ))
        );

        // A target the graph holds under another configuration than the one
        // reported is the box the report lands on.
        let mut reconfigured = Outcomes::default();
        reconfigured.report_build(target("//app:bin", CONFIG), BuildOutcome::Built);
        reconfigured.report_build(target("//app:bin", EXEC_CONFIG), BuildOutcome::NotBuilt);
        assert_eq!(
            selected(&reconfigured, "target://app:bin"),
            Some((
                Status::Unfinished,
                "//app:bin not built: the build ended first".into(),
                None
            ))
        );
    }

    #[test]
    fn the_card_is_up_for_a_troubled_build_until_the_reader_puts_it_elsewhere() {
        let (quiet, troubled) = super::super::tests::quiet_and_troubled();
        assert!(!quiet.troubled() && troubled.troubled());

        let mut card = Card::new();
        assert!(!card.is_up(&quiet));
        assert!(card.is_up(&troubled));

        card.toggle(&troubled);
        assert!(!card.is_up(&troubled) && !card.is_up(&quiet));
        card.toggle(&troubled);
        assert!(card.is_up(&troubled) && card.is_up(&quiet));

        let mut card = Card::new();
        card.toggle(&quiet);
        assert!(card.is_up(&quiet));
    }
}
