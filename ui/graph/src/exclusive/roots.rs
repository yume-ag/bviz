//! The setting naming the roots of a dominator tree as a Bazel target set, and
//! the points of a unit such a set roots the tree at.

use super::{CloseRoots, EditRoots, ExclusivePanel, RootedBy};
use crate::Palette;
use crate::model::{GraphModel, Prepared, Shared};
use crate::point::{unit_names, written_name, written_target_label};
use bviz_graph_layout::Support;
use bviz_graph_layout::dominators::{Actions, Chosen, ConfiguredTargets, ScopeGraph};
use bviz_graph_layout::metrics::{ByUnit, Scope, Unit};
use bviz_graph_layout::select;
use bviz_graph_layout::shown::written_by_place;
use bviz_graph_layout::target_pattern::{TargetIndex, TargetPattern};
use bviz_graph_layout::target_set::{PatternError, Resolved, TargetSet, TopLevel};
use bviz_graph_layout::{Id, Kind};
use bviz_ui_components::{
    Button, Hint, KeyScope, MenuItem, Nesting, OverflowRows, Priority, QueryField, ToolbarGroup,
    menu_chrome,
};
use gpui::{
    App, ClickEvent, Context, Div, Entity, FocusHandle, Focusable, Pixels, SharedString,
    Subscription, Window, div, prelude::*, px,
};
use std::sync::Arc;

/// The key context the sheet editing the roots carries, which [`CloseRoots`]
/// is scoped to.
pub(crate) const KEY_CONTEXT: KeyScope = KeyScope::within(
    &super::KEY_CONTEXT,
    Nesting::Inside,
    "GraphExclusiveRoots",
    "Roots",
);

/// Width the field is drawn at, wide enough for its placeholder.
const FIELD_WIDTH: Pixels = px(380.);

const PLACEHOLDER: &str = "top-level, or target patterns: //... -//tools/...";

/// What the sheet says the field takes.
const HELP: &str = "\
Root target patterns · empty: top-level
-pattern removes · # comment · from workspace root
wildcards skip manual · test_suite: itself only
not applied: .bazelignore, --deleted_packages,
--build_tag_filters, --build_tests_only, incompatible targets";

/// What the points a target set roots a tree at are picked from, by the
/// unit of the tree. Each graph is that of a scope of the unit its place
/// names, [`Within::of`] alone making one.
pub(super) struct Within(Graphs);

enum Graphs {
    Targets,
    /// The graph's configured targets.
    Copies(Prepared<ScopeGraph>),
    Actions {
        /// The configured targets holding the actions.
        configured: Prepared<ScopeGraph>,
        actions: Prepared<ScopeGraph>,
    },
}

/// The graphs of a [`Within`] as what their units make them.
enum Views<'a> {
    Targets,
    Copies(ConfiguredTargets<'a>),
    Actions {
        configured: ConfiguredTargets<'a>,
        actions: Actions<'a>,
    },
}

impl Within {
    /// What the roots of a tree of `scope` are picked from; absent while the
    /// graph `model` holds is still being read.
    pub(super) fn of(scope: Scope, model: &mut GraphModel) -> Option<Self> {
        Some(Within(match scope.by_unit() {
            ByUnit::Target => Graphs::Targets,
            ByUnit::ConfiguredTarget { .. } => Graphs::Copies(model.scope_graph(scope)?),
            ByUnit::Action { configured_targets } => Graphs::Actions {
                configured: model.scope_graph(configured_targets)?,
                actions: model.scope_graph(scope)?,
            },
        }))
    }

    fn views(&self) -> Views<'_> {
        let views = match &self.0 {
            Graphs::Targets => Some(Views::Targets),
            Graphs::Copies(configured) => configured.wait().configured_targets().map(Views::Copies),
            Graphs::Actions {
                configured,
                actions,
            } => (configured.wait().configured_targets())
                .zip(actions.wait().actions())
                .map(|(configured, actions)| Views::Actions {
                    configured,
                    actions,
                }),
        };
        views.expect("a graph of the unit its place in `Within` names")
    }
}

/// The roots a target set names, and what they leave unsaid.
pub(super) struct Found {
    pub(super) roots: Chosen,
    /// The targets the set names, which root the tree of targets.
    pub(super) named: Chosen,
    pub(super) notices: Vec<String>,
}

