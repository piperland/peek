//! The per-language conformance gate.
//!
//! # What this is
//!
//! E4 asks for "per-language measurable gates": symbol precision and recall,
//! definition/call/reference/import/member resolution, inheritance, incremental
//! correctness, query correctness and context quality. This is the mechanism that
//! turns that sentence into numbers, and it is the only thing standing between
//! the registry and a claim nobody can check.
//!
//! `registry.rs` holds one specification, and the `Language` enum advertises
//! twenty-seven. Every other language is therefore **not extractable**, which is
//! the honest state and the one the registry's own comment promises: a language
//! without a spec has no extraction rules, so a file in that language is skipped
//! with a stated reason rather than reported as a file with zero symbols. E6
//! forbids fake language numbers, and E2 is unfalsifiable until a gate like this
//! one can say which of the eleven cohort languages actually works.
//!
//! # How a number is produced
//!
//! For each language with a fixture under `tests/fixtures/gate/<language>/`:
//!
//! 1. copy the fixture to a scratch directory the gate owns;
//! 2. index it with the ordinary `build_full` path;
//! 3. read the graph back through the public store and query API;
//! 4. score each dimension against the hand-written ground truth in `gate.expect`,
//!    as a fraction of a stated population;
//! 5. do the same work incrementally — edit, delete, rename — and compare the
//!    refreshed index with a full build of the identical tree, row for row.
//!
//! Nothing here infers what the engine *should* have found. Every denominator is
//! a population somebody wrote down by reading the fixture source.
//!
//! # Why the published matrix is generated
//!
//! `LANGUAGE_MATRIX.md` and `docs/language-matrix.json` are written from a
//! measurement, and `the_published_matrix_is_the_current_measurement` fails when
//! the committed copy stops matching a fresh render. A number in a document that
//! nobody recomputes is a number that drifts; this way it cannot.

mod expect;
mod incremental;
mod matrix;
mod measure;
mod score;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use peek_core::model::Language;

use matrix::Row;
use measure::{DIMENSIONS, Measurement};

/// The directory holding one subdirectory per language fixture.
fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/gate")
}

/// The repository root, from the crate manifest two levels up.
fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("the repository root resolves")
}

/// Every language that has a fixture, discovered rather than listed.
///
/// A fixture is a directory with a `gate.expect` in it, and its name must parse as
/// a language the enum knows. Nothing is hardcoded, so adding a second language is
/// a directory and an expectation file rather than a code change — which is what
/// makes "runnable on any registered language" true rather than aspirational.
fn discovered() -> Vec<(Language, PathBuf)> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(fixture_root()) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.join("gate.expect").is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        match name.parse::<Language>() {
            Ok(language) => found.push((language, path)),
            Err(error) => panic!("{name}: {error}; a fixture directory names its language"),
        }
    }
    found.sort_by_key(|(language, _)| language.as_str());
    found
}

/// Measure one language.
fn measure_language(language: Language, directory: &Path) -> Row {
    let corpus = match expect::parse(language, directory) {
        Ok(corpus) => corpus,
        Err(problems) => panic!(
            "the ground truth for {} does not parse:\n  {}",
            language,
            problems
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n  ")
        ),
    };
    let floors = corpus.floors.clone();

    let scratch = measure::Scratch::new(&format!("gate-{}", language.as_str()));
    let live = scratch.crate_copy(&corpus.directory);
    let (store, report) = measure::build(&live);
    let graph = measure::Graph::read(&store);

    let incremental = incremental::run(&corpus.directory, language);
    let (query, query_failures) = measure::score_queries(&corpus, &store, &graph);
    let (context, context_failures) = measure::score_context(&corpus, &store);

    let measurement: Measurement = measure::measure(
        &corpus,
        &graph,
        report,
        incremental.fraction(),
        query,
        context,
    )
    .with_failures(query_failures, context_failures);

    print_measurement(&measurement, &incremental);

    Row {
        language,
        measurement: Some(measurement),
        reason: String::new(),
        floors,
    }
}

/// The reason a language has no measurements.
///
/// Two cases, and they are not the same fact. No specification means no
/// extraction rules exist at all. A specification without a fixture means rules
/// exist and nobody has measured them, which is worse in a way: the rules could be
/// wrong and nothing would say so.
fn reason_for(language: Language) -> String {
    match peek_core::extract::LanguageSpec::for_language(language) {
        None => "no LanguageSpec is registered, so this language has no extraction rules; a \
                 file in it is skipped with a stated reason"
            .to_owned(),
        Some(_) => "a LanguageSpec is registered but no labelled fixture exists, so nothing has \
                    been measured"
            .to_owned(),
    }
}

/// Every language the enum knows, in the enum's own order.
fn all_rows() -> Vec<Row> {
    let mut rows = Vec::new();
    for language in Language::ALL {
        match discovered()
            .into_iter()
            .find(|(candidate, _)| *candidate == *language)
        {
            Some((_, directory)) => rows.push(measure_language(*language, &directory)),
            None => rows.push(Row {
                language: *language,
                measurement: None,
                reason: reason_for(*language),
                floors: BTreeMap::new(),
            }),
        }
    }
    rows
}

