//! The order a list lists the candidates a query matches in.

use nucleo::pattern::Pattern;
use nucleo::{Matcher, Utf32Str};
use std::cmp::Reverse;
use std::ops::Range;

/// How many of the matcher's best-scored matches are ranked and listed. A
/// match the matcher scores lower but that a hit on the node's own name lifts
/// is found only within this window, which is wide enough to hold the near
/// ties of a short query.
const WINDOW: usize = 256;

/// The most matches ranked and listed where the score at the end of the
/// [`WINDOW`] is shared past it. The matcher breaks a tie on its score by the
/// text's length and its order, which the ranking's own tie-breaks override,
/// so every match sharing that score is ranked, up to this many.
pub(super) const MAX_WINDOW: usize = 4096;

/// What the ranking reads of a matched candidate past the matcher's score.
pub(crate) struct Standing {
    /// The chars of the candidate's text that are no part of its own name.
    pub(crate) path: Range<u32>,
    /// Whether the drawing shows what the candidate names.
    pub(crate) drawn: bool,
    /// How much the candidate weighs among those otherwise tied, such as how
    /// many nodes depend on the one it names.
    pub(crate) weight: u32,
}

/// The chars after which a segment of a candidate's text starts: a part of
/// a path or a label, or of a name made of words.
const SEGMENT_STARTS: &[char] = &['/', ':', '@', '.', '-', '_', '+', ' '];

/// Where a query stands whole in a candidate's text.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Anchor {
    /// Nowhere: its characters are matched apart.
    Fuzzy,
    /// Where a segment of the text starts.
    Segment,
    /// Where the text starts.
    Start,
}

/// Where `query` stands whole in `text`, compared in smart case as the
/// matcher compares: ignoring case unless the query holds an uppercase
/// letter.
fn anchor(text: Utf32Str<'_>, query: &[char]) -> Anchor {
    let sensitive = query.iter().any(|c| c.is_uppercase());
    let same = |a: char, b: char| match sensitive {
        true => a == b,
        false => a.to_lowercase().eq(b.to_lowercase()),
    };
    let len = text.len();
    if query.is_empty() || query.len() > len {
        return Anchor::Fuzzy;
    }
    let stands_at = |start: usize| {
        (query.iter().enumerate()).all(|(at, &c)| same(text.get((start + at) as u32), c))
    };
    if stands_at(0) {
        return Anchor::Start;
    }
    let segment = (1..=len - query.len())
        .any(|start| SEGMENT_STARTS.contains(&text.get(start as u32 - 1)) && stands_at(start));
    match segment {
        true => Anchor::Segment,
        false => Anchor::Fuzzy,
    }
}

/// What a row is ranked by, first to last: where the query stands whole,
/// where its matched characters fall, the matcher's score, whether the
/// candidate is drawn, and its weight.
type Key = (Anchor, Hit, u32, bool, u32);

/// Where in a candidate's text a query's matched characters fall.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Hit {
    /// At least one falls in the candidate's path.
    Path,
    /// Every one falls within the node's own name.
    Name,
}

/// One row a list lists.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Row {
    /// The index the candidate was handed to the matcher by.
    pub(crate) candidate: u32,
    /// The char indices of the candidate's text the query matched, ascending.
    pub(crate) matched: Vec<u32>,
}

