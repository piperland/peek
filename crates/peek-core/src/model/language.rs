//! Languages and their capability tiers.
//!
//! # Why tiers live in the type system
//!
//! Cortex advertised 28 languages. Fourteen of them extracted **zero symbols**, and seven shared
//! no node-type vocabulary at all with the single hardcoded match arm they were all wired to. The
//! only automated surfaces — `cortex doctor`'s `supported_languages` and the benchmark table —
//! reported all 28 as supported, because nothing in the code could express the difference.
//!
//! Peek therefore models the tier explicitly. [`CapabilityTier`] is what `doctor`, the MCP
//! `server_info` primitive, and the published capability matrix all read, so a language cannot be
//! reported as supported unless a conformance test has promoted it.
//!
//! The promotion rule is not advisory: [`Language::tier`] returns [`CapabilityTier::Unverified`]
//! for any language whose conformance gate has not been recorded as passing. Adding a grammar
//! dependency changes nothing on its own.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// How much semantic support Peek can actually prove for a language.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityTier {
    /// No passing conformance test. Must never be advertised as supported.
    Unverified,
    /// Honest best-effort: files, symbols, definitions, hierarchy, imports, obvious references.
    /// Results are marked as carrying reduced semantic strength.
    SecondParity,
    /// All applicable gates pass: extraction, resolution, inheritance, incremental correctness,
    /// query correctness, and context quality.
    FirstParity,
}

impl CapabilityTier {
    pub fn as_str(self) -> &'static str {
        match self {
            CapabilityTier::Unverified => "unverified",
            CapabilityTier::SecondParity => "second_parity",
            CapabilityTier::FirstParity => "first_parity",
        }
    }
}

impl fmt::Display for CapabilityTier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A programming language Peek can be asked to index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Language {
    TypeScript,
    JavaScript,
    Python,
    Rust,
    Go,
    Java,
    CSharp,
    C,
    Cpp,
    Kotlin,
    Swift,
    Ruby,
    Php,
    Scala,
    Elixir,
    Erlang,
    Dart,
    Lua,
    R,
    Julia,
    Haskell,
    OCaml,
    Clojure,
    Bash,
    ObjectiveC,
    Zig,
}

impl Language {
    /// The first-parity target cohort, in descending agent-ecosystem weight.
    ///
    /// Membership here is a *goal*, not a claim. A language is only reported as first parity once
    /// [`Self::tier`] says so.
    pub const FIRST_PARITY_TARGETS: &'static [Language] = &[
        Language::TypeScript,
        Language::JavaScript,
        Language::Python,
        Language::Rust,
        Language::Go,
        Language::Java,
        Language::CSharp,
        Language::C,
        Language::Cpp,
        Language::Kotlin,
        Language::Swift,
    ];

    /// The file extensions that map to this language, lowercase and without the dot.
    pub const fn extensions(self) -> &'static [&'static str] {
        match self {
            Language::TypeScript => &["ts", "tsx", "mts", "cts"],
            Language::JavaScript => &["js", "jsx", "mjs", "cjs"],
            Language::Python => &["py", "pyi", "pyw"],
            Language::Rust => &["rs"],
            Language::Go => &["go"],
            Language::Java => &["java"],
            Language::CSharp => &["cs"],
            Language::C => &["c", "h"],
            Language::Cpp => &["cpp", "cc", "cxx", "hpp", "hh", "hxx", "c++", "h++"],
            Language::Kotlin => &["kt", "kts"],
            Language::Swift => &["swift"],
            Language::Ruby => &["rb", "rake"],
            Language::Php => &["php"],
            Language::Scala => &["scala", "sc"],
            Language::Elixir => &["ex", "exs"],
            Language::Erlang => &["erl", "hrl"],
            Language::Dart => &["dart"],
            Language::Lua => &["lua"],
            Language::R => &["r"],
            Language::Julia => &["jl"],
            Language::Haskell => &["hs", "lhs"],
            Language::OCaml => &["ml", "mli"],
            Language::Clojure => &["clj", "cljs", "cljc", "edn"],
            Language::Bash => &["sh", "bash", "zsh"],
            Language::ObjectiveC => &["m", "mm"],
            Language::Zig => &["zig"],
        }
    }

    /// The canonical lowercase name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Language::TypeScript => "typescript",
            Language::JavaScript => "javascript",
            Language::Python => "python",
            Language::Rust => "rust",
            Language::Go => "go",
            Language::Java => "java",
            Language::CSharp => "csharp",
            Language::C => "c",
            Language::Cpp => "cpp",
            Language::Kotlin => "kotlin",
            Language::Swift => "swift",
            Language::Ruby => "ruby",
            Language::Php => "php",
            Language::Scala => "scala",
            Language::Elixir => "elixir",
            Language::Erlang => "erlang",
            Language::Dart => "dart",
            Language::Lua => "lua",
            Language::R => "r",
            Language::Julia => "julia",
            Language::Haskell => "haskell",
            Language::OCaml => "ocaml",
            Language::Clojure => "clojure",
            Language::Bash => "bash",
            Language::ObjectiveC => "objectivec",
            Language::Zig => "zig",
        }
    }

    /// Infer the language from a file extension.
    ///
    /// The single source of truth for extension mapping. It is a table rather than a long
    /// `match` so that the first-parity and second-parity sets can never drift apart from it.
    pub fn from_extension(extension: &str) -> Option<Self> {
        let lowered = extension.to_ascii_lowercase();
        Self::ALL
            .iter()
            .copied()
            .find(|language| language.extensions().contains(&lowered.as_str()))
    }

    /// Every language Peek knows how to address.
    pub const ALL: &'static [Language] = &[
        Language::TypeScript,
        Language::JavaScript,
        Language::Python,
        Language::Rust,
        Language::Go,
        Language::Java,
        Language::CSharp,
        Language::C,
        Language::Cpp,
        Language::Kotlin,
        Language::Swift,
        Language::Ruby,
        Language::Php,
        Language::Scala,
        Language::Elixir,
        Language::Erlang,
        Language::Dart,
        Language::Lua,
        Language::R,
        Language::Julia,
        Language::Haskell,
        Language::OCaml,
        Language::Clojure,
        Language::Bash,
        Language::ObjectiveC,
        Language::Zig,
    ];

    /// The capability tier Peek can currently prove for this language.
    ///
    /// **Every language is currently [`CapabilityTier::Unverified`].** This is not a placeholder
    /// to be filled in optimistically: a language is promoted here only when its conformance gate
    /// passes, and no gate has been implemented yet. Reporting anything else would be exactly
    /// the claim Cortex made and could not support.
    pub const fn tier(self) -> CapabilityTier {
        CapabilityTier::Unverified
    }

    /// Whether Peek may describe this language as supported in any user-facing surface.
    pub const fn is_advertisable(self) -> bool {
        !matches!(self.tier(), CapabilityTier::Unverified)
    }
}

