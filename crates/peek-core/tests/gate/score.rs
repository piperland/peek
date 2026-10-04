//! Arithmetic for the gate: fractions with a stated denominator, and multiset
//! matching against ground truth.
//!
//! # Why this module has its own tests
//!
//! A scorer is the one part of a measurement gate that cannot be checked by the
//! thing it measures. If `matched` is computed as an intersection where it
//! should be a minimum over multiplicities, or if a zero denominator is reported
//! as `1.00`, every number downstream is wrong in a way that looks like a
//! passing gate. So the scorer's own behaviour is pinned here, against inputs
//! that are wrong on purpose, and each of those tests fails if the arithmetic
//! moves.
//!
//! That is also the answer to "which of your checks can fail": these four, plus
//! every test in `measure.rs` that asserts on a measurement rather than
//! printing one.

use std::collections::BTreeMap;
use std::fmt;

/// A count over a stated denominator.
///
/// There is no `PartialOrd` and no `is_good` method on purpose. A fraction is not
/// better or worse than another fraction; it is a number and a population, and
/// the gate decides what to do with them by comparing against a declared floor
/// in one place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fraction {
    pub numerator: u64,
    pub denominator: u64,
}

impl Fraction {
    pub const fn new(numerator: u64, denominator: u64) -> Self {
        Self {
            numerator,
            denominator,
        }
    }

    /// Whether this fraction has a population at all.
    ///
    /// A zero denominator is not a perfect score. It is an unanswerable question,
    /// and it must never be rendered as `1.00`, because that is how a language
    /// nobody wrote a fixture for comes to look better than one that was measured.
    pub const fn is_measured(&self) -> bool {
        self.denominator > 0
    }

    /// Whether every element of the population is accounted for.
    pub const fn is_whole(&self) -> bool {
        self.denominator > 0 && self.numerator == self.denominator
    }

    /// The value in hundredths of a percent, rounded half up.
    ///
    /// `None` when there is no population. The rounding is here so the printed
    /// figure is stable; the raw numerator and denominator are printed beside it
    /// everywhere it appears, so nothing is decided by the rounding.
    pub fn hundredths(&self) -> Option<u64> {
        if self.denominator == 0 {
            return None;
        }
        let n = u128::from(self.numerator);
        let d = u128::from(self.denominator);
        Some(((n * 20_000 + d) / (2 * d)) as u64)
    }

    /// Whether this fraction reaches a floor given in hundredths of a percent.
    ///
    /// Compared as `n * 10000 >= floor * d` in `u128`, so a floor of `100.00`
    /// means exactly "every element", and a floor is never satisfied by an empty
    /// population.
    pub fn reaches(&self, floor_hundredths: u64) -> bool {
        if self.denominator == 0 {
            return false;
        }
        u128::from(self.numerator) * 10_000
            >= u128::from(floor_hundredths) * u128::from(self.denominator)
    }

    /// `1.00 (34/83)` — the figure and its denominator, always together.
    pub fn render(&self) -> String {
        match self.hundredths() {
            None => format!("n/a (0/0)"),
            Some(hundredths) => format!(
                "{}.{:02} ({}/{})",
                hundredths / 100,
                hundredths % 100,
                self.numerator,
                self.denominator
            ),
        }
    }
}

impl fmt::Display for Fraction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.render())
    }
}

/// A multiset of keys, because ground truth counts occurrences.
///
/// A set would be wrong wherever a source declares the same identity twice. Two
/// `impl` blocks for one type produce two entities that share a path, a kind and
/// a qualified name; a set says one and the engine is then charged a false
/// positive for emitting the second, which is a finding about the identity
/// scheme rather than about extraction.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Multiset {
    counts: BTreeMap<String, u32>,
}

impl Multiset {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, key: impl Into<String>) {
        *self.counts.entry(key.into()).or_insert(0) += 1;
    }

    pub fn count_of(&self, key: &str) -> u32 {
        self.counts.get(key).copied().unwrap_or(0)
    }

    pub fn total(&self) -> u64 {
        self.counts.values().map(|count| u64::from(*count)).sum()
    }

    pub fn distinct(&self) -> usize {
        self.counts.len()
    }

    /// Keys present in either side, for a diagnostic that names both the missing
    /// and the spurious rather than only the smaller count.
    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.counts.keys()
    }

    pub fn merged_keys(a: &Self, b: &Self) -> Vec<String> {
        let mut keys: Vec<String> = a.keys().cloned().collect();
        for key in b.keys() {
            if !a.counts.contains_key(key) {
                keys.push(key.clone());
            }
        }
        keys.sort();
        keys.dedup();
        keys
    }

    /// How many occurrences are shared, counting the minimum on each side.
    pub fn matched(&self, other: &Self) -> u64 {
        let mut total = 0u64;
        for (key, mine) in &self.counts {
            let theirs = other.counts.get(key).copied().unwrap_or(0);
            total += u64::from(*mine.min(&theirs));
        }
        total
    }
}

