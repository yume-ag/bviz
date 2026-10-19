//! Completion of the word typed into the query field: the graph's target
//! labels, its packages in both wildcard forms, its repositories, the query
//! functions where one can stand, and the words of a function's argument
//! that is one of a vocabulary, matched against the word under the cursor and
//! listed under the field.

use super::{AcceptCompletion, ChipBar, command, completion_stack};
use crate::Palette;
use crate::fuzzy::{self, Feeder, Matching, Row, Standing, TEXT_SIZE};
use crate::holder::Holder;
use crate::jump::{Candidates, bring_into_view, labelled, package_of};
use crate::model::{Prepared, Ready};
use bviz_graph_layout::query_lang::{
    ArgType, Command, Function, Params, Place, Readable, Vocabulary, word_at, written_word,
};
use bviz_graph_layout::{Id, Kind};
use bviz_label::names::RepoNames;
use bviz_label::shown::Parts;
use bviz_ui_components::gesture::{
    Button as PointerButton, Entry, GestureRow, Gestures, Input, Place as PointerPlace, Pointer,
};
use bviz_ui_components::{
    LabelLine, ListScroll, TextTooltip, key_text_within, list_scrollbar, menu_chrome,
};
use gpui::{
    App, ClickEvent, Context, Deferred, Div, Entity, Focusable, KeyContext, MouseButton,
    MouseDownEvent, Pixels, SharedString, Stateful, TextStyleRefinement, Window, div, prelude::*,
    px, uniform_list,
};
use rustc_hash::FxHashMap;
use std::borrow::Cow;
use std::ops::Range;

/// The width the list stands at, wide enough for a label of some depth; a
/// longer one is cut at its start, which keeps its name in view.
const LIST_WIDTH: Pixels = px(480.);

/// The most rows the list stands tall, few enough to read at a glance; the
/// rest of the matches scroll through it.
const VISIBLE_ROWS: usize = 10;

/// What the list does for a gesture in [`GESTURES`].
#[derive(Clone, Copy)]
enum CompletionIntent {
    Accept,
}

const GESTURES: Gestures<CompletionIntent> = Gestures::new(&[Entry::new(
    Input::click(PointerButton::Left),
    PointerPlace::On("a completion"),
    "Accept completion",
    CompletionIntent::Accept,
)]);

/// The gestures the list answers to.
pub(super) fn gestures() -> impl Iterator<Item = GestureRow> {
    GESTURES.rows()
}

/// What a completion names.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Named {
    Target(Id),
    Package,
    Repository,
    Function(Function),
    Word(Vocabulary),
}

impl Named {
    fn name(self) -> &'static str {
        match self {
            Named::Target(_) => "target",
            Named::Package => "package",
            Named::Repository => "repository",
            Named::Function(_) => "function",
            Named::Word(Vocabulary::RepoCategory) => "repository origin",
            Named::Word(Vocabulary::RuleCategory) => "rule category",
            Named::Word(Vocabulary::Configuration) => "configuration",
        }
    }
}

/// What a word standing at a place is offered.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Offers {
    ahead: Option<Ahead>,
    patterns: bool,
}

/// What a word is offered ahead of the graph's patterns.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Ahead {
    Functions,
    Words(Vocabulary),
}

impl Offers {
    fn at(place: Place) -> Offers {
        let (ahead, patterns) = match place {
            Place::Pattern => (None, true),
            Place::Operand => (Some(Ahead::Functions), true),
            Place::Callee => (Some(Ahead::Functions), false),
            Place::Word(vocabulary) => (Some(Ahead::Words(vocabulary)), false),
        };
        Offers { ahead, patterns }
    }
}

/// The function `Function::ALL` holds at `index`, as a completion.
fn function_completion(index: u32) -> Completion<'static> {
    let function = Function::ALL[index as usize];
    Completion {
        text: function.name(),
        path: 0..0,
        named: Named::Function(function),
        weight: 0,
    }
}

/// The rows of the functions `word` matches, ranked as the graph's patterns
/// are.
fn function_rows(word: &str) -> Vec<Row> {
    rank_few(Function::ALL.map(Function::name), word)
}

/// `candidates` ranked for `word` as the graph's patterns are, each row
/// holding its index among them; few enough to match on the UI thread as the
/// word changes.
fn rank_few<'a>(candidates: impl IntoIterator<Item = &'a str>, word: &str) -> Vec<Row> {
    fuzzy::rank_few((0u32..).zip(candidates), word, |_| Standing {
        path: 0..0,
        drawn: false,
        weight: 0,
    })
}

/// The completions ranked on the UI thread as the word changes.
enum Few {
    /// Each row's candidate is an index into [`Function::ALL`].
    Functions(Vec<Row>),
    /// Each row's candidate is an index into the vocabulary's words.
    Words(Vocabulary, Vec<Row>),
}

impl Few {
    fn rows(&self) -> &[Row] {
        match self {
            Few::Functions(rows) | Few::Words(_, rows) => rows,
        }
    }

