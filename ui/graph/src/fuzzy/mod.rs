//! Fuzzy matching of a query against candidates a list offers, run off the UI
//! thread, and the rows it ranks drawn with the matched characters picked out.

mod rank;
mod workers;

use bviz_label::shown::Shown;
use gpui::{
    BackgroundExecutor, Context, HighlightStyle, Hsla, Pixels, Rems, SharedString, StyledText,
    Task, UnderlineStyle, px, rems,
};
use nucleo::pattern::{CaseMatching, Normalization, Pattern};
use nucleo::{Config, Matcher, Status, Utf32Str, Utf32String};
pub(crate) use rank::{Row, Standing};
use std::cell::RefCell;
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// How often a running match is polled for what it has found, often enough
/// for the rows to keep pace with typing.
const POLL: Duration = Duration::from_millis(16);

/// The weight of the line under a row's matched characters.
const MATCH_UNDERLINE: Pixels = px(1.);

/// The size a list of matches is set in, which its rows are measured at.
pub(crate) const TEXT_SIZE: Rems = rems(0.875);

/// The matcher's settings: every candidate's text reads as a path.
fn config() -> Config {
    let mut config = Config::DEFAULT;
    config.set_match_paths();
    config
}

/// The engine a [`Matching`] matches on.
type Chosen = workers::Workers;

/// What matches a query against the candidates handed to it, by index, and
/// keeps the latest result.
trait Engine {
    /// What candidates are handed to the engine through.
    type Sink: Sink + Send + 'static;

    fn new(executor: &BackgroundExecutor) -> Self;

    /// Matches `query` in place of the query last matched.
    fn search(&mut self, query: &str);

    /// Drops every candidate, to be handed afresh through the sink returned.
    fn restart(&mut self) -> Self::Sink;

    /// Advances the match without waiting on it, reporting whether its result
    /// changed and whether it still runs.
    fn tick(&mut self) -> Status;

    /// How many candidates the latest result matches.
    fn matched_count(&self) -> u32;

    /// How many candidates the engine holds.
    fn item_count(&self) -> u32;

    /// The candidates the latest result matches, best scored first, each by
    /// its index and its text.
    fn found(&self) -> impl Iterator<Item = (u32, Utf32Str<'_>)>;

    /// The pattern the latest result was matched by.
    fn pattern(&self) -> &Pattern;
}

/// Where an engine's candidates are handed to it.
trait Sink {
    /// Hands the engine the candidate `index` reading `text`.
    fn hand(&self, index: u32, text: &str);
}

/// Reaches the matching `view` holds, while it holds one.
pub(crate) type Locate<V, S> = fn(&mut V) -> Option<&mut Matching<S>>;

/// Takes up a result of a matching that changed since the last poll.
pub(crate) type TakeUp<V> = fn(&mut V, &mut Context<V>);

/// A matcher over candidates handed to it by index into a source of type `S`,
/// with the rows its latest result ranks.
pub(crate) struct Matching<S> {
    engine: Chosen,
    /// Finds the characters each row's query matched, for ranking the rows
    /// and highlighting them, on a row's text as written and as shown.
    matcher: RefCell<Matcher>,
    /// The pattern the rows were ranked by.
    pattern: Pattern,
    /// The query last handed to the matcher, which an edit extending it
    /// narrows rather than matches afresh.
    query: SharedString,
    /// The query the rows were ranked for, which trails `query` until the
    /// matcher is next polled.
    ranked: SharedString,
    /// Absent until the source being handed to the matcher has landed.
    landed: Option<Landed<S>>,
    /// How many candidates the query matches, of which the rows list the
    /// first.
    matched: u32,
    /// Whether the last pass the matcher was polled on is still running.
    running: bool,
    /// Whether candidates are still being handed to the matcher.
    streaming: bool,
    _injecting: Injecting,
    _polling: Task<()>,
}

/// The source a matcher's candidates index into, and the rows ranked of
/// them.
struct Landed<S> {
    source: S,
    rows: Vec<Row>,
}

/// The matcher engine a panel keeps while no list of its holds it, so that a
/// list opened takes it up rather than starting an engine of its own.
#[derive(Default)]
pub(crate) struct Pool(Option<Chosen>);

impl Pool {
    /// A matching with no query, on the parked engine where one is parked,
    /// and otherwise on a new one.
    pub(crate) fn take<S>(&mut self, executor: &BackgroundExecutor) -> Matching<S> {
        Matching::new(self.0.take().unwrap_or_else(|| Chosen::new(executor)))
    }

