//! The matcher engine of a build without threads: matching on the UI thread,
//! a slice of the candidates per tick, so that typing stays responsive while
//! a large list is matched.

use super::rank::MAX_WINDOW;
use super::{Engine, Offered, Sink, config};
use gpui::BackgroundExecutor;
use nucleo::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
use nucleo::{Matcher, Status, Utf32Str, Utf32String};
use std::cmp::Reverse;
use std::mem;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

/// How long a tick matches, or a feed hands over candidates, before handing
/// the thread back: a share of a frame small enough that the frame still
/// draws on time.
const SLICE: Duration = Duration::from_millis(6);

/// How many candidates are matched or handed over between reads of the clock.
const CLOCK_STRIDE: usize = 128;

/// A candidate: the index it was handed by, and its text.
pub(super) struct Item {
    index: u32,
    text: Utf32String,
}

/// Candidates handed to the engine and not yet taken in by a tick.
pub(super) type Inbox = Arc<Mutex<Vec<Item>>>;

/// A matcher matching a slice of its candidates per tick, which reports the
/// result of the last pass to reach the end of the candidates until the next
/// one does.
pub(super) struct Sliced {
    items: Vec<Item>,
    inbox: Inbox,
    matcher: Matcher,
    /// The pattern the running pass matches.
    pattern: Pattern,
    pass: Pass,
    reported: Reported,
    /// Whether the pass found or was asked anything the report lacks.
    unreported: bool,
}

/// One pattern's way through the candidates: first those of `among` it was
/// narrowed to, then every candidate from `tail` on, through those handed in
/// while it runs.
#[derive(Default)]
struct Pass {
    /// Positions in the items.
    among: Vec<u32>,
    /// How many of `among` were matched.
    next: usize,
    /// The first position past `among` not yet matched.
    tail: usize,
    /// The score and position of each candidate matched.
    found: Vec<(u32, u32)>,
}

impl Pass {
    /// The position of the next candidate to match, while one is left.
    fn advance(&mut self, items: usize) -> Option<u32> {
        if let Some(&at) = self.among.get(self.next) {
            self.next += 1;
            return Some(at);
        }
        (self.tail < items).then(|| {
            self.tail += 1;
            (self.tail - 1) as u32
        })
    }

    fn done(&self, items: usize) -> bool {
        self.next == self.among.len() && self.tail == items
    }

    /// The pass of a pattern matching no candidate this one leaves out: over
    /// those this one found and those it has yet to match.
    fn narrowed(self) -> Pass {
        let mut among: Vec<u32> =
            Vec::with_capacity(self.found.len() + self.among.len() - self.next);
        among.extend(self.found.iter().map(|&(_, at)| at));
        among.extend_from_slice(&self.among[self.next..]);
        Pass {
            among,
            next: 0,
            tail: self.tail,
            found: Vec::new(),
        }
    }
}

/// What a pass found by the time it reached the end of the candidates.
#[derive(Default)]
struct Reported {
    pattern: Pattern,
    /// The score and position of each candidate matched; once `ordered`, the
    /// best of them first and in order, as far as a ranking reads.
    found: Vec<(u32, u32)>,
    ordered: bool,
}

impl Sliced {
    fn empty() -> Self {
        Sliced {
            items: Vec::new(),
            inbox: Inbox::default(),
            matcher: Matcher::new(config()),
            pattern: Pattern::default(),
            pass: Pass::default(),
            reported: Reported::default(),
            unreported: true,
        }
    }

    /// Matches until `stop` says to, taking in what was handed first, and
    /// reports the pass once it reaches the end of the candidates.
    fn run(&mut self, mut stop: impl FnMut() -> bool) -> Status {
        let handed = mem::take(&mut *self.inbox.lock().unwrap_or_else(PoisonError::into_inner));
        self.items.extend(handed);
        let before = self.pass.found.len();
        'slice: loop {
            for _ in 0..CLOCK_STRIDE {
                let Some(at) = self.pass.advance(self.items.len()) else {
                    break 'slice;
                };
                let text = self.items[at as usize].text.slice(..);
                if let Some(score) = self.pattern.score(text, &mut self.matcher) {
                    self.pass.found.push((score, at));
                }
            }
            if stop() {
                break;
            }
        }
        self.unreported |= self.pass.found.len() != before;
        let done = self.pass.done(self.items.len());
        let changed = done && mem::take(&mut self.unreported);
        if changed {
            self.reported.pattern.clone_from(&self.pattern);
            self.reported.found.clone_from(&self.pass.found);
            self.reported.ordered = false;
        }
        Status {
            changed,
            running: !done,
        }
    }
}