/// Print one language's numbers, so a run without `--nocapture` still says
/// something useful in its log and a run with it says everything.
fn print_measurement(measurement: &Measurement, incremental: &incremental::Incremental) {
    println!("\n=== {} ===", measurement.language.as_str());
    for dimension in &measurement.dimensions {
        println!("  {:<22} {}", dimension.name, dimension.value.render());
    }
    println!("  index: {}", measurement.index_report.summary());
    println!(
        "  entities held out of the symbol denominator (file, package, module layout): {}",
        measurement.structural_entities
    );
    for operation in &incremental.operations {
        println!("  incremental: {}", operation.describe());
    }
    for failure in measurement.query_failures.iter().take(10) {
        println!("  query failure: {failure}");
    }
    for failure in measurement.context_failures.iter().take(10) {
        println!("  context failure: {failure}");
    }
    for (dimension, gaps) in &measurement.missing {
        if gaps.is_empty() {
            continue;
        }
        println!("  {dimension}: {} unaccounted", gaps.len());
        for gap in gaps.iter().take(6) {
            println!("    - {gap}");
        }
    }
    for (dimension, extras) in &measurement.spurious {
        if extras.is_empty() {
            continue;
        }
        println!("  {dimension}: {} unlabelled", extras.len());
        for extra in extras.iter().take(6) {
            println!("    + {extra}");
        }
    }
}

/// The note the published Markdown carries.
///
/// No date and no run identifier: a generated file that changes on every run is a
/// file whose diff nobody reads, and the freshness check would then be
/// meaningless.
fn generated_note() -> String {
    format!(
        "Generated by `crates/peek-core/tests/language_gate.rs` from a measurement, and checked \
         on every run: a committed copy that no longer matches a fresh render fails the test. \
         Reproduce with `./scripts/language-gate.sh`. Engine version {}.\n",
        peek_core::VERSION
    )
}

fn render(rows: &[Row]) -> (String, String) {
    (
        matrix::to_markdown(rows, &generated_note()),
        matrix::to_json(rows),
    )
}

// ---------------------------------------------------------------------------
// The gate
// ---------------------------------------------------------------------------

