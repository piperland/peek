//! Pricing: how many tokens a piece of context costs.
//!
//! The budget is a **hard input** to the context compiler, not an output it computes after the
//! fact. That makes the counting rule part of the contract rather than an implementation detail,
//! so it is written down here, in one place, and every number in a pack is produced by the one
//! function in this file.

use serde::{Deserialize, Serialize};

/// UTF-8 bytes per token, the only approximation in this module.
///
/// **The rule.** `tokens(text) = ceil(text.len() / CHARS_PER_TOKEN)`, where `text.len()` is the
/// **byte** length of the UTF-8 encoding.
///
/// **Why bytes and not characters.** A character is a Unicode scalar value, and its cost as
/// tokens varies by more than an order of magnitude across scripts: a CJK ideograph is commonly
/// one or two tokens and is three bytes, while an emoji is several tokens and is four bytes.
/// Counting characters divides a number that means two different things by another one. Bytes
/// are uniform, and the residual error — high for emoji, low for long runs of ASCII punctuation
/// — is stated rather than hidden.
///
/// **Why three and not four.** The commonly quoted figure for byte-pair tokenisers is about four
/// characters per token on English prose. Code is not prose: it is denser in punctuation and
/// short identifiers, and the same tokenisers spend more tokens per byte on it. Three is
/// therefore a deliberately pessimistic figure and not a measured one. Pessimistic is the
/// direction that matters: a budget is a promise that the pack fits, so the estimate has to err
/// high — over-count rather than under-count. The cost of erring high is that a pack may
/// under-fill its budget, and this module makes that visible as `remaining_tokens` rather than
/// padding the answer to look full.
///
/// **What this is not.** It is not a tokenizer and does not reproduce any particular model's
/// token stream. Nothing in the engine depends on which model consumes a pack, which is
/// deliberate: an index that cannot be opened because a model was renamed is a worse failure than
/// an index with a stated, conservative estimate. A real tokenizer would be a new dependency and
/// a new way for this number to be wrong without anybody noticing.
/// [`TokenCounter::from_chars_per_token`] is the seam — swap the arithmetic in
/// [`TokenCounter::count_bytes`] and every number in the pack stays self-consistent, because
/// every number in the pack comes from this one function.
pub const CHARS_PER_TOKEN: u32 = 3;

/// The counting rule a pack was priced with.
///
/// Carried inside the pack so a consumer can reproduce the arithmetic, and so two packs priced by
/// different rules are visibly different rather than silently incomparable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenCounter {
    chars_per_token: u32,
}

impl TokenCounter {
    /// Count with an explicit rule.
    ///
    /// Zero is accepted and means "one token per byte", which is the most pessimistic rule
    /// available rather than a division by zero. See [`CHARS_PER_TOKEN`] for the default and the
    /// reasoning behind it.
    #[must_use]
    pub const fn from_chars_per_token(chars_per_token: u32) -> Self {
        Self { chars_per_token }
    }

    /// The rule as written, e.g. `ceil(utf8 bytes / 3)`.
    #[must_use]
    pub fn rule(self) -> String {
        format!("ceil(utf8 bytes / {})", self.chars_per_token)
    }

    /// The divisor. Exposed so a caller can state the rule without re-deriving it.
    #[must_use]
    pub const fn chars_per_token(self) -> u32 {
        self.chars_per_token
    }

    /// Price some text.
    #[must_use]
    pub fn count(self, text: &str) -> u64 {
        self.count_bytes(u64::try_from(text.len()).unwrap_or(u64::MAX))
    }

    /// Price a byte count that did not come from a `str`.
    ///
    /// Rounds **up**, always. A cost of zero for non-empty text would be a way for the compiler
    /// to include something for free, and free text is how a budget quietly stops being a budget.
    #[must_use]
    pub fn count_bytes(self, bytes: u64) -> u64 {
        match self.chars_per_token {
            0 => bytes,
            unit => bytes.div_ceil(unit),
        }
    }
}

impl Default for TokenCounter {
    fn default() -> Self {
        Self {
            chars_per_token: CHARS_PER_TOKEN,
        }
    }
}

/// What one piece of the answer costs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Cost {
    /// Tokens, by the pack's own [`TokenCounter`].
    pub tokens: u64,
    /// UTF-8 bytes. Kept beside the token count because it is the quantity the estimate is
    /// actually derived from, and a consumer holding a different tokenizer needs the bytes.
    pub bytes: u64,
}

impl Cost {
    /// The cost of nothing.
    pub const ZERO: Cost = Cost {
        tokens: 0,
        bytes: 0,
    };

    /// Price some text.
    #[must_use]
    pub fn of(text: &str, counter: TokenCounter) -> Self {
        let bytes = u64::try_from(text.len()).unwrap_or(u64::MAX);
        Self {
            tokens: counter.count_bytes(bytes),
            bytes,
        }
    }

    /// Add two costs.
    #[must_use]
    pub fn sum(self, other: Cost) -> Cost {
        Cost {
            tokens: self.tokens.saturating_add(other.tokens),
            bytes: self.bytes.saturating_add(other.bytes),
        }
    }

    /// Whether this costs nothing at all.
    #[must_use]
    pub fn is_zero(&self) -> bool {
        self.tokens == 0
    }
}

#[cfg(test)]
mod tests {
    use super::{CHARS_PER_TOKEN, Cost, TokenCounter};

    #[test]
    fn the_default_rule_is_the_documented_one() {
        // Pinned as a literal as well as against the constant, so that changing
        // `CHARS_PER_TOKEN` without re-reading the reasoning in its documentation fails a test
        // here rather than silently changing every budget in the product.
        assert_eq!(CHARS_PER_TOKEN, 3);
        assert_eq!(TokenCounter::default().chars_per_token(), 3);
        assert_eq!(TokenCounter::default().rule(), "ceil(utf8 bytes / 3)");
    }

    #[test]
    fn a_cost_rounds_up_so_no_text_is_free() {
        let counter = TokenCounter::default();
        assert_eq!(counter.count(""), 0);
        assert_eq!(counter.count("a"), 1, "one byte is a third of a token, and rounds up to one");
        assert_eq!(counter.count("abc"), 1);
        assert_eq!(counter.count("abcd"), 2, "four bytes is one token and a bit");
        assert_eq!(counter.count(&"x".repeat(300)), 100);
    }

    #[test]
    fn a_zero_divisor_prices_every_byte_as_a_token_rather_than_dividing_by_zero() {
        let counter = TokenCounter::from_chars_per_token(0);
        assert_eq!(counter.count_bytes(7), 7);
        assert_eq!(counter.count(""), 0);
    }

    #[test]
    fn a_finer_rule_makes_the_same_text_cost_more() {
        // The rule is a parameter, so the direction of its effect is checkable rather than
        // assumed.
        let text = "fn retry(&self, attempt: u32) -> Result<Receipt> { ... }";
        let coarse = TokenCounter::from_chars_per_token(4).count(text);
        let fine = TokenCounter::from_chars_per_token(2).count(text);
        assert!(
            fine > coarse,
            "a smaller divisor cannot cost less: {fine} vs {coarse} on {text:?}"
        );
    }

    #[test]
    fn cost_carries_the_bytes_the_estimate_was_derived_from() {
        let cost = Cost::of("héllo", TokenCounter::default());
        assert_eq!(cost.bytes, 6, "é is two bytes in UTF-8");
        assert_eq!(cost.tokens, 2);
        assert_eq!(cost, Cost::of("héllo", TokenCounter::default()));
        assert!(Cost::ZERO.is_zero());
        assert_eq!(Cost::ZERO.sum(cost), cost);
    }
}
