//! The jump palette: a fuzzy finder over the nodes of the document's graph
//! that moves the view holding it onto the one picked. Finding never changes
//! what is drawn; the filter is what does.

pub(crate) mod graph;
mod index;

use crate::Palette;
use crate::chips::{self, ChipBar, Drawn};
use crate::fuzzy::{self, Matching, Row, Standing, TEXT_SIZE};
use crate::hidden::HiddenBy;
use crate::holder::{HOLDERS, Holder};
use crate::model::{GraphModel, Prepared, Ready, Shared, SourceReplaced};
use crate::point::kind_names;
use bviz_graph_layout::Id;
use bviz_graph_layout::select::labels::{carried_label, package_label};
use bviz_ui_components::LabelLine;
use bviz_ui_components::gesture::{
    Button as PointerButton, Entry, GestureRow, Gestures, Input, Place, Pointer,
};
use bviz_ui_components::text_input::{self, TextEdited};
use bviz_ui_components::{
    KeyScope, ListScroll, Nesting, TextInput, TextTooltip, TruncatingLabel, key_text_within,
    list_scrollbar, menu_chrome, overlay_canvas,
};
use gpui::{
    Action, App, Bounds, ClickEvent, Context, Div, Entity, EventEmitter, FocusHandle, Focusable,
    Hsla, KeyContext, MouseButton, MouseDownEvent, Pixels, Rems, Render, ScrollStrategy,
    SharedString, Stateful, Subscription, TextStyleRefinement, Window, actions, div, prelude::*,
    px, relative, rems, uniform_list,
};
use index::Candidate;
#[cfg(test)]
pub(crate) use index::tests::shared as fixture;
pub(crate) use index::{Candidates, Scope, container, labelled, node_text, package_of, shown};
use rustc_hash::FxHashSet;

actions!(
    bviz_graph_jump,
    [
        /// Open jump palette
        Open,
        /// Preview previous match
        Previous,
        /// Preview next match
        Next,
        /// Jump to match
        Go,
        /// Show why match is in build
        AskWhy,
        /// Always show match
        AlwaysShow,
        /// Add match as query chip
        AddChip,
        /// Close palette and go back
        Close,
        /// Search next scope
        NextScope,
        /// Search previous scope
        PreviousScope,
    ]
);

/// The key context the palette carries, scoping the keys that move through
/// and act on its rows.
pub(crate) const KEY_CONTEXT: KeyScope =
    KeyScope::within_each(&HOLDERS, Nesting::Inside, "GraphJump", "Jump palette");

/// What the palette does for a gesture in [`GESTURES`].
#[derive(Clone, Copy)]
enum JumpIntent {
    JumpTo,
}

const GESTURES: Gestures<JumpIntent> = Gestures::new(&[Entry::new(
    Input::click(PointerButton::Left),
    Place::On("a palette row"),
    "Jump to match",
    JumpIntent::JumpTo,
)]);

/// The gestures the palette answers to.
pub(crate) fn gestures() -> impl Iterator<Item = GestureRow> {
    GESTURES.rows()
}

const WIDTH: Pixels = px(560.);

const PLACEHOLDER: &str = "jump to…";

/// The size the scope's name is set in, which its slot is measured at.
const SCOPE_TEXT_SIZE: Rems = rems(0.75);

/// The most rows the list stands tall, few enough to read at a glance; the
/// rest of the matches scroll through it.
const VISIBLE_ROWS: usize = 12;

/// How much more readily what holds a row's node gives up width than the
/// row's own text, enough that it is cut away before the text is cut at all.
const CONTAINER_SHRINK: f32 = 1000.;

/// Where a node stands in the view holding a palette, as its row tells.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Stands {
    Here,
    Hidden(HiddenBy),
    /// Nothing the view draws stands for the node, whatever the filter.
    Absent,
}

/// Reads where a node stands in the view holding a palette.
pub(crate) type StandsIn = Box<dyn Fn(Id, &App) -> Stands>;

