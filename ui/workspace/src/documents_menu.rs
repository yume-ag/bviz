use crate::workspace::{ResetArrangement, ToggleDocuments};
use crate::{DocumentId, Documents, OpenDocument, ViewId, ViewState, Workspace};
use bviz_ui_components::{Button, KeyScope, Nesting, action_label, key_text_in, menu_chrome};
use bviz_ui_theme::{ThemeColors, over};
use gpui::{
    Action, AnyElement, Context, ElementId, Entity, FocusHandle, Hsla, IntoElement, Pixels, Render,
    SharedString, Subscription, WeakEntity, Window, deferred, div, prelude::*, px, relative,
};

gpui::actions!(
    bviz_workspace,
    [
        /// Close Documents menu
        CloseDocuments,
    ]
);

/// The key context the menu carries, which [`CloseDocuments`] is scoped to.
pub(crate) const KEY_CONTEXT: KeyScope = KeyScope::within(
    &crate::workspace::KEY_CONTEXT,
    Nesting::Inside,
    "DocumentsMenu",
    "Documents menu",
);

/// Side of the mark at the head of a view's row.
const ROW_MARK: Pixels = px(13.);

/// Breathing room between the trigger and the menu it opens.
const MENU_GAP: Pixels = px(4.);

/// How far a view's row is set in from the document heading it stands under.
const VIEW_INDENT: Pixels = px(14.);

/// Room kept between a row's ends and what it holds.
const ROW_PAD: Pixels = px(8.);

/// The tallest the list stands before it scrolls, short enough that a session
/// holding many documents cannot open a menu past the top of the window.
const MAX_LIST_HEIGHT: Pixels = px(420.);

/// How a view's row reads: the mark at its head and the fill under it. The label
/// is set in the menu's own ink on every row, so what a view has on screen is
/// said by the mark and the fill alone and no row's label is read at less than
/// full body contrast.
struct RowStyle {
    mark: &'static str,
    /// The ink the mark is drawn in. An `svg` takes its colour from its own
    /// style rather than from the row's, so the mark carries its ink here.
    mark_ink: Hsla,
    /// The fill the row rests on, as it stands over the menu's own surface.
    fill: Hsla,
}

/// What the menu itself rests on, which is the fill of any row carrying none of
/// its own.
fn surface(colors: &ThemeColors) -> Hsla {
    colors.panel
}

fn row_style(state: ViewState, colors: &ThemeColors) -> RowStyle {
    match state {
        ViewState::Visible => RowStyle {
            mark: "check",
            mark_ink: colors.text,
            fill: Hsla::from(over(colors.selection, surface(colors))),
        },
        ViewState::Open => RowStyle {
            mark: "circle",
            mark_ink: colors.text,
            fill: surface(colors),
        },
        ViewState::Closed => RowStyle {
            mark: "plus",
            mark_ink: colors.text_muted,
            fill: surface(colors),
        },
    }
}

/// The fill a row resting on `fill` takes under the cursor, composited over it
/// so a row saying a pane shows its view goes on saying it.
fn hovered(fill: Hsla, colors: &ThemeColors) -> Hsla {
    Hsla::from(over(colors.element_hover, fill))
}

/// A view a click shows, and how its row reads.
struct ViewRow {
    document: DocumentId,
    view: ViewId,
    label: SharedString,
    state: ViewState,
}

/// How one document is listed: a document showing a single view is named by that
/// view's own row, and one showing several heads a group of rows naming each.
enum Listing {
    Alone(ViewRow),
    Under {
        label: SharedString,
        views: Vec<ViewRow>,
    },
}

/// `documents` as rows, in the order the registry holds them.
fn listings(documents: Vec<OpenDocument>) -> Vec<Listing> {
    let mut listings = Vec::with_capacity(documents.len());
    for document in documents {
        let id = document.id;
        let views: Vec<ViewRow> = document
            .views
            .into_iter()
            .map(|view| ViewRow {
                document: id,
                view: view.id,
                label: view.label.into(),
                state: view.state,
            })
            .collect();
        listings.push(match <[ViewRow; 1]>::try_from(views) {
            Ok([only]) => Listing::Alone(ViewRow {
                label: document.label,
                ..only
            }),
            Err(views) => Listing::Under {
                label: document.label,
                views,
            },
        });
    }
    listings
}

