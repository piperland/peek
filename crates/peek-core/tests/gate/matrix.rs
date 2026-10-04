//! Rendering the gate's numbers into the two published artefacts.
//!
//! `LANGUAGE_MATRIX.md` and `docs/language-matrix.json` are **generated**. They
//! are written by this module from a measurement, and `mod.rs` has a test that
//! fails when the committed copy stops matching a fresh render. That is the only
//! thing that keeps a published number honest: nobody has to remember to update
//! it, and nobody can update it without a measurement behind them.
//!
//! The renderer never rounds away a number and never prints one without its
//! denominator. A cell is `40.96 (34/83)`, and a language with no specification is
//! `not extractable` rather than a row of dashes that could be misread as a
//! failing measurement.

use std::collections::BTreeMap;
use std::path::Path;

use peek_core::extract::LanguageSpec;
use peek_core::model::Language;

use super::measure::{DIMENSIONS, Measurement};

/// The schema tag, so a consumer can refuse a file it does not understand rather
/// than read fields that have moved.
pub const SCHEMA: &str = "peek.language-matrix/1";

/// What each column measures, and what its denominator is.
pub const MEANINGS: &[(&str, &str, &str)] = &[
    (
        "symbol_precision",
        "of the declaration entities the engine emitted, how many the fixture labels",
        "emitted declaration entities, with repository-structure entities held out and counted",
    ),
    (
        "symbol_recall",
        "of the declarations the fixture labels, how many the engine emitted",
        "labelled declarations, counted with multiplicity",
    ),
    (
        "definitions",
        "of the labelled declarations, how many an incoming structural edge points at",
        "distinct labelled declaration identities",
    ),
    (
        "calls",
        "of the labelled call sites, how many produced a call relation",
        "labelled call sites, counted with multiplicity",
    ),
    (
        "references",
        "of the labelled uses of a name, how many produced a reference relation",
        "labelled uses of a name",
    ),
    (
        "resolution_correctness",
        "of the decided relations the fixture says where they must point, how many point at the entity it names",
        "labelled relations the engine placed in a `resolved` or `inferred` state; a relation left undecided is a gap and is counted beside this figure, never inside it",
    ),
    (
        "imports",
        "of the labelled import bindings, how many produced an import relation carrying the same module and alias",
        "labelled import bindings",
    ),
    (
        "imports_module_retained",
        "of the imports whose module path must survive resolution, how many still carry it",
        "labelled imports whose module the graph has to be able to name",
    ),
    (
        "members",
        "of the labelled type members, how many a structural edge reaches from the owning type",
        "labelled type members",
    ),
    (
        "inheritance_subject",
        "of the labelled `inherits`/`implements` clauses, how many produced an edge naming both the base and the type that declared it",
        "labelled inheritance clauses",
    ),
    (
        "inheritance_base",
        "of the labelled `inherits`/`implements` clauses, how many produced an edge naming the base at all",
        "labelled inheritance clauses",
    ),
    (
        "negative_references",
        "of the labelled non-references, how many the engine kept non-references",
        "labelled non-references",
    ),
    (
        "negative_inheritance",
        "of the labelled types that declare no base, how many carry no inheritance edge",
        "labelled types that declare no base",
    ),
    (
        "incremental",
        "of the rows in an incrementally-refreshed index, how many are identical to a full build of the same tree",
        "entity and relation rows, summed over an edit, a delete and a rename, both indexes counted",
    ),
    (
        "query",
        "of the query assertions, how many the surface answered correctly",
        "the hand-written assertions, plus two derived from every labelled call that resolved",
    ),
    (
        "context",
        "of the context questions, how many the pack answered by naming the right entity",
        "labelled context questions, including the ones that must be refused as ambiguous",
    ),
];

// ---------------------------------------------------------------------------
// A minimal JSON writer
// ---------------------------------------------------------------------------