/// The points `within` the target set `text` roots a tree at: the targets it
/// names among `targets`, their top-level copies, or the actions a build of
/// those copies starts from.
pub(super) fn find(
    text: &str,
    targets: &Prepared<Arc<TargetIndex>>,
    within: &Within,
) -> Result<Found, PatternError> {
    let set = TargetSet::parse(text)?;
    let index = targets.wait();
    let Resolved {
        targets: named,
        left_out,
    } = set.resolve(index)?;
    let mut notices: Vec<String> = Vec::new();
    let shared = targets.shared();
    let repos = shared.labels.repos();
    let left_out = left_out
        .iter()
        .map(|&id| written_name(&shared.names, &shared.facets, repos, id));
    notices.extend(manual_left_out(left_out.collect()));
    notices.extend(manual_untold(&set, index.tags()).map(str::to_owned));
    let (base, facets) = (&shared.base, &shared.facets);
    let top = |configured| TopLevel::of(&named, base, facets, configured);
    let (points, top) = match within.views() {
        Views::Targets => (named.clone(), None),
        Views::Copies(configured) => {
            let top = top(configured);
            (top.copies().to_vec(), Some(top))
        }
        Views::Actions {
            configured,
            actions,
        } => {
            let top = top(configured);
            (top.actions(base, configured, actions), Some(top))
        }
    };
    if let Some(top) = &top {
        notices.extend(copies_noted(top.guessed(), top.generated(), top.uncopied()));
    }
    Ok(Found {
        roots: Chosen::new(points),
        named: Chosen::new(named),
        notices,
    })
}

/// The most manual targets the field names as left out by a wildcard.
const NAMED_MANUAL: usize = 3;

/// What the field notes of the manual targets the set's wildcards left out,
/// labelled `left_out`, naming the first few; nothing where there are none.
/// The targets a bare package names come first, being the ones a reader
/// names without a wildcard and then misses under one.
fn manual_left_out(mut left_out: Vec<String>) -> Option<String> {
    let count = left_out.len();
    let (targets, name_them) = match count {
        0 => return None,
        1 => ("target", "name it to include it"),
        _ => ("targets", "name them to include them"),
    };
    left_out.sort_by_key(|label| !names_its_package(label));
    left_out.truncate(NAMED_MANUAL);
    let more = if left_out.len() < count { ", …" } else { "" };
    let named = left_out.join(", ");
    Some(format!(
        "{count} manual {targets} left out by wildcards: {named}{more} — {name_them}"
    ))
}

/// Whether `label` is the target the bare name of its package stands for,
/// as `//foo/bar:bar` is what `//foo/bar` names, and `@repo//:repo` what
/// `@repo` does.
fn names_its_package(label: &str) -> bool {
    !label.contains(':') || written_by_place(label)
}

/// What the field notes of `set` where `tags`, how far the inputs state the
/// tags of their rules, lets a wildcard of it take in a rule tagged manual:
/// nothing where no wildcard adds targets or every rule states its tags.
fn manual_untold(set: &TargetSet, tags: Support) -> Option<&'static str> {
    if !leaves_out_manual(set) {
        return None;
    }
    match tags {
        Support::Available => None,
        Support::Partial { .. } => Some(MANUAL_PARTLY_TOLD),
        Support::Unavailable { .. } => Some(MANUAL_UNTOLD),
    }
}

const MANUAL_PARTLY_TOLD: &str = "some rules state no tags, so wildcards may take in manual ones";

const MANUAL_UNTOLD: &str = "manual targets can't be told apart in this input";

/// Whether `set` holds a pattern that leaves out the rules tagged manual: a
/// wildcard adding its targets.
fn leaves_out_manual(set: &TargetSet) -> bool {
    set.patterns().iter().any(|signed| {
        let wildcard = matches!(
            signed.pattern(),
            TargetPattern::InPackage { .. } | TargetPattern::Below { .. }
        );
        wildcard && !signed.negative()
    })
}

/// What the field notes of the targets whose top-level copies were `guessed`,
/// and of those that have none: the `generated` files and the other
/// `uncopied` targets.
fn copies_noted(guessed: u32, generated: u32, uncopied: u32) -> impl Iterator<Item = String> {
    let guessed = match guessed {
        0 => None,
        1 => Some("1 target's top-level copy guessed".to_owned()),
        _ => Some(format!("{guessed} targets' top-level copies guessed")),
    };
    let generated = match generated {
        0 => None,
        1 => Some("1 generated file can't be traced to its rule in this input".to_owned()),
        _ => Some(format!(
            "{generated} generated files can't be traced to their rules in this input"
        )),
    };
    let uncopied = (uncopied > 0).then(|| {
        let have = if uncopied == 1 { "has" } else { "have" };
        let targets = unit_names(Unit::Target).counted(uncopied);
        format!("{targets} {have} no configured copy")
    });
    guessed.into_iter().chain(generated).chain(uncopied)
}