/// The rows for the matches in `found` of `query`, parsed as `pattern`, best
/// scored first, each candidate given by its index and the text it was
/// matched by, in the order a list lists them: the query written whole where
/// the text starts, then where a segment of it starts, before its
/// characters matched apart; then a hit on the candidate's own name before
/// one reaching into its path, then by the matcher's score, then those
/// drawn, then the weightiest, as `standing` tells of each index. Only the
/// first of `found` are ranked: the [`WINDOW`] and every match tying its
/// last score, up to [`MAX_WINDOW`].
pub(crate) fn rank<'a>(
    found: impl Iterator<Item = (u32, Utf32Str<'a>)>,
    query: &str,
    pattern: &Pattern,
    matcher: &mut Matcher,
    standing: impl Fn(u32) -> Standing,
) -> Vec<Row> {
    let query: Vec<char> = query.chars().collect();
    let mut ranked: Vec<(Key, Row)> = Vec::with_capacity(WINDOW);
    let mut last = None;
    for (index, text) in found.take(MAX_WINDOW) {
        let mut matched: Vec<u32> = Vec::with_capacity(pattern.atoms.len());
        let Some(score) = pattern.indices(text, matcher, &mut matched) else {
            continue;
        };
        if ranked.len() >= WINDOW && last != Some(score) {
            break;
        }
        last = Some(score);
        matched.sort_unstable();
        matched.dedup();
        let standing = standing(index);
        let hit = match matched.iter().any(|at| standing.path.contains(at)) {
            true => Hit::Path,
            false => Hit::Name,
        };
        let key = (
            anchor(text, &query),
            hit,
            score,
            standing.drawn,
            standing.weight,
        );
        ranked.push((
            key,
            Row {
                candidate: index,
                matched,
            },
        ));
    }
    ranked.sort_by_key(|(key, _)| Reverse(*key));
    ranked.into_iter().map(|(_, row)| row).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use nucleo::pattern::{CaseMatching, Normalization};
    use nucleo::{Config, Utf32String};

    /// A candidate's text, the chars of it leading up to its name, and its
    /// weight.
    struct Candidate {
        text: String,
        path: Range<u32>,
        weight: u32,
    }

    fn candidate(text: &str, name_start: u32, weight: u32) -> Candidate {
        Candidate {
            text: text.into(),
            path: 0..name_start,
            weight,
        }
    }

    /// The texts `rank` lists for `query` over `candidates`, of which those
    /// at the indices `drawn` names are drawn.
    fn ranked(candidates: Vec<Candidate>, query: &str, drawn: &[u32]) -> Vec<String> {
        let texts: Vec<Utf32String> = candidates
            .iter()
            .map(|candidate| Utf32String::from(candidate.text.as_str()))
            .collect();
        let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
        let mut config = Config::DEFAULT;
        config.set_match_paths();
        let mut matcher = Matcher::new(config);
        let found = (0u32..).zip(texts.iter().map(|text| text.slice(..)));
        let standing = |index: u32| {
            let candidate = &candidates[index as usize];
            Standing {
                path: candidate.path.clone(),
                drawn: drawn.contains(&index),
                weight: candidate.weight,
            }
        };
        rank(found, query, &pattern, &mut matcher, standing)
            .into_iter()
            .map(|row| candidates[row.candidate as usize].text.clone())
            .collect()
    }

    #[test]
    fn a_hit_in_the_name_beats_one_in_the_package_path() {
        let listed = ranked(
            vec![
                candidate("//net/http:client", 10, 0),
                candidate("//util:http", 7, 0),
            ],
            "http",
            &[],
        );
        assert_eq!(listed, ["//util:http", "//net/http:client"]);
    }

    #[test]
    fn among_equal_hits_a_drawn_candidate_comes_first() {
        let listed = ranked(
            vec![candidate("//a:lib", 4, 0), candidate("//b:lib", 4, 0)],
            "lib",
            &[1],
        );
        assert_eq!(listed, ["//b:lib", "//a:lib"]);
    }

    #[test]
    fn among_equal_drawn_hits_the_weightiest_comes_first() {
        let listed = ranked(
            vec![
                candidate("//a:lib", 4, 2),
                candidate("//b:lib", 4, 9),
                candidate("//c:lib", 4, 9),
            ],
            "lib",
            &[0, 1],
        );
        assert_eq!(listed, ["//b:lib", "//a:lib", "//c:lib"]);
    }

    /// A name need not end the text: a hit on an action's leading mnemonic
    /// beats one reaching into another action's output directories.
    #[test]
    fn a_path_inside_the_text_is_no_part_of_the_name() {
        let action = |text: &str, path: Range<u32>| Candidate {
            text: text.into(),
            path,
            weight: 0,
        };
        let listed = ranked(
            vec![
                action("Genrule bazel-out/k8/bin/javac/out.jar", 8..31),
                action("Javac bazel-out/k8/bin/app/lib.jar", 6..27),
            ],
            "javac",
            &[],
        );
        assert_eq!(
            listed,
            [
                "Javac bazel-out/k8/bin/app/lib.jar",
                "Genrule bazel-out/k8/bin/javac/out.jar"
            ]
        );
    }

    /// A query written whole where a candidate's text starts, as a pattern
    /// being typed is, ranks before one whose characters are matched apart,
    /// however the name and the score fall.
    #[test]
    fn a_query_where_the_text_starts_comes_first() {
        let listed = ranked(
            vec![
                candidate(
                    "@sdk//Versions/Current/Headers/IOKit/graphics:GTrace.hpp",
                    20,
                    0,
                ),
                candidate("//ui/graph/...", 5, 0),
                candidate("//ui/graph:*", 5, 0),
            ],
            "//ui/gr",
            &[0],
        );
        assert_eq!(
            &listed[2],
            "@sdk//Versions/Current/Headers/IOKit/graphics:GTrace.hpp"
        );
    }

    /// A query written whole where a segment starts ranks before one matched
    /// apart, and after one where the whole text starts.
    #[test]
    fn a_query_where_a_segment_starts_comes_next() {
        let listed = ranked(
            vec![
                candidate("//a:open_the_edges", 4, 9),
                candidate("//graph:openedges_test", 8, 0),
                candidate("openedges", 0, 0),
            ],
            "openedges",
            &[0],
        );
        assert_eq!(
            listed,
            ["openedges", "//graph:openedges_test", "//a:open_the_edges"]
        );
    }

    /// An uppercase letter makes the comparison mind case, as it makes the
    /// matcher's.
    #[test]
    fn an_anchor_is_found_in_smart_case() {
        let text = Utf32String::from("//app:Lib");
        let chars = |query: &str| query.chars().collect::<Vec<char>>();
        assert_eq!(anchor(text.slice(..), &chars("lib")), Anchor::Segment);
        assert_eq!(anchor(text.slice(..), &chars("Lib")), Anchor::Segment);
        assert_eq!(anchor(text.slice(..), &chars("LIB")), Anchor::Fuzzy);
        assert_eq!(anchor(text.slice(..), &chars("//APP")), Anchor::Fuzzy);
        assert_eq!(anchor(text.slice(..), &chars("//ap")), Anchor::Start);
    }

    /// Matches tying the score at the end of the window are ranked too, so a
    /// drawn one the matcher lists past it still comes first.
    #[test]
    fn the_matches_tying_the_last_in_the_window_are_ranked_too() {
        let tied = |count: usize| -> Vec<Candidate> {
            (0..count as u32)
                .map(|id| candidate(&format!("//p{id:04}:lib"), 8, 0))
                .collect()
        };
        let past = WINDOW as u32 + 5;
        let listed = ranked(tied(WINDOW + 10), "lib", &[past]);
        assert_eq!(listed[0], format!("//p{past:04}:lib"));

        let beyond = MAX_WINDOW as u32 + 5;
        let listed = ranked(tied(MAX_WINDOW + 10), "lib", &[beyond]);
        assert_ne!(listed[0], format!("//p{beyond:04}:lib"));
    }

    #[test]
    fn every_match_is_listed_and_a_text_the_query_misses_is_not() {
        const MATCHING: u32 = 20;
        let mut candidates: Vec<Candidate> = (0..MATCHING)
            .map(|id| candidate(&format!("//p{id}:lib"), 0, 0))
            .collect();
        candidates.push(candidate("//x:other", 4, 0));
        let listed = ranked(candidates, "lib", &[]);
        assert_eq!(listed.len(), MATCHING as usize);
        assert!(!listed.contains(&"//x:other".to_owned()));
    }
}