    /// The completion `row`, one of [`Few::rows`], offers.
    fn completion(&self, row: &Row) -> Completion<'static> {
        match self {
            Few::Functions(_) => function_completion(row.candidate),
            Few::Words(vocabulary, _) => Completion {
                text: vocabulary.words()[row.candidate as usize],
                path: 0..0,
                named: Named::Word(*vocabulary),
                weight: 0,
            },
        }
    }
}

/// The tooltip of every row offering one of [`Function::EXTENSIONS`] that
/// evaluates over the graph.
const EXTENSION_NOTE: &str = "Not in Bazel's query language";

/// The kind a row offering `function` names, and its tooltip, if any, over a
/// graph `commands` were read from whose data `readable` states.
fn function_note(
    function: Function,
    commands: &[Command],
    readable: Readable,
) -> (&'static str, Option<String>) {
    let kind = match function.is_extension() {
        true => "bviz extension",
        false => "function",
    };
    let note = (function.unevaluable())
        .or_else(|| readable.check(function).err())
        .or_else(|| function.is_extension().then(|| EXTENSION_NOTE.to_owned()))
        .or_else(|| {
            if commands.iter().any(|command| command.reads(function)) {
                return None;
            }
            let commands: Vec<&str> = commands.iter().map(|command| command.name()).collect();
            Some(format!("Not in {} · bviz-evaluated", commands.join(" or ")))
        });
    (kind, note)
}

/// One pattern, function or word of a vocabulary the field offers to complete
/// a word with.
struct Completion<'a> {
    /// The pattern unquoted, the function's name or the vocabulary's word,
    /// which a word is matched against.
    text: &'a str,
    /// The chars of `text` leading up to the name of what it names.
    path: Range<u32>,
    named: Named,
    /// How many nodes depend on a target, or how many targets a package or a
    /// repository holds.
    weight: u32,
}

/// A pattern the field offers that is no target's own label.
#[derive(Debug)]
struct Pattern {
    text: Box<str>,
    path: Range<u32>,
    named: Named,
    weight: u32,
}

/// Every pattern the field offers over one graph, indexed targets first:
/// the palette's candidates that are targets, read through it, then the
/// packages and repositories they sit in.
pub(crate) struct Completions {
    candidates: Ready<Candidates>,
    /// The targets, as their indices among `candidates`.
    targets: Vec<u32>,
    patterns: Vec<Pattern>,
}

impl Completions {
    pub(crate) fn of(candidates: Prepared<Candidates>) -> Self {
        let candidates = candidates.ready();
        let (held, shared) = (&*candidates, candidates.shared());
        let targets: Vec<u32> = (0..held.len())
            .filter(|&index| {
                let candidate = held.get(index);
                match candidate.kind {
                    Kind::Target => true,
                    Kind::Source => labelled(shared, candidate.id),
                    _ => false,
                }
            })
            .collect();
        let labels = targets.iter().map(|&index| &*held.get(index).text);
        let patterns = patterns(labels);
        Completions {
            candidates,
            targets,
            patterns,
        }
    }

    fn len(&self) -> u32 {
        (self.targets.len() + self.patterns.len()) as u32
    }

    fn get(&self, index: u32) -> Completion<'_> {
        let index = index as usize;
        match self.targets.get(index) {
            Some(&target) => {
                let candidate = self.candidates.get(target);
                Completion {
                    text: &candidate.text,
                    path: candidate.path.clone(),
                    named: Named::Target(candidate.id),
                    weight: candidate.rdeps,
                }
            }
            None => {
                let pattern = &self.patterns[index - self.targets.len()];
                Completion {
                    text: &pattern.text,
                    path: pattern.path.clone(),
                    named: pattern.named,
                    weight: pattern.weight,
                }
            }
        }
    }
}

/// The patterns over the targets `labels` names: each package one sits in,
/// as every target beneath it and as every target in it, and each
/// repository whose root package holds none, as every target beneath its
/// root; each weighing the targets it holds.
fn patterns<'a>(labels: impl Iterator<Item = &'a str>) -> Vec<Pattern> {
    let mut packages: FxHashMap<&str, u32> = FxHashMap::default();
    let mut repos: FxHashMap<&str, u32> = FxHashMap::default();
    for label in labels {
        *packages.entry(package_of(label)).or_default() += 1;
        if let Some((repo, _)) = label.split_once("//").filter(|(repo, _)| !repo.is_empty()) {
            *repos.entry(repo).or_default() += 1;
        }
    }
    let mut packages: Vec<(&str, u32)> = packages.into_iter().collect();
    packages.sort_unstable();
    let mut repos: Vec<(&str, u32)> = repos
        .into_iter()
        .filter(|(repo, _)| !packages_hold(&packages, &format!("{repo}//")))
        .collect();
    repos.sort_unstable();
    let mut patterns: Vec<Pattern> = Vec::with_capacity(packages.len() * 2 + repos.len());
    for (package, targets) in packages {
        let path = 0..chars(&package[..package.rfind('/').map_or(0, |slash| slash + 1)]);
        let beneath = match package.ends_with('/') {
            true => format!("{package}..."),
            false => format!("{package}/..."),
        };
        for text in [beneath, format!("{package}:*")] {
            patterns.push(Pattern {
                text: text.into_boxed_str(),
                path: path.clone(),
                named: Named::Package,
                weight: targets,
            });
        }
    }
    for (repo, targets) in repos {
        let sigil = repo.len() - repo.trim_start_matches('@').len();
        patterns.push(Pattern {
            path: 0..sigil as u32,
            text: format!("{repo}//...").into_boxed_str(),
            named: Named::Repository,
            weight: targets,
        });
    }
    patterns
}