    /// Parks the engine of `matching`, which stops matching; an engine parked
    /// already stays, and the other is dropped.
    pub(crate) fn park<S>(&mut self, matching: Matching<S>) {
        self.0.get_or_insert(matching.engine);
    }
}

impl<S> Matching<S> {
    fn new(mut engine: Chosen) -> Self {
        engine.search("");
        Matching {
            engine,
            matcher: RefCell::new(Matcher::new(config())),
            pattern: Pattern::default(),
            query: SharedString::default(),
            ranked: SharedString::default(),
            landed: None,
            matched: 0,
            running: false,
            streaming: false,
            _injecting: Injecting::none(),
            _polling: Task::ready(()),
        }
    }

    pub(crate) fn query(&self) -> &SharedString {
        &self.query
    }

    pub(crate) fn rows(&self) -> &[Row] {
        self.landed.as_ref().map_or(&[], |landed| &landed.rows)
    }

    /// The row at `at`, with the source its candidate indexes into.
    pub(crate) fn row(&self, at: usize) -> Option<(&S, &Row)> {
        let landed = self.landed.as_ref()?;
        Some((&landed.source, landed.rows.get(at)?))
    }

    pub(crate) fn matched(&self) -> u32 {
        self.matched
    }

    pub(crate) fn streaming(&self) -> bool {
        self.streaming
    }

    /// Whether the rows were ranked for the query last handed to the matcher.
    pub(crate) fn current(&self) -> bool {
        self.ranked == self.query
    }

    /// Hands the matcher `query`; reports whether it differs from the last.
    pub(crate) fn set_query(&mut self, query: SharedString) -> bool {
        if query == self.query {
            return false;
        }
        self.engine.search(&query);
        self.query = query;
        true
    }

    /// Reports whether the matcher's result changed since the last poll,
    /// without waiting on it.
    fn tick(&mut self) -> bool {
        let status = self.engine.tick();
        self.running = status.running;
        status.changed
    }

    /// Whether the matcher runs or is still handed candidates.
    fn at_work(&self) -> bool {
        self.running || self.streaming
    }

    /// Lists the rows the matcher's latest result ranks first, as `standing`
    /// tells of each candidate's index into the source; it is asked only of an
    /// index handed to the matcher.
    pub(crate) fn rerank(&mut self, standing: impl Fn(&S, u32) -> Standing) {
        self.ranked = self.query.clone();
        self.matched = self.engine.matched_count();
        let Some(Landed { source, rows }) = &mut self.landed else {
            return;
        };
        if self.query.is_empty() || self.engine.item_count() == 0 {
            rows.clear();
            return;
        }
        self.pattern = self.engine.pattern().clone();
        *rows = rank::rank(
            self.engine.found(),
            &self.query,
            &self.pattern,
            self.matcher.get_mut(),
            |index| standing(source, index),
        );
    }
}

/// Hands candidates to a matcher off the UI thread.
pub(crate) struct Feeder {
    sink: <Chosen as Engine>::Sink,
    stopped: Arc<AtomicBool>,
}

impl Feeder {
    /// Hands the matcher the candidate `index` reading `text`; reports whether
    /// the matcher still takes candidates from this feeder.
    pub(crate) fn push(&self, index: u32, text: &str) -> bool {
        if self.stopped.load(Ordering::Relaxed) {
            return false;
        }
        self.sink.hand(index, text);
        true
    }
}

/// Candidates being handed to the matcher off the UI thread, stopped once
/// this is dropped: a restarted matcher reads nothing handed to it through an
/// injector taken before.
struct Injecting {
    stopped: Arc<AtomicBool>,
    _task: Task<()>,
}

impl Injecting {
    fn none() -> Self {
        Injecting {
            stopped: Arc::default(),
            _task: Task::ready(()),
        }
    }
}

impl Drop for Injecting {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
    }
}

