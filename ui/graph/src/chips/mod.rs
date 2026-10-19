//! The query bar: chips written in Bazel's query language, stacked into the
//! scope the document's views draw within.
//!
//! What the stack and the chip being typed name is counted off the UI thread
//! as they change; the filter takes the stack up only once it is applied,
//! since restricting the drawing to a scope costs a whole relayout.

mod complete;
mod pill;
mod reorder;

pub(crate) use complete::Completions;

use crate::Palette;
use crate::filter::DocumentFilter;
use crate::holder::{HOLDERS, Holder};
use crate::model::{
    FilterChanged, GraphModel, QueryChanged, QueryCounted, QueryRefused, SourceReplaced,
};
use bviz_graph_layout::query_lang::{ChipId, Command, Evaluation, Stack};
use bviz_graph_layout::{Id, Input};
use bviz_ui_components::gesture::{
    self, Button as PointerButton, Entry, GestureRow, Gestures, Place, Pointer,
};
use bviz_ui_components::{
    Button, ByRoom, KeyScope, Menu, MenuEvent, MenuItem, Nesting, QueryField, QueryFieldEvent,
    TextTooltip, load_in_background, menu_chrome, over_window, text_input,
};
use gpui::{
    Action, AnyElement, App, ClickEvent, ClipboardItem, Context, Div, Entity, FocusHandle,
    Focusable, KeyBinding, KeyContext, MouseButton, MouseDownEvent, Pixels, Point, Render,
    ScrollHandle, SharedString, Stateful, Subscription, Task, Window, actions, div, point,
    prelude::*, px,
};
use rustc_hash::FxHashSet;
use std::sync::Arc;

actions!(
    bviz_graph_chips,
    [
        /// Focus query field
        Focus,
        /// Add text as chip
        Commit,
        /// Apply query
        Apply,
        /// Leave query bar
        Leave,
        /// Select previous chip
        SelectPrevious,
        /// Select next chip
        SelectNext,
        /// Enable or disable chip
        Toggle,
        /// Negate chip
        Negate,
        /// Move chip back
        MoveBack,
        /// Move chip forward
        MoveForward,
        /// Remove chip
        Remove,
        /// Edit chip
        Edit,
        /// Copy chip
        CopyChip,
        /// Open chip menu
        OpenChipMenu,
        /// Copy as Bazel query
        CopyStack,
        /// Clear query
        ClearQuery,
        /// Revert query
        RevertQuery,
        /// Aim at previous completion
        CompletePrevious,
        /// Aim at next completion
        CompleteNext,
        /// Accept completion
        AcceptCompletion,
        /// Close completions
        CloseCompletion,
    ]
);

/// The key context the bar carries, scoping the keys that act on its chips.
pub(crate) const KEY_CONTEXT: KeyScope =
    KeyScope::within_each(&HOLDERS, Nesting::Inside, "GraphChips", "Query bar");

/// The key context the bar's field carries.
pub(crate) const FIELD_KEY_CONTEXT: KeyScope = KeyScope::within(
    &KEY_CONTEXT,
    Nesting::Inside,
    "GraphChipField",
    "Query field",
);

/// The key context the field carries while the list completing its word
/// stands open, scoping the keys acting on the list.
pub(crate) const COMPLETION_KEY_CONTEXT: KeyScope = KeyScope::within(
    &FIELD_KEY_CONTEXT,
    Nesting::Inside,
    "GraphChipCompletion",
    "Query completions",
);

/// The state of the completion's list while the arrow keys have moved onto a
/// row, which Enter then accepts.
pub(crate) const AIMED_KEY_CONTEXT: KeyScope = KeyScope::within(
    &COMPLETION_KEY_CONTEXT,
    Nesting::State,
    "GraphChipCompletionAimed",
    "Aimed completion",
);

/// Width the field is drawn at, wide enough for a call over a package.
const FIELD_WIDTH: Pixels = px(200.);

/// The widest the bar's notice stands before it wraps.
const NOTICE_WIDTH: Pixels = px(480.);

const PLACEHOLDER: &str = "query: //foo/... or deps()";

/// The bar's keys.
pub(crate) fn bindings() -> Vec<KeyBinding> {
    let field = Some(FIELD_KEY_CONTEXT.id());
    let on_chips = format!("{KEY_CONTEXT} && !{FIELD_KEY_CONTEXT}");
    let on_chips = Some(on_chips.as_str());
    let completing = Some(COMPLETION_KEY_CONTEXT.id());
    let aimed = Some(AIMED_KEY_CONTEXT.id());
    // Outside the text fields of any view holding a bar, or anywhere in the
    // bar, its own field included: one predicate, which the keys help lists
    // once.
    let outside_fields = HOLDERS.map(text_input::unless_typing).join(" || ");
    let holders = Some(outside_fields.as_str());
    let holder_or_bar = format!("{outside_fields} || {KEY_CONTEXT}");
    let holder_or_bar = Some(holder_or_bar.as_str());
    vec![
        KeyBinding::new("up", CompletePrevious, completing),
        KeyBinding::new("down", CompleteNext, completing),
        KeyBinding::new("tab", AcceptCompletion, completing),
        KeyBinding::new("enter", AcceptCompletion, aimed),
        KeyBinding::new("escape", CloseCompletion, completing),
        KeyBinding::new("q", Focus, holders),
        KeyBinding::new("secondary-enter", Apply, holder_or_bar),
        KeyBinding::new("enter", Commit, field),
        KeyBinding::new("escape", Leave, field),
        KeyBinding::new("left", SelectPrevious, on_chips),
        KeyBinding::new("right", SelectNext, on_chips),
        KeyBinding::new("space", Toggle, on_chips),
        KeyBinding::new("!", Negate, on_chips),
        KeyBinding::new("alt-up", MoveBack, on_chips),
        KeyBinding::new("alt-down", MoveForward, on_chips),
        KeyBinding::new("delete", Remove, on_chips),
        KeyBinding::new("backspace", Remove, on_chips),
        KeyBinding::new("enter", Edit, on_chips),
        KeyBinding::new("escape", Leave, on_chips),
        KeyBinding::new("secondary-c", CopyChip, on_chips),
        KeyBinding::new("shift-f10", OpenChipMenu, on_chips),
        KeyBinding::new("menu", OpenChipMenu, on_chips),
        KeyBinding::new("secondary-shift-c", CopyStack, holder_or_bar),
        KeyBinding::new(
            "secondary-shift-backspace",
            ClearQuery,
            Some(KEY_CONTEXT.id()),
        ),
        KeyBinding::new("secondary-z", RevertQuery, Some(KEY_CONTEXT.id())),
    ]
}