impl fmt::Display for Language {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Returned when a string does not name a known language.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseLanguageError(pub String);

impl fmt::Display for ParseLanguageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown language: {}", self.0)
    }
}

impl std::error::Error for ParseLanguageError {}

impl FromStr for Language {
    type Err = ParseLanguageError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let lowered = s.trim().to_ascii_lowercase();
        Language::ALL
            .iter()
            .copied()
            // Accept the common aliases so that `c++` and `c#` resolve like their extensions.
            .find(|language| {
                language.as_str() == lowered
                    || match language {
                        Language::Cpp => matches!(lowered.as_str(), "c++" | "cplusplus"),
                        Language::CSharp => lowered == "c#",
                        Language::ObjectiveC => matches!(lowered.as_str(), "objc" | "objective-c"),
                        _ => false,
                    }
            })
            .ok_or_else(|| ParseLanguageError(s.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::{CapabilityTier, Language};

    #[test]
    fn every_extension_maps_back_to_its_language() {
        for language in Language::ALL {
            for extension in language.extensions() {
                let resolved = Language::from_extension(extension)
                    .unwrap_or_else(|| panic!("{extension} should resolve to {language}"));
                assert_eq!(
                    resolved,
                    *language,
                    "extension {extension} is claimed by more than one language"
                );
            }
        }
    }

    #[test]
    fn extension_matching_is_case_insensitive() {
        assert_eq!(Language::from_extension("RS"), Some(Language::Rust));
        assert_eq!(Language::from_extension("Rs"), Some(Language::Rust));
        assert_eq!(Language::from_extension("TSX"), Some(Language::TypeScript));
    }

    #[test]
    fn unknown_extension_is_none() {
        assert_eq!(Language::from_extension("txt"), None);
        assert_eq!(Language::from_extension(""), None);
        // Cortex indexed `.d.ts` as TypeScript source and `*.min.js` as JavaScript. Generated
        // output is not source; detection must not claim it.
        assert_eq!(Language::from_extension("min"), None);
    }

    #[test]
    fn names_round_trip() {
        for language in Language::ALL {
            let parsed: Language = language.as_str().parse().expect("round trip");
            assert_eq!(parsed, *language);
        }
    }

    #[test]
    fn common_aliases_parse() {
        assert_eq!("c++".parse::<Language>(), Ok(Language::Cpp));
        assert_eq!("c#".parse::<Language>(), Ok(Language::CSharp));
        assert_eq!("objc".parse::<Language>(), Ok(Language::ObjectiveC));
        assert_eq!("  Rust  ".parse::<Language>(), Ok(Language::Rust));
    }

    #[test]
    fn unknown_language_name_is_an_error() {
        assert!("cobol".parse::<Language>().is_err());
    }

    #[test]
    fn no_language_is_advertisable_before_its_gate_passes() {
        // This is the mechanical antidote to Cortex advertising 28 languages of which 14
        // extracted nothing. It must stay true until a conformance gate promotes a language.
        for language in Language::ALL {
            assert_eq!(
                language.tier(),
                CapabilityTier::Unverified,
                "{language} must not claim a tier before its conformance gate passes"
            );
            assert!(
                !language.is_advertisable(),
                "{language} must not be advertised before its conformance gate passes"
            );
        }
    }

    #[test]
    fn first_parity_targets_are_all_known_languages() {
        for target in Language::FIRST_PARITY_TARGETS {
            assert!(
                Language::ALL.contains(target),
                "{target} is a first-parity target but is not in ALL"
            );
        }
        assert!(Language::FIRST_PARITY_TARGETS.len() >= 10);
    }

    #[test]
    fn all_covers_first_parity_targets() {
        for target in Language::FIRST_PARITY_TARGETS {
            assert!(Language::ALL.contains(target));
        }
        // Sanity: the registry and the target list must not have drifted into duplication.
        let mut sorted = Language::ALL.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), Language::ALL.len());
    }
}