/// A JSON value, so the published file is built from values rather than from
/// string concatenation with hand-placed commas.
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(u64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    fn obj(entries: Vec<(&str, Json)>) -> Json {
        Json::Obj(
            entries
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value))
                .collect(),
        )
    }

    fn str(value: impl Into<String>) -> Json {
        Json::Str(value.into())
    }

    fn render(&self, indent: usize) -> String {
        let pad = "  ".repeat(indent);
        let inner = "  ".repeat(indent + 1);
        match self {
            Json::Null => "null".to_owned(),
            Json::Bool(value) => value.to_string(),
            Json::Num(value) => value.to_string(),
            Json::Str(value) => quote(value),
            Json::Arr(items) if items.is_empty() => "[]".to_owned(),
            Json::Arr(items) => {
                let body = items
                    .iter()
                    .map(|item| format!("{inner}{}", item.render(indent + 1)))
                    .collect::<Vec<_>>()
                    .join(",\n");
                format!("[\n{body}\n{pad}]")
            }
            Json::Obj(entries) if entries.is_empty() => "{}".to_owned(),
            Json::Obj(entries) => {
                let body = entries
                    .iter()
                    .map(|(key, value)| {
                        format!("{inner}{}: {}", quote(key), value.render(indent + 1))
                    })
                    .collect::<Vec<_>>()
                    .join(",\n");
                format!("{{\n{body}\n{pad}}}")
            }
        }
    }
}

/// A JSON string literal.
///
/// Escaped here rather than assumed: a fixture path or a candidate list is the
/// kind of text that eventually contains a quote or a backslash, and a published
/// file that stops being valid JSON is worse than no file.
fn quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if (control as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", control as u32));
            }
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

// ---------------------------------------------------------------------------
// The matrix
// ---------------------------------------------------------------------------

/// One row: a language, and either its measurements or the reason it has none.
#[derive(Debug, Clone)]
pub struct Row {
    pub language: Language,
    pub measurement: Option<Measurement>,
    /// Why this language has no measurements, when it has none.
    pub reason: String,
    pub floors: BTreeMap<String, u64>,
}

impl Row {
    fn cell(&self, dimension: &str) -> String {
        match &self.measurement {
            None => "not extractable".to_owned(),
            Some(measurement) => measurement.get(dimension).render(),
        }
    }

    fn spec(&self) -> Option<&'static LanguageSpec> {
        LanguageSpec::for_language(self.language)
    }
}

/// Build the JSON document.
pub fn to_json(rows: &[Row]) -> String {
    let languages = rows
        .iter()
        .map(|row| {
            let extractable = row.measurement.is_some();
            let mut entries: Vec<(&str, Json)> = vec![
                ("language", Json::str(row.language.as_str())),
                ("advertised_tier", Json::str(row.language.tier().as_str())),
                ("advertisable", Json::Bool(row.language.is_advertisable())),
                ("extractable", Json::Bool(extractable)),
                ("spec_registered", Json::Bool(row.spec().is_some())),
            ];
            match row.spec() {
                None => {
                    entries.push(("spec", Json::Null));
                    entries.push(("reason", Json::str(row.reason.clone())));
                }
                Some(spec) => {
                    entries.push((
                        "spec",
                        Json::obj(vec![
                            ("symbol_rules", Json::Num(spec.symbols.len() as u64)),
                            ("call_rules", Json::Num(spec.calls.len() as u64)),
                            ("import_rules", Json::Num(spec.imports.len() as u64)),
                            ("inheritance", Json::Bool(spec.inheritance.is_some())),
                            ("references", Json::Bool(spec.references.is_some())),
                            ("module_layout", Json::Bool(spec.modules.is_some())),
                        ]),
                    ));
                }
            }
            match &row.measurement {
                None => {}
                Some(measurement) => {
                    entries.push((
                        "measurements",
                        Json::Obj(
                            measurement
                                .dimensions
                                .iter()
                                .map(|dimension| {
                                    let value = dimension.value;
                                    (
                                        dimension.name.to_owned(),
                                        Json::obj(vec![
                                            ("numerator", Json::Num(value.numerator)),
                                            ("denominator", Json::Num(value.denominator)),
                                            (
                                                "percent",
                                                Json::str(
                                                    value
                                                        .hundredths()
                                                        .map_or("unmeasured".to_owned(), |h| {
                                                            format!("{}.{:02}", h / 100, h % 100)
                                                        }),
                                                ),
                                            ),
                                        ]),
                                    )
                                })
                                .collect(),
                        ),
                    ));
                    entries.push((
                        "floors",
                        Json::Obj(
                            row.floors
                                .iter()
                                .map(|(name, basis_points)| {
                                    (
                                        name.clone(),
                                        Json::obj(vec![
                                            ("basis_points", Json::Num(*basis_points)),
                                            (
                                                "percent",
                                                Json::Str(format!(
                                                    "{}.{:02}",
                                                    basis_points / 100,
                                                    basis_points % 100
                                                )),
                                            ),
                                        ]),
                                    )
                                })
                                .collect(),
                        ),
                    ));
                    entries.push((
                        "entity_counts",
                        Json::Obj(
                            measurement
                                .entity_counts
                                .iter()
                                .map(|(kind, count)| (kind.clone(), Json::Num(*count)))
                                .collect(),
                        ),
                    ));
                    entries.push((
                        "entities_held_out_of_the_symbol_denominator",
                        Json::Num(measurement.structural_entities),
                    ));
                    entries.push((
                        "relation_states",
                        Json::Obj(
                            measurement
                                .class_states
                                .iter()
                                .map(|(class, states)| {
                                    (
                                        class.clone(),
                                        Json::Obj(
                                            states
                                                .iter()
                                                .map(|(state, count)| {
                                                    (state.clone(), Json::Num(*count))
                                                })
                                                .collect(),
                                        ),
                                    )
                                })
                                .collect(),
                        ),
                    ));
                    entries.push(("index_report", index_report(&measurement.index_report)));
                    // The two placement counts beside the fraction, because the
                    // fraction alone is the number that can be improved by
                    // declining to answer. A reader who sees `resolution_correctness`
                    // with no denominator beside it cannot tell a resolver that got
                    // its decided edges right from one that decided three and got
                    // them right, and those are very different engines.
                    entries.push((
                        "placement",
                        Json::obj(vec![
                            ("decided", Json::Num(measurement.placement.decided)),
                            ("correct", Json::Num(measurement.placement.correct)),
                            (
                                "wrong",
                                Json::Num(measurement.placement.wrong.len() as u64),
                            ),
                            (
                                "undecided",
                                Json::Num(measurement.placement.undecided.len() as u64),
                            ),
                            (
                                "unlabelled_relations",
                                Json::Num(measurement.placement.absent.len() as u64),
                            ),
                        ]),
                    ));
                }
            }
            Json::obj(entries)
        })
        .collect();

    let document = Json::obj(vec![
        ("schema", Json::str(SCHEMA)),
        (
            "generated_by",
            Json::str("crates/peek-core/tests/language_gate.rs"),
        ),
        ("engine_version", Json::str(peek_core::VERSION)),
        ("contract_clause", Json::str("E4")),
        (
            "dimensions",
            Json::Arr(DIMENSIONS.iter().map(|name| Json::str(*name)).collect()),
        ),
        (
            "dimension_meanings",
            Json::Obj(
                MEANINGS
                    .iter()
                    .map(|(name, measures, denominator)| {
                        (
                            (*name).to_owned(),
                            Json::obj(vec![
                                ("measures", Json::str(*measures)),
                                ("denominator", Json::str(*denominator)),
                            ]),
                        )
                    })
                    .collect(),
            ),
        ),
        ("languages", Json::Arr(languages)),
    ]);
    format!("{}\n", document.render(0))
}