/// What a palette asks of the view holding it, for the node a row names.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum JumpEvent {
    /// The keys aimed at the row: show the node without moving onto it for
    /// good.
    Preview(Id),
    Go(Id),
    AskWhy(Id),
    AlwaysShow(Id),
    /// The palette closed with no row picked: return the view to where it
    /// stood as the palette opened.
    Closed,
    /// The palette closed having acted on a row elsewhere, as adding it to
    /// the query bar does: put away the preview, leaving the view where it
    /// stands.
    Dismissed,
}

/// The jump palette a view of the graph document stands over its drawing.
pub(crate) struct JumpPalette {
    model: Entity<GraphModel>,
    holder: Holder,
    /// The view holding the palette, which closing it gives the focus back
    /// to.
    host: FocusHandle,
    /// The query bar of the view holding the palette, which a row is added
    /// to as a chip.
    chips: Entity<ChipBar>,
    /// The nodes the holder draws, which the rows rank first.
    drawn: Option<Drawn>,
    standing: StandsIn,
    open: Option<Jump>,
    /// How far the open palette's rows have scrolled.
    scroll: ListScroll,
    _source_replaced: Subscription,
}

/// The palette while it is open over the drawing.
struct Jump {
    field: Entity<TextInput>,
    _edits: Subscription,
    scope: Scope,
    /// Matches the field's query against the indices of the candidates
    /// `scope` offers.
    matching: Matching<Ready<Candidates>>,
    candidates: Prepared<Candidates>,
    /// The row the keys are on, which is previewed once a key moved it.
    aimed: usize,
    /// Where the palette was last painted, the band of the view a preview
    /// keeps the node it shows clear of.
    painted: Option<Bounds<Pixels>>,
}

impl EventEmitter<JumpEvent> for JumpPalette {}

impl JumpPalette {
    /// A palette over `model` in the view `holder` that `host` focuses, adding
    /// chips to `chips`, ranking first the nodes `drawn` reads and badging each
    /// row as `standing` reads it.
    pub(crate) fn new(
        model: Entity<GraphModel>,
        holder: Holder,
        host: FocusHandle,
        chips: Entity<ChipBar>,
        drawn: Option<Drawn>,
        standing: StandsIn,
        cx: &mut Context<Self>,
    ) -> Self {
        let source_replaced = cx.subscribe(&model, |view, _, _: &SourceReplaced, cx| {
            _ = view.dismiss(cx)
        });
        JumpPalette {
            model,
            holder,
            host,
            chips,
            drawn,
            standing,
            open: None,
            scroll: ListScroll::new(),
            _source_replaced: source_replaced,
        }
    }

    pub(crate) fn is_open(&self) -> bool {
        self.open.is_some()
    }

    /// Where the open palette was last painted.
    pub(crate) fn painted(&self) -> Option<Bounds<Pixels>> {
        self.open.as_ref()?.painted
    }

    /// Opens the palette offering what `scope` offers, or takes the focus
    /// back to it where it is open already; reports whether it opened. A
    /// graph still being read offers nothing to find.
    pub(crate) fn open(
        &mut self,
        scope: Scope,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let opened = self.open.is_none();
        if opened {
            let Some(candidates) = self.model.read(cx).candidates() else {
                return false;
            };
            let field = cx.new(|cx| TextInput::new(cx, PLACEHOLDER, 0));
            let edits = cx.subscribe(&field, |view, _, _: &TextEdited, cx| view.edited(cx));
            self.open = Some(Jump {
                field,
                _edits: edits,
                scope,
                matching: self.model.update(cx, |model, cx| model.take_matcher(cx)),
                candidates,
                aimed: 0,
                painted: None,
            });
            self.scroll.to_top();
            self.inject(cx);
        }
        if let Some(jump) = &self.open {
            let field = jump.field.read(cx).focus_handle(cx);
            field.focus(window, cx);
        }
        cx.notify();
        opened
    }

    /// Closes the palette, leaving the view where it stands; reports whether
    /// it was open.
    pub(crate) fn dismiss(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(jump) = self.open.take() else {
            return false;
        };
        (self.model).update(cx, |model, _| model.park_matcher(jump.matching));
        cx.notify();
        true
    }

