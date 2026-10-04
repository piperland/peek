//! The expectation file: hand-written ground truth, parsed.
//!
//! # What this file is and is not
//!
//! It is a claim about what the fixture's source says. It is written by reading
//! the source, never by reading the engine's output — otherwise the gate would
//! be a snapshot of current behaviour with assertions attached, and it could not
//! fail for the reason that matters.
//!
//! It is also where each language's floor lives, so that promoting a language and
//! ratcheting its floor are the same reviewable act rather than two independent
//! edits that can drift.
//!
//! The parser is strict on purpose. An unknown keyword, a wrong field count or a
//! bad floor is an error naming the file and the line, because a silently ignored
//! ground-truth line is a smaller denominator and therefore a *better* score.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use peek_core::model::Language;

use super::score::Multiset;

/// An entity identity without the ordinal: `path | kind | qualified_name`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Key {
    pub path: String,
    pub kind: String,
    pub qualified_name: String,
}

impl Key {
    pub fn new(path: &str, kind: &str, qualified_name: &str) -> Self {
        Self {
            path: path.to_owned(),
            kind: kind.to_owned(),
            qualified_name: qualified_name.to_owned(),
        }
    }

    pub fn render(&self) -> String {
        format!("{} | {} | {}", self.path, self.kind, self.qualified_name)
    }
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.render())
    }
}

/// One labelled relation fact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Labelled {
    /// Repository-relative path of the file the fact is in.
    pub path: String,
    /// The enclosing symbol's qualified name, or the file's name at file level.
    pub subject: String,
    /// The name the fact is about.
    pub object: String,
}

/// A labelled call site, which carries the kind of its caller.
///
/// The kind is on the line rather than guessed at. A qualified name does not
/// determine an `EntityKind` — `counted` is both a macro and a function in the
/// Rust fixture — and a query can only name one entity, so a gate that inferred
/// the kind would be measuring its own inference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelledCall {
    pub path: String,
    pub kind: String,
    pub subject: String,
    pub object: String,
}

/// An import binding, which carries a module and an optional alias.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelledImport {
    pub path: String,
    pub local: String,
    pub module: String,
    pub alias: Option<String>,
}

/// A pair of entity identities: a question and the answer that satisfies it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelledPair {
    pub target: Key,
    pub expected: Key,
}

/// A context question and the entity its answer has to contain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelledContext {
    pub question: String,
    pub expected: Key,
}

/// Everything one language's fixture claims.
#[derive(Debug, Clone)]
pub struct Corpus {
    pub language: Language,
    /// Directory holding the fixture sources.
    pub directory: PathBuf,
    pub symbols: Vec<Key>,
    pub members: Vec<Labelled>,
    pub calls: Vec<LabelledCall>,
    pub imports: Vec<LabelledImport>,
    /// Imports whose module the graph must still be able to name.
    pub import_no_modules: Vec<Labelled>,
    pub references: Vec<Labelled>,
    pub no_references: Vec<Labelled>,
    pub inherits: Vec<Labelled>,
    pub implements: Vec<Labelled>,
    pub no_inherits: Vec<Labelled>,
    pub callers: Vec<LabelledPair>,
    pub callees: Vec<LabelledPair>,
    pub implementations: Vec<LabelledPair>,
    pub contexts: Vec<LabelledContext>,
    pub ambiguous_contexts: Vec<String>,
    /// Dimension name to a floor in hundredths of a percent.
    pub floors: BTreeMap<String, u64>,
}

impl Corpus {
    /// The labelled declarations as a multiset, for scoring precision and recall.
    pub fn symbol_multiset(&self) -> Multiset {
        let mut set = Multiset::new();
        for key in &self.symbols {
            set.add(key.render());
        }
        set
    }

    /// The distinct labelled declaration identities.
    ///
    /// Definitions are scored over distinct identities rather than over lines,
    /// because two `impl` blocks for one type are one identity with two rows and
    /// asking "does the engine have a definition edge for this symbol" has the
    /// same answer twice.
    pub fn distinct_symbols(&self) -> Vec<&Key> {
        let mut seen: Vec<&Key> = Vec::new();
        for key in &self.symbols {
            if !seen.contains(&key) {
                seen.push(key);
            }
        }
        seen
    }
}

/// A parse failure, with the line that caused it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub file: PathBuf,
    pub line: usize,
    pub message: String,
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}: {}", self.file.display(), self.line, self.message)
    }
}