/// A precision/recall pair from two multisets.
///
/// `truth` is the hand-written ground truth and `found` is what the engine
/// emitted. Precision is over `found`; recall is over `truth`. Reporting only
/// one of them is how a language that extracts nothing scores perfectly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Match {
    pub matched: u64,
    pub truth_total: u64,
    pub found_total: u64,
}

impl Match {
    pub fn of(truth: &Multiset, found: &Multiset) -> Self {
        Self {
            matched: truth.matched(found),
            truth_total: truth.total(),
            found_total: found.total(),
        }
    }

    pub fn recall(&self) -> Fraction {
        Fraction::new(self.matched, self.truth_total)
    }

    pub fn precision(&self) -> Fraction {
        Fraction::new(self.matched, self.found_total)
    }
}

#[cfg(test)]
mod tests {
    use super::{Fraction, Match, Multiset};

    fn multiset(keys: &[&str]) -> Multiset {
        let mut set = Multiset::new();
        for key in keys {
            set.add(*key);
        }
        set
    }

    #[test]
    fn a_missing_symbol_shows_up_in_recall_and_not_in_precision() {
        // The engine found one of the two declarations it should have. Recall is
        // half; precision over what it actually emitted is perfect.
        let truth = multiset(&["a", "b"]);
        let found = multiset(&["a"]);
        let scored = Match::of(&truth, &found);
        assert_eq!(scored.recall(), Fraction::new(1, 2));
        assert_eq!(scored.precision(), Fraction::new(1, 1));
    }

    #[test]
    fn a_spurious_symbol_shows_up_in_precision_and_not_in_recall() {
        let truth = multiset(&["a"]);
        let found = multiset(&["a", "x"]);
        let scored = Match::of(&truth, &found);
        assert_eq!(scored.recall(), Fraction::new(1, 1));
        assert_eq!(scored.precision(), Fraction::new(1, 2));
    }

    #[test]
    fn extracting_nothing_is_not_a_perfect_score() {
        // The failure this whole engine exists to prevent: a language with no
        // extraction rules emits no symbols, so its precision has no population
        // and must read as unmeasured rather than as 1.00.
        let truth = multiset(&["a"]);
        let found = Multiset::new();
        let scored = Match::of(&truth, &found);
        assert_eq!(scored.recall(), Fraction::new(0, 1));
        assert!(!scored.precision().is_measured());
        assert!(!scored.precision().is_whole());
        assert_eq!(scored.precision().render(), "n/a (0/0)");
        // And a floor is never satisfied by an empty population, whatever it is.
        assert!(!scored.precision().reaches(0));
    }

    #[test]
    fn a_repeated_identity_is_counted_rather_than_collapsed() {
        // Two `impl` blocks for one type are two entities. Collapsing them to a
        // set would report the engine's second row as spurious.
        let mut truth = Multiset::new();
        truth.add("t");
        truth.add("t");
        let mut found = Multiset::new();
        found.add("t");
        let scored = Match::of(&truth, &found);
        assert_eq!(scored.recall(), Fraction::new(1, 2));
        assert_eq!(scored.precision(), Fraction::new(1, 1));
        assert_eq!(truth.total(), 2);
        assert_eq!(truth.distinct(), 1);
    }

    #[test]
    fn emitting_more_copies_than_the_source_declares_is_a_false_positive() {
        let mut truth = Multiset::new();
        truth.add("t");
        let mut found = Multiset::new();
        found.add("t");
        found.add("t");
        let scored = Match::of(&truth, &found);
        assert_eq!(scored.recall(), Fraction::new(1, 1));
        assert_eq!(scored.precision(), Fraction::new(1, 2));
    }

    #[test]
    fn floors_are_compared_exactly_rather_than_through_a_rounded_figure() {
        // 1/3 is 33.33%. A floor of 33.33 passes; 33.34 does not. Comparing the
        // rounded strings would get this backwards at the boundary.
        let third = Fraction::new(1, 3);
        assert_eq!(third.hundredths(), Some(3333));
        assert!(third.reaches(3333));
        assert!(!third.reaches(3334));
        assert!(third.reaches(0));
    }

    #[test]
    fn a_floor_of_one_hundred_means_every_element() {
        assert!(Fraction::new(7, 7).reaches(10_000));
        assert!(!Fraction::new(6, 7).reaches(10_000));
        // 99.99% is not 100%, and must not pass a floor of 100.
        assert!(!Fraction::new(9_999, 10_000).reaches(10_000));
    }

    #[test]
    fn a_rendered_figure_always_carries_its_denominator() {
        // The rule the whole matrix rests on: a number with no denominator is the
        // defect, so the renderer cannot produce one.
        assert_eq!(Fraction::new(34, 83).render(), "0.41 (34/83)");
        assert_eq!(Fraction::new(0, 5).render(), "0.00 (0/5)");
        assert_eq!(Fraction::new(5, 5).render(), "1.00 (5/5)");
    }

    #[test]
    fn merged_keys_name_both_the_missing_and_the_spurious() {
        let truth = multiset(&["a"]);
        let found = multiset(&["b"]);
        assert_eq!(Multiset::merged_keys(&truth, &found), vec!["a", "b"]);
    }
}