/// The query bar a view of the graph document stands in its toolbar. The
/// chips it stages are the document's [`GraphModel::query`], which every bar
/// over the document shows and edits alike; what is typed, selected and
/// scrolled is this bar's own.
pub(crate) struct ChipBar {
    model: Entity<GraphModel>,
    holder: Holder,
    /// The view holding the bar, which leaving the bar gives the focus back
    /// to.
    host: FocusHandle,
    /// The nodes the holder draws, which completion ranks first.
    drawn: Option<Drawn>,
    field: Entity<QueryField>,
    /// Held by the bar while a chip is selected, so that the keys acting on
    /// chips reach the bar.
    focus_handle: FocusHandle,
    selected: Option<ChipId>,
    /// The chip the field holds the text of, which committing the field
    /// rewrites.
    editing: Option<Editing>,
    /// Counts each change to the stack or the field, so that an evaluation of
    /// a stack since changed lands nothing.
    generation: u64,
    landed: Option<Landed>,
    /// Whether the stack, with the field's chip in place, names other than
    /// what was applied; restated on every change to either.
    staged: bool,
    /// What the bar last has to say of an action taken on it, until the stack
    /// or the field changes.
    notice: Option<SharedString>,
    /// How much of the holder's view the document's filter shows, as the
    /// holder last counted it.
    shown: Option<Shown>,
    /// Why the holder marks nothing for the node the document holds
    /// selected.
    unmarked: Option<Unmarked>,
    /// Scrolls the row of chips.
    scroll: ScrollHandle,
    /// Where a press on a pill stands, which carried far enough drags the
    /// chip.
    gesture: reorder::Gesture,
    completer: complete::Completer,
    menu: Option<OpenMenu>,
    _evaluating: Task<()>,
    /// Counts what the field's chip names once typing settles.
    _typed: Subscription,
    /// Counts the stack afresh as any bar over the document edits it.
    _staged: Subscription,
    /// Shows the stack as the document counted it.
    _counted: Subscription,
    /// Says why a query this bar asked to be applied was not.
    _refused: Subscription,
    /// Restates whether the stack is staged as another one is applied.
    _applied: Subscription,
    /// Counts the stack afresh over the graph the document has read.
    _source_replaced: Subscription,
}

/// The nodes a bar's holder draws, read as the completion ranks its rows.
pub(crate) type Drawn = Box<dyn Fn(&App) -> Arc<FxHashSet<Id>>>;

/// How many of the points a view draws in its own unit the document's
/// filter shows, each counted as [`crate::filter::Shown`] counts a node.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Shown {
    pub(crate) shown: usize,
    pub(crate) of: usize,
    /// What the view calls its points, in the plural.
    pub(crate) many: &'static str,
    /// What the view says of how it draws what is shown.
    pub(crate) note: Option<&'static str>,
}

/// Why a view marks nothing for the node its document holds selected.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Unmarked {
    /// The view's filter, or how it draws through it, hides the node.
    Hidden,
    /// Nothing the view draws stands for the node.
    Absent,
    /// The view draws the node, but not from where it stands.
    Outside,
}

impl Unmarked {
    fn describe(self) -> &'static str {
        match self {
            Unmarked::Hidden => "selection hidden",
            Unmarked::Absent => "selection not in this view",
            Unmarked::Outside => "selection outside this focus",
        }
    }
}

/// A chip the field holds the text of, as that text stood when the edit
/// began. Another bar removing the chip or rewriting its text ends the edit,
/// so that committing the field never overwrites an edit made elsewhere: the
/// field's text stays, as the draft of a chip of its own.
struct Editing {
    chip: ChipId,
    from: String,
}

impl Editing {
    /// Whether `stack` still holds the chip edited, its text untouched.
    fn survives(&self, stack: &Stack) -> bool {
        (stack.chip(self.chip)).is_some_and(|chip| chip.text() == self.from)
    }
}

/// What evaluating the stack with the field's chip in place came to.
struct Landed {
    generation: u64,
    /// The chip the field held, where it held text and edited none.
    draft: Option<ChipId>,
    evaluation: Evaluation,
}

struct OpenMenu {
    menu: Entity<Menu>,
    _dismissed: Subscription,
}

impl ChipBar {
    /// A bar over `model` in the view `holder` that `host` focuses, whose
    /// completion ranks first the nodes `drawn` reads.
    pub(crate) fn new(
        model: Entity<GraphModel>,
        holder: Holder,
        host: FocusHandle,
        drawn: Option<Drawn>,
        cx: &mut Context<Self>,
    ) -> Self {
        let field = cx.new(|cx| QueryField::new("chips", PLACEHOLDER, FIELD_WIDTH, cx));
        let typed = cx.subscribe(&field, |view, _, event: &QueryFieldEvent, cx| match event {
            QueryFieldEvent::CursorMoved => {
                complete::follow(view, cx);
                cx.notify();
            }
            QueryFieldEvent::Edited => {
                complete::follow(view, cx);
                restage(view, cx);
                cx.notify();
            }
            QueryFieldEvent::Settled => changed(view, cx),
        });
        let staged = cx.subscribe(&model, |view, _, _: &QueryChanged, cx| restacked(view, cx));
        let counted = cx.subscribe(&model, |_, _, _: &QueryCounted, cx| cx.notify());
        let refused = cx.subscribe(&model, |view, _, refused: &QueryRefused, cx| {
            if refused.by == cx.entity_id() {
                view.notice = Some(refused.why.clone());
                cx.notify();
            }
        });
        let applied = cx.subscribe(&model, |view, _, _: &FilterChanged, cx| {
            restage(view, cx);
            cx.notify();
        });
        let source_replaced =
            cx.subscribe(&model, |view, _, _: &SourceReplaced, cx| reset(view, cx));
        let mut bar = ChipBar {
            model,
            holder,
            host,
            drawn,
            field,
            focus_handle: cx.focus_handle(),
            selected: None,
            editing: None,
            generation: 0,
            landed: None,
            staged: false,
            notice: None,
            shown: None,
            unmarked: None,
            scroll: ScrollHandle::new(),
            gesture: reorder::Gesture::Idle,
            completer: complete::Completer::default(),
            menu: None,
            _evaluating: Task::ready(()),
            _typed: typed,
            _staged: staged,
            _counted: counted,
            _refused: refused,
            _applied: applied,
            _source_replaced: source_replaced,
        };
        changed(&mut bar, cx);
        bar
    }