/// The header setting naming the roots as a target set, and the sheet that
/// edits it under the setting.
pub(super) struct Setting {
    field: Entity<QueryField>,
    /// Held by the sheet while it is open, whose losing focus closes it and
    /// roots the tree at what the field holds.
    sheet: FocusHandle,
    open: bool,
    /// The targets of a query taken for the roots, while the field still
    /// holds them as taken.
    taken: Option<Taken>,
    /// Why the last take from the filter took nothing.
    untaken: Option<&'static str>,
    /// Closes the sheet as focus leaves it; installed on the first render,
    /// where the window is in hand.
    closing: Option<Subscription>,
}

/// The targets a query named, written into the field as their labels, so
/// that the field edits them as any other set; taken once, never following
/// the query afterwards.
struct Taken {
    text: SharedString,
    targets: u32,
    /// How many targets the query named, of which the first `targets`
    /// were taken.
    of: u32,
}

impl Setting {
    pub(super) fn new(cx: &mut Context<ExclusivePanel>) -> Self {
        Setting {
            field: cx.new(|cx| QueryField::new("exclusive-roots", PLACEHOLDER, FIELD_WIDTH, cx)),
            sheet: cx.focus_handle(),
            open: false,
            taken: None,
            untaken: None,
            closing: None,
        }
    }

    pub(super) fn field(&self) -> &Entity<QueryField> {
        &self.field
    }

    /// The target set the field holds; none where it is empty.
    pub(super) fn target_set(&self, cx: &App) -> Option<SharedString> {
        let text = self.field.read(cx).text(cx);
        (!text.trim().is_empty()).then_some(text)
    }

    /// Takes up another filter, which makes moot why the last take took
    /// nothing.
    pub(super) fn filter_changed(&mut self) {
        self.untaken = None;
    }

    /// The targets taken for the roots, where the field still holds them.
    fn taken(&self, text: &str) -> Option<&Taken> {
        self.taken.as_ref().filter(|taken| taken.text == text)
    }
}

/// The labels of the rule targets the nodes `scope` names belong to, as a
/// reader writes them, each once and in label order. A file is a target of
/// its own, but building it as a root builds nothing its rule does not, so
/// only rules are taken: an action or a configured target stands for its
/// rule, and a file for none.
fn named_targets(scope: &select::Scope, shared: &Shared) -> Vec<String> {
    let (base, facets) = (&shared.base, &shared.facets);
    let is_rule =
        |id: Id| (base.nodes.get(id.0 as usize)).is_some_and(|node| node.kind == Kind::Target);
    let named = base.nodes.iter().filter(|node| scope.names(node.id));
    let mut targets: Vec<Id> = named
        .filter_map(|node| facets.label(node.id))
        .filter(|&id| is_rule(id))
        .collect();
    targets.sort_unstable();
    targets.dedup();
    let mut labels: Vec<String> = Vec::with_capacity(targets.len());
    labels.extend(
        targets
            .into_iter()
            .filter_map(|id| written_target_label(id, shared)),
    );
    labels.sort_unstable();
    labels
}

/// Why taking the roots from the filter takes nothing.
const NO_TARGETS: &str = "the query applied names no rule targets";

/// The most targets taken from the filter, past which the field would hold
/// more text than a reader can edit.
const MOST_TAKEN: usize = 1000;

/// The first [`MOST_TAKEN`] of `labels`, and how many there were.
fn capped(mut labels: Vec<String>) -> (Vec<String>, u32) {
    let of = labels.len() as u32;
    labels.truncate(MOST_TAKEN);
    (labels, of)
}