/// Hands the matching `locate` reaches the source `prepare` yields and the
/// candidates `feed` pushes from it, in place of any it held, off the UI
/// thread, polling it through `take_up`. The source lands before the first
/// candidate is pushed, so every row ranked can be read from it.
pub(crate) fn inject<V: 'static, S: Clone + Send + 'static>(
    view: &mut V,
    locate: Locate<V, S>,
    take_up: TakeUp<V>,
    prepare: impl FnOnce() -> S + Send + 'static,
    feed: impl FnOnce(&S, &Feeder) + Send + 'static,
    cx: &mut Context<V>,
) {
    let Some(matching) = locate(view) else {
        return;
    };
    let sink = matching.engine.restart();
    matching.landed = None;
    matching.matched = 0;
    matching.streaming = true;
    let feeder = Feeder {
        sink,
        stopped: Arc::default(),
    };
    let stopped = Arc::clone(&feeder.stopped);
    let task = cx.spawn(async move |view, cx| {
        let source = cx
            .background_executor()
            .spawn(async move { prepare() })
            .await;
        let fed = source.clone();
        let landed = view.update(cx, |view, _| {
            let matching = locate(view)?;
            matching.landed = Some(Landed {
                source,
                rows: Vec::new(),
            });
            Some(())
        });
        if let Ok(Some(())) = landed {
            cx.background_executor()
                .spawn(async move { feed(&fed, &feeder) })
                .await;
        }
        let _ = view.update(cx, |view, cx| {
            if let Some(matching) = locate(view) {
                matching.streaming = false;
            }
            poll(view, locate, take_up, cx);
        });
    });
    matching._injecting = Injecting {
        stopped,
        _task: task,
    };
    poll(view, locate, take_up, cx);
}

/// Polls the matching `locate` reaches every [`POLL`] through `take_up`, for
/// as long as it runs or is handed candidates.
pub(crate) fn poll<V: 'static, S: 'static>(
    view: &mut V,
    locate: Locate<V, S>,
    take_up: TakeUp<V>,
    cx: &mut Context<V>,
) {
    let Some(matching) = locate(view) else {
        return;
    };
    matching._polling = cx.spawn(async move |view, cx| {
        loop {
            let running = view.update(cx, |view, cx| {
                let Some(matching) = locate(view) else {
                    return false;
                };
                if matching.tick() || !matching.current() {
                    take_up(view, cx);
                }
                locate(view).is_some_and(|matching| matching.at_work())
            });
            if !running.unwrap_or(false) {
                break;
            }
            cx.background_executor().timer(POLL).await;
        }
    });
}