    /// Whether the field holds the text of the chip `id`.
    fn edits(&self, id: ChipId) -> bool {
        self.editing
            .as_ref()
            .is_some_and(|editing| editing.chip == id)
    }

    /// `stack` with the field's text in place: as the chip it edits, or as a
    /// chip on top where it edits none. Returns the chip added.
    fn with_field(&self, stack: &Stack, cx: &App) -> (Stack, Option<ChipId>) {
        let text = self.field.read(cx).text(cx);
        let mut stack = stack.clone();
        let text = text.trim();
        let draft = match self.editing.as_ref().map(|editing| editing.chip) {
            Some(id) => {
                if !text.is_empty() {
                    stack.set_text(id, text);
                }
                None
            }
            None => (!text.is_empty()).then(|| stack.push(text)),
        };
        (stack, draft)
    }

    /// Takes up how much of the holder's view the document's filter shows,
    /// which the bar's status line reads.
    pub(crate) fn set_shown(&mut self, shown: Option<Shown>, cx: &mut Context<Self>) {
        if self.shown != shown {
            self.shown = shown;
            cx.notify();
        }
    }

    /// Takes up why the holder marks nothing for the node selected, which
    /// the bar's status line says.
    pub(crate) fn set_unmarked(&mut self, unmarked: Option<Unmarked>, cx: &mut Context<Self>) {
        if self.unmarked != unmarked {
            self.unmarked = unmarked;
            cx.notify();
        }
    }

    /// The evaluation landed for the stack as it stands, if one has.
    fn current(&self) -> Option<&Landed> {
        self.landed
            .as_ref()
            .filter(|landed| landed.generation == self.generation)
    }

    /// Scrolls the row of chips to the chip `id` of `stack`, wherever its
    /// group put it. The last source stands just ahead of the first
    /// refinement, so for it the row scrolls to that refinement instead: both
    /// come into view, with the rule between them, wherever the row holds room
    /// for the two.
    fn scroll_to(&self, stack: &Stack, id: ChipId) {
        let last_source = stack.sources().last().map(|chip| chip.id());
        let at = match last_source == Some(id) && !stack.refinements().is_empty() {
            true => Some(stack.sources().len()),
            false => stack.position(id),
        };
        if let Some(at) = at {
            self.scroll.scroll_to_item(at);
        }
    }
}

/// Restates whether the stack, with the field's chip in place, names other
/// than what was applied.
fn restage(view: &mut ChipBar, cx: &App) {
    let model = view.model.read(cx);
    let (stack, _) = view.with_field(model.query(), cx);
    let written = stack
        .compile()
        .map(|compiled| compiled.map(|c| c.expr.to_string()));
    let applied = (model.filter().applied()).map(|applied| applied.scope().query());
    view.staged = match written {
        Ok(written) => written.as_deref() != applied,
        Err(_) => true,
    };
}

/// Starts the bar over the graph the document has read: what the chips name
/// is evaluated afresh.
fn reset(view: &mut ChipBar, cx: &mut Context<ChipBar>) {
    complete::release(view, cx);
    view.landed = None;
    changed(view, cx);
}

/// Takes up a change any bar made to the stack: ends an edit of a chip
/// removed or rewritten, as [`Editing`] says, and a selection of a chip
/// removed.
fn restacked(view: &mut ChipBar, cx: &mut Context<ChipBar>) {
    let stack = view.model.read(cx).query();
    let edited_elsewhere = (view.editing.as_ref()).is_some_and(|editing| !editing.survives(stack));
    let selection_gone = (view.selected).is_some_and(|id| stack.chip(id).is_none());
    if edited_elsewhere {
        view.editing = None;
    }
    if selection_gone {
        view.selected = None;
    }
    changed(view, cx);
    if edited_elsewhere {
        view.notice = Some(EDITED_ELSEWHERE.into());
    }
}

/// What the bar says as an edit elsewhere ends its own.
const EDITED_ELSEWHERE: &str = "chip changed in another view; the text here stays as a new chip";

/// Takes up a change to the stack or the field: counts it afresh off the UI
/// thread.
fn changed(view: &mut ChipBar, cx: &mut Context<ChipBar>) {
    view.generation += 1;
    view.notice = None;
    restage(view, cx);
    evaluate(view, cx);
    cx.notify();
}

/// Counts the stack with the field's text in place: the document counts the
/// stack as staged once for every bar whose field holds nothing, and a bar
/// counts its own only while its field holds text.
fn evaluate(view: &mut ChipBar, cx: &mut Context<ChipBar>) {
    if view.field.read(cx).text(cx).trim().is_empty() {
        view.landed = None;
        view._evaluating = Task::ready(());
        view.model.update(cx, GraphModel::count_query);
        return;
    }
    let (stack, draft) = view.with_field(view.model.read(cx).query(), cx);
    let generation = view.generation;
    // An empty stack names nothing, which needs no universe to know: a graph
    // nobody queries never prepares one.
    if stack.is_empty() {
        view._evaluating = Task::ready(());
        let evaluation = Evaluation {
            chips: Vec::new(),
            value: None,
        };
        let nothing = Landed {
            generation,
            draft,
            evaluation,
        };
        landed(view, nothing, cx);
        return;
    }
    let model = view.model.read(cx);
    let Some(universe) = model.universe() else {
        return;
    };
    let prefixes = model.prefixes();
    view._evaluating = load_in_background(
        cx,
        move || Landed {
            generation,
            draft,
            evaluation: stack.evaluate(universe.wait(), &prefixes),
        },
        landed,
    );
}