const KEYWORDS: &[&str] = &[
    "symbol",
    "member",
    "call",
    "import",
    "import_no_module",
    "reference",
    "no_reference",
    "inherits",
    "implements",
    "no_inherits",
    "callers",
    "callees",
    "implementations",
    "context",
    "context_ambiguous",
    "floor",
];

/// Parse one `gate.expect`.
pub fn parse(language: Language, directory: &Path) -> Result<Corpus, Vec<Problem>> {
    let file = directory.join("gate.expect");
    let text = std::fs::read_to_string(&file).map_err(|error| {
        vec![Problem {
            file: file.clone(),
            line: 0,
            message: format!("cannot read the expectation file: {error}"),
        }]
    })?;

    let mut problems: Vec<Problem> = Vec::new();
    let mut corpus = Corpus {
        language,
        directory: directory.to_path_buf(),
        symbols: Vec::new(),
        members: Vec::new(),
        calls: Vec::new(),
        imports: Vec::new(),
        import_no_modules: Vec::new(),
        references: Vec::new(),
        no_references: Vec::new(),
        inherits: Vec::new(),
        implements: Vec::new(),
        no_inherits: Vec::new(),
        callers: Vec::new(),
        callees: Vec::new(),
        implementations: Vec::new(),
        contexts: Vec::new(),
        ambiguous_contexts: Vec::new(),
        floors: BTreeMap::new(),
    };

    for (index, raw) in text.lines().enumerate() {
        let line = index + 1;
        let stripped = raw.trim();
        if stripped.is_empty() || stripped.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = stripped.split('|').map(str::trim).collect();
        let keyword = fields[0];
        if !KEYWORDS.contains(&keyword) {
            problems.push(Problem {
                file: file.clone(),
                line,
                message: format!("unknown keyword `{keyword}`; expected one of {KEYWORDS:?}"),
            });
            continue;
        }
        if let Err(message) = absorb(&mut corpus, keyword, &fields) {
            problems.push(Problem {
                file: file.clone(),
                line,
                message,
            });
        }
    }

    if problems.is_empty() {
        Ok(corpus)
    } else {
        Err(problems)
    }
}

/// Field counts per keyword, so a mistyped line is a parse error rather than an
/// index panic three lines later.
fn arity(keyword: &str) -> usize {
    match keyword {
        "symbol" => 4,
        "member" | "reference" | "no_reference" | "inherits" | "implements" | "no_inherits" => 4,
        "call" => 5,
        "import" => 5,
        "import_no_module" => 3,
        "callers" | "callees" | "implementations" => 7,
        "context" => 5,
        "context_ambiguous" => 2,
        "floor" => 3,
        _ => 0,
    }
}

fn absorb(corpus: &mut Corpus, keyword: &str, fields: &[&str]) -> Result<(), String> {
    if fields.len() != arity(keyword) {
        return Err(format!(
            "`{keyword}` takes {} fields, this line has {}",
            arity(keyword),
            fields.len()
        ));
    }
    match keyword {
        "symbol" => corpus.symbols.push(Key::new(fields[1], fields[2], fields[3])),
        "member" => corpus.members.push(Labelled {
            path: fields[1].to_owned(),
            subject: fields[2].to_owned(),
            object: fields[3].to_owned(),
        }),
        "call" => corpus.calls.push(LabelledCall {
            path: fields[1].to_owned(),
            kind: fields[2].to_owned(),
            subject: fields[3].to_owned(),
            object: fields[4].to_owned(),
        }),
        "import" => corpus.imports.push(LabelledImport {
            path: fields[1].to_owned(),
            local: fields[2].to_owned(),
            module: fields[3].to_owned(),
            alias: alias_of(fields[4]),
        }),
        "import_no_module" => corpus.import_no_modules.push(Labelled {
            path: fields[1].to_owned(),
            subject: fields[2].to_owned(),
            object: String::new(),
        }),
        "reference" => corpus.references.push(Labelled {
            path: fields[1].to_owned(),
            subject: fields[2].to_owned(),
            object: fields[3].to_owned(),
        }),
        "no_reference" => corpus.no_references.push(Labelled {
            path: fields[1].to_owned(),
            subject: fields[2].to_owned(),
            object: fields[3].to_owned(),
        }),
        "inherits" => corpus.inherits.push(Labelled {
            path: fields[1].to_owned(),
            subject: fields[2].to_owned(),
            object: fields[3].to_owned(),
        }),
        "implements" => corpus.implements.push(Labelled {
            path: fields[1].to_owned(),
            subject: fields[2].to_owned(),
            object: fields[3].to_owned(),
        }),
        "no_inherits" => corpus.no_inherits.push(Labelled {
            path: fields[1].to_owned(),
            subject: fields[2].to_owned(),
            object: fields[3].to_owned(),
        }),
        "callers" | "implementations" => {
            let pair = LabelledPair {
                target: Key::new(fields[1], fields[2], fields[3]),
                expected: Key::new(fields[4], fields[5], fields[6]),
            };
            match keyword {
                "callers" => corpus.callers.push(pair),
                _ => corpus.implementations.push(pair),
            }
        }
        "callees" => corpus.callees.push(LabelledPair {
            target: Key::new(fields[1], fields[2], fields[3]),
            expected: Key::new(fields[4], fields[5], fields[6]),
        }),
        "context" => corpus.contexts.push(LabelledContext {
            question: fields[1].to_owned(),
            expected: Key::new(fields[2], fields[3], fields[4]),
        }),
        "context_ambiguous" => corpus.ambiguous_contexts.push(fields[1].to_owned()),
        "floor" => {
            let name = fields[1].to_owned();
            let value = parse_basis_points(fields[2])
                .ok_or_else(|| format!("`{}` is not a hundredths-of-a-percent figure", fields[2]))?;
            if corpus.floors.insert(name.clone(), value).is_some() {
                return Err(format!("floor `{name}` is declared twice"));
            }
        }
        _ => return Err(format!("unhandled keyword `{keyword}`")),
    }
    Ok(())
}