/// Whether the sorted `packages` hold `package`.
fn packages_hold(packages: &[(&str, u32)], package: &str) -> bool {
    packages
        .binary_search_by(|(held, _)| (*held).cmp(package))
        .is_ok()
}

fn chars(text: &str) -> u32 {
    text.chars().count() as u32
}

/// The query field's completion.
#[derive(Default)]
pub(super) struct Completer {
    /// What is offered and matched, from the field's first edit over a loaded
    /// graph until the graph is replaced.
    offered: Option<Offered>,
    /// How far the list has scrolled.
    scroll: ListScroll,
    /// What the field held as the list was closed, and where the word under
    /// the cursor stood, which keep it closed until either changes.
    closed_at: Option<(SharedString, Option<Range<usize>>)>,
}

/// What the field offers, listed [`Few`] rows first, then the graph's
/// patterns.
struct Offered {
    /// Matches the word against the graph's patterns.
    matching: Matching<Ready<Completions>>,
    few: Few,
    /// Where the word the rows complete stood in the field as it was matched.
    word: Option<Range<usize>>,
    /// What the word is offered.
    offers: Offers,
    /// The row the arrow keys moved onto, which Enter accepts; none until
    /// they move, so that Enter commits the field as typed.
    aimed: Option<usize>,
}

impl Offered {
    /// The matching of the patterns, where the word is offered them.
    fn patterns(&self) -> Option<&Matching<Ready<Completions>>> {
        self.offers.patterns.then_some(&self.matching)
    }

    fn rows(&self) -> usize {
        self.few.rows().len() + self.patterns().map_or(0, |patterns| patterns.rows().len())
    }

    /// The row at `at`, with what it completes the word as.
    fn row(&self, at: usize) -> Option<(Completion<'_>, &Row)> {
        let few = self.few.rows();
        match few.get(at) {
            Some(row) => Some((self.few.completion(row), row)),
            None => {
                let (completions, row) = self.patterns()?.row(at - few.len())?;
                Some((completions.get(row.candidate), row))
            }
        }
    }

    /// Whether the row at `at` was ranked for the word as it stands.
    fn current(&self, at: usize) -> bool {
        at < self.few.rows().len() || self.matching.current()
    }
}

/// The matching of what the field offers.
fn matching(view: &mut ChipBar) -> Option<&mut Matching<Ready<Completions>>> {
    Some(&mut view.completer.offered.as_mut()?.matching)
}

/// Stops offering, parking the matcher's pool for another list to take up;
/// the next edit offers afresh.
pub(super) fn release(view: &mut ChipBar, cx: &mut Context<ChipBar>) {
    if let Some(offered) = view.completer.offered.take() {
        (view.model).update(cx, |model, _| model.park_matcher(offered.matching));
    }
}

/// Matches the word now under the field's cursor, starting to offer the
/// graph's patterns on the first edit or cursor move made over it.
pub(super) fn follow(view: &mut ChipBar, cx: &mut Context<ChipBar>) {
    if view.completer.offered.is_none() {
        let Some(completions) = view.model.read(cx).completions() else {
            return;
        };
        view.completer.offered = Some(Offered {
            matching: view.model.update(cx, |model, _| model.take_matcher()),
            few: Few::Functions(Vec::new()),
            word: None,
            offers: Offers::at(Place::Pattern),
            aimed: None,
        });
        let prepare = move || completions.ready();
        let feed = |completions: &Ready<Completions>, feeder: &Feeder| {
            for index in 0..completions.len() {
                if !feeder.push(index, completions.get(index).text) {
                    return;
                }
            }
        };
        fuzzy::inject(view, matching, take_up, prepare, feed, cx);
    }
    let field = view.field.read(cx);
    let text = field.text(cx);
    let typed = word_at(&text, field.cursor(cx));
    let offers = typed.as_ref().map(|word| Offers::at(word.place));
    let few = match (&typed, offers.and_then(|offers| offers.ahead)) {
        (Some(word), Some(Ahead::Functions)) => Few::Functions(function_rows(word.text)),
        (Some(word), Some(Ahead::Words(vocabulary))) => Few::Words(
            vocabulary,
            rank_few(vocabulary.words().iter().copied(), word.text),
        ),
        _ => Few::Functions(Vec::new()),
    };
    let query = match &typed {
        Some(word) if offers.is_some_and(|offers| offers.patterns) => word.text.to_owned(),
        _ => String::new(),
    };
    let word = typed.map(|word| word.span);
    let Some(offered) = view.completer.offered.as_mut() else {
        return;
    };
    offered.word = word;
    offered.few = few;
    offered.offers = offers.unwrap_or(Offers::at(Place::Pattern));
    offered.aimed = None;
    view.completer.scroll.to_top();
    if offered.matching.set_query(query.into()) {
        fuzzy::poll(view, matching, take_up, cx);
    }
    cx.notify();
}