    /// Hands the matcher the candidates the scope offers, in place of any it
    /// held, off the UI thread.
    fn inject(&mut self, cx: &mut Context<Self>) {
        let Some(jump) = self.open.as_ref() else {
            return;
        };
        let (candidates, scope) = (jump.candidates.clone(), jump.scope);
        let prepare = move || candidates.ready();
        let feed = move |candidates: &Ready<Candidates>, feeder: &fuzzy::Feeder| {
            for index in candidates.in_scope(scope) {
                if !feeder.push(index, &candidates.get(index).text) {
                    return;
                }
            }
        };
        fuzzy::inject(self, matching, take_up, prepare, feed, cx);
    }

    /// Hands the matcher the query the field holds.
    fn edited(&mut self, cx: &mut Context<Self>) {
        let Some(jump) = self.open.as_mut() else {
            return;
        };
        let query = jump.field.read(cx).content();
        if !jump.matching.set_query(query) {
            return;
        }
        jump.aimed = 0;
        self.scroll.to_top();
        fuzzy::poll(self, matching, take_up, cx);
    }

    /// Moves to the next scope, or with `back` the previous one, and offers
    /// its candidates under the same query.
    fn cycle_scope(&mut self, back: bool, cx: &mut Context<Self>) {
        let Some(jump) = self.open.as_mut() else {
            return;
        };
        jump.scope = match back {
            true => jump.scope.previous(),
            false => jump.scope.next(),
        };
        jump.aimed = 0;
        self.scroll.to_top();
        self.inject(cx);
        cx.notify();
    }

    /// Moves the aim `step` rows and previews the row it lands on.
    fn aim(&mut self, step: isize, cx: &mut Context<Self>) {
        let Some(jump) = self.open.as_mut() else {
            return;
        };
        let rows = jump.matching.rows().len();
        if rows == 0 {
            return;
        }
        jump.aimed = stepped(jump.aimed, step, rows);
        bring_into_view(&self.scroll, jump.aimed);
        if let Some(id) = aimed(jump) {
            cx.emit(JumpEvent::Preview(id));
        }
        cx.notify();
    }

    /// Closes the palette over the aimed row's node, asking the view holding
    /// it for what `event` makes of that node.
    fn act(&mut self, event: fn(Id) -> JumpEvent, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = self.open.as_ref().and_then(aimed) {
            self.act_on(event(id), window, cx);
        }
    }

    /// Closes the palette, gives the focus back to the view holding it, and
    /// asks that view for `event`.
    fn act_on(&mut self, event: JumpEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.dismiss(cx);
        self.host.focus(window, cx);
        cx.emit(event);
    }

    /// Adds the aimed row's node to the holder's query bar as a chip, closing
    /// the palette.
    fn add_chip(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.open.as_ref().and_then(aimed) else {
            return;
        };
        let loaded = self.model.read(cx).loaded();
        let Some(literal) = loaded.and_then(|loaded| literal(&loaded.shared, id)) else {
            return;
        };
        self.act_on(JumpEvent::Dismissed, window, cx);
        self.chips
            .update(cx, |bar, cx| chips::push(bar, &literal, cx));
    }

    /// Whether asking why a row's node is in the build has a graph panel to
    /// draw the answer: the holder, or one the document defers to.
    fn answers_why(&self, cx: &App) -> bool {
        self.holder == Holder::Graph || self.model.read(cx).reveals()
    }