/// The session's open documents and the views each can show, as a menu opening
/// upward out of the window's status bar. A view is shown in the window this
/// menu belongs to, wherever it was last open.
pub(crate) struct DocumentsMenu {
    open: bool,
    /// The window a chosen view is shown in.
    workspace: WeakEntity<Workspace>,
    /// Listed here, and written to by a choice.
    documents: Entity<Documents>,
    focus_handle: FocusHandle,
    /// Re-lists as documents open and as their views gain or lose a panel.
    _changed: Subscription,
    /// Closes the menu when focus leaves it.
    _dismiss: Subscription,
}

impl DocumentsMenu {
    pub(crate) fn new(
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let documents = Documents::global(cx);
        let changed = cx.observe(&documents, |_, _, cx| cx.notify());
        let focus_handle = cx.focus_handle();
        let dismiss = cx.on_focus_out(&focus_handle, window, |this, _, _, cx| this.close(cx));
        Self {
            // A window can be opened from an update of the workspace opening it
            // and is drawn before that update unwinds, with that workspace
            // leased; the open menu reads every workspace of the session, and
            // reading a leased one panics. A menu opens on a press, never on the
            // frame its window is built in.
            open: false,
            workspace,
            documents,
            focus_handle,
            _changed: changed,
            _dismiss: dismiss,
        }
    }

    /// Opens the menu, or closes it and hands focus to the panel the window
    /// shows, so that panel's keys answer again. Reads the workspace, so it must
    /// not be called while the workspace is leased.
    pub(crate) fn toggle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.open {
            self.close(cx);
            if let Some(workspace) = self.workspace.upgrade() {
                let panel = workspace.read(cx).active_panel(cx);
                window.focus(&panel.focus_handle(cx), cx);
            }
            return;
        }
        self.open = true;
        // Held focus is what an outside click takes away, and the taking is what
        // dismisses the menu.
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    /// Closes the menu, leaving focus where the outside click or the chosen view
    /// has taken it.
    fn close(&mut self, cx: &mut Context<Self>) {
        if self.open {
            self.open = false;
            cx.notify();
        }
    }

    /// Lays this menu's own window out as it opened, closing the menu over it.
    fn reset_arrangement(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close(cx);
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            workspace.reset_arrangement(window, cx);
        });
    }

    /// Shows `view` of `document` in this menu's own window, closing the menu
    /// over it.
    fn show(
        &mut self,
        document: DocumentId,
        view: ViewId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close(cx);
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        self.documents.update(cx, |documents, cx| {
            documents.show(document, view, &workspace, window, cx);
        });
    }

    fn trigger(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        Button::new(
            "documents-menu",
            cx.listener(|this, _, window, cx| this.toggle(window, cx)),
        )
        .ghost()
        .compact()
        .label("Documents")
        .icon(if self.open { "caret-down" } else { "caret-up" })
        .keys_for(&ToggleDocuments, &self.focus_handle)
    }

    /// The open menu, laid out above the trigger: the status bar it opens from
    /// sits at the window's bottom edge, so there is nothing below it to open
    /// into.
    fn popover(
        &self,
        colors: &ThemeColors,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let rows = self.rows(colors, cx);
        deferred(
            div()
                .absolute()
                .bottom(relative(1.))
                .left_0()
                .pb(MENU_GAP)
                .child(
                    div()
                        .id("documents-menu-list")
                        .occlude()
                        .flex()
                        .flex_col()
                        .py_1()
                        .max_h(MAX_LIST_HEIGHT)
                        .overflow_y_scroll()
                        .bg(surface(colors))
                        .border_1()
                        .border_color(colors.border)
                        .rounded_md()
                        .shadow_lg()
                        .text_color(colors.text)
                        .children(rows)
                        .child(self.reset_row(colors, window, cx)),
                ),
        )
    }

    fn rows(&self, colors: &ThemeColors, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let listed = listings(self.documents.read(cx).documents(cx));
        let mut rows: Vec<AnyElement> = Vec::with_capacity(listed.len());
        for listing in listed {
            match listing {
                Listing::Alone(view) => {
                    let row = self.view_row(rows.len(), view, px(0.), colors, cx);
                    rows.push(row.into_any_element());
                }
                Listing::Under { label, views } => {
                    rows.push(heading(label, colors).into_any_element());
                    for view in views {
                        let row = self.view_row(rows.len(), view, VIEW_INDENT, colors, cx);
                        rows.push(row.into_any_element());
                    }
                }
            }
        }
        rows
    }

    /// The row that lays the window out as its files described themselves.
    /// It acts on the window rather than on any one view, so it carries no mark
    /// and is ruled off from the rows naming views.
    fn reset_row(
        &self,
        colors: &ThemeColors,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let hovered = hovered(surface(colors), colors);
        div()
            .id("documents-menu-reset")
            .flex()
            .flex_row()
            .items_center()
            .px(ROW_PAD)
            .pt_1()
            .pb_0p5()
            .mt_1()
            .border_t_1()
            .border_color(colors.border)
            .cursor_pointer()
            .hover(move |s| s.bg(hovered))
            .on_click(cx.listener(|this, _, window, cx| this.reset_arrangement(window, cx)))
            .justify_between()
            .gap(menu_chrome::LABEL_GAP)
            .child(
                div()
                    .whitespace_nowrap()
                    .child(action_label(&ResetArrangement, cx).unwrap_or(ResetArrangement.name())),
            )
            .children(
                key_text_in(&ResetArrangement, &self.focus_handle, window)
                    .map(|keys| menu_chrome::row_keys(keys, colors)),
            )
    }

    /// A view's row, `at` its place in the list: its mark, its label set in `by`
    /// from the menu's leading edge, and a click showing it here.
    fn view_row(
        &self,
        at: usize,
        view: ViewRow,
        by: Pixels,
        colors: &ThemeColors,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let style = row_style(view.state, colors);
        let hovered = hovered(style.fill, colors);
        let (document, shown) = (view.document, view.view);
        div()
            .id(ElementId::NamedInteger(
                "documents-menu-row".into(),
                at as u64,
            ))
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .pl(ROW_PAD + by)
            .pr(ROW_PAD)
            .py_0p5()
            .cursor_pointer()
            .bg(style.fill)
            .hover(move |s| s.bg(hovered))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.show(document, shown, window, cx);
            }))
            .child(
                bviz_ui_assets::icon(style.mark)
                    .size(ROW_MARK)
                    .flex_none()
                    .text_color(style.mark_ink),
            )
            .child(div().whitespace_nowrap().child(view.label))
    }
}