/// Keeps the list closed while the field holds what it holds now.
pub(super) fn hold_closed(view: &mut ChipBar, cx: &App) {
    view.completer.closed_at = Some(at_cursor(view, cx));
}

/// Takes up what the matcher has found since the last poll.
fn take_up(view: &mut ChipBar, cx: &mut Context<ChipBar>) {
    let drawn = (view.drawn.as_ref()).map_or_else(Default::default, |drawn| drawn(cx));
    let Some(offered) = view.completer.offered.as_mut() else {
        return;
    };
    offered.matching.rerank(|completions, index| {
        let completion = completions.get(index);
        Standing {
            path: completion.path.clone(),
            drawn: matches!(completion.named, Named::Target(id) if drawn.contains(&id)),
            weight: completion.weight,
        }
    });
    let rows = offered.rows();
    offered.aimed = offered.aimed.filter(|&aimed| aimed < rows);
    cx.notify();
}

/// What the field holds, and where the word under its cursor stands.
fn at_cursor(view: &ChipBar, cx: &App) -> (SharedString, Option<Range<usize>>) {
    let field = view.field.read(cx);
    let text = field.text(cx);
    let word = word_at(&text, field.cursor(cx)).map(|word| word.span);
    (text, word)
}

/// What is offered, while the list stands open under the field: the field
/// has the focus, the cursor is on the word matched, the list was not closed
/// over what the field holds and the word at its cursor, and the best match
/// is not the word as it is written.
fn shown<'a>(view: &'a ChipBar, window: &Window, cx: &App) -> Option<&'a Offered> {
    let offered = view.completer.offered.as_ref()?;
    let field = view.field.read(cx);
    if !field.focus_handle(cx).is_focused(window) {
        return None;
    }
    let at = at_cursor(view, cx);
    if view.completer.closed_at.as_ref() == Some(&at) {
        return None;
    }
    let (text, word) = at;
    let word = word?;
    if offered.word.as_ref() != Some(&word) {
        return None;
    }
    let (best, _) = offered.row(0)?;
    // A pattern written out as its best match has nothing left to complete;
    // a function's name still opens its call.
    let complete = match best.named {
        Named::Function(_) => false,
        _ => written_word(best.text) == text[word],
    };
    (!complete).then_some(offered)
}

/// Whether the list stands open under the field.
pub(super) fn is_shown(view: &ChipBar, window: &Window, cx: &App) -> bool {
    shown(view, window, cx).is_some()
}

/// What completing a word writes in place of it.
#[derive(PartialEq, Eq, Debug)]
struct Insertion<'a> {
    /// Where the word replaced stands.
    span: Range<usize>,
    text: Cow<'a, str>,
    /// Where in `text` the cursor is left.
    cursor: usize,
}

/// What completes the word of `text` the cursor at `cursor` stands in as
/// `completion`: a pattern written to scan as one word; a function's name
/// alone where its `(` follows, and else the name and a pair of parentheses
/// with the cursor between them.
fn insertion<'a>(text: &str, cursor: usize, completion: &Completion<'a>) -> Option<Insertion<'a>> {
    let word = word_at(text, cursor)?;
    let (text, cursor) = match (completion.named, word.place) {
        (Named::Function(_), Place::Callee) => (Cow::Borrowed(completion.text), None),
        (Named::Function(_), _) => {
            let call = format!("{}()", completion.text);
            let inside = call.len() - ")".len();
            (Cow::Owned(call), Some(inside))
        }
        _ => (written_word(completion.text), None),
    };
    let cursor = cursor.unwrap_or(text.len());
    Some(Insertion {
        span: word.span,
        text,
        cursor,
    })
}

/// Replaces the word under the cursor with the row at `at`, or with the aimed
/// row or else the first, and closes the list. Rows ranked for an earlier
/// query are not accepted.
pub(super) fn accept(
    view: &mut ChipBar,
    at: Option<usize>,
    window: &mut Window,
    cx: &mut Context<ChipBar>,
) {
    let Some(offered) = shown(view, window, cx) else {
        return;
    };
    let at = at.or(offered.aimed).unwrap_or(0);
    if !offered.current(at) {
        return;
    }
    let Some((completion, _)) = offered.row(at) else {
        return;
    };
    let field = view.field.read(cx);
    let text = field.text(cx);
    let Some(insertion) = insertion(&text, field.cursor(cx), &completion) else {
        return;
    };
    let Insertion { span, text, cursor } = insertion;
    let (text, cursor) = (text.into_owned(), span.start + cursor);
    view.field.update(cx, |field, cx| {
        field.replace(span, &text, cx);
        field.set_cursor(cursor, cx);
    });
    hold_closed(view, cx);
    cx.notify();
}

/// Closes the list until the field holds something else.
pub(super) fn close(view: &mut ChipBar, cx: &mut Context<ChipBar>) {
    hold_closed(view, cx);
    cx.notify();
}

/// Moves the aim `step` rows.
pub(super) fn aim(view: &mut ChipBar, step: isize, cx: &mut Context<ChipBar>) {
    let Some(offered) = view.completer.offered.as_mut() else {
        return;
    };
    offered.aimed = stepped(offered.aimed, step, offered.rows());
    if let Some(aimed) = offered.aimed {
        bring_into_view(&view.completer.scroll, aimed);
    }
    cx.notify();
}