impl ExclusivePanel {
    /// Opens the sheet editing the roots, its field taking focus.
    pub(super) fn edit_roots(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.roots.closing.is_none() {
            let closing = cx.on_focus_out(&self.roots.sheet, window, |view, _, _, cx| {
                view.roots.open = false;
                view.reroot(cx);
                cx.notify();
            });
            self.roots.closing = Some(closing);
        }
        self.roots.open = true;
        self.roots.untaken = None;
        self.roots.field.read(cx).focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    /// Closes the sheet editing the roots, handing focus back to the panel,
    /// and roots the tree at what the field holds without waiting for it to
    /// settle.
    pub(super) fn close_roots(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.roots.open = false;
        self.focus_handle.focus(window, cx);
        self.reroot(cx);
        cx.notify();
    }

    /// Roots the tree at the targets the query applied names, as they stand.
    fn take_from_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let model = self.model.read(cx);
        let (Some(applied), Some(loaded)) = (model.filter().applied(), model.loaded()) else {
            return;
        };
        let (labels, of) = capped(named_targets(applied.scope(), &loaded.shared));
        let roots = &mut self.roots;
        if labels.is_empty() {
            roots.untaken = Some(NO_TARGETS);
            cx.notify();
            return;
        }
        let text: SharedString = labels.join(" ").into();
        roots.untaken = None;
        roots.taken = Some(Taken {
            text: text.clone(),
            targets: labels.len() as u32,
            of,
        });
        (roots.field).update(cx, |field, cx| field.set_text(text, cx));
        roots.field.read(cx).focus_handle(cx).focus(window, cx);
        self.reroot(cx);
        cx.notify();
    }
}

/// Room the setting gives what it reads, past which it is cut short.
const READING_WIDTH: Pixels = px(260.);

/// What the setting reads of the target set `text`, `taken` from the filter
/// where it was.
fn reading(text: &str, taken: Option<&Taken>) -> SharedString {
    if let Some(taken) = taken {
        let targets = unit_names(Unit::Target).counted(taken.of);
        return match taken.targets < taken.of {
            true => format!("{FROM_FILTER}: first {} of {targets}", taken.targets).into(),
            false => format!("{FROM_FILTER}: {targets}").into(),
        };
    }
    match text.trim() {
        "" => TOP_LEVEL.into(),
        set => set.to_owned().into(),
    }
}

/// What the setting reads of the targets taken from the filter.
const FROM_FILTER: &str = "from filter";

/// Why the roots cannot be taken from the filter.
const NO_QUERY: &str = "no query applied";

/// What the setting reads with the field empty.
const TOP_LEVEL: &str = "top-level";

/// The words the setting is labelled with, as a build of its roots reads.
const BUILD: &str = "bazel build";

/// How long the setting holds out against the panel narrowing.
const ROOTS: Priority = 3;

/// The setting naming the roots in the panel's header, which opens the
/// [`sheet`] editing them, and in the strip's overflow menu the row doing so.
pub(super) fn setting(
    view: &ExclusivePanel,
    palette: &Palette,
    window: &mut Window,
    cx: &Context<ExclusivePanel>,
) -> ToolbarGroup {
    let colors = &palette.colors;
    let text = view.roots.field.read(cx).text(cx);
    let set = !text.trim().is_empty();
    let reading = reading(&text, view.roots.taken(&text));
    let width = menu_chrome::widest(window, std::iter::once(reading.clone())).min(READING_WIDTH);
    let focus = &view.focus_handle;
    let trigger = || {
        let hint = Hint::default().keys_for(&EditRoots, focus);
        let open: fn(&ExclusivePanel) -> bool = |view| view.roots.open;
        menu_chrome::trigger(
            "exclusive-roots-setting".into(),
            colors,
            hint,
            (cx.weak_entity(), open),
            width,
            reading.clone(),
        )
        .on_click(
            cx.listener(|view, _: &ClickEvent, window, cx| match view.roots.open {
                true => view.close_roots(window, cx),
                false => view.edit_roots(window, cx),
            }),
        )
    };
    let panel = cx.weak_entity();
    let row = MenuItem::action(&EditRoots, Some(focus), window, cx, move |window, cx| {
        panel
            .update(cx, |view, cx| view.edit_roots(window, cx))
            .ok();
    });
    ToolbarGroup::named(ROOTS, BUILD, OverflowRows::row(row, set), trigger)
}