impl Reported {
    /// Puts the best of what was found first and in order, best scored first
    /// and then by index, as far as a ranking reads.
    fn order(&mut self, items: &[Item]) {
        if mem::replace(&mut self.ordered, true) {
            return;
        }
        let key = |&(score, at): &(u32, u32)| (Reverse(score), items[at as usize].index);
        let found = &mut self.found;
        if found.len() > MAX_WINDOW {
            found.select_nth_unstable_by_key(MAX_WINDOW, key);
        }
        let read = found.len().min(MAX_WINDOW);
        found[..read].sort_unstable_by_key(key);
    }
}

impl Engine for Sliced {
    type Sink = Inbox;

    fn new(_: &BackgroundExecutor) -> Self {
        Sliced::empty()
    }

    fn search(&mut self, query: &str) {
        let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
        let pass = mem::take(&mut self.pass);
        self.pass = match narrows(&self.pattern, &pattern) {
            true => pass.narrowed(),
            false => Pass::default(),
        };
        self.pattern = pattern;
        self.unreported = true;
    }

    fn restart(&mut self) -> Inbox {
        self.items.clear();
        self.inbox = Inbox::default();
        self.pass = Pass::default();
        self.reported.found.clear();
        self.unreported = true;
        Arc::clone(&self.inbox)
    }

    fn feed<S: Send + 'static>(
        sink: Inbox,
        source: S,
        offer: impl Fn(&S) -> Offered<'_> + Send + 'static,
        stopped: Arc<AtomicBool>,
        executor: &BackgroundExecutor,
    ) -> impl Future<Output = ()> + 'static {
        let executor = executor.clone();
        async move {
            let mut offered = offer(&source);
            loop {
                let deadline = bviz_time::now() + SLICE;
                while bviz_time::now() < deadline {
                    for _ in 0..CLOCK_STRIDE {
                        if stopped.load(Ordering::Relaxed) {
                            return;
                        }
                        let Some((index, text)) = offered.next() else {
                            return;
                        };
                        sink.hand(index, text);
                    }
                }
                executor.timer(Duration::ZERO).await;
            }
        }
    }

    fn tick(&mut self) -> Status {
        let deadline = bviz_time::now() + SLICE;
        self.run(|| bviz_time::now() >= deadline)
    }

    fn matched_count(&self) -> u32 {
        self.reported.found.len() as u32
    }

    fn item_count(&self) -> u32 {
        self.items.len() as u32
    }

    fn found(&mut self) -> impl Iterator<Item = (u32, Utf32Str<'_>)> {
        self.reported.order(&self.items);
        let found = &self.reported.found;
        found[..found.len().min(MAX_WINDOW)].iter().map(|&(_, at)| {
            let item = &self.items[at as usize];
            (item.index, item.text.slice(..))
        })
    }

    fn pattern(&self) -> &Pattern {
        &self.reported.pattern
    }
}

impl Sink for Inbox {
    fn hand(&self, index: u32, text: &str) {
        let item = Item {
            index,
            text: text.into(),
        };
        self.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(item);
    }
}

