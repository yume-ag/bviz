//! When a query typed after another can match by narrowing the other's
//! result.

use nucleo::pattern::{AtomKind, Pattern};

/// Whether every candidate `new` matches is one `old` matches, as far as their
/// atoms tell: `new` repeats every atom of `old` but the last, then extends
/// its needle where a longer needle can only match less, and may add atoms.
pub(super) fn narrows(old: &Pattern, new: &Pattern) -> bool {
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

/// Pairs of queries typed one after the other, each turning on whether the
/// first narrows to the second.
#[cfg(test)]
pub(super) const TYPED: &[(&str, &str)] = &[
    ("fo", "foo"),
    ("a", "a b"),
    ("foo$", "foo$x"),
    ("a\\", "a\\ b"),
    ("a\\", "a\\$"),
    ("!a", "!ab"),
    ("^", "^a"),
    ("'", "'a"),
    ("!", "!a"),
    ("a", "a$"),
    ("abc", "ab"),
    ("a", "aB"),
];

/// Texts with the characters the queries of [`TYPED`] turn on.
#[cfg(test)]
pub(super) const TEXTS: &[&str] = &[
    "foo", "foox", "foo$x", "a\\", "a\\ b", "a b", "a$", "a\\$", "ab", "b", "^a", "'a", "Abc",
    "aBc", "abc", "xa", "x'a",
];

#[cfg(test)]
mod tests {
    use super::*;
    use nucleo::pattern::{CaseMatching, Normalization};

    fn narrowing(old: &str, new: &str) -> bool {
        let parse = |query| Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
        narrows(&parse(old), &parse(new))
    }

    #[test]
    fn a_longer_needle_narrows() {
        assert!(narrowing("fo", "foo"));
        assert!(narrowing("'fo", "'foo"));
        assert!(narrowing("^fo", "^foo"));
        assert!(narrowing("a", "a b"));
        assert!(narrowing("a b", "a bc"));
    }

    #[test]
    fn a_postfix_extended_past_its_end_does_not_narrow() {
        assert!(!narrowing("foo$", "foo$x"));
    }

    #[test]
    fn an_escape_completed_does_not_narrow() {
        assert!(!narrowing("a\\", "a\\ b"));
        assert!(!narrowing("a\\", "a\\$"));
    }

    #[test]
    fn a_negation_extended_does_not_narrow() {
        assert!(!narrowing("!a", "!ab"));
    }

    #[test]
    fn a_bare_operator_matches_everything_and_so_narrows() {
        assert!(narrowing("^", "^a"));
        assert!(narrowing("'", "'a"));
        assert!(narrowing("!", "!a"));
    }

    #[test]
    fn an_operator_typed_after_a_needle_does_not_narrow() {
        assert!(!narrowing("a", "a$"));
    }

    #[test]
    fn an_earlier_atom_changed_does_not_narrow() {
        assert!(!narrowing("a b", "ax b"));
        assert!(!narrowing("abc", "ab"));
    }

    #[test]
    fn an_uppercase_letter_typed_narrows_to_its_case() {
        assert!(narrowing("a", "aB"));
    }
}