/// The name of a document standing over the rows of the views it can show.
fn heading(label: SharedString, colors: &ThemeColors) -> impl IntoElement + use<> {
    div()
        .px(ROW_PAD)
        .pt_1()
        .pb_0p5()
        .text_color(colors.text_muted)
        .whitespace_nowrap()
        .child(label)
}

impl Render for DocumentsMenu {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = bviz_ui_theme::theme(window, cx).colors;
        div()
            .relative()
            .flex_none()
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &CloseDocuments, window, cx| {
                if this.open {
                    this.toggle(window, cx);
                } else {
                    cx.propagate();
                }
            }))
            .child(self.trigger(cx))
            .children(self.open.then(|| self.popover(&colors, window, cx)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::documents::OpenView;
    use crate::{Document, Member, Role, ViewKind, ViewSpec};
    use bviz_ui_theme::{
        MIN_SECONDARY_CONTRAST, MIN_TEXT_CONTRAST, MIN_VISIBLE_STEP, built_in_themes,
        contrast_ratio, level_step,
    };

    const TIMELINE: ViewKind = ViewKind::new("timeline");
    const FLAME: ViewKind = ViewKind::new("flame");
    const GRAPH: ViewKind = ViewKind::new("graph");
    const CONSOLE: ViewKind = ViewKind::new("console");

    const STATES: [ViewState; 3] = [ViewState::Closed, ViewState::Open, ViewState::Visible];

    fn document(id: u64, label: &str, views: Vec<OpenView>) -> OpenDocument {
        OpenDocument {
            id: DocumentId::for_test(id),
            label: label.into(),
            views,
        }
    }

    /// A view of `kind` of the first file of its document.
    fn view(kind: ViewKind, label: &'static str, state: ViewState) -> OpenView {
        OpenView {
            id: ViewId::for_test(0, kind),
            label,
            state,
        }
    }

    /// A document showing several views is a heading over a row per view, each
    /// naming the view rather than the document they share.
    #[test]
    fn a_document_of_several_views_heads_a_row_for_each() {
        let listed = listings(vec![document(
            0,
            "build.profile.gz",
            vec![
                view(TIMELINE, "Timeline", ViewState::Visible),
                view(FLAME, "Flame Graph", ViewState::Closed),
            ],
        )]);
        let [Listing::Under { label, views }] = &listed[..] else {
            panic!("a document of two views is listed under a heading");
        };
        assert_eq!(label, "build.profile.gz");
        let labels: Vec<&str> = views.iter().map(|view| view.label.as_ref()).collect();
        assert_eq!(labels, ["Timeline", "Flame Graph"]);
        assert_eq!(views[1].state, ViewState::Closed);
    }

    fn file(label: &'static str, first: ViewKind, rest: Vec<ViewKind>) -> Member {
        let spec = |kind| ViewSpec::for_test(kind, Role::Analysis);
        Member::new(
            label.into(),
            spec(first),
            rest.into_iter().map(spec).collect(),
        )
    }

    /// What a window opening with `first` and `others` lists before any panel
    /// shows a view.
    fn opening_listings(first: Document, others: Vec<Document>) -> Vec<Listing> {
        listings(OpenDocument::unshown(std::iter::once(first).chain(others)))
    }

    /// A joined document heads a row for every view of every file, in the order
    /// the files were joined, under the label it was joined under.
    #[test]
    fn a_joined_document_heads_a_row_for_every_view_of_its_files() {
        let joined = Document::join(
            "Invocation a5f7f3e6".into(),
            file("build.profile.gz", TIMELINE, vec![FLAME]),
            file("build.bep", CONSOLE, Vec::new()),
            vec![file("aquery.pb", GRAPH, Vec::new())],
        );
        let listed = opening_listings(joined, Vec::new());
        let [Listing::Under { label, views }] = &listed[..] else {
            panic!("a joined document is listed under a heading");
        };
        assert_eq!(label, "Invocation a5f7f3e6");
        let rows: Vec<(ViewId, &str)> = views
            .iter()
            .map(|row| (row.view, row.label.as_ref()))
            .collect();
        assert_eq!(
            rows,
            [
                (ViewId::for_test(0, TIMELINE), "timeline"),
                (ViewId::for_test(0, FLAME), "flame"),
                (ViewId::for_test(1, CONSOLE), "console"),
                (ViewId::for_test(2, GRAPH), "graph"),
            ]
        );
    }

    /// The name each document is listed under: a heading, or the one row of a
    /// document showing one view.
    fn listed_names(listed: &[Listing]) -> Vec<&str> {
        listed
            .iter()
            .map(|listing| match listing {
                Listing::Alone(row) => row.label.as_ref(),
                Listing::Under { label, .. } => label.as_ref(),
            })
            .collect()
    }

    /// A file opened by itself is listed under the number its tabs carry, which
    /// counts the files of its name in every document; a joined document is
    /// listed under its own label, whatever its files are numbered.
    #[test]
    fn a_file_is_listed_under_the_number_it_has_among_the_files_of_every_document() {
        let session = || {
            Document::join(
                "Invocation a5f7f3e6".into(),
                file("build.profile.gz", TIMELINE, vec![FLAME]),
                file("aquery.pb", GRAPH, Vec::new()),
                Vec::new(),
            )
        };
        let listed = opening_listings(
            session(),
            vec![
                file("aquery.pb", GRAPH, Vec::new()).into(),
                file("build.profile.gz", TIMELINE, vec![FLAME]).into(),
                file("build.bep", CONSOLE, Vec::new()).into(),
            ],
        );
        assert_eq!(
            listed_names(&listed),
            [
                "Invocation a5f7f3e6",
                "aquery.pb (2)",
                "build.profile.gz (2)",
                "build.bep",
            ]
        );
    }

    #[test]
    fn joined_documents_sharing_a_label_are_listed_by_number() {
        let session = |profile| {
            Document::join(
                "Invocation a5f7f3e6".into(),
                file(profile, TIMELINE, vec![FLAME]),
                file("build.bep", CONSOLE, Vec::new()),
                Vec::new(),
            )
        };
        let listed = opening_listings(session("a.profile.gz"), vec![session("b.profile.gz")]);
        assert_eq!(
            listed_names(&listed),
            ["Invocation a5f7f3e6", "Invocation a5f7f3e6 (2)"]
        );
    }

    /// One view is the whole of what its document can show, so the row naming it
    /// names the document: a heading over a single row would only say the same
    /// thing twice.
    #[test]
    fn a_document_of_one_view_is_named_by_that_view_s_row() {
        let listed = listings(vec![document(
            0,
            "aquery.pb",
            vec![view(GRAPH, "Graph", ViewState::Visible)],
        )]);
        let [Listing::Alone(row)] = &listed[..] else {
            panic!("a document of one view is listed as one row");
        };
        assert_eq!(row.label, "aquery.pb");
        assert_eq!(row.view, ViewId::for_test(0, GRAPH));
    }

    /// A row is what a click is dispatched from, so it has to name the document
    /// and the view it stands for — including where two documents show views of
    /// the same kinds.
    #[test]
    fn every_row_names_the_document_and_view_it_shows() {
        let listed = listings(vec![
            document(
                0,
                "a.profile.gz",
                vec![
                    view(TIMELINE, "Timeline", ViewState::Visible),
                    view(FLAME, "Flame Graph", ViewState::Open),
                ],
            ),
            document(
                1,
                "b.profile.gz",
                vec![
                    view(TIMELINE, "Timeline", ViewState::Closed),
                    view(FLAME, "Flame Graph", ViewState::Closed),
                ],
            ),
        ]);
        let named: Vec<(DocumentId, ViewId)> = listed
            .iter()
            .flat_map(|listing| match listing {
                Listing::Alone(row) => std::slice::from_ref(row),
                Listing::Under { views, .. } => &views[..],
            })
            .map(|row| (row.document, row.view))
            .collect();
        let view = |kind| ViewId::for_test(0, kind);
        assert_eq!(
            named,
            [
                (DocumentId::for_test(0), view(TIMELINE)),
                (DocumentId::for_test(0), view(FLAME)),
                (DocumentId::for_test(1), view(TIMELINE)),
                (DocumentId::for_test(1), view(FLAME)),
            ]
        );
    }

    /// What a view has on screen is told by the mark at the head of its row, so
    /// no two states may carry the same one.
    #[test]
    fn every_state_carries_a_mark_of_its_own() {
        for theme in built_in_themes() {
            let mut marks: Vec<&str> = STATES
                .iter()
                .map(|&state| row_style(state, &theme.colors).mark)
                .collect();
            marks.sort_unstable();
            marks.dedup();
            assert_eq!(marks.len(), STATES.len(), "{}", theme.name);
        }
    }

    /// A row names the view a click shows, so its label has to be read on
    /// whatever fill the row carries — at rest and under the cursor both.
    #[test]
    fn every_row_keeps_its_label_on_the_fill_it_carries() {
        for theme in built_in_themes() {
            let colors = &theme.colors;
            for state in STATES {
                let style = row_style(state, colors);
                for fill in [style.fill, hovered(style.fill, colors)] {
                    let contrast = contrast_ratio(colors.text, fill);
                    assert!(
                        contrast >= MIN_TEXT_CONTRAST,
                        "{}: {state:?} at {contrast}:1",
                        theme.name
                    );
                }
            }
        }
    }

    /// The row that puts the window back rests on the menu's own surface, and
    /// its label has to be read there at rest and under the cursor both.
    #[test]
    fn the_row_that_puts_the_window_back_keeps_its_label() {
        for theme in built_in_themes() {
            let colors = &theme.colors;
            for fill in [surface(colors), hovered(surface(colors), colors)] {
                let contrast = contrast_ratio(colors.text, fill);
                assert!(
                    contrast >= MIN_TEXT_CONTRAST,
                    "{}: {contrast}:1",
                    theme.name
                );
            }
        }
    }

    /// The mark is what says which of the three states a view is in, so it has
    /// to be seen on the fill of the row carrying it, cursor or none.
    #[test]
    fn every_mark_is_seen_on_the_row_carrying_it() {
        for theme in built_in_themes() {
            let colors = &theme.colors;
            for state in STATES {
                let style = row_style(state, colors);
                for fill in [style.fill, hovered(style.fill, colors)] {
                    let contrast = contrast_ratio(style.mark_ink, fill);
                    assert!(
                        contrast >= MIN_SECONDARY_CONTRAST,
                        "{}: {state:?} at {contrast}:1",
                        theme.name
                    );
                }
            }
        }
    }

    /// The fill under a view a pane is showing is what parts its row from the
    /// rows of views no pane shows, so the two must not read as one surface.
    #[test]
    fn the_row_of_a_showing_view_stands_off_the_rows_around_it() {
        for theme in built_in_themes() {
            let colors = &theme.colors;
            let showing = row_style(ViewState::Visible, colors).fill;
            let rest = row_style(ViewState::Open, colors).fill;
            let step = level_step(showing, rest);
            assert!(step >= MIN_VISIBLE_STEP, "{}: {step} levels", theme.name);
        }
    }
}