    /// The lines under the rows: how many nodes the query matches, and the
    /// keys.
    fn footer(&self, jump: &Jump, muted: Hsla, cx: &App) -> Div {
        let matching = &jump.matching;
        let listed = matching.rows().len();
        let tally = match (matching.query().is_empty(), matching.matched()) {
            (true, _) => None,
            (false, 0) if matching.streaming() => Some("matching…".to_owned()),
            (false, 0) => Some("no matches".to_owned()),
            (false, 1) => Some("1 match".to_owned()),
            (false, matched) if (listed as u32) < matched => {
                Some(format!("{matched} matches, best {listed} listed"))
            }
            (false, matched) => Some(format!("{matched} matches")),
        };
        // Read within the stack the field is drawn in, which a palette just
        // opened has drawn in no frame yet.
        let field = field_stack(self.holder);
        let answers_why = self.answers_why(cx);
        let line = |hints: &[Hint]| {
            let asks_why = |action: &&dyn Action| action.name() == AskWhy.name();
            let offered = |(actions, _): &&Hint| answers_why || !actions.iter().any(asks_why);
            let hints = hints.iter().filter(offered).filter_map(|(actions, does)| {
                let keys: Option<Vec<_>> = actions
                    .iter()
                    .map(|action| key_text_within(*action, &field, cx))
                    .collect();
                Some(
                    div()
                        .flex()
                        .flex_none()
                        .whitespace_nowrap()
                        .child(format!("{} {does}", keys?.join("/")))
                        .child(
                            div()
                                .w(HINT_SEPARATOR_WIDTH)
                                .flex()
                                .justify_center()
                                .child("·"),
                        ),
                )
            });
            // The hints wrap between one another, and each line of them ends
            // past the clipped edge by one separator, so no line shows a
            // trailing one.
            div().min_w_0().overflow_hidden().flex().flex_col().child(
                div()
                    .mr(-HINT_SEPARATOR_WIDTH)
                    .flex()
                    .flex_wrap()
                    .justify_end()
                    .children(hints),
            )
        };
        let [first, second] = KEY_HINTS.map(line);
        div()
            .flex()
            .flex_col()
            .px(menu_chrome::ROW_INSET)
            .pt_1()
            .text_xs()
            .text_color(muted)
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .children(tally.map(|tally| TruncatingLabel::new("jump-tally", tally))),
                    )
                    .child(first),
            )
            .child(second)
    }
}

/// The open palette's matching.
fn matching(view: &mut JumpPalette) -> Option<&mut Matching<Ready<Candidates>>> {
    Some(&mut view.open.as_mut()?.matching)
}

/// Takes up what the matcher has found since the last poll, keeping the aim
/// on the row it was on.
fn take_up(view: &mut JumpPalette, cx: &mut Context<JumpPalette>) {
    let drawn = view
        .drawn
        .as_ref()
        .map_or_else(Default::default, |drawn| drawn(cx));
    let Some(jump) = view.open.as_mut() else {
        return;
    };
    let was = jump.aimed;
    let aimed = jump.matching.rows().get(was).map(|row| row.candidate);
    rerank(jump, &drawn);
    let rows = jump.matching.rows();
    jump.aimed = aimed
        .and_then(|aimed| rows.iter().position(|row| row.candidate == aimed))
        .unwrap_or(0);
    // The list follows the aim only when the aimed row moves, so a reader
    // scrolling it while matches stream in keeps their place.
    if jump.aimed != was {
        bring_into_view(&view.scroll, jump.aimed);
    }
    cx.notify();
}

/// Lists the rows the matcher's latest result ranks first.
fn rerank(jump: &mut Jump, drawn: &FxHashSet<Id>) {
    jump.matching.rerank(|candidates, index| {
        let candidate = candidates.get(index);
        Standing {
            path: candidate.path.clone(),
            drawn: drawn.contains(&candidate.id),
            weight: candidate.rdeps,
        }
    });
}

/// The row `step` rows on from `aimed` among `rows` rows, held to the list.
pub(crate) fn stepped(aimed: usize, step: isize, rows: usize) -> usize {
    aimed
        .saturating_add_signed(step)
        .min(rows.saturating_sub(1))
}

/// Scrolls the list just far enough to show the row `aimed`.
pub(crate) fn bring_into_view(scroll: &ListScroll, aimed: usize) {
    scroll
        .uniform_handle()
        .scroll_to_item(aimed, ScrollStrategy::Nearest);
}

/// The node of the aimed row.
fn aimed(jump: &Jump) -> Option<Id> {
    let (candidates, row) = jump.matching.row(jump.aimed)?;
    Some(candidates.get(row.candidate).id)
}