fn landed(view: &mut ChipBar, landed: Landed, cx: &mut Context<ChipBar>) {
    if landed.generation == view.generation {
        view.landed = Some(landed);
        cx.notify();
    }
}

/// Edits the chips the document stages, which every bar over it then counts
/// afresh.
fn edit_stack<R>(
    view: &mut ChipBar,
    cx: &mut Context<ChipBar>,
    edit: impl FnOnce(&mut Stack) -> R,
) -> R {
    (view.model).update(cx, |model, cx| model.edit_query(edit, cx))
}

/// Scrolls the row of chips to the chip `id`.
fn scroll_to(view: &ChipBar, id: ChipId, cx: &App) {
    view.scroll_to(view.model.read(cx).query(), id);
}

fn apply(view: &mut ChipBar, cx: &mut Context<ChipBar>) {
    let by = cx.entity_id();
    view.model.update(cx, |model, cx| model.apply_query(by, cx));
}

/// Puts the field's text on the stack, as the chip it edits or as a chip on
/// top, empties the field and applies the stack. Text that does not parse
/// stays in the field, the bar saying why.
fn commit(view: &mut ChipBar, cx: &mut Context<ChipBar>) {
    let text = view.field.read(cx).text(cx);
    let text = text.trim();
    if let Some(error) = syntax_error(text) {
        view.notice = Some(format!("not added: {error}").into());
        cx.notify();
        return;
    }
    let editing = view.editing.take().map(|editing| editing.chip);
    let scrolled = edit_stack(view, cx, |stack| match editing {
        Some(id) if text.is_empty() => {
            stack.remove(id);
            None
        }
        Some(id) => {
            stack.set_text(id, text);
            Some(id)
        }
        None if text.is_empty() => None,
        None => Some(stack.push(text)),
    });
    if let Some(id) = scrolled {
        scroll_to(view, id, cx);
    }
    view.field.update(cx, |field, cx| field.clear(cx));
    apply(view, cx);
}

/// Why `text` does not parse as a chip; `None` where it does, or is empty.
fn syntax_error(text: &str) -> Option<String> {
    if text.is_empty() {
        return None;
    }
    let mut probe = Stack::default();
    let id = probe.push(text);
    let chip = probe.chip(id)?;
    chip.parsed().err().map(|error| error.message.clone())
}

/// Adds a chip reading `text` on top of the stack and applies it.
pub(crate) fn push(view: &mut ChipBar, text: &str, cx: &mut Context<ChipBar>) {
    let id = edit_stack(view, cx, |stack| stack.push(text));
    scroll_to(view, id, cx);
    apply(view, cx);
}

/// Takes the focus into the field.
pub(crate) fn focus(view: &mut ChipBar, window: &mut Window, cx: &mut Context<ChipBar>) {
    view.selected = None;
    let field = view.field.read(cx).focus_handle(cx);
    field.focus(window, cx);
    cx.notify();
}

/// Gives the focus back to the panel, dropping an edit under way.
fn leave(view: &mut ChipBar, window: &mut Window, cx: &mut Context<ChipBar>) {
    view.selected = None;
    complete::release(view, cx);
    if view.editing.take().is_some() {
        view.field.update(cx, |field, cx| field.clear(cx));
    }
    view.host.focus(window, cx);
    cx.notify();
}

/// Selects the chip `id`, taking the focus onto the bar and scrolling the
/// chip into view.
fn select(view: &mut ChipBar, id: ChipId, window: &mut Window, cx: &mut Context<ChipBar>) {
    view.selected = Some(id);
    if let Some(at) = view.model.read(cx).query().position(id) {
        view.scroll.scroll_to_item(at);
    }
    view.focus_handle.focus(window, cx);
    cx.notify();
}

/// Moves the selection `step` chips along; past the last chip the field takes
/// the focus.
fn step(view: &mut ChipBar, step: isize, window: &mut Window, cx: &mut Context<ChipBar>) {
    let stack = view.model.read(cx).query();
    let at = (view.selected).and_then(|id| stack.position(id));
    let Some(at) = at else {
        return;
    };
    let next = at.checked_add_signed(step);
    match next.map(|next| stack.get(next).map(|chip| chip.id())) {
        Some(Some(next)) => select(view, next, window, cx),
        Some(None) => focus(view, window, cx),
        None => {}
    }
}

/// Runs `edit` on the selected chip's stack entry and re-evaluates the stack.
fn edit_selected(view: &mut ChipBar, cx: &mut Context<ChipBar>, edit: fn(&mut Stack, ChipId)) {
    if let Some(id) = view.selected {
        edit_stack(view, cx, |stack| edit(stack, id));
        scroll_to(view, id, cx);
    }
}

fn move_selected(view: &mut ChipBar, by: isize, cx: &mut Context<ChipBar>) {
    if let Some(id) = view.selected
        && edit_stack(view, cx, |stack| stack.move_by(id, by))
    {
        scroll_to(view, id, cx);
    }
}

/// Takes the chip `id` off the stack, selecting the one that took its place.
fn remove(view: &mut ChipBar, id: ChipId, window: &mut Window, cx: &mut Context<ChipBar>) {
    let Some(at) = edit_stack(view, cx, |stack| stack.remove(id)) else {
        return;
    };
    if view.edits(id) {
        view.editing = None;
        view.field.update(cx, |field, cx| field.clear(cx));
    }
    let stack = view.model.read(cx).query();
    let next = stack
        .get(at)
        .or_else(|| stack.chips().last())
        .map(|chip| chip.id());
    match next {
        Some(next) if view.selected == Some(id) => select(view, next, window, cx),
        None if view.selected == Some(id) => focus(view, window, cx),
        _ => {}
    }
}

/// Puts the chip `id`'s text in the field for editing.
fn edit(view: &mut ChipBar, id: ChipId, window: &mut Window, cx: &mut Context<ChipBar>) {
    let Some(chip) = view.model.read(cx).query().chip(id) else {
        return;
    };
    let text = chip.text().to_string();
    view.editing = Some(Editing {
        chip: id,
        from: text.clone(),
    });
    view.field.update(cx, |field, cx| field.set_text(text, cx));
    complete::hold_closed(view, cx);
    focus(view, window, cx);
}