/// The sheet editing the roots, open under the strip `view` stands its
/// setting in; outside the strip, so that it stands whether the setting stands
/// in the strip or has left it for the strip's overflow menu.
pub(super) fn sheet(
    view: &ExclusivePanel,
    palette: &Palette,
    cx: &Context<ExclusivePanel>,
) -> Option<impl IntoElement + use<>> {
    let colors = &palette.colors;
    let roots = &view.roots;
    roots.open.then(|| {
        let applied = view.model.read(cx).filter().applied();
        let query = applied.map(|applied| applied.scope().query());
        let take =
            cx.listener(|view, _: &ClickEvent, window, cx| view.take_from_filter(window, cx));
        let source = match query {
            Some(query) => format!("targets of {query}"),
            None => NO_QUERY.to_owned(),
        };
        let from_filter = div()
            .w(FIELD_WIDTH)
            .flex()
            .items_center()
            .gap_2()
            .text_sm()
            .child(
                Button::new("exclusive-roots-from-filter", take)
                    .label("Roots from filter")
                    .compact()
                    .disabled(query.is_none()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_color(colors.text_muted)
                    .child(source),
            );
        let untaken = roots.untaken.map(|why| {
            div()
                .text_sm()
                .text_color(colors.error)
                .whitespace_nowrap()
                .child(why)
        });
        let sheet = div()
            .track_focus(&roots.sheet)
            .key_context(KEY_CONTEXT)
            .on_action(cx.listener(|view, _: &CloseRoots, window, cx| view.close_roots(window, cx)))
            .flex()
            .flex_col()
            .gap_2()
            .px_2()
            .child(roots.field.clone())
            .child(
                div()
                    .text_sm()
                    .text_color(colors.text_muted)
                    .whitespace_nowrap()
                    .children(HELP.lines().map(|line| div().child(line))),
            )
            .child(from_filter)
            .children(untaken);
        menu_chrome::under_trigger(menu_chrome::sheet(colors).child(sheet))
    })
}

/// The line under the strip saying that the target set the field holds
/// failed with `error`, and what the tree drawn is rooted `by` in its place.
pub(super) fn failure_line(error: &PatternError, by: Option<RootedBy>, palette: &Palette) -> Div {
    line(palette)
        .text_color(palette.colors.error)
        .child(failure(&error.to_string(), by))
}

/// The line under the strip reading `notices`, what the roots chosen leave
/// unsaid of the target set that chose them.
pub(super) fn notices_line(notices: &[String], palette: &Palette) -> Div {
    line(palette)
        .text_color(palette.colors.text_muted)
        .child(notices.join(NOTICE_SEPARATOR))
}

fn line(palette: &Palette) -> Div {
    let colors = &palette.colors;
    div()
        .flex_none()
        .px_2()
        .py_1()
        .text_sm()
        .bg(colors.surface)
        .border_b_1()
        .border_color(colors.border)
}

/// What the line reads of a set that failed with `error` while the tree
/// drawn is rooted `by` what stands in for it.
fn failure(error: &str, by: Option<RootedBy>) -> String {
    match by {
        Some(RootedBy::Targets) => format!("{error} — showing the last valid set"),
        Some(RootedBy::Tops) => format!("{error} — showing the whole build"),
        None => error.to_owned(),
    }
}

/// Joins the parts of a line of notices.
pub(crate) const NOTICE_SEPARATOR: &str = " · ";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_guessed_and_missing_are_noted_only_where_there_are_any() {
        let noted = |guessed, generated, uncopied| {
            copies_noted(guessed, generated, uncopied).collect::<Vec<_>>()
        };
        assert!(noted(0, 0, 0).is_empty());
        assert_eq!(noted(1, 0, 0), ["1 target's top-level copy guessed"]);
        let all = [
            "3 targets' top-level copies guessed",
            "2 generated files can't be traced to their rules in this input",
            "1 target has no configured copy",
        ];
        assert_eq!(noted(3, 2, 1), all);
        assert_eq!(noted(0, 0, 2), ["2 targets have no configured copy"]);
        assert_eq!(
            noted(0, 1, 0),
            ["1 generated file can't be traced to its rule in this input"]
        );
    }

    #[test]
    fn the_manual_targets_left_out_are_counted_and_the_first_named() {
        let noted = |labels: &[&str]| {
            manual_left_out(labels.iter().map(|label| label.to_string()).collect())
        };
        assert_eq!(noted(&[]), None);
        assert_eq!(
            noted(&["//dist:dist"]).unwrap(),
            "1 manual target left out by wildcards: //dist:dist — name it to include it"
        );
        assert_eq!(
            noted(&["//a:x", "//b:y"]).unwrap(),
            "2 manual targets left out by wildcards: //a:x, //b:y — name them to include them"
        );
        assert_eq!(
            noted(&["//a:w", "//a:x", "//a:y", "//a:z"]).unwrap(),
            "4 manual targets left out by wildcards: //a:w, //a:x, //a:y, … — name them to \
             include them"
        );
    }

    #[test]
    fn the_manual_targets_a_bare_package_names_are_named_first() {
        let labels = [
            "//dist:a",
            "//dist:b",
            "//dist:c",
            "//dist:dist",
            "//tools/env:env",
        ];
        let noted = manual_left_out(labels.map(str::to_owned).to_vec()).unwrap();
        assert_eq!(
            noted,
            "5 manual targets left out by wildcards: //dist:dist, //tools/env:env, //dist:a, … — \
             name them to include them"
        );
        for named_bare in ["//dist", "//dist:dist", "@repo//:repo", "@repo//a/b:b"] {
            assert!(names_its_package(named_bare), "{named_bare}");
        }
        for named_whole in ["//dist:all", "//:dist", "@repo//:all", "//a/b:a"] {
            assert!(!names_its_package(named_whole), "{named_whole}");
        }
    }

    /// Only a wildcard adding targets leaves the manual ones out, so only a
    /// set holding one can let manual targets through unnoticed.
    #[test]
    fn only_a_wildcard_adding_targets_leaves_out_manual_ones() {
        let leaves_out = |text: &str| leaves_out_manual(&TargetSet::parse(text).unwrap());
        assert!(leaves_out("//..."));
        assert!(leaves_out("//a:b //c:all"));
        assert!(leaves_out("//a:*"));
        assert!(!leaves_out("//a:b //c"));
        assert!(!leaves_out("//a:b -//c/... -//d:all"));
        assert!(!leaves_out("a/b.txt"));
    }

    /// A wildcard takes a rule stating no tags for one carrying none, which
    /// the field says as far as it holds: of some rules, or of all.
    #[test]
    fn manual_targets_a_wildcard_may_take_in_are_noted_as_far_as_tags_go_unstated() {
        let noted = |text: &str, tags| manual_untold(&TargetSet::parse(text).unwrap(), tags);
        let partial = Support::Partial {
            covered: 1,
            relevant: 2,
            flag: None,
        };
        let unavailable = Support::Unavailable {
            flag: bviz_bazel_flags::OUTPUT_RULE_ATTRS,
        };
        assert_eq!(noted("//...", Support::Available), None);
        assert_eq!(noted("//...", partial), Some(MANUAL_PARTLY_TOLD));
        assert_eq!(noted("//...", unavailable), Some(MANUAL_UNTOLD));
        assert_eq!(noted("//a:b", unavailable), None);
    }

    /// The setting reads the set it holds, the targets taken from the filter
    /// only while it still holds them as taken, and the top-level points
    /// when empty.
    #[test]
    fn the_setting_reads_what_it_holds() {
        let taken = Taken {
            text: "//a:b //c:d".into(),
            targets: 2,
            of: 2,
        };
        let held = |text: &str| reading(text, Some(&taken).filter(|taken| taken.text == text));
        assert_eq!(held(""), TOP_LEVEL);
        assert_eq!(held("  "), TOP_LEVEL);
        assert_eq!(held(" //... "), "//...");
        assert_eq!(held("//a:b //c:d"), "from filter: 2 targets");
        assert_eq!(held("//a:b //c:d -//c:d"), "//a:b //c:d -//c:d");
        let first = Taken { of: 2500, ..taken };
        assert_eq!(
            reading("//a:b //c:d", Some(&first)),
            "from filter: first 2 of 2500 targets"
        );
    }

    #[test]
    fn at_most_a_bounded_number_of_targets_is_taken() {
        let labels = |count: usize| (0..count).map(|n| format!("//a:{n}")).collect();
        let (taken, of) = capped(labels(MOST_TAKEN + 5));
        assert_eq!((taken.len(), of), (MOST_TAKEN, MOST_TAKEN as u32 + 5));
        let (taken, of) = capped(labels(3));
        assert_eq!((taken.len(), of), (3, 3));
    }

    #[test]
    fn a_failed_set_says_what_the_tree_is_rooted_by_in_its_place() {
        let error = "no such target //a:b";
        assert_eq!(
            failure(error, Some(RootedBy::Targets)),
            "no such target //a:b — showing the last valid set"
        );
        assert_eq!(
            failure(error, Some(RootedBy::Tops)),
            "no such target //a:b — showing the whole build"
        );
        assert_eq!(failure(error, None), error);
    }
}