/// A quoted target pattern naming `id`: the label of the target it belongs
/// to, or every target of the package a package box stands for.
fn literal(shared: &Shared, id: Id) -> Option<String> {
    let pattern = match carried_label(&shared.names, &shared.facets, id) {
        Some(label) => shared.labels.written(label).into_owned(),
        None => format!("{}:*", package_label(&shared.names, id)?),
    };
    let quote = if pattern.contains('"') { '\'' } else { '"' };
    Some(format!("{quote}{pattern}{quote}"))
}

impl Render for JumpPalette {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(jump) = self.open.as_ref() else {
            return div();
        };
        let palette = Palette::from_theme(&bviz_ui_theme::theme(window, cx));
        let colors = &palette.colors;
        let text = TextStyleRefinement {
            font_size: Some(TEXT_SIZE.into()),
            ..TextStyleRefinement::default()
        };
        let row = window.with_text_style(Some(text), |window| menu_chrome::row_height(window));
        let scope_text = TextStyleRefinement {
            font_size: Some(SCOPE_TEXT_SIZE.into()),
            ..TextStyleRefinement::default()
        };
        let scope_width = window.with_text_style(Some(scope_text), |window| {
            let labels = Scope::CYCLE.iter().map(|scope| scope.label().into());
            menu_chrome::widest(window, labels)
        });
        let entity = cx.entity();
        div()
            .absolute()
            .top_2()
            .left_0()
            .right_0()
            .flex()
            .justify_center()
            .child(
                menu_chrome::surface(div(), colors)
                    .key_context(KEY_CONTEXT)
                    .on_action(cx.listener(|view, _: &Previous, _, cx| view.aim(-1, cx)))
                    .on_action(cx.listener(|view, _: &Next, _, cx| view.aim(1, cx)))
                    .on_action(
                        cx.listener(|view, _: &Go, window, cx| view.act(JumpEvent::Go, window, cx)),
                    )
                    .on_action(cx.listener(|view, _: &AskWhy, window, cx| {
                        if view.answers_why(cx) {
                            view.act(JumpEvent::AskWhy, window, cx)
                        }
                    }))
                    .on_action(cx.listener(|view, _: &AlwaysShow, window, cx| {
                        view.act(JumpEvent::AlwaysShow, window, cx)
                    }))
                    .on_action(
                        cx.listener(|view, _: &AddChip, window, cx| view.add_chip(window, cx)),
                    )
                    .on_action(cx.listener(|view, _: &Close, window, cx| {
                        view.act_on(JumpEvent::Closed, window, cx)
                    }))
                    .on_action(
                        cx.listener(|view, _: &NextScope, _, cx| view.cycle_scope(false, cx)),
                    )
                    .on_action(
                        cx.listener(|view, _: &PreviousScope, _, cx| view.cycle_scope(true, cx)),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|view, e: &MouseDownEvent, _, cx| {
                            view.scroll.press(e.position);
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    )
                    .occlude()
                    .w(WIDTH)
                    .max_w(relative(0.9))
                    .flex()
                    .flex_col()
                    .text_size(TEXT_SIZE)
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .items_center()
                            .gap_2()
                            .px(menu_chrome::ROW_INSET)
                            .pb_1()
                            .child(div().flex_1().child(jump.field.clone()))
                            .child(
                                div()
                                    .flex_none()
                                    .w(scope_width)
                                    .flex()
                                    .justify_end()
                                    .text_size(SCOPE_TEXT_SIZE)
                                    .text_color(colors.text_muted)
                                    .child(jump.scope.label()),
                            ),
                    )
                    .children(
                        (!jump.matching.rows().is_empty())
                            .then(|| rows(jump, &self.scroll, row, &palette, cx)),
                    )
                    .child(self.footer(jump, colors.text_muted, cx))
                    .child(overlay_canvas(move |bounds, _, cx| {
                        entity.update(cx, |view, _| {
                            if let Some(jump) = view.open.as_mut() {
                                jump.painted = Some(bounds);
                            }
                        });
                    })),
            )
    }
}