/// Whether every candidate `new` matches is one `old` matches, as far as their
/// atoms tell: `new` repeats every atom of `old` but the last, then extends
/// its needle where a longer needle can only match less, and may add atoms.
fn narrows(old: &Pattern, new: &Pattern) -> bool {
    let Some((last, before)) = old.atoms.split_last() else {
        return true;
    };
    let Some(extended) = new.atoms.get(before.len()) else {
        return false;
    };
    let extends = matches!(
        last.kind,
        AtomKind::Fuzzy | AtomKind::Substring | AtomKind::Prefix
    ) && extended.kind == last.kind
        && !last.negative
        && !extended.negative
        && extended.needle_text().len() >= last.needle_text().len()
        && extended.needle_text().slice(..last.needle_text().len()) == last.needle_text();
    extends && new.atoms.starts_with(before)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(query: &str) -> Pattern {
        Pattern::parse(query, CaseMatching::Smart, Normalization::Smart)
    }

    fn narrowing(old: &str, new: &str) -> bool {
        narrows(&parse(old), &parse(new))
    }

    /// Texts with the characters the queries below turn on.
    const TEXTS: &[&str] = &[
        "foo", "foox", "foo$x", "a\\", "a\\ b", "a b", "a$", "a\\$", "ab", "b", "^a", "'a", "Abc",
        "aBc", "abc", "xa", "x'a",
    ];

    /// How many times the candidates repeat [`TEXTS`], enough that a pass
    /// stopped after its first stride is left part way.
    const REPEATS: usize = 20;

    /// The candidates: [`TEXTS`] over and over, [`REPEATS`] times.
    fn candidates() -> impl Iterator<Item = (u32, &'static str)> {
        (0..).zip(TEXTS.iter().copied().cycle().take(TEXTS.len() * REPEATS))
    }

    /// The candidates an engine run to rest finds for each of `queries` typed
    /// in turn, in the order found.
    fn typed(queries: &[&str]) -> Vec<u32> {
        assert!(TEXTS.len() * REPEATS > 2 * CLOCK_STRIDE);
        let mut engine = Sliced::empty();
        let sink = engine.restart();
        for (index, text) in candidates() {
            sink.hand(index, text);
        }
        let mut stops = 0;
        for query in queries {
            engine.search(query);
            // A stop after every stride leaves each pass part way.
            engine.run(|| {
                stops += 1;
                true
            });
        }
        while engine.run(|| false).running {}
        assert!(stops > 0);
        engine.found().map(|(index, _)| index).collect()
    }

    /// The candidates `query` matches, best scored first and then by index.
    fn matched(query: &str) -> Vec<u32> {
        let pattern = parse(query);
        let mut matcher = Matcher::new(config());
        let mut found: Vec<(u32, u32)> = candidates()
            .filter_map(|(index, text)| {
                let text = Utf32String::from(text);
                Some((pattern.score(text.slice(..), &mut matcher)?, index))
            })
            .collect();
        found.sort_by_key(|&(score, index)| (Reverse(score), index));
        found.into_iter().map(|(_, index)| index).collect()
    }

    /// Each query typed after the other finds what it finds typed alone.
    fn finds_as_typed_alone(old: &str, new: &str) {
        assert!(!matched(new).is_empty());
        assert_eq!(typed(&[old, new]), matched(new), "{old:?} then {new:?}");
    }

    #[test]
    fn a_longer_needle_narrows() {
        assert!(narrowing("fo", "foo"));
        assert!(narrowing("'fo", "'foo"));
        assert!(narrowing("^fo", "^foo"));
        assert!(narrowing("a", "a b"));
        assert!(narrowing("a b", "a bc"));
        finds_as_typed_alone("fo", "foo");
        finds_as_typed_alone("a", "a b");
    }

    #[test]
    fn a_postfix_extended_past_its_end_does_not_narrow() {
        assert!(!narrowing("foo$", "foo$x"));
        finds_as_typed_alone("foo$", "foo$x");
    }

    #[test]
    fn an_escape_completed_does_not_narrow() {
        assert!(!narrowing("a\\", "a\\ b"));
        assert!(!narrowing("a\\", "a\\$"));
        finds_as_typed_alone("a\\", "a\\ b");
        finds_as_typed_alone("a\\", "a\\$");
    }

    #[test]
    fn a_negation_extended_does_not_narrow() {
        assert!(!narrowing("!a", "!ab"));
        finds_as_typed_alone("!a", "!ab");
    }

    #[test]
    fn a_bare_operator_matches_everything_and_so_narrows() {
        assert!(narrowing("^", "^a"));
        assert!(narrowing("'", "'a"));
        assert!(narrowing("!", "!a"));
        finds_as_typed_alone("^", "^a");
        finds_as_typed_alone("'", "'a");
        finds_as_typed_alone("!", "!a");
    }

    #[test]
    fn an_operator_typed_after_a_needle_does_not_narrow() {
        assert!(!narrowing("a", "a$"));
        finds_as_typed_alone("a", "a$");
    }

    #[test]
    fn an_earlier_atom_changed_does_not_narrow() {
        assert!(!narrowing("a b", "ax b"));
        assert!(!narrowing("abc", "ab"));
        finds_as_typed_alone("abc", "ab");
    }

    #[test]
    fn an_uppercase_letter_typed_narrows_to_its_case() {
        assert!(narrowing("a", "aB"));
        finds_as_typed_alone("a", "aB");
    }

    /// A query typed leaves the result of the last before it standing until
    /// its own pass has been through every candidate.
    #[test]
    fn a_query_reports_nothing_until_its_pass_ends() {
        let mut engine = Sliced::empty();
        let sink = engine.restart();
        for (index, text) in candidates() {
            sink.hand(index, text);
        }
        engine.search("foo");
        while engine.run(|| false).running {}
        let before = engine.matched_count();
        engine.search("b");
        let partway = engine.run(|| true);
        assert!(partway.running && !partway.changed);
        assert_eq!(engine.matched_count(), before);
        while engine.run(|| false).running {}
        let found: Vec<u32> = engine.found().map(|(index, _)| index).collect();
        assert_eq!(found, matched("b"));
    }

    #[test]
    fn candidates_handed_mid_pass_are_matched() {
        let mut engine = Sliced::empty();
        let sink = engine.restart();
        sink.hand(0, "foo");
        engine.search("fo");
        while engine.run(|| false).running {}
        sink.hand(1, "fox");
        engine.search("foo");
        sink.hand(2, "food");
        while engine.run(|| false).running {}
        assert_eq!(engine.matched_count(), 2);
        let mut found: Vec<u32> = engine.found().map(|(index, _)| index).collect();
        found.sort_unstable();
        assert_eq!(found, [0, 2]);
    }
}