/// Copies the text of the chip `id`, the bar saying it did.
fn copy_chip(view: &mut ChipBar, id: ChipId, _: &mut Window, cx: &mut Context<ChipBar>) {
    let Some(text) = (view.model.read(cx).query().chip(id)).map(|chip| chip.text().to_string())
    else {
        return;
    };
    view.notice = Some(format!("copied {text}").into());
    cx.write_to_clipboard(ClipboardItem::new_string(text));
    cx.notify();
}

/// Selects the last chip, for a backspace in the emptied field.
fn select_last(view: &mut ChipBar, window: &mut Window, cx: &mut Context<ChipBar>) -> bool {
    let empty = view.field.read(cx).text(cx).is_empty();
    match view
        .model
        .read(cx)
        .query()
        .chips()
        .last()
        .map(|chip| chip.id())
    {
        Some(last) if empty => {
            select(view, last, window, cx);
            true
        }
        _ => false,
    }
}

/// The window point under the pill of the chip `id`, held inside the row of
/// chips; the row's own corner where the row has not laid the pill out.
fn foot(view: &ChipBar, id: ChipId, cx: &App) -> Point<Pixels> {
    let row = view.scroll.bounds();
    let at = view.model.read(cx).query().position(id);
    let pill = at.and_then(|at| view.scroll.bounds_for_item(at));
    match pill {
        Some(pill) => {
            let left = pill.left() + view.scroll.offset().x;
            point(left.max(row.left()).min(row.right()), row.bottom())
        }
        None => row.bottom_left(),
    }
}

/// Puts up the menu of what can be done with the chip `id` at `at`.
fn open_menu(
    view: &mut ChipBar,
    id: ChipId,
    at: Point<Pixels>,
    window: &mut Window,
    cx: &mut Context<ChipBar>,
) {
    if view.model.read(cx).query().chip(id).is_none() {
        return;
    }
    let panel = cx.weak_entity();
    let act = |act: fn(&mut ChipBar, ChipId, &mut Window, &mut Context<ChipBar>)| {
        let panel = panel.clone();
        move |window: &mut Window, cx: &mut App| {
            panel.update(cx, |view, cx| act(view, id, window, cx)).ok();
        }
    };
    let keys_in = Some(&view.focus_handle);
    let item = |action: &dyn Action, chosen| MenuItem::action(action, keys_in, window, cx, chosen);
    let items = vec![
        item(
            &Negate,
            act(|view, id, _, cx| {
                edit_stack(view, cx, |stack| stack.toggle_negated(id));
                scroll_to(view, id, cx);
            }),
        ),
        item(
            &Toggle,
            act(|view, id, _, cx| {
                edit_stack(view, cx, |stack| stack.toggle_enabled(id));
            }),
        ),
        item(&Edit, act(edit)),
        item(&Remove, act(remove)),
        item(&CopyChip, act(copy_chip)),
    ];
    view.selected = Some(id);
    put_up(view, items, at, window, cx);
}

/// Puts up the menu of what can be done with the whole stack at `at`.
fn open_stack_menu(
    view: &mut ChipBar,
    at: Point<Pixels>,
    window: &mut Window,
    cx: &mut Context<ChipBar>,
) {
    let panel = cx.weak_entity();
    let act = |act: fn(&mut ChipBar, &mut Context<ChipBar>)| {
        let panel = panel.clone();
        move |_: &mut Window, cx: &mut App| {
            panel.update(cx, act).ok();
        }
    };
    let keys_in = Some(&view.focus_handle);
    let item = |action: &dyn Action, chosen| MenuItem::action(action, keys_in, window, cx, chosen);
    let mut items = vec![
        item(&CopyStack, act(copy_stack)),
        item(&ClearQuery, act(clear_query)),
    ];
    if view.staged {
        items.push(item(&RevertQuery, act(revert_query)));
    }
    put_up(view, items, at, window, cx);
}

/// Puts the stack applied last back in the bar, emptying the field, which
/// takes back every edit made since; with none applied, the bar is emptied.
fn revert_query(view: &mut ChipBar, cx: &mut Context<ChipBar>) {
    if !view.staged {
        return;
    }
    view.selected = None;
    view.editing = None;
    view.field.update(cx, |field, cx| field.clear(cx));
    view.model.update(cx, GraphModel::revert_query);
}

/// Takes every chip off the stack and empties the field. Like every other
/// edit to the stack it is staged, the scope applied standing until the
/// stack is applied or the applied stack is reverted to.
fn clear_query(view: &mut ChipBar, cx: &mut Context<ChipBar>) {
    edit_stack(view, cx, |stack| {
        let ids: Vec<ChipId> = stack.chips().map(|chip| chip.id()).collect();
        for id in ids {
            stack.remove(id);
        }
    });
    view.selected = None;
    view.editing = None;
    view.field.update(cx, |field, cx| field.clear(cx));
}

/// Puts up a menu of `items` at `at`, which gives the focus back to the bar
/// as it closes holding it.
fn put_up(
    view: &mut ChipBar,
    items: Vec<MenuItem>,
    at: Point<Pixels>,
    window: &mut Window,
    cx: &mut Context<ChipBar>,
) {
    let menu = cx.new(|cx| Menu::new(items, at, window, cx));
    let dismissed = cx.subscribe_in(&menu, window, |view, _, event: &MenuEvent, window, cx| {
        let MenuEvent::Dismissed { holding_focus } = event;
        view.menu = None;
        if *holding_focus {
            view.focus_handle.focus(window, cx);
        }
        cx.notify();
    });
    view.menu = Some(OpenMenu {
        menu,
        _dismissed: dismissed,
    });
    cx.notify();
}

/// The Bazel command whose output `input` is.
pub(super) fn command(input: Input) -> Command {
    match input {
        Input::Aquery => Command::Aquery,
        Input::Cquery => Command::Cquery,
        Input::Query => Command::Query,
    }
}