/// The rows for `query` over the few `candidates`, each given by its index
/// and its text, ranked as a [`Matching`] ranks, on the calling thread: for a
/// list short enough that matching it costs less than handing it off.
pub(crate) fn rank_few<'a>(
    candidates: impl Iterator<Item = (u32, &'a str)>,
    query: &str,
    standing: impl Fn(u32) -> Standing,
) -> Vec<Row> {
    let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    let texts: Vec<(u32, Utf32String)> = candidates
        .map(|(index, text)| (index, Utf32String::from(text)))
        .collect();
    let mut matcher = Matcher::new(config());
    let mut found: Vec<(u32, u32, Utf32Str<'_>)> = Vec::with_capacity(texts.len());
    found.extend(texts.iter().filter_map(|(index, text)| {
        let score = pattern.score(text.slice(..), &mut matcher)?;
        Some((score, *index, text.slice(..)))
    }));
    found.sort_by_key(|&(score, index, _)| (std::cmp::Reverse(score), index));
    let found = found.into_iter().map(|(_, index, text)| (index, text));
    rank::rank(found, query, &pattern, &mut matcher, standing)
}

impl<S> Matching<S> {
    /// The chars of `shown`, a row's text as it is shown, that pick out what
    /// the query matched of `row`'s text as written, ascending: where each
    /// matched char stands in `shown` while every one of them shows there, and
    /// otherwise the chars of `shown` the query matches, where it matches.
    pub(crate) fn picked(&self, shown: &Shown, row: &Row) -> Vec<u32> {
        picked(
            shown,
            &row.matched,
            &self.pattern,
            &mut self.matcher.borrow_mut(),
        )
    }
}

/// [`Matching::picked`] for `matched`, the chars of the text as written that
/// `pattern` matched, as `matcher` matches.
fn picked(shown: &Shown, matched: &[u32], pattern: &Pattern, matcher: &mut Matcher) -> Vec<u32> {
    if matched.iter().all(|&at| shown.at(at).is_some()) {
        return shown_matches(shown, matched);
    }
    let mut chars = Vec::new();
    let text = Utf32Str::new(shown.text(), &mut chars);
    let mut rematched: Vec<u32> = Vec::with_capacity(matched.len());
    match pattern.indices(text, matcher, &mut rematched) {
        Some(_) => {
            rematched.sort_unstable();
            rematched.dedup();
            rematched
        }
        None => shown_matches(shown, matched),
    }
}

/// The chars of `shown` standing for the chars `matched` of the text as it
/// is written, ascending. A shorthand shows a name's chars where its place
/// reads them, ahead of the chars of the place matched before them.
fn shown_matches(shown: &Shown, matched: &[u32]) -> Vec<u32> {
    let mut shown_matched: Vec<u32> = matched.iter().filter_map(|&at| shown.at(at)).collect();
    shown_matched.sort_unstable();
    shown_matched.dedup();
    shown_matched
}

/// `text` with its `path` muted and the `matched` chars picked out over it.
pub(crate) fn highlighted(
    text: SharedString,
    path: &Range<u32>,
    matched: &[u32],
    muted: Hsla,
    picked: Hsla,
) -> StyledText {
    let highlights = highlights(&text, path, matched, muted, picked);
    StyledText::new(text).with_highlights(highlights)
}

/// The byte ranges of `text` that [`highlighted`] draws apart, each with how.
pub(crate) fn highlights(
    text: &str,
    path: &Range<u32>,
    matched: &[u32],
    muted: Hsla,
    picked: Hsla,
) -> Vec<(Range<usize>, HighlightStyle)> {
    styled_runs(text, path, matched)
        .into_iter()
        .map(|(range, style)| {
            let highlight = match style {
                Style::Path => HighlightStyle {
                    color: Some(muted),
                    ..HighlightStyle::default()
                },
                Style::Matched => matched_style(picked),
            };
            (range, highlight)
        })
        .collect()
}

/// How the chars a query matched are picked out, in `picked`: by colour and
/// an underline but in the row's own weight, since a row is cut to fit as
/// measured in that weight, so a wider one would run past the cut and be
/// clipped unmarked.
pub(crate) fn matched_style(picked: Hsla) -> HighlightStyle {
    HighlightStyle {
        color: Some(picked),
        underline: Some(UnderlineStyle {
            thickness: MATCH_UNDERLINE,
            color: Some(picked),
            wavy: false,
        }),
        ..HighlightStyle::default()
    }
}

/// How a run of a row's text is drawn apart from the candidate's name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Style {
    Path,
    Matched,
}

