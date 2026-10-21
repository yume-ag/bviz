//! The matcher engine of a build with threads: nucleo's, matching on a pool
//! of worker threads.

use super::{Engine, Sink, config};
use gpui::BackgroundExecutor;
use nucleo::pattern::{CaseMatching, Normalization, Pattern};
use nucleo::{Injector, Nucleo, Status, Utf32Str};
use std::mem::ManuallyDrop;
use std::sync::Arc;

/// A matcher and its worker pool, dropped on the background executor, since
/// dropping a [`Nucleo`] waits for its worker to stop.
pub(super) struct Workers {
    nucleo: ManuallyDrop<Nucleo<u32>>,
    /// The query last matched, none before the first.
    query: Option<String>,
    executor: BackgroundExecutor,
}

impl Engine for Workers {
    type Sink = Injector<u32>;

    fn new(executor: &BackgroundExecutor) -> Self {
        let workers = Some(bviz_parallel::worker_count());
        Workers {
            nucleo: ManuallyDrop::new(Nucleo::new(config(), Arc::new(|| {}), workers, 1)),
            query: None,
            executor: executor.clone(),
        }
    }

    fn search(&mut self, query: &str) {
        search(&mut self.nucleo, query, self.query.as_deref());
        self.query = Some(query.to_owned());
    }

    fn restart(&mut self) -> Injector<u32> {
        self.nucleo.restart(true);
        self.nucleo.injector()
    }

    fn tick(&mut self) -> Status {
        self.nucleo.try_tick()
    }

    fn matched_count(&self) -> u32 {
        self.nucleo.snapshot().matched_item_count()
    }

    fn item_count(&self) -> u32 {
        self.nucleo.snapshot().item_count()
    }

    fn found(&self) -> impl Iterator<Item = (u32, Utf32Str<'_>)> {
        (self.nucleo.snapshot().matched_items(..))
            .map(|item| (*item.data, item.matcher_columns[0].slice(..)))
    }

    fn pattern(&self) -> &Pattern {
        self.nucleo.snapshot().pattern().column_pattern(0)
    }
}

/// Hands `nucleo` `query` in place of `previous`, narrowing the last result
/// where the query extends the previous one.
fn search(nucleo: &mut Nucleo<u32>, query: &str, previous: Option<&str>) {
    let narrowing = previous.is_some_and(|previous| query.starts_with(previous));
    nucleo.pattern.reparse(
        0,
        query,
        CaseMatching::Smart,
        Normalization::Smart,
        narrowing,
    );
}

impl Sink for Injector<u32> {
    fn hand(&self, index: u32, text: &str) {
        self.push(index, |_, columns| columns[0] = text.into());
    }
}

impl Drop for Workers {
    fn drop(&mut self) {
        // SAFETY: `nucleo` is not touched again.
        let nucleo = unsafe { ManuallyDrop::take(&mut self.nucleo) };
        self.executor.spawn(async move { drop(nucleo) }).detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nucleo::Matcher;

    /// Enough candidates that a match pass can still be running when the next
    /// query is typed.
    const CANDIDATES: u32 = 100_000;

    /// How many packages the candidates spread over.
    const PACKAGES: u32 = 97;

    /// Queries typed in quick succession, each polled once with `try_tick`,
    /// end on the last query's matches once polling finds the matcher at rest.
    #[test]
    fn polling_with_try_tick_reaches_the_last_query() {
        let workers = Some(bviz_parallel::worker_count());
        let mut nucleo: Nucleo<u32> = Nucleo::new(config(), Arc::new(|| {}), workers, 1);
        let texts: Vec<String> = (0..CANDIDATES)
            .map(|index| format!("//pkg{}/sub:target_{index}", index % PACKAGES))
            .collect();
        let injector = nucleo.injector();
        for (index, text) in (0..).zip(&texts) {
            injector.hand(index, text);
        }
        let queries = ["p", "pk", "pkg1", "pkg1/s", "t", "ta", "tar3"];
        let mut typed = "";
        for query in queries {
            search(&mut nucleo, query, Some(typed));
            nucleo.try_tick();
            typed = query;
        }
        while nucleo.try_tick().running {
            std::thread::yield_now();
        }
        let pattern = Pattern::parse(typed, CaseMatching::Smart, Normalization::Smart);
        let mut matcher = Matcher::new(config());
        let mut chars = Vec::new();
        let expected = texts
            .iter()
            .filter(|text| {
                let text = Utf32Str::new(text, &mut chars);
                pattern.score(text, &mut matcher).is_some()
            })
            .count();
        assert!(expected > 0);
        assert_eq!(nucleo.snapshot().matched_item_count() as usize, expected);
    }
}