/// Build the Markdown.
///
/// Two tables rather than one fourteen-column table, because a table nobody can
/// read on a screen is not a published matrix.
pub fn to_markdown(rows: &[Row], generated_note: &str) -> String {
    let mut out = String::new();
    out.push_str("# Language capability matrix\n\n");
    out.push_str(generated_note);
    out.push('\n');

    out.push_str(
        "\n## How to read a number\n\n\
         Every cell is a percentage over a stated population: `40.96 (34/83)` means 34 of 83, which is 40.96%.\n\
         A language with no registered specification is written **not extractable**, which is\n\
         the state the registry's own comment promises: no spec, no extraction rules, and no\n\
         claim that anything was measured. It is not a zero, and it is not a failure — there is\n\
         nothing to have measured.\n\n\
         Precision and recall are separate because a language that extracts nothing has perfect\n\
         precision over an empty population, which is why an unmeasured cell says so rather than\n\
         rendering `1.00`.\n",
    );

    out.push_str("\n## What each column measures\n\n");
    out.push_str("| Dimension | Measures | Denominator |\n|---|---|---|\n");
    for (name, measures, denominator) in MEANINGS {
        out.push_str(&format!("| `{name}` | {measures} | {denominator} |\n"));
    }

    out.push_str("\n## Extraction\n\n");
    out.push_str(
        "| Language | Advertised tier | Spec | Symbol precision | Symbol recall | Definitions | \
         Calls | References | Imports | Members |\n",
    );
    out.push_str("|---|---|---|---|---|---|---|---|---|---|\n");
    for row in rows {
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
            row.language.as_str(),
            row.language.tier().as_str(),
            if row.spec().is_some() { "yes" } else { "none" },
            row.cell("symbol_precision"),
            row.cell("symbol_recall"),
            row.cell("definitions"),
            row.cell("calls"),
            row.cell("references"),
            row.cell("imports"),
            row.cell("members"),
        ));
    }

    out.push_str("\n## Resolution and correctness\n\n");
    out.push_str(
        "| Language | Inheritance (subject) | Inheritance (base) | No self-reference | \
         No false inheritance | Resolution correctness | Incremental | Query | Context |\n",
    );
    out.push_str("|---|---|---|---|---|---|---|---|---|\n");
    for row in rows {
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
            row.language.as_str(),
            row.cell("inheritance_subject"),
            row.cell("inheritance_base"),
            row.cell("negative_references"),
            row.cell("negative_inheritance"),
            row.cell("resolution_correctness"),
            row.cell("incremental"),
            row.cell("query"),
            row.cell("context"),
        ));
    }

    out.push_str(
        "\n### Decided and wrong\n\n\
         `resolution_correctness` is a fraction over the relations the engine **decided**, so\n\
         it can be raised by declining to decide more. These counts are published beside it for\n\
         that reason: a language cannot read well by answering less. `Decided and wrong` is a\n\
         confidently wrong edge, which is a claim, and `Undecided` is a gap, which is an\n\
         absence a reader can see. They are never added together.\n\n",
    );
    out.push_str(
        "| Language | Decided | Right | Decided and wrong | Undecided | Labelled but no relation |\n\
         |---|---|---|---|---|---|\n",
    );
    for row in rows {
        let Some(measurement) = &row.measurement else {
            continue;
        };
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} |\n",
            row.language.as_str(),
            measurement.placement.decided,
            measurement.placement.correct,
            measurement.placement.wrong.len(),
            measurement.placement.undecided.len(),
            measurement.placement.absent.len(),
        ));
    }

    out.push_str(
        "\n### Every decided-and-wrong edge, by name\n\n\
         A rate is not an audit: a short list is one somebody can read against the source, and a\n\
         fraction is not. Each line names the relation, the rung that placed it, the entity it\n\
         landed on, and what the fixture says should have happened.\n\n",
    );
    let mut any_wrong = false;
    for row in rows {
        let Some(measurement) = &row.measurement else {
            continue;
        };
        if measurement.placement.wrong.is_empty() {
            continue;
        }
        any_wrong = true;
        out.push_str(&format!("**{}**\n\n", row.language.as_str()));
        for edge in &measurement.placement.wrong {
            out.push_str(&format!("- {edge}\n"));
        }
        out.push('\n');
    }
    if !any_wrong {
        out.push_str("No language has a decided edge pointing at the wrong entity.\n");
    }

    out.push_str("\n## Relation states per class\n\n");
    out.push_str(
        "Counts, not verdicts. An `ambiguous` edge is the engine refusing to guess, which is a\n\
         correct outcome for a name with two candidates and an incorrect one for `mod::f()`.\n\n",
    );
    out.push_str(
        "| Language | Class | Total | Resolved | Inferred | Ambiguous | Unresolved | Pending |\n",
    );
    out.push_str("|---|---|---|---|---|---|---|---|\n");
    for row in rows {
        let Some(measurement) = &row.measurement else {
            continue;
        };
        for class in classes_of_interest(&measurement.class_states) {
            let states = &measurement.class_states[&class];
            let total: u64 = states.values().sum();
            out.push_str(&format!(
                "| {} | `{class}` | {total} | {} | {} | {} | {} | {} |\n",
                row.language.as_str(),
                count(states, "resolved"),
                count(states, "inferred"),
                count(states, "ambiguous"),
                count(states, "unresolved"),
                count(states, "pending"),
            ));
        }
    }

    out.push_str("\n## Languages with no registered specification\n\n");
    out.push_str(
        "Each of these is addressable by the `Language` enum and has no extraction rules. Peek\n\
         skips such a file and says so with a reason; it does not extract zero symbols from it\n\
         and report success, which is the failure the registry's own comment was written to\n\
         prevent.\n\n",
    );
    out.push_str("| Language | Extensions | Advertised tier | Why |\n|---|---|---|---|\n");
    for row in rows {
        if row.measurement.is_some() {
            continue;
        }
        out.push_str(&format!(
            "| {} | {} | {} | {} |\n",
            row.language.as_str(),
            row.language.extensions().join(", "),
            row.language.tier().as_str(),
            row.reason
        ));
    }

    out.push_str(
        "\n## Why no language is promoted by this file\n\n\
         A measurement is not a promotion. `Language::tier` answers `unverified` for every\n\
         language, including the one with a fixture, and that stays true until a recorded\n\
         decision promotes it — because E2 asks for eleven languages each passing E4, and this\n\
         gate has a fixture for one of them. The table above is what the other ten would have to\n\
         be measured against.\n\n\
         A grammar dependency confers nothing. `registry.rs` holds one specification, and a\n\
         language absent from it is absent rather than silently empty, which is the property the\n\
         predecessor did not have.\n",
    );

    out.push_str("\n## Reproducing\n\n```\n./scripts/language-gate.sh\n```\n");
    out
}