/// The byte ranges of `text` drawn apart from the name: the chars in `path` as
/// path and the `matched` chars as matched, a matched char in the path drawn
/// matched.
fn styled_runs(text: &str, path: &Range<u32>, matched: &[u32]) -> Vec<(Range<usize>, Style)> {
    let mut runs: Vec<(Range<usize>, Style)> = Vec::with_capacity(matched.len() + 1);
    for (index, (at, c)) in (0u32..).zip(text.char_indices()) {
        let style = if matched.binary_search(&index).is_ok() {
            Some(Style::Matched)
        } else if path.contains(&index) {
            Some(Style::Path)
        } else {
            None
        };
        let Some(style) = style else {
            continue;
        };
        let end = at + c.len_utf8();
        match runs.last_mut() {
            Some((range, last)) if *last == style && range.end == at => range.end = end,
            _ => runs.push((at..end, style)),
        }
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::*;
    use bviz_label::names::RepoNames;

    /// The chars of `written` as `names` shows it that [`picked`] picks out
    /// for `query`, matched against `written` as a list matches it.
    fn picked_for(names: &RepoNames, written: &str, query: &str) -> Vec<u32> {
        let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
        let mut matcher = Matcher::new(config());
        let mut chars = Vec::new();
        let mut matched = Vec::new();
        let text = Utf32Str::new(written, &mut chars);
        pattern.indices(text, &mut matcher, &mut matched).unwrap();
        matched.sort_unstable();
        matched.dedup();
        picked(&names.show(written), &matched, &pattern, &mut matcher)
    }

    /// A query matching the text as written in the part of a repository's
    /// name its shown name leaves out picks out what it matches of the text
    /// shown, its `@` the `@` shown.
    #[test]
    fn a_match_in_a_part_left_out_is_found_in_the_text_shown() {
        let written = "@@llvm++glibc+glibc//:glibc_crt1.object";
        let names = RepoNames::of_labels([written].into_iter());
        assert_eq!(names.show(written).text(), "@glibc//:glibc_crt1.object");
        assert_eq!(picked_for(&names, written, "@glibc"), [0, 1, 2, 3, 4, 5]);
        let picked = picked_for(&names, written, "glibc");
        assert_eq!(picked.len(), "glibc".len());
        assert!(picked.iter().all(|&at| at > 0));
    }

    /// Where the query matches nothing of the text shown, as one naming the
    /// part a shortening leaves out does, the chars that show of those it
    /// matched are picked out, and none in their stead.
    #[test]
    fn a_match_the_text_shown_lacks_keeps_what_shows_of_it() {
        let written = "@@llvm++glibc+glibc//:crt1";
        let names = RepoNames::of_labels([written].into_iter());
        assert_eq!(names.show(written).text(), "@glibc//:crt1");
        assert_eq!(picked_for(&names, written, "@@llvm"), [0]);
    }

    /// A match whose every char shows keeps where it fell, even where the
    /// text shown reads it elsewhere too.
    #[test]
    fn a_match_that_shows_is_picked_out_where_it_fell() {
        let written = "//a/lib:lib";
        let names = RepoNames::of_labels([written].into_iter());
        assert_eq!(names.show(written).text(), "//a/lib");
        assert_eq!(picked_for(&names, written, "a"), [2]);
    }

    #[test]
    fn the_path_is_muted_and_the_matched_chars_picked_out_over_it() {
        assert_eq!(
            styled_runs("//a:lib", &(0..4), &[1, 4, 5]),
            [
                (0..1, Style::Path),
                (1..2, Style::Matched),
                (2..4, Style::Path),
                (4..6, Style::Matched),
            ]
        );
    }

    #[test]
    fn a_path_inside_the_text_is_muted_alone() {
        assert_eq!(
            styled_runs("Javac out/lib.jar", &(6..10), &[0]),
            [(0..1, Style::Matched), (6..10, Style::Path)]
        );
    }

    #[test]
    fn a_run_is_measured_in_bytes_where_its_chars_are_counted() {
        assert_eq!(
            styled_runs("é:é", &(0..2), &[2]),
            [(0..3, Style::Path), (3..5, Style::Matched)]
        );
    }
}