/// Whether the arrow keys have moved onto a row.
pub(super) fn is_aimed(view: &ChipBar) -> bool {
    view.completer
        .offered
        .as_ref()
        .is_some_and(|offered| offered.aimed.is_some())
}

/// The row `step` rows on from `aimed` among `rows` rows: the first one down
/// from none, none up from the first, and held to the list past its end.
fn stepped(aimed: Option<usize>, step: isize, rows: usize) -> Option<usize> {
    let last = rows.checked_sub(1)?;
    match aimed {
        None if step > 0 => Some(0),
        None => None,
        Some(at) => at.checked_add_signed(step).map(|at| at.min(last)),
    }
}

/// The list under the field, for the field's `relative` holder to hold;
/// `None` while it is closed. It stands no taller than the window, past
/// which its rows scroll.
pub(super) fn list(
    view: &ChipBar,
    palette: &Palette,
    window: &mut Window,
    cx: &Context<ChipBar>,
) -> Option<Deferred> {
    let offered = shown(view, window, cx)?;
    let count = offered.rows();
    let text = TextStyleRefinement {
        font_size: Some(TEXT_SIZE.into()),
        ..TextStyleRefinement::default()
    };
    let row = window.with_text_style(Some(text), |window| menu_chrome::row_height(window));
    let room = window.viewport_size();
    let height = menu_chrome::sheet_height(count.min(VISIBLE_ROWS), row, px(0.), room.height);
    let entity = cx.entity();
    let listing = *palette;
    let rows = uniform_list("chip-completions", count, move |range, _, cx| {
        let view = entity.read(cx);
        let (Some(offered), Some(loaded)) = (
            view.completer.offered.as_ref(),
            view.model.read(cx).loaded(),
        ) else {
            return Vec::new();
        };
        let inputs = loaded.shared.inputs.inputs();
        let commands: Vec<Command> = inputs.map(command).collect();
        let readable = loaded.readable;
        range
            .filter_map(|index| {
                let (completion, row) = offered.row(index)?;
                let (kind, note) = match completion.named {
                    Named::Function(function) => function_note(function, &commands, readable),
                    named => (named.name(), None),
                };
                let accepts = match offered.aimed {
                    Some(aimed) => (aimed == index).then_some(Accepts::Aimed),
                    None => (index == 0).then_some(Accepts::First),
                };
                let row = Listed {
                    completion: &completion,
                    row,
                    names: loaded.shared.labels.repos().names(),
                    matching: &offered.matching,
                    index,
                    kind,
                    note,
                    accepts,
                    accepts_keys: accepts.and_then(|accepts| accepts.keys(view.holder, cx)),
                };
                Some(listed(row, &listing, &entity))
            })
            .collect()
    })
    .flex_1()
    .min_h_0()
    .w_full()
    .track_scroll(view.completer.scroll.uniform_handle());
    let sheet = menu_chrome::sheet(&palette.colors)
        .w(LIST_WIDTH)
        .max_w(room.width)
        .h(height)
        .text_size(TEXT_SIZE)
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|view, event: &MouseDownEvent, _, cx| {
                view.completer.scroll.press(event.position);
                // Keep the press off the panel, which would take the focus
                // from the field and so close the list mid-click.
                cx.stop_propagation();
                cx.notify();
            }),
        )
        .child(rows)
        .child(list_scrollbar(
            cx.entity(),
            |view: &ChipBar| &view.completer.scroll,
            |view: &mut ChipBar| &mut view.completer.scroll,
            palette.scroll,
        ));
    Some(menu_chrome::under_trigger(sheet))
}

/// What one row of the list shows.
struct Listed<'a> {
    completion: &'a Completion<'a>,
    row: &'a Row,
    /// The names the graph shows repositories by.
    names: &'a RepoNames,
    /// What matched the word against the row.
    matching: &'a Matching<Ready<Completions>>,
    index: usize,
    kind: &'static str,
    /// The row's tooltip.
    note: Option<String>,
    accepts: Option<Accepts>,
    /// The keys that accept the row, where any do.
    accepts_keys: Option<SharedString>,
}