/// The counters of a full build, as a value.
///
/// **Not** `IndexReport::summary()`. The summary ends with the wall-clock
/// duration, and this document is compared byte-for-byte against a fresh render by
/// `the_published_matrix_is_the_current_measurement`. A timing in a committed file
/// would make that comparison fail on every run for a reason that has nothing to do
/// with the engine, which is how a check like that gets deleted.
///
/// The duration is available to a human from `cargo test -- --nocapture`; it is
/// simply not part of the published state.
fn index_report(report: &peek_core::indexer::IndexReport) -> Json {
    Json::obj(vec![
        ("generation", Json::Num(report.generation)),
        ("files_indexed", Json::Num(report.files_indexed)),
        ("files_skipped", Json::Num(report.files_skipped)),
        ("files_unsupported", Json::Num(report.files_unsupported)),
        ("files_degraded", Json::Num(report.files_degraded)),
        ("files_removed", Json::Num(report.files_removed)),
        ("entities_written", Json::Num(report.entities_written)),
        ("entities_removed", Json::Num(report.entities_removed)),
        ("relations_written", Json::Num(report.relations_written)),
        ("relations_undecided", Json::Num(report.relations_undecided)),
        ("tests_found", Json::Num(report.tests_found)),
    ])
}

/// The relation classes worth printing, in a fixed order.
fn classes_of_interest(states: &BTreeMap<String, BTreeMap<String, u64>>) -> Vec<String> {
    const ORDER: &[&str] = &[
        "defines",
        "contains",
        "owns",
        "imports",
        "exports",
        "reexports",
        "references",
        "calls",
        "inherits",
        "implements",
    ];
    ORDER
        .iter()
        .filter(|class| states.contains_key(**class))
        .map(|class| (*class).to_owned())
        .collect()
}