/// The list of the rows, `row` tall each and [`VISIBLE_ROWS`] of them in
/// view, drawing only those in view and scrolling under a bar.
fn rows(
    jump: &Jump,
    scroll: &ListScroll,
    row: Pixels,
    palette: &Palette,
    cx: &Context<JumpPalette>,
) -> impl IntoElement + use<> {
    let entity = cx.entity();
    let listing = *palette;
    let count = jump.matching.rows().len();
    let listed = uniform_list("jump-rows", count, move |range, _, cx| {
        let cx: &App = cx;
        let view = entity.read(cx);
        let (Some(jump), Some(loaded)) = (view.open.as_ref(), view.model.read(cx).loaded()) else {
            return Vec::new();
        };
        let reveals = view.model.read(cx).reveals();
        range
            .filter_map(|index| {
                let (candidates, row) = jump.matching.row(index)?;
                let candidate = candidates.get(row.candidate);
                let shown = Shown {
                    aimed: jump.aimed == index,
                    stands: (view.standing)(candidate.id, cx),
                    shows_in_graph: reveals,
                };
                Some(listed(
                    &loaded.shared,
                    jump,
                    (candidate, row),
                    index,
                    shown,
                    &listing,
                    &entity,
                ))
            })
            .collect()
    })
    .h(row * count.min(VISIBLE_ROWS) as f32)
    .w_full()
    .track_scroll(scroll.uniform_handle());
    div()
        .relative()
        .flex()
        .flex_col()
        .child(listed)
        .child(list_scrollbar(
            cx.entity(),
            |view: &JumpPalette| &view.scroll,
            |view: &mut JumpPalette| &mut view.scroll,
            palette.scroll,
        ))
}

/// The width a separator between two of the footer's hints takes.
const HINT_SEPARATOR_WIDTH: Rems = rems(0.75);

/// The key contexts from the view's in to the palette field's text.
fn field_stack(holder: Holder) -> [KeyContext; 3] {
    [
        holder.scope().into(),
        KEY_CONTEXT.into(),
        text_input::KEY_CONTEXT.into(),
    ]
}

/// A hint of the palette's footer: the keys it names, ahead of a word for what
/// they do.
type Hint = (&'static [&'static dyn Action], &'static str);

/// The keys the palette's footer names, a line of hints each.
const KEY_HINTS: [&[Hint]; 2] = [
    &[
        (&[&Previous, &Next], "preview"),
        (&[&NextScope], "scope"),
        (&[&Close], "back"),
    ],
    &[
        (&[&Go], "jump"),
        (&[&AlwaysShow], "always show"),
        (&[&AskWhy], "why"),
        (&[&AddChip], "add chip"),
    ],
];

/// How a row is drawn beside what it names: whether the keys are on it, and
/// where its node stands in the view.
#[derive(Clone, Copy)]
struct Shown {
    aimed: bool,
    stands: Stands,
    /// Whether a graph panel is open to show a node the view draws nothing
    /// for.
    shows_in_graph: bool,
}

/// One row of the open palette `jump`, at `index`, offering `candidate`: the
/// node's text with the characters the query matched picked out, what holds
/// it where the text leaves that out, and its badges.
fn listed(
    shared: &Shared,
    jump: &Jump,
    (candidate, row): (&Candidate, &Row),
    index: usize,
    shown: Shown,
    palette: &Palette,
    entity: &Entity<JumpPalette>,
) -> Stateful<Div> {
    let colors = &palette.colors;
    let tip = colors.tooltip();
    let id = candidate.id;
    let entity = entity.clone();
    menu_chrome::clear_of_scrollbar(menu_chrome::aimed_row(
        ("jump-row", index).into(),
        colors,
        shown.aimed,
    ))
    .w_full()
    .on_click(move |event: &ClickEvent, window, cx| {
        GESTURES.dispatch(Pointer::click(event), |intent| match intent {
            JumpIntent::JumpTo => {
                entity.update(cx, |view, cx| view.act_on(JumpEvent::Go(id), window, cx));
                true
            }
        });
    })
    .child(div().flex_grow_1().min_w_0().child({
        let (reading, parts) = index::reading(shared, candidate);
        let matched = jump.matching.picked(&reading, row);
        let picked = fuzzy::matched_style(palette.highlight.into());
        LabelLine::parted(reading.into_text(), parts, colors.text_muted)
            .picked(matched, picked)
            .hinted("jump-text", candidate.text.to_string())
    }))
    .children(
        index::container(shared, candidate.id, candidate.kind).map(|written| {
            let shown = index::shown(shared, &written);
            div()
                .flex_shrink(CONTAINER_SHRINK)
                .min_w_0()
                .text_xs()
                .text_color(colors.text_muted)
                .child(LabelLine::new(shown, colors.text_muted).hinted("jump-container", written))
        }),
    )
    .child(badge(kind_names(candidate.kind).one, colors.text_muted))
    .children(
        index::configuration(shared, candidate)
            .map(|configuration| badge(configuration, colors.text_muted)),
    )
    .children(match shown.stands {
        Stands::Here => None,
        Stands::Hidden(by) => Some(
            badge("hidden", colors.warning)
                .id(("jump-hidden", index))
                .tooltip(TextTooltip::builder(by.describe().into(), tip)),
        ),
        Stands::Absent => Some(
            badge(NOT_IN_VIEW, colors.text_muted)
                .id(("jump-absent", index))
                .tooltip(TextTooltip::builder(
                    absent_tip(shown.shows_in_graph).into(),
                    tip,
                )),
        ),
    })
}