/// Copies the stack as a Bazel query, or says on the bar why it is none.
///
/// The query is written for the deepest of the commands the graph was read
/// from, where that command reads every function the stack calls; and the bar
/// says so where the graph joins several commands' output, which no one
/// command reads alike.
pub(crate) fn copy_stack(view: &mut ChipBar, cx: &mut Context<ChipBar>) {
    let inputs = view
        .model
        .read(cx)
        .loaded()
        .map(|loaded| loaded.shared.inputs);
    let deepest = inputs.and_then(|inputs| inputs.inputs().last());
    let preferred = deepest.map_or(Command::Query, command);
    let joint = inputs.is_some_and(|inputs| inputs.inputs().count() > 1);
    let text = view.model.read(cx).query().bazel_text(preferred);
    view.notice = Some(match text {
        Ok(query) => {
            let command = query.command.name();
            cx.write_to_clipboard(ClipboardItem::new_string(query.text));
            match joint {
                true => format!("copied as bazel {command}; read here over a joint graph").into(),
                false => format!("copied as bazel {command}").into(),
            }
        }
        Err(unportable) => format!("not a Bazel query: {unportable}").into(),
    });
    cx.notify();
}

/// Commits the field and applies the stack, for the key that does so from
/// anywhere in the holder.
pub(crate) fn apply_staged(view: &mut ChipBar, cx: &mut Context<ChipBar>) {
    commit(view, cx);
}

/// What the status line does for a gesture in [`STATUS_GESTURES`].
#[derive(Clone, Copy)]
enum StatusIntent {
    Focus,
}

const STATUS_GESTURES: Gestures<StatusIntent> = Gestures::new(&[Entry::new(
    gesture::Input::press(PointerButton::Left),
    Place::On("the query status"),
    "Focus query field",
    StatusIntent::Focus,
)]);

/// The gestures the bar answers to.
pub(crate) fn gestures() -> impl Iterator<Item = GestureRow> {
    (pill::GESTURES.rows())
        .chain(STATUS_GESTURES.rows())
        .chain(complete::gestures())
}

impl Render for ChipBar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::from_theme(&bviz_ui_theme::theme(window, cx));
        bar(self, &palette, window, cx)
    }
}