fn alias_of(field: &str) -> Option<String> {
    match field {
        "-" | "" => None,
        other => Some(other.to_owned()),
    }
}

/// The number of hundredths of a percent in `text`.
///
/// **One spelling, and it is a percentage**: `100`, `73.12`, `0.5`. An earlier
/// version also accepted a bare integer of hundredths, which made `100` mean two
/// different things — a hundred percent in one reading and one percent in the other
/// — and a floor file that reads both ways is worse than one that reads one way.
/// The comparison in `score::Fraction::reaches` still works in exact hundredths, so
/// nothing is lost by requiring the readable form at the boundary.
///
/// Nothing is guessed. A value above a hundred percent is refused rather than
/// clamped, `PLACEHOLDER` is refused so a floor that was never measured cannot
/// become an assertion that asserts nothing, and anything else is refused so a typo
/// cannot become a floor of zero.
/// The floor a spelling declares, in hundredths of a percent.
///
/// Exposed so `score`'s tests can check that the floor a fraction hands out is one
/// this accepts and can reach, which is the pairing that is easy to get wrong: a
/// floor and a measurement are written by different hands and compared by neither.
pub fn floor_of(text: &str) -> Option<u64> {
    parse_basis_points(text)
}

fn parse_basis_points(text: &str) -> Option<u64> {
    if text == "PLACEHOLDER" {
        return None;
    }
    let (whole, fraction) = match text.split_once('.') {
        None => (text, ""),
        Some((whole, fraction)) => (whole, fraction),
    };
    if fraction.len() > 2 || !whole.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let whole: u64 = whole.parse().ok()?;
    let fraction: u64 = if fraction.is_empty() {
        0
    } else {
        format!("{fraction:0<2}").parse().ok()?
    };
    if whole > 100 || (whole == 100 && fraction > 0) {
        return None;
    }
    Some(whole * 100 + fraction)
}

#[cfg(test)]
mod tests {
    use super::{Corpus, Key, alias_of, parse_basis_points};

    fn corpus() -> Corpus {
        parse_fixtures()
    }

    fn parse_fixtures() -> Corpus {
        super::parse(
            peek_core::model::Language::Rust,
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/gate/rust"),
        )
        .unwrap_or_else(|problems| panic!("{}", problems[0]))
    }

    #[test]
    fn the_rust_fixture_parses_with_no_problems() {
        let parsed = corpus();
        assert!(!parsed.symbols.is_empty());
        assert!(!parsed.calls.is_empty());
        assert!(!parsed.imports.is_empty());
        assert!(!parsed.references.is_empty());
        assert!(!parsed.inherits.is_empty());
        assert!(!parsed.contexts.is_empty());
    }

    #[test]
    fn every_labelled_path_is_a_file_the_fixture_actually_contains() {
        // A ground-truth line naming a file that does not exist shrinks the
        // denominator of every dimension and inflates the score, so it is
        // checked against the tree rather than trusted.
        let parsed = corpus();
        let mut checked = 0usize;
        for key in &parsed.symbols {
            assert!(
                parsed.directory.join(&key.path).is_file(),
                "{} names a file the fixture does not contain",
                key.render()
            );
            checked += 1;
        }
        for call in &parsed.calls {
            assert!(
                parsed.directory.join(&call.path).is_file(),
                "call line names a missing file: {call:?}"
            );
            checked += 1;
        }
        assert!(checked > 0);
    }