/// The badge of a row whose node the view draws nothing for.
const NOT_IN_VIEW: &str = "not in this view";

/// What the badge of a row whose node the view draws nothing for says,
/// offering the graph panel where one is open to show the node.
fn absent_tip(shows_in_graph: bool) -> &'static str {
    match shows_in_graph {
        true => "Nothing in this view stands for it; Enter offers to show it in the graph",
        false => "Nothing in this view stands for it",
    }
}

fn badge(text: impl Into<SharedString>, color: Hsla) -> Div {
    div()
        .flex_none()
        .px_1()
        .rounded_sm()
        .border_1()
        .border_color(color)
        .text_xs()
        .text_color(color)
        .child(text.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bviz_ui_components::binding_within;
    use gpui::Keymap;

    /// Every key the footer names is bound where the palette's field stands
    /// in every view holding one, so no hint drops out of it.
    #[test]
    fn every_footer_hint_names_keys() {
        let mut bindings = crate::bindings(bviz_ui_components::Host::Desktop);
        bindings.extend(text_input::bindings());
        let keymap = Keymap::new(bindings);
        for holder in Holder::ALL {
            for (actions, does) in KEY_HINTS.into_iter().flatten() {
                for action in *actions {
                    assert!(
                        binding_within(&keymap, *action, &field_stack(holder)).is_some(),
                        "{holder:?}: {does}"
                    );
                }
            }
        }
    }

    #[test]
    fn every_gesture_is_reachable_and_fits_the_help() {
        assert_eq!(GESTURES.unreachable(), []);
        assert_eq!(GESTURES.overlong(), []);
    }

    /// A node is added as the target it belongs to, and a package box as
    /// every target of its package.
    #[test]
    fn a_node_is_added_as_a_quoted_pattern_naming_its_target() {
        let shared = index::tests::shared();
        let literal = |name: &str| literal(&shared, shared.names.get(name).unwrap());
        assert_eq!(
            literal("target://app:lib").as_deref(),
            Some("\"//app:lib\"")
        );
        assert_eq!(
            literal("action:bazel-out/app/liblib.a").as_deref(),
            Some("\"//app:lib\"")
        );
        assert_eq!(literal("pkg:app").as_deref(), Some("\"//app:*\""));
    }

    /// The aim reaches every listed row, past those in view, and stops at
    /// either end of the list.
    #[test]
    fn the_aim_walks_every_row_and_stops_at_the_ends() {
        let rows = VISIBLE_ROWS * 3;
        let mut aimed = 0;
        for _ in 0..rows * 2 {
            aimed = stepped(aimed, 1, rows);
        }
        assert_eq!(aimed, rows - 1);
        assert_eq!(stepped(aimed, -2, rows), rows - 3);
        assert_eq!(stepped(1, -5, rows), 0);
        assert_eq!(stepped(0, 1, 0), 0);
    }
}