/// One row: the pattern or the function's signature with the characters the
/// word matched picked out, and what it names. A function no graph
/// evaluates is dimmed.
fn listed(listed: Listed, palette: &Palette, entity: &Entity<ChipBar>) -> Stateful<Div> {
    let Listed {
        completion,
        row,
        names,
        matching,
        index,
        kind,
        note,
        accepts,
        accepts_keys,
    } = listed;
    let colors = &palette.colors;
    let entity = entity.clone();
    let function = match completion.named {
        Named::Function(function) => Some(function),
        _ => None,
    };
    let ink = match function.and_then(Function::unevaluable) {
        Some(_) => colors.text_disabled,
        None => colors.text,
    };
    let text = match function {
        Some(function) => {
            let signature = signature(function);
            let name = chars(function.name())..chars(&signature);
            div()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis_start()
                .child(fuzzy::highlighted(
                    signature.into(),
                    &name,
                    &row.matched,
                    colors.text_muted,
                    palette.highlight.into(),
                ))
                .into_any_element()
        }
        None => {
            let line = pattern_line(completion, row, names, matching, palette);
            line.hinted("text", completion.text.to_owned())
                .into_any_element()
        }
    };
    let aimed = accepts == Some(Accepts::Aimed);
    menu_chrome::clear_of_scrollbar(menu_chrome::aimed_row(
        ("chip-completion", index).into(),
        colors,
        aimed,
    ))
    .when_some(note, |row, note| {
        row.tooltip(TextTooltip::builder(note.into(), colors.tooltip()))
    })
    .w_full()
    .on_click(move |event: &ClickEvent, window, cx| {
        GESTURES.dispatch(Pointer::click(event), |intent| match intent {
            CompletionIntent::Accept => {
                entity.update(cx, |view, cx| accept(view, Some(index), window, cx));
                true
            }
        });
    })
    .child(div().flex_1().min_w_0().text_color(ink).child(text))
    .children(accepts_keys.map(|keys| {
        div()
            .flex_none()
            .px_1()
            .rounded_sm()
            .border_1()
            .border_color(colors.text_muted)
            .text_xs()
            .text_color(colors.text_muted)
            .child(keys)
    }))
    .child(
        div()
            .flex_none()
            .text_xs()
            .text_color(colors.text_muted)
            .child(kind),
    )
}

/// The pattern `completion` offers as `names` shows it, parted where it leads
/// up to what it names, the chars `matching` matched of `row` picked out.
fn pattern_line(
    completion: &Completion,
    row: &Row,
    names: &RepoNames,
    matching: &Matching<Ready<Completions>>,
    palette: &Palette,
) -> LabelLine {
    let shown = names.show(completion.text);
    let matched = matching.picked(&shown, row);
    let text = shown.text();
    let led = || {
        let start = shown.at(completion.path.start)?;
        let end = shown.at(completion.path.end).unwrap_or(chars(text));
        Parts::led(text, start..end)
    };
    let parts = Parts::of_label(text).or_else(led).unwrap_or_default();
    let picked = fuzzy::matched_style(palette.highlight.into());
    LabelLine::parted(shown.into_text(), parts, palette.colors.text_muted).picked(matched, picked)
}

/// How a row reads `function`: its name and Bazel's signature for it, an
/// argument left off marked `?`.
fn signature(function: Function) -> String {
    let signature = function.signature();
    let reads = |argument: ArgType| match argument {
        ArgType::Expr => "expr",
        ArgType::Word => "word",
        ArgType::Int => "int",
    };
    let arguments: Vec<String> = match signature.params {
        Params::Fixed(arguments) => (arguments.iter().enumerate())
            .map(|(at, &argument)| match at < signature.mandatory {
                true => reads(argument).to_owned(),
                false => format!("{}?", reads(argument)),
            })
            .collect(),
        Params::Repeated(argument) => vec![reads(argument).to_owned(), "…".to_owned()],
    };
    format!("{}({})", function.name(), arguments.join(", "))
}

/// Which row the keys accepting a completion accept: the first while none is
/// aimed at, else the row aimed at.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Accepts {
    First,
    Aimed,
}

impl Accepts {
    /// The keys the keymap accepts the row by.
    fn keys(self, holder: Holder, cx: &App) -> Option<SharedString> {
        key_text_within(&AcceptCompletion, &self.stack(holder), cx)
    }