/// The bar: the chips, the field, and what applies them, with the menu and
/// the notice it puts up.
fn bar(
    view: &ChipBar,
    palette: &Palette,
    window: &mut Window,
    cx: &Context<ChipBar>,
) -> impl IntoElement + use<> {
    let chips = view;
    let model = view.model.read(cx);
    let stack = model.query();
    let current = chips.current();
    let evaluation = (chips.landed.as_ref())
        .map(|landed| &landed.evaluation)
        .or(model.counted());
    let outcome = |id: ChipId| evaluation?.outcome(id).cloned();
    let focused = chips.focus_handle.is_focused(window);
    let mut pills = Vec::with_capacity(stack.len());
    let mut after_source = false;
    for chip in stack.chips() {
        let selected = focused && chips.selected == Some(chip.id());
        let editing = chips.edits(chip.id());
        let outcome = outcome(chip.id());
        let pill = pill::pill(chip, outcome, selected, editing, palette, cx);
        let joins = match (after_source, chip.is_source()) {
            (true, true) => Some(Joint::Or),
            (true, false) => Some(Joint::Then),
            (false, _) => None,
        };
        after_source = chip.is_source();
        // A pill and what joins it to the one before are one child of the
        // row, so the row's children stand at the chips' own positions.
        let (ahead, behind) = reorder::marked(view, chip.id(), cx);
        let drop_mark = |shown: bool| {
            shown.then(|| {
                div()
                    .w(DROP_MARK_WIDTH)
                    .h(JOINT_RULE_HEIGHT)
                    .bg(palette.colors.accent)
            })
        };
        pills.push(
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap_1()
                .children(joins.map(|joint| joint.mark(palette)))
                .children(drop_mark(ahead))
                .child(pill)
                .children(drop_mark(behind)),
        );
    }
    let draft = current
        .and_then(|landed| Some((landed.draft?, landed)))
        .and_then(|(draft, landed)| landed.evaluation.outcome(draft).cloned());
    let staged = chips.staged;
    let completing = complete::is_shown(view, window, cx);
    let aimed = completing && complete::is_aimed(view);
    let completions = complete::list(view, palette, window, cx);
    let carrying = reorder::handlers(view, cx);
    let staged_dot = || {
        staged.then(|| {
            div()
                .flex_none()
                .size(STAGED_DOT)
                .rounded_full()
                .bg(palette.colors.accent)
        })
    };
    let status = status(view.model.read(cx).filter(), view.shown, view.unmarked);
    let status_line = |id| status_line(id, status.clone(), palette, cx);
    let whole =
        div()
            .size_full()
            .flex()
            .items_center()
            .gap_1()
            .when(!pills.is_empty(), |bar| {
                bar.child(
                    div()
                        .id("graph-chip-row")
                        .flex()
                        .min_w_0()
                        .items_center()
                        .gap_1()
                        .overflow_x_scroll()
                        .track_scroll(&chips.scroll)
                        .children(pills),
                )
            })
            .child(
                div()
                    .flex_none()
                    .relative()
                    .key_context(FIELD_KEY_CONTEXT)
                    .on_action(cx.listener(|view, _: &Commit, _, cx| commit(view, cx)))
                    .capture_action(cx.listener(|view, _: &text_input::Backspace, window, cx| {
                        if select_last(view, window, cx) {
                            cx.stop_propagation();
                        }
                    }))
                    .child(
                        div()
                            .when(completing, |field| {
                                field.key_context(completion_context(aimed))
                            })
                            .on_action(cx.listener(|view, _: &CompletePrevious, _, cx| {
                                complete::aim(view, -1, cx)
                            }))
                            .on_action(cx.listener(|view, _: &CompleteNext, _, cx| {
                                complete::aim(view, 1, cx)
                            }))
                            .on_action(cx.listener(|view, _: &AcceptCompletion, window, cx| {
                                complete::accept(view, None, window, cx)
                            }))
                            .on_action(cx.listener(|view, _: &CloseCompletion, _, cx| {
                                complete::close(view, cx)
                            }))
                            .child(chips.field.clone()),
                    )
                    .children(completions),
            )
            .children(
                draft
                    .and_then(|outcome| pill::draft(&outcome, palette))
                    .map(|draft| draft.flex_none()),
            )
            .children(staged_dot())
            .child(
                div().flex_none().child(
                    Button::new(
                        "chips-apply",
                        cx.listener(|view, _: &ClickEvent, _, cx| commit(view, cx)),
                    )
                    .label("apply")
                    .keys_for(&Apply, &chips.focus_handle)
                    .compact()
                    .disabled(!staged),
                ),
            )
            .children((!stack.is_empty() || staged).then(|| {
                div().flex_none().child(
                    Button::new(
                        "chips-menu",
                        cx.listener(|view, event: &ClickEvent, window, cx| {
                            open_stack_menu(view, event.position(), window, cx)
                        }),
                    )
                    .ghost()
                    .icon("dots-three-vertical")
                    .tooltip("Query menu"),
                )
            }))
            .children(status.is_some().then(|| status_line("chips-status")));
    let held_open = focused
        || view.field.read(cx).focus_handle(cx).is_focused(window)
        || view.menu.is_some()
        || !matches!(view.gesture, reorder::Gesture::Idle);
    let content = match held_open {
        true => whole.into_any_element(),
        false => {
            let folded = div()
                .size_full()
                .flex()
                .items_center()
                .gap_1()
                .child(status_line("chips-status-folded"))
                .children(staged_dot());
            ByRoom::width(
                MIN_WIDTH,
                whole.into_any_element(),
                folded.into_any_element(),
            )
            .into_any_element()
        }
    };
    div()
        .id("graph-chips")
        .relative()
        .flex_1()
        .min_w_0()
        .h_full()
        .key_context(KEY_CONTEXT)
        .track_focus(&chips.focus_handle)
        .on_action(cx.listener(|view, _: &Leave, window, cx| leave(view, window, cx)))
        .on_action(cx.listener(|view, _: &SelectPrevious, window, cx| step(view, -1, window, cx)))
        .on_action(cx.listener(|view, _: &SelectNext, window, cx| step(view, 1, window, cx)))
        .on_action(
            cx.listener(|view, _: &Toggle, _, cx| edit_selected(view, cx, Stack::toggle_enabled)),
        )
        .on_action(
            cx.listener(|view, _: &Negate, _, cx| edit_selected(view, cx, Stack::toggle_negated)),
        )
        .on_action(cx.listener(|view, _: &MoveBack, _, cx| move_selected(view, -1, cx)))
        .on_action(cx.listener(|view, _: &MoveForward, _, cx| move_selected(view, 1, cx)))
        .on_action(cx.listener(|view, _: &Remove, window, cx| {
            if let Some(id) = view.selected {
                remove(view, id, window, cx);
            }
        }))
        .on_action(cx.listener(|view, _: &Edit, window, cx| {
            if let Some(id) = view.selected {
                edit(view, id, window, cx);
            }
        }))
        .on_action(cx.listener(|view, _: &ClearQuery, _, cx| clear_query(view, cx)))
        .on_action(cx.listener(|view, _: &RevertQuery, _, cx| revert_query(view, cx)))
        .on_action(cx.listener(|view, _: &CopyChip, window, cx| {
            if let Some(id) = view.selected {
                copy_chip(view, id, window, cx);
            }
        }))
        .on_action(cx.listener(|view, _: &OpenChipMenu, window, cx| {
            if let Some(id) = view.selected {
                let at = foot(view, id, cx);
                open_menu(view, id, at, window, cx);
            }
        }))
        // Sets the bar's min-content width, which a strip keeps clear for it
        // while the strip's groups can give way, without holding the bar
        // that wide once it is given less.
        .child(div().w(MIN_WIDTH).h_0())
        .child(content)
        .children(carrying)
        // The notice opens where the completion list does, which takes the
        // place while it is up.
        .children(
            (!completing)
                .then(|| notice(view, palette).map(menu_chrome::under_trigger))
                .flatten(),
        )
        .children(
            view.menu
                .as_ref()
                .map(|open| over_window(&open.menu, window)),
        )
}

/// The line saying what the filter the bar applies shows of the holder's
/// view, `status`, or prompting for a query where it shows everything; a
/// click on it takes the focus into the field.
fn status_line(
    id: &'static str,
    status: Option<String>,
    palette: &Palette,
    cx: &Context<ChipBar>,
) -> Stateful<Div> {
    let text: SharedString = status.unwrap_or_else(|| FOLDED_PROMPT.to_owned()).into();
    let colors = &palette.colors;
    let hover = colors.text;
    div()
        .id(id)
        .min_w_0()
        .overflow_hidden()
        .whitespace_nowrap()
        .text_ellipsis()
        .text_color(colors.text_muted)
        .cursor_pointer()
        .hover(move |style| style.text_color(hover))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|view, event: &MouseDownEvent, window, cx| {
                STATUS_GESTURES.dispatch(Pointer::press(event), |intent| match intent {
                    // Taken on the press, which would otherwise focus the bar
                    // and unfold it, moving the line from under the release.
                    StatusIntent::Focus => {
                        focus(view, window, cx);
                        cx.stop_propagation();
                        true
                    }
                });
            }),
        )
        .tooltip(TextTooltip::builder(text.clone(), colors.tooltip()))
        .child(text)
}

/// What the document's `filter` does to the holder's view, which shows
/// `shown` of its points: how many chips it applies and nodes a reader pinned
/// hidden or faded, and how many points that leaves shown; then why the view
/// marks nothing for the node selected, where it is `unmarked`. `None` where
/// there is nothing to say.
fn status(
    filter: &DocumentFilter,
    shown: Option<Shown>,
    unmarked: Option<Unmarked>,
) -> Option<String> {
    let unmarked = unmarked.map(|unmarked| unmarked.describe().to_owned());
    if !filter.narrows() {
        return unmarked;
    }
    let counted = |count: usize, one: &str, many: &str| match count {
        1 => format!("1 {one}"),
        count => format!("{count} {many}"),
    };
    let chips = (filter.applied()).map(|applied| {
        let enabled = applied.stack().chips().filter(|chip| chip.enabled());
        counted(enabled.count(), "chip", "chips")
    });
    let pins = filter.narrowing_pins();
    let pins = (pins > 0).then(|| counted(pins, "pin", "pins"));
    let (shown, note) = match shown {
        Some(Shown {
            shown,
            of,
            many,
            note,
        }) => (Some(format!("{shown} of {of} {many} shown")), note),
        None => (None, None),
    };
    let parts = [chips, pins, shown, note.map(str::to_owned), unmarked];
    let parts: Vec<String> = parts.into_iter().flatten().collect();
    Some(parts.join(STATUS_SEPARATOR))
}