    #[test]
    fn a_floor_is_read_in_hundredths_of_a_percent() {
        assert_eq!(parse_basis_points("100"), Some(10_000));
        assert_eq!(parse_basis_points("73.12"), Some(7312));
        assert_eq!(parse_basis_points("0.5"), Some(50));
        assert_eq!(parse_basis_points("0"), Some(0));
    }

    #[test]
    fn a_bare_integer_of_hundredths_is_refused_rather_than_guessed_at() {
        // `7312` is 73.12% in the spelling this file uses and 7312% in the one it
        // rejects. Accepting both made `100` mean either a hundred percent or one
        // percent depending on which reading the reader had in mind, so exactly one
        // spelling survives.
        assert_eq!(parse_basis_points("7312"), None);
        assert_eq!(parse_basis_points("1"), Some(100));
    }

    #[test]
    fn a_malformed_floor_is_refused_rather_than_read_as_zero() {
        // The one number in the file that must never be invented: a typo here
        // would otherwise become a floor of zero and stop asserting anything.
        assert_eq!(parse_basis_points("PLACEHOLDER"), None);
        assert_eq!(parse_basis_points(""), None);
        assert_eq!(parse_basis_points("-1"), None);
        assert_eq!(parse_basis_points("100.5"), None);
        assert_eq!(parse_basis_points("1.234"), None);
        assert_eq!(parse_basis_points("abc"), None);
        // Above a hundred percent. A floor of `101` would be a demand nothing can
        // meet, and the gate would fail for a reason that reads like a regression.
        assert_eq!(parse_basis_points("101"), None);
    }

    #[test]
    fn a_dash_means_no_alias_and_a_word_means_one() {
        assert_eq!(alias_of("-"), None);
        assert_eq!(alias_of(""), None);
        assert_eq!(alias_of("Renamed"), Some("Renamed".to_owned()));
    }

    #[test]
    fn a_key_renders_with_the_separator_a_reader_can_check_by_eye() {
        assert_eq!(
            Key::new("src/lib.rs", "function", "summarise").render(),
            "src/lib.rs | function | summarise"
        );
    }

    #[test]
    fn a_line_with_the_wrong_field_count_is_a_parse_error() {
        // Written inline rather than as a fixture, because the failure it guards
        // is a malformed line and the committed fixtures are all well formed.
        let directory = std::env::temp_dir().join(format!(
            "peek-gate-arity-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&directory).expect("create the scratch directory");
        std::fs::write(
            directory.join("gate.expect"),
            "symbol | src/lib.rs | function\ncontext | q | a | b\n",
        )
        .expect("write the expectation file");
        let problems = super::parse(peek_core::model::Language::Rust, &directory)
            .expect_err("both lines are malformed");
        assert_eq!(problems.len(), 2);
        assert!(problems[0].message.contains("takes 4 fields"), "{problems:?}");
        assert!(
            problems[1].message.contains("takes 5 fields"),
            "{problems:?}"
        );
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn an_unknown_keyword_is_a_parse_error_naming_the_line() {
        let directory = std::env::temp_dir().join(format!(
            "peek-gate-keyword-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&directory).expect("create the scratch directory");
        std::fs::write(directory.join("gate.expect"), "symbolic | a | b | c\n")
            .expect("write the expectation file");
        let problems = super::parse(peek_core::model::Language::Rust, &directory)
            .expect_err("the keyword is not in the vocabulary");
        assert_eq!(problems[0].line, 1);
        assert!(problems[0].message.contains("unknown keyword"), "{problems:?}");
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_duplicate_floor_is_refused_rather_than_silently_winning() {
        let directory = std::env::temp_dir().join(format!(
            "peek-gate-floor-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&directory).expect("create the scratch directory");
        std::fs::write(
            directory.join("gate.expect"),
            "floor | query | 50\nfloor | query | 90\n",
        )
        .expect("write the expectation file");
        let problems = super::parse(peek_core::model::Language::Rust, &directory)
            .expect_err("the second floor is a conflict");
        assert!(
            problems[0].message.contains("declared twice"),
            "{problems:?}"
        );
        let _ = std::fs::remove_dir_all(&directory);
    }
}