#[test]
fn every_registered_language_meets_its_recorded_floor() {
    // One test for the gate itself, because the gate is one claim: this engine can
    // state, per language and per dimension, what it does and what it does not,
    // and it has not gone backwards since the floor was recorded.
    let rows = all_rows();
    let measured: Vec<&Row> = rows.iter().filter(|row| row.measurement.is_some()).collect();
    assert!(
        !measured.is_empty(),
        "no language has a fixture, so the gate measures nothing and every cell in the matrix \
         would read as `not extractable`"
    );

    let mut failures: Vec<String> = Vec::new();
    for row in &measured {
        let measurement = row.measurement.as_ref().expect("a measured row");
        for dimension in DIMENSIONS {
            let Some(floor) = row.floors.get(*dimension) else {
                failures.push(format!(
                    "{}: `{dimension}` has no recorded floor, so nothing asserts it",
                    row.language.as_str()
                ));
                continue;
            };
            let value = measurement.get(dimension);
            if !value.reaches(*floor) {
                failures.push(format!(
                    "{}: `{dimension}` measured {} against a floor of {}.{:02}",
                    row.language.as_str(),
                    value.render(),
                    floor / 100,
                    floor % 100
                ));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "the gate must be able to fail, and it just did:\n  {}",
        failures.join("\n  ")
    );
}

#[test]
fn the_published_matrix_is_the_current_measurement() {
    // The check that makes a published number honest. It fails when the engine
    // changes, and it fails when somebody edits the document by hand, which is the
    // other thing it is for.
    let rows = all_rows();
    let (markdown, json) = render(&rows);
    let (markdown_path, json_path) = matrix::artefact_paths(&repository_root());

    let committed_markdown = std::fs::read_to_string(&markdown_path).unwrap_or_else(|error| {
        panic!(
            "cannot read {}: {error}. Run ./scripts/language-gate.sh with PEEK_GATE_WRITE=1 to \
             write it",
            markdown_path.display()
        )
    });
    assert_eq!(
        committed_markdown, markdown,
        "{} does not match a fresh measurement; run ./scripts/language-gate.sh to regenerate it",
        markdown_path.display()
    );

    let committed_json = std::fs::read_to_string(&json_path).unwrap_or_else(|error| {
        panic!(
            "cannot read {}: {error}. Run ./scripts/language-gate.sh with PEEK_GATE_WRITE=1 to \
             write it",
            json_path.display()
        )
    });
    assert_eq!(
        committed_json, json,
        "{} does not match a fresh measurement; run ./scripts/language-gate.sh to regenerate it",
        json_path.display()
    );
}

#[test]
fn every_language_in_the_enum_has_a_row_and_a_reason() {
    // A language absent from the matrix is a language whose capability is unstated,
    // which is the thing E6 exists to prevent. This asserts the row count and that
    // every unmeasured row says why.
    let rows = all_rows();
    assert_eq!(
        rows.len(),
        Language::ALL.len(),
        "every language the enum advertises needs a row, measured or not"
    );
    for row in &rows {
        if row.measurement.is_none() {
            assert!(
                !row.reason.is_empty(),
                "{} has no measurement and no stated reason",
                row.language.as_str()
            );
            assert!(
                peek_core::extract::LanguageSpec::for_language(row.language).is_none()
                    || !row.reason.contains("no labelled fixture"),
                "{} claims a specification but the reason says it has none",
                row.language.as_str()
            );
        }
    }
}

#[test]
fn a_language_without_a_specification_is_not_extractable() {
    // The row the registry's comment promises, asserted rather than asserted in
    // prose. TypeScript is the sharpest case: it is the first-parity target the
    // predecessor claimed and it has no rules here.
    let rows = all_rows();
    for row in &rows {
        if peek_core::extract::LanguageSpec::for_language(row.language).is_some() {
            continue;
        }
        assert!(
            row.measurement.is_none(),
            "{} has no specification and so nothing to measure",
            row.language.as_str()
        );
        assert!(!row.language.is_advertisable());
        assert_eq!(row.language.tier().as_str(), "unverified");
    }
    assert!(
        rows.iter()
            .any(|row| row.language == Language::TypeScript && row.measurement.is_none()),
        "TypeScript is a first-parity target with no specification; the matrix must say so"
    );
}

#[test]
fn a_registered_language_with_a_fixture_is_measured_on_every_dimension() {
    // A dimension that is measured and then omitted is a column of zeroes. The
    // dimension list, the floor file and the measurement are checked to agree.
    for (language, directory) in discovered() {
        let corpus = expect::parse(language, &directory)
            .unwrap_or_else(|problems| panic!("{problems:?}"));
        let missing: Vec<&str> = DIMENSIONS
            .iter()
            .copied()
            .filter(|dimension| !corpus.floors.contains_key(*dimension))
            .collect();
        assert!(
            missing.is_empty(),
            "{} records no floor for {missing:?}, so those columns are not asserted",
            language.as_str()
        );
        let extra: Vec<&str> = corpus
            .floors
            .keys()
            .map(String::as_str)
            .filter(|name| !DIMENSIONS.contains(name))
            .collect();
        assert!(
            extra.is_empty(),
            "{} records a floor for {extra:?}, which is not a dimension the gate measures",
            language.as_str()
        );
    }
}

#[test]
fn an_incremental_refresh_leaves_nothing_undecided_and_nothing_dangling() {
    // The 348-relation lesson, asserted. An earlier A/B measurement found 348
    // relations stranded as `pending` after refreshing one file of
    // `rust-lang/regex`, and no test could see it because every test read the
    // store after a *full* build. Three operations are run here — edit, delete,
    // rename — and both the index's own count and the refresh's own report are
    // required to be zero.
    for (language, directory) in discovered() {
        let outcome = incremental::run(&directory, language);
        for operation in &outcome.operations {
            assert_eq!(
                operation.undecided, 0,
                "{} for {}: {}",
                operation.name,
                language.as_str(),
                operation.describe()
            );
            assert_eq!(
                operation.orphans, 0,
                "{} for {}: a refresh must demote the edges that pointed into the file it \
                 rewrote, not leave them dangling",
                operation.name,
                language.as_str()
            );
        }
        assert!(
            outcome.is_clean(),
            "{}: an incremental rebuild must agree with a full one:\n  {}",
            language.as_str(),
            outcome
                .operations
                .iter()
                .map(incremental::Operation::describe)
                .collect::<Vec<_>>()
                .join("\n  ")
        );
    }
}

#[test]
fn the_gate_writes_its_artefacts_when_asked() {
    // The write path is a test rather than a side effect of the reading one,
    // because a test suite that rewrites a committed file on every run is a test
    // suite nobody can run on a dirty tree. `PEEK_GATE_WRITE=1` is the opt-in, and
    // this test is the only thing that performs it.
    if std::env::var("PEEK_GATE_WRITE").is_err() {
        return;
    }
    let rows = all_rows();
    let (markdown, json) = render(&rows);
    let (markdown_path, json_path) = matrix::artefact_paths(&repository_root());
    std::fs::create_dir_all(
        json_path
            .parent()
            .expect("the json path has a parent directory"),
    )
    .expect("create the directory the json lives in");
    std::fs::write(&markdown_path, &markdown).expect("write the matrix");
    std::fs::write(&json_path, &json).expect("write the json");
    println!(
        "wrote {} and {}",
        markdown_path.display(),
        json_path.display()
    );
}