/// Stands between the parts of the bar's status line.
const STATUS_SEPARATOR: &str = " · ";

/// What the folded bar reads where the filter shows everything.
const FOLDED_PROMPT: &str = "query…";

/// The narrowest the bar stands whole: the field, what applies it, and room
/// beside them for a chip; narrower, it folds to its status line until it
/// takes the focus.
const MIN_WIDTH: Pixels = px(420.);

/// The narrowest the folded bar stands beside a strip's controls before the
/// strip gives up reaching them: room for the start of its status line.
pub(crate) const FOLDED_WIDTH: Pixels = px(120.);

/// The key contexts from `holder`'s in to the field's text while its
/// completion's list stands open.
pub(crate) fn completion_stack(holder: Holder, aimed: bool) -> [KeyContext; 5] {
    [
        holder.scope().into(),
        KEY_CONTEXT.into(),
        FIELD_KEY_CONTEXT.into(),
        completion_context(aimed),
        text_input::KEY_CONTEXT.into(),
    ]
}

/// The key context of the field while its completion's list stands open.
fn completion_context(aimed: bool) -> KeyContext {
    let mut context = KeyContext::from(COMPLETION_KEY_CONTEXT);
    if aimed {
        context.add(AIMED_KEY_CONTEXT.id());
    }
    context
}

/// What the bar last has to say, put up under the bar, clear of the strip
/// holding it, whose row it would otherwise widen.
fn notice(view: &ChipBar, palette: &Palette) -> Option<impl IntoElement + use<>> {
    let notice = view.notice.clone()?;
    Some(
        div()
            .max_w(NOTICE_WIDTH)
            .px_2()
            .py_1()
            .rounded_md()
            .bg(palette.container)
            .text_xs()
            .text_color(palette.label)
            .child(notice),
    )
}

/// What stands between a pill and the one before it: `or` between two
/// sources, whose union the stack starts from, and a rule where the
/// refinements of that union begin.
#[derive(Clone, Copy)]
enum Joint {
    Or,
    Then,
}

impl Joint {
    fn mark(self, palette: &Palette) -> AnyElement {
        match self {
            Joint::Or => div()
                .text_xs()
                .text_color(palette.colors.text_muted)
                .child("or")
                .into_any_element(),
            Joint::Then => div()
                .w(JOINT_RULE_WIDTH)
                .h(JOINT_RULE_HEIGHT)
                .bg(palette.colors.border)
                .into_any_element(),
        }
    }
}

/// The width of the mark standing where a dragged chip would land.
const DROP_MARK_WIDTH: Pixels = px(2.);

/// The rule standing where a stack's refinements begin.
const JOINT_RULE_WIDTH: Pixels = px(1.);
const JOINT_RULE_HEIGHT: Pixels = px(16.);

/// The size of the dot saying the stack holds changes not yet applied.
const STAGED_DOT: Pixels = px(6.);

#[cfg(test)]
mod tests {
    use super::*;
    use bviz_graph_layout::select::{Pin, Visibility};

    /// An edit outlives a change to its chip's switches, and not one to its
    /// text or the chip's removal.
    #[test]
    fn an_edit_ends_as_its_chip_is_rewritten_or_removed() {
        let mut stack = Stack::default();
        let chip = stack.push("//a:all");
        let editing = Editing {
            chip,
            from: "//a:all".to_owned(),
        };
        stack.toggle_negated(chip);
        stack.toggle_enabled(chip);
        assert!(editing.survives(&stack));
        stack.set_text(chip, "//b:all");
        assert!(!editing.survives(&stack));
        stack.remove(chip);
        assert!(!editing.survives(&stack));
    }

    #[test]
    fn every_status_gesture_is_reachable_and_fits_the_help() {
        assert_eq!(STATUS_GESTURES.unreachable(), []);
        assert_eq!(STATUS_GESTURES.overlong(), []);
    }

    /// The status line is up only while the filter narrows the graph, and
    /// reads what narrows it and how much of the view that leaves.
    #[test]
    fn the_status_line_reads_what_narrows_the_view_and_what_is_left() {
        let mut filter = DocumentFilter::default();
        let shown = Shown {
            shown: 12,
            of: 400,
            many: "targets",
            note: None,
        };
        assert_eq!(status(&filter, Some(shown), None), None);
        filter.set_pins([(Id(3), Pin::Set(Visibility::Hidden))].into());
        assert_eq!(status(&filter, None, None).as_deref(), Some("1 pin"));
        assert_eq!(
            status(&filter, Some(shown), None).as_deref(),
            Some("1 pin · 12 of 400 targets shown")
        );
        let noted = Shown {
            note: Some("widths count shown"),
            ..shown
        };
        assert_eq!(
            status(&filter, Some(noted), None).as_deref(),
            Some("1 pin · 12 of 400 targets shown · widths count shown")
        );
    }

    /// A view marking nothing for the node selected says so whether or not
    /// the filter narrows the graph.
    #[test]
    fn the_status_line_says_where_the_selection_is_not_marked() {
        let mut filter = DocumentFilter::default();
        let hidden = Some(Unmarked::Hidden);
        assert_eq!(
            status(&filter, None, hidden).as_deref(),
            Some("selection hidden")
        );
        filter.set_pins([(Id(3), Pin::Set(Visibility::Hidden))].into());
        assert_eq!(
            status(&filter, None, Some(Unmarked::Absent)).as_deref(),
            Some("1 pin · selection not in this view")
        );
    }
}