    /// The key contexts the field in `holder` stands in while the keys accept
    /// this row.
    fn stack(self, holder: Holder) -> [KeyContext; 5] {
        completion_stack(holder, self == Accepts::Aimed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bviz_ui_components::{KeyStyle, binding_text, binding_within, text_input};
    use gpui::Keymap;

    /// Tab accepts the first row while none is aimed at, Enter there
    /// committing the field as typed, and Enter accepts a row aimed at.
    #[test]
    fn the_keys_named_for_accepting_follow_the_aim() {
        let mut bindings = crate::bindings();
        bindings.extend(text_input::bindings());
        let keymap = Keymap::new(bindings);
        for holder in Holder::ALL {
            let keys = |accepts: Accepts| {
                binding_within(&keymap, &AcceptCompletion, &accepts.stack(holder))
                    .map(|binding| binding_text(binding, KeyStyle::Words))
            };
            assert_eq!(keys(Accepts::First).as_deref(), Some("Tab"));
            assert_eq!(keys(Accepts::Aimed).as_deref(), Some("Enter"));
        }
    }

    /// What a graph `holding` nodes of those kinds states, every axis
    /// available.
    fn readable(holding: &[Kind]) -> Readable {
        let nodes =
            (holding.iter().enumerate()).map(|(at, &kind)| bviz_graph_layout::graph::Node {
                id: Id(at as u32),
                label: format!("n{at}"),
                kind,
                group: false,
                parent: None,
            });
        let graph = bviz_graph_layout::graph::Graph {
            nodes: nodes.collect(),
            edges: Vec::new(),
        };
        let capabilities = bviz_graph_layout::capability::CapabilitiesBuilder::default().seal();
        Readable::of(&graph, &capabilities)
    }

    /// An extension says it is bviz's own whatever commands the graph was
    /// read from, unless the graph lacks what it reads.
    #[test]
    fn an_extension_is_noted_as_bviz_own_unless_the_graph_lacks_its_data() {
        let with_actions = readable(&[Kind::ConfiguredTarget, Kind::Action]);
        let note = |commands: &[Command], readable| {
            function_note(Function::RunfilesPlumbing, commands, readable)
        };
        let own = ("bviz extension", Some(EXTENSION_NOTE.to_owned()));
        assert_eq!(note(&[Command::Aquery], with_actions), own);
        assert_eq!(note(&[Command::Cquery, Command::Aquery], with_actions), own);
        let without_actions = readable(&[Kind::ConfiguredTarget]);
        assert_eq!(
            note(&[Command::Cquery], without_actions),
            (
                "bviz extension",
                Some("runfiles_plumbing() requires an aquery".to_owned())
            )
        );
    }

    /// A Bazel function the graph's commands read carries no note; one they
    /// do not is noted as evaluated by bviz, or as needing what the graph
    /// lacks.
    #[test]
    fn a_bazel_function_is_noted_only_off_the_graph_commands() {
        let targets = readable(&[Kind::Target]);
        let note = |function, commands: &[Command]| function_note(function, commands, targets);
        assert_eq!(note(Function::Deps, &[Command::Query]), ("function", None));
        assert_eq!(
            note(Function::Config, &[Command::Query, Command::Aquery]),
            (
                "function",
                Some("Not in query or aquery · bviz-evaluated".to_owned())
            )
        );
        assert_eq!(
            note(Function::Inputs, &[Command::Query]),
            ("function", Some("inputs() requires an aquery".to_owned()))
        );
    }

    #[test]
    fn every_gesture_is_reachable_and_fits_the_help() {
        assert_eq!(GESTURES.unreachable(), []);
        assert_eq!(GESTURES.overlong(), []);
    }

    fn offered(labels: &[&str]) -> Vec<(String, String, Named)> {
        patterns(labels.iter().copied())
            .into_iter()
            .map(|pattern| {
                let text = pattern.text.to_string();
                let path: String = text.chars().take(pattern.path.end as usize).collect();
                (text, path, pattern.named)
            })
            .collect()
    }

    fn texts(offered: &[(String, String, Named)]) -> Vec<&str> {
        offered.iter().map(|(text, _, _)| text.as_str()).collect()
    }

    /// A package is offered as every target beneath it and as every target in
    /// it, and a repository as every target beneath its root.
    #[test]
    fn a_label_offers_its_package_and_its_repository() {
        let offered = offered(&["//app/net:client", "@zlib//src:z"]);
        assert_eq!(
            texts(&offered),
            [
                "//app/net/...",
                "//app/net:*",
                "@zlib//src/...",
                "@zlib//src:*",
                "@zlib//...",
            ]
        );
    }

    /// Only the name a pattern names counts as a hit on its name.
    #[test]
    fn the_path_leads_up_to_the_name() {
        let offered = offered(&["//app/net:client", "@zlib//:z"]);
        let path = |text: &str| {
            let (_, path, _) = offered.iter().find(|(held, _, _)| held == text).unwrap();
            path.clone()
        };
        assert_eq!(path("//app/net/..."), "//app/");
        assert_eq!(path("//app/net:*"), "//app/");
        assert_eq!(path("@zlib//..."), "@zlib//");
    }

    /// A repository whose root package is offered already is offered once.
    #[test]
    fn a_repository_with_a_root_package_is_offered_as_that_package() {
        let offered = offered(&["@zlib//:z"]);
        assert_eq!(texts(&offered), ["@zlib//...", "@zlib//:*"]);
        assert!(offered.iter().all(|(_, _, named)| *named == Named::Package));
    }

    /// Down reaches the first row from none aimed at, up from the first
    /// leaves none aimed at, and the aim holds to the list.
    #[test]
    fn the_aim_starts_at_none_and_returns_there() {
        assert_eq!(stepped(None, 1, 5), Some(0));
        assert_eq!(stepped(None, -1, 5), None);
        assert_eq!(stepped(Some(0), -1, 5), None);
        assert_eq!(stepped(Some(3), 1, 5), Some(4));
        assert_eq!(stepped(Some(4), 1, 5), Some(4));
        assert_eq!(stepped(None, 1, 0), None);
    }

    /// The text completing the word at `cursor` of `text` as `pattern`, and
    /// where it stands.
    fn replacement(text: &str, cursor: usize, pattern: &str) -> Option<(Range<usize>, String)> {
        let completion = Completion {
            text: pattern,
            path: 0..0,
            named: Named::Package,
            weight: 0,
        };
        let insertion = insertion(text, cursor, &completion)?;
        assert_eq!(insertion.cursor, insertion.text.len());
        Some((insertion.span, insertion.text.into_owned()))
    }

    fn function(function: Function) -> Completion<'static> {
        Completion {
            text: function.name(),
            path: 0..0,
            named: Named::Function(function),
            weight: 0,
        }
    }

    #[test]
    fn a_completion_replaces_the_word_quoted_where_it_must_be() {
        let text = "deps(//ap) + x";
        let (span, written) = replacement(text, 9, "//app:lib").unwrap();
        assert_eq!((span, &*written), (5..9, "//app:lib"));

        let canonical = "@@rules_rs++crate+crates__cbindgen//:cbindgen";
        let (span, written) = replacement("cbind", 5, canonical).unwrap();
        assert_eq!((span, &*written), (0..5, canonical));

        let (_, written) = replacement("a", 1, "//a:my lib").unwrap();
        assert_eq!(written, "\"//a:my lib\"");

        let (span, _) = replacement("x + \"//a b", 10, "//a b:c").unwrap();
        assert_eq!(span, 4..10);
        assert!(replacement("a |b", 2, "//a").is_none());
    }

    /// A word in a quotation left open is replaced up to where an unquoted
    /// word would end, keeping what follows it.
    #[test]
    fn a_completion_in_an_open_quotation_keeps_what_follows() {
        let text = "deps(\"//ap) + x";
        let (span, written) = replacement(text, 10, "//app:lib").unwrap();
        let mut completed = text.to_owned();
        completed.replace_range(span, &written);
        assert_eq!(completed, "deps(//app:lib) + x");
    }

    /// A function completed where an expression starts is written with its
    /// parentheses and the cursor between them; one completed before its
    /// own `(` is written as its name alone.
    #[test]
    fn a_function_is_written_as_a_call_unless_its_parenthesis_follows() {
        let deps = function(Function::Deps);
        let opened = insertion("x + de", 6, &deps).unwrap();
        assert_eq!(
            opened,
            Insertion {
                span: 4..6,
                text: Cow::Borrowed("deps()"),
                cursor: 5,
            }
        );
        let renamed = insertion("dep(x)", 3, &deps).unwrap();
        assert_eq!(
            (renamed.span, &*renamed.text, renamed.cursor),
            (0..3, "deps", 4)
        );
    }

    /// A function is offered only where a function can stand, only a
    /// function before a `(`, and only a vocabulary's words where an argument
    /// is one of them.
    #[test]
    fn what_a_word_is_offered_follows_where_it_stands() {
        let offers = |place| {
            let Offers { ahead, patterns } = Offers::at(place);
            (ahead, patterns)
        };
        let functions = Some(Ahead::Functions);
        assert_eq!(offers(Place::Operand), (functions, true));
        assert_eq!(offers(Place::Callee), (functions, false));
        assert_eq!(offers(Place::Pattern), (None, true));
        let origins = Place::Word(Vocabulary::RepoCategory);
        assert_eq!(
            offers(origins),
            (Some(Ahead::Words(Vocabulary::RepoCategory)), false)
        );
    }

    /// A word of a vocabulary is offered as itself, written in place of the
    /// word typed.
    #[test]
    fn a_vocabularys_words_are_ranked_and_written_out() {
        let few = |vocabulary: Vocabulary, word: &str| {
            Few::Words(
                vocabulary,
                rank_few(vocabulary.words().iter().copied(), word),
            )
        };
        let texts = |few: &Few| -> Vec<&str> {
            (few.rows().iter())
                .map(|row| few.completion(row).text)
                .collect()
        };
        assert_eq!(texts(&few(Vocabulary::RepoCategory, "mod"))[0], "module");
        assert_eq!(texts(&few(Vocabulary::RuleCategory, "plu")), ["plumbing"]);
        assert_eq!(texts(&few(Vocabulary::Configuration, "any")), ["anyexec"]);
        let module = few(Vocabulary::RepoCategory, "mod");
        let completion = module.completion(&module.rows()[0]);
        let inserted = insertion("repo_category(mod", 17, &completion).unwrap();
        assert_eq!((inserted.span, &*inserted.text), (14..17, "module"));
        let every = few(Vocabulary::RepoCategory, "");
        assert_eq!(texts(&every).len(), Vocabulary::RepoCategory.words().len());
        let completion = every.completion(&every.rows()[0]);
        let inserted = insertion("repo_category()", 14, &completion).unwrap();
        assert_eq!(inserted.span, 14..14);
    }

    /// The functions a word matches are ranked as patterns are, a name the
    /// word starts first.
    #[test]
    fn the_functions_a_word_matches_are_ranked() {
        let names = |word: &str| -> Vec<&str> {
            function_rows(word)
                .iter()
                .map(|row| Function::ALL[row.candidate as usize].name())
                .collect()
        };
        assert_eq!(names("deps")[0], "deps");
        assert_eq!(names("rdeps")[0], "rdeps");
        assert!(names("de").contains(&"rdeps"));
        assert_eq!(names("kin"), ["kind"]);
        assert!(names("//ui/gr").is_empty());
    }

    /// Every function reads as its name and its arguments, an optional one
    /// marked.
    #[test]
    fn a_function_reads_as_its_signature() {
        assert_eq!(signature(Function::Deps), "deps(expr, int?)");
        assert_eq!(signature(Function::Kind), "kind(word, expr)");
        assert_eq!(signature(Function::Rdeps), "rdeps(expr, expr, int?)");
        assert_eq!(signature(Function::RBuildFiles), "rbuildfiles(word, …)");
        for function in Function::ALL {
            assert!(signature(function).starts_with(&format!("{}(", function.name())));
        }
    }
}