fn count(states: &BTreeMap<String, u64>, state: &str) -> u64 {
    states.get(state).copied().unwrap_or(0)
}

/// Where the two artefacts are written, relative to the repository root.
pub fn artefact_paths(root: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    (
        root.join("LANGUAGE_MATRIX.md"),
        root.join("docs").join("language-matrix.json"),
    )
}

#[cfg(test)]
mod tests {
    use super::{Json, MEANINGS, quote};

    #[test]
    fn every_dimension_in_the_meanings_table_is_one_the_gate_measures() {
        // A column nobody fills in is a column that reads as a zero. The two
        // lists have to be the same list.
        for (name, _, _) in MEANINGS {
            assert!(
                super::super::measure::DIMENSIONS.contains(name),
                "{name} is documented but never measured"
            );
        }
        assert_eq!(MEANINGS.len(), super::super::measure::DIMENSIONS.len());
    }

    #[test]
    fn a_string_with_a_quote_in_it_is_still_valid_json() {
        assert_eq!(quote(r#"a"b"#), r#""a\"b""#);
        assert_eq!(quote("a\\b"), r#""a\\b""#);
        assert_eq!(quote("a\nb"), r#""a\nb""#);
        assert_eq!(quote("a\u{1}b"), "\"a\\u0001b\"");
    }

    #[test]
    fn a_control_character_is_escaped_rather_than_written_raw() {
        assert_eq!(quote("\u{7}"), "\"\\u0007\"");
    }

    #[test]
    fn an_empty_object_and_array_render_as_themselves() {
        assert_eq!(Json::Obj(Vec::new()).render(0), "{}");
        assert_eq!(Json::Arr(Vec::new()).render(0), "[]");
    }

    #[test]
    fn nested_values_are_indented_one_level_per_depth() {
        let document = Json::Obj(vec![(
            "a".to_owned(),
            Json::Arr(vec![Json::Obj(vec![("b".to_owned(), Json::Num(1))])]),
        )]);
        assert_eq!(
            document.render(0),
            "{\n  \"a\": [\n    {\n      \"b\": 1\n    }\n  ]\n}"
        );
    }
}
