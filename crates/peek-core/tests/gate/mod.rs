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

mod binding;
mod expect;
mod incremental;
mod matrix;
mod measure;
mod scope;
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
    if std::env::var("PEEK_GATE_DUMP").is_ok() {
        measure::dump(&store, &graph);
    }

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

/// One language's ground truth and the index built from a copy of its fixture.
///
/// The scratch is kept alive for as long as the borrow, so a caller cannot be handed a
/// graph whose tree has already been deleted. Split out of [`measure_language`] because
/// the binding measurement wants the same index and no summary.
fn measured(
    language: Language,
    directory: &Path,
) -> (expect::Corpus, measure::Scratch, measure::Graph) {
    let corpus = expect::parse(language, directory).unwrap_or_else(|problems| {
        panic!(
            "the ground truth for {} does not parse:\n  {}",
            language,
            problems
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n  ")
        )
    });
    let scratch = measure::Scratch::new(&format!("gate-{}", language.as_str()));
    let live = scratch.crate_copy(&corpus.directory);
    let (store, _report) = measure::build(&live);
    let graph = measure::Graph::read(&store);
    (corpus, scratch, graph)
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
        "  placement: {} of {} decided relations point at the entity the fixture names, so {} \
         edges are wrong; {} labelled sites are wrong, {} undecided, {} labelled with no \
         relation, {} of {} labelled relations carry a placement claim",
        measurement.placement.correct,
        measurement.placement.decided,
        measurement.placement.wrong_edges,
        measurement.placement.wrong.len(),
        measurement.placement.undecided.len(),
        measurement.placement.absent.len(),
        measurement.placement.covered,
        measurement.placement.labeled,
    );
    for (key, edge) in measurement.placement.wrong.iter().take(10) {
        println!("  decided and wrong: {edge}");
        // The reachability reading beside the edge it belongs to, keyed rather
        // than matched by prose. The verdict is what tells an extractor defect
        // from a resolver one, and a verdict with no edge beside it is not
        // checkable.
        let Some(reach) = measurement
            .placement
            .reach
            .iter()
            .find(|reach| &reach.key == key)
        else {
            continue;
        };
        println!(
            "    reachability of `{}`: {} ({} in the index declare the name: {})",
            key,
            reach.verdict(),
            reach.carriers.len(),
            if reach.carriers.is_empty() {
                "none".to_owned()
            } else {
                reach.carriers.join(", ")
            }
        );
        println!(
            "    the use's own scope declares: {} (the label names one of them: {})",
            if reach.scope_declarations.is_empty() {
                "nothing".to_owned()
            } else {
                reach.scope_declarations.join(", ")
            },
            reach.label_in_source_scope
        );
    }
    for gap in measurement.placement.absent.iter().take(6) {
        println!("  labelled, no relation: {gap}");
    }
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
    let measured: Vec<&Row> = rows
        .iter()
        .filter(|row| row.measurement.is_some())
        .collect();
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

/// R-020. Two counts of different things must never share an arithmetic population.
///
/// `placement` publishes `decided` (relation rows) beside `wrong_sites` (labelled sites) and
/// `wrong_edges` (rows). A reader who took `wrong_sites` for the row count computed `48 - 6 = 42`
/// where the answer is `48 - 12 = 36`. Both numbers were true about their own quantity and the error
/// was entirely in the reader's arithmetic — which is the failure mode: nothing in the artefact said
/// which population a number belonged to.
///
/// So this asserts the *relationship* rather than the values. `decided - correct` must equal
/// `wrong_edges`, and `wrong_sites` must be at most `wrong_edges`, because one labelled site can
/// carry several rows. If a future change ever conflates the two, the second assertion fails.
#[test]
fn the_placement_counts_are_distinguishable_populations() {
    let rows = all_rows();
    let json = matrix::to_json(&rows);
    let placement = json
        .split("\"placement\"")
        .nth(1)
        .expect("the matrix publishes a placement block");

    let number = |key: &str| -> u64 {
        let at = placement
            .find(&format!("\"{key}\":"))
            .unwrap_or_else(|| panic!("placement publishes `{key}`"))
            + format!("\"{key}\":").len();
        placement[at..]
            .chars()
            .skip_while(|c| !c.is_ascii_digit())
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse()
            .unwrap_or_else(|_| panic!("`{key}` is not a number"))
    };

    let decided = number("decided");
    let correct = number("correct");
    let wrong_edges = number("wrong_edges");
    let wrong_sites = number("wrong_sites");

    assert_eq!(
        decided - correct,
        wrong_edges,
        "every decided relation is either right or wrong: decided {decided} - correct {correct} \
         must be the wrong EDGE count, which is {wrong_edges}. If this fails, the two counts have \
         been mixed."
    );
    assert!(
        wrong_sites <= wrong_edges,
        "a labelled site is a place, not a row: {wrong_sites} sites cannot carry more edges than \
         there are {wrong_edges} wrong edges. If this fails, sites and edges are the same number \
         under two names."
    );
    assert!(
        correct <= decided,
        "correct {correct} cannot exceed decided {decided}: the fraction would be above 1"
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
        committed_markdown,
        markdown,
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
        committed_json,
        json,
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
        let corpus =
            expect::parse(language, &directory).unwrap_or_else(|problems| panic!("{problems:?}"));
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
        // A placement label naming a file the fixture does not contain cannot be
        // checked by anybody, so it is refused here rather than scored as a
        // failure. A *relation* that no `reference`, `call` or `import` line
        // claims is a different matter: the `binds` population is written out in
        // full so it can name relations the existence dimensions deliberately do
        // not score, and it is cross-checked against the file rather than against
        // those lines. Forcing the two to be identical would mean adding existence
        // labels to reach a placement label, and would move a published dimension
        // for a change to an unrelated one.
        for bind in &corpus.binds {
            assert!(
                corpus.directory.join(&bind.path).is_file(),
                "{}: `{}` places a relation in a file the fixture does not contain",
                language.as_str(),
                bind.key(),
            );
            // The source kind is on the line rather than inferred, so a typo in it
            // would silently match nothing and shrink the denominator of the
            // dimension it exists to measure. Checked against the kinds the
            // fixture declares in that file: a relation is written inside a
            // declaration the fixture already labels.
            //
            // **`file` is the one kind it never labels.** A `use` statement is at
            // file level, so an `imports` relation is written in the file entity —
            // and the file entity is held out of the symbol denominator on
            // purpose, because labelling a file is labelling the harness's own
            // layout rather than the language. So `file` is allowed here for the
            // same reason it is excluded there, and no other kind is.
            let kinds_the_fixture_declares = corpus.symbols.iter().filter(|symbol| {
                symbol.path == bind.path && (symbol.kind == bind.kind || bind.kind == "file")
            });
            assert!(
                kinds_the_fixture_declares.count() > 0 || bind.kind == "file",
                "{}: `{}` places a relation in a `{}`, and the fixture labels no such \
                 declaration in `{}`",
                language.as_str(),
                bind.key(),
                bind.kind,
                bind.path,
            );
        }
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
                operation.undecided,
                0,
                "{} for {}: {}",
                operation.name,
                language.as_str(),
                operation.describe()
            );
            assert_eq!(
                operation.orphans,
                0,
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

// ---------------------------------------------------------------------------
// Which rule for a local binding the population supports
// ---------------------------------------------------------------------------
//
// Four tests, and each can fail on its own. They are deliberately not one test: the first
// two are preconditions on the measurement being complete and its entityless list being
// spelled the way the grammar spells it, and a failure in either says the measurement
// never ran rather than that an engine change moved a number. Only the last two are
// claims about the two candidate rules.

#[test]
fn every_reference_relation_is_joined_to_exactly_one_occurrence() {
    // The precondition, by name. `binding::measure` panics on a relation whose span holds
    // no identifier, so this passes when the join is total; it exists so that a failure
    // names the *coverage* rather than arriving as a panic from inside a helper.
    for (language, directory) in discovered() {
        let (corpus, _scratch, graph) = measured(language, &directory);
        let rows = binding::measure(&corpus, &graph);
        assert!(
            !rows.is_empty(),
            "{}: no `{}` relation was joined to the source, so every count would be over an \
             empty population",
            language.as_str(),
            binding::CLASS
        );
        assert_eq!(
            rows.len(),
            graph
                .relations
                .iter()
                .filter(|relation| relation.kind == binding::CLASS)
                .count(),
            "{}: the joined rows are not the relations the index holds",
            language.as_str()
        );
    }
}

#[test]
fn a_binding_is_classified_entityless_at_least_once() {
    // The entityless node-type list has to be spelled the way the grammar spells it, or the
    // classifier simply never fires and Q's damage reads as zero for the wrong reason. This
    // is the check that separates "Q does no harm" from "Q matched nothing".
    for (language, directory) in discovered() {
        let (corpus, _scratch, graph) = measured(language, &directory);
        let rows = binding::measure(&corpus, &graph);
        let entityless = binding::refusals(&rows, binding::Rule::Entityless);
        assert!(
            entityless > 0,
            "{}: no identifier was found to be bound by an entityless binder, so the rule under \
             test matched nothing and its damage count of zero says nothing",
            language.as_str()
        );
        assert!(
            entityless < binding::refusals(&rows, binding::Rule::Positional),
            "{}: `{}` refuses {} occurrences and `{}` refuses {entityless}. A binding inside a \
             body is also inside a body, so the first rule must have the larger population; if \
             the two are the same size the two rules cannot be told apart and this measurement \
             cannot choose between them",
            language.as_str(),
            binding::Rule::Entityless.as_str(),
            entityless,
            binding::Rule::Positional.as_str()
        );
    }
}

#[test]
fn the_binding_rule_damages_no_relation_the_fixture_gives_a_referent_for() {
    // The half of the decision that holds. Q refuses no decided-and-right edge, in the
    // labelled population or in the whole index, so it is the rule that can be adopted.
    for (language, directory) in discovered() {
        let (corpus, _scratch, graph) = measured(language, &directory);
        let rows = binding::measure(&corpus, &graph);
        binding::report(&rows);
        let damage = binding::named_referents(&rows)
            .into_iter()
            .filter(|(_, refused_by)| *refused_by == binding::Rule::Entityless)
            .map(|(relation, _)| relation)
            .collect::<Vec<_>>();
        assert!(
            damage.is_empty(),
            "{}: `{}` refuses {} labelled relations the fixture says do have a referent. The \
             rule is only admissible while that is zero:\n  {}",
            language.as_str(),
            binding::Rule::Entityless.as_str(),
            damage.len(),
            damage.join("\n  ")
        );
    }
}

#[test]
fn the_positional_rule_damages_something_and_that_is_why_it_was_rejected() {
    // The half of the decision that rejects, recorded as a check rather than as a comment.
    //
    // **A rejected rule needs its rejection pinned, or the next reader re-derives it.** P —
    // "a name inside a function body is local" — has the larger population by
    // construction, because a local binding is also inside a body, and that larger
    // population is most of what the fixture says *does* have a referent: a field read, a
    // parameter, an imported symbol. Adopting it would unresolve those, and
    // `resolution_correctness` would go **up** while the graph lost edges.
    //
    // Asserting that P does damage, rather than that it does not, is what makes this a
    // test: it fails if the fixture stops containing the case that refutes P.
    for (language, directory) in discovered() {
        let (corpus, _scratch, graph) = measured(language, &directory);
        let rows = binding::measure(&corpus, &graph);
        let damage = binding::named_referents(&rows)
            .into_iter()
            .filter(|(_, refused_by)| *refused_by == binding::Rule::Positional)
            .map(|(relation, _)| relation)
            .collect::<Vec<_>>();
        println!(
            "{}: `{}` refuses {} decided-and-right relations, which is the cost that rejects it",
            language.as_str(),
            binding::Rule::Positional.as_str(),
            damage.len()
        );
        assert!(
            !damage.is_empty(),
            "{}: `{}` refuses nothing the fixture gives a referent for. Either the rule has \
             become admissible — in which case it is the rule to adopt and this test is lying \
             about why it was rejected — or the fixture no longer contains the field reads, \
             the parameters and the imported symbols that are the whole reason.",
            language.as_str(),
            binding::Rule::Positional.as_str()
        );
    }
}

#[test]
fn every_relation_the_fixture_says_names_nothing_reaches_an_entityless_binder() {
    // The other half, and the one that keeps Q from being vacuously true. If a
    // `binds_nothing` site reached no entityless binder, Q would refuse nothing there
    // either and the decided-and-wrong edges would survive it.
    //
    // **Ten of these rows are the occurrence that *writes* the binding** rather than one
    // inside its scope — one per `let` or `for` the fixture declares. They are counted and
    // named apart, because a rule that only looks at occurrences inside a binding's scope
    // leaves every one of them decided and wrong, and a damage count of zero over the rest
    // is exactly what that incompleteness looks like.
    for (language, directory) in discovered() {
        let (corpus, _scratch, graph) = measured(language, &directory);
        let rows = binding::measure(&corpus, &graph);
        let locals = binding::named_locals(&rows);
        let unbound = locals
            .iter()
            .filter(|(_, binder, _)| binder.is_none())
            .map(|(relation, _, _)| relation.clone())
            .collect::<Vec<_>>();
        assert!(
            unbound.is_empty(),
            "{}: {} labelled relations say no entity is the referent, but no binder with no \
             entity reaches the name, so `{}` would not refuse them and the repair it is \
             supposed to buy would not happen:\n  {}",
            language.as_str(),
            unbound.len(),
            binding::Rule::Entityless.as_str(),
            unbound.join("\n  ")
        );
        let introducing = locals
            .iter()
            .filter(|(_, _, introduces)| *introduces)
            .count();
        println!(
            "  {}: {} relations say no entity is the referent, of which {} are the occurrence \
             that writes the binding rather than one inside its scope",
            language.as_str(),
            locals.len(),
            introducing
        );
    }
}

// ---------------------------------------------------------------------------
// The extractor emits the class the measurement priced
// ---------------------------------------------------------------------------
//
// Three tests, in the order a reader should want them: the class fires at all, it agrees
// with the independent tree walk on every row, and it is on no row the fixture gives a
// referent for. The second is the one that cannot be satisfied by a classifier that does
// nothing, because it is a comparison over the whole population rather than a count of one
// kind of row.

#[test]
fn the_extractor_writes_the_local_binding_class_and_damages_nothing() {
    for (language, directory) in discovered() {
        let (corpus, _scratch, graph) = measured(language, &directory);
        let rows = binding::measure(&corpus, &graph);
        binding::report(&rows);

        // The class has to reach real relations, or "it damaged nothing" is an absence.
        assert!(
            binding::engine_refusals(&rows) > 0,
            "{}: the extractor wrote no `{}` on any of its {} `{}` relations, so the \
             binding table matched nothing and every count below would be over an empty \
             population",
            language.as_str(),
            binding::LOCAL_BINDING,
            rows.len(),
            binding::CLASS
        );

        // The engine's classifier and the measurement are two implementations of one reading,
        // written from the same grammar facts. Their agreement is corroboration rather than
        // proof, and the disagreement list is the part of it that can be acted on.
        let disagreeing = binding::disagreements(&rows);
        assert!(
            disagreeing.is_empty(),
            "{}: the extractor and the measurement disagree on {} of {} `{}` relations. \
             One of the two is wrong about a row nobody looked at by hand:\n  {}",
            language.as_str(),
            disagreeing.len(),
            rows.len(),
            binding::CLASS,
            disagreeing.join("\n  ")
        );

        // The damage, read off the engine's own output rather than off the measurement. A
        // field read, a parameter or an imported symbol refused here would be a correct edge
        // replaced by a gap — the move R-021 rejected rule P for.
        let damaged = binding::named_referents_refused_by_engine(&rows);
        assert!(
            damaged.is_empty(),
            "{}: the extractor wrote `{}` on {} labelled relations the fixture says do have a \
             referent:\n  {}",
            language.as_str(),
            binding::LOCAL_BINDING,
            damaged.len(),
            damaged.join("\n  ")
        );
    }
}

#[test]
fn the_class_reaches_every_relation_the_fixture_says_binds_nothing() {
    // The other half, and the one that stops the class being vacuously safe. If a
    // `binds_nothing` site reached no binder, the damage count above would be zero for
    // entirely the wrong reason — the same quiet no-op a misspelt node type produces.
    //
    // **The second assertion is the completion signal, and it now asserts the repair.** It used
    // to require at least one of these rows to still be decided, which is what it meant while the
    // class was carried by the extractor and read by nothing: every row it printed was an edge the
    // engine had decided and got wrong. `Resolver::decide` now refuses them at its head, so the
    // standing statement is the opposite one — none of them may be decided again. Both directions
    // are live: a class that stopped reaching these rows fails the first assertion, and a resolver
    // that stopped reading it fails the second.
    for (language, directory) in discovered() {
        let (corpus, _scratch, graph) = measured(language, &directory);
        let rows = binding::measure(&corpus, &graph);
        let nothing = rows
            .iter()
            .filter(|row| matches!(row.claim, Some(binding::Claim::Nothing)))
            .count();
        let reached = rows
            .iter()
            .filter(|row| {
                matches!(row.claim, Some(binding::Claim::Nothing)) && row.engine_refuses()
            })
            .count();
        let outstanding = binding::nothing_but_decided(&rows);
        println!(
            "  {}: {reached} of {nothing} relations labelled `binds_nothing` carry `{}`, and {} \
             of those are still decided and wrong",
            language.as_str(),
            binding::LOCAL_BINDING,
            outstanding.len()
        );
        assert_eq!(
            reached,
            nothing,
            "{}: {nothing} labelled relations say no entity is the referent and only {reached} \
             of them carry `{}`, so the others are decided on an entity that does not exist",
            language.as_str(),
            binding::LOCAL_BINDING
        );
        assert!(
            outstanding.is_empty(),
            "{}: {} relations the fixture says bind nothing are decided anyway, so the head rule \
             is not reading the class they carry:\n  {}",
            language.as_str(),
            outstanding.len(),
            outstanding.join("\n  ")
        );
    }
}

// ---------------------------------------------------------------------------
// Which placement rule the surviving wrong edges support
// ---------------------------------------------------------------------------
//
// Three tests, in the order a reader should want them: the price is printed, the two
// clauses are shown to be two, and the pair is shown to be admissible and complete.
// The first is a precondition — a measurement that matches nothing prints zeroes and
// a zero read as "this rule is free" is exactly the absence this file exists to
// catch.

#[test]
fn the_placement_clauses_are_priced_over_the_whole_relation_population() {
    for (language, directory) in discovered() {
        let (corpus, _scratch, graph) = measured(language, &directory);
        scope::report(&corpus, &graph);

        let rows: Vec<&measure::RelationRow> = graph
            .relations
            .iter()
            .filter(|row| !matches!(row.kind, "contains" | "defines" | "owns"))
            .collect();
        assert!(
            !rows.is_empty(),
            "{}: the index holds no relation a placement clause could price, so every count below \
             is over an empty population",
            language.as_str()
        );
    }
}

#[test]
fn the_two_scope_clauses_are_not_one_rule() {
    // The discriminating question, asked over the whole labelled population rather than over the
    // four rows that motivated it.
    //
    // **The claim is about coverage, not about damage, so it survives the fix.** "The source's own
    // scope wins" is the whole of the naive rule. If it were also the whole of the answer, every
    // label whose name competes with a binding of an unrelated declaration would also have that
    // name declared in the use's own scope, and the two clause populations would be the same set.
    // They are not: a field read has no declaration of its own at the use site, so the first clause
    // has nothing to say about it and the second clause is the only thing that does.
    //
    // Asserted as a difference rather than as a count of four, so the test says what it means: if a
    // future fixture made the two populations equal, this would fail with a message naming the rule
    // it would then have to be, rather than silently agreeing with it.
    for (language, directory) in discovered() {
        let (corpus, _scratch, graph) = measured(language, &directory);
        let own = scope::covers(&corpus, &graph, scope::Clause::OwnScope);
        let foreign = scope::covers(&corpus, &graph, scope::Clause::ForeignBinding);
        let only_foreign: Vec<&String> = foreign.difference(&own).collect();

        assert!(
            !own.is_empty(),
            "{}: no labelled relation has the name declared in the use's own scope, so the first \
             clause matched nothing and its damage count of zero says nothing",
            language.as_str()
        );
        assert!(
            !only_foreign.is_empty(),
            "{}: every label the second clause covers is also covered by the first, so \"the source's \
             own scope wins\" would be the whole rule and the second is redundant. The labels it is \
             the only clause about are:\n  {}",
            language.as_str(),
            if only_foreign.is_empty() {
                "none".to_owned()
            } else {
                only_foreign
                    .iter()
                    .map(|key| key.as_str())
                    .collect::<Vec<_>>()
                    .join("\n  ")
            }
        );
    }
}

#[test]
fn no_scope_clause_damages_a_placement_the_fixture_gives_a_referent_for() {
    // The admissibility criterion, and it is arithmetic rather than taste: a clause is admissible
    // only when the edges it would un-place are edges the fixture says are right.
    //
    // **This is the check that would catch the trap in the question.** A rule that preferred the
    // source's own scope blindly would replace `model.rs entry`'s answer to `label` and `count` —
    // the field shorthand reads the *parameter* — with the field of the same name, and
    // `format_line`'s answer to `count` with `Entry.count`. Those are decided-and-right today, and
    // the clause is written so that they are not: a scope declaration wins only over a candidate
    // outside it, and a binding outside the scope is not a candidate at all rather than a weaker
    // one. It cost a damage count of six to find that distinction out — the first version of the
    // predicate asked whether a placement was a *member* of the scope chain rather than a
    // declaration the scope makes, and read every right answer as wrong.
    //
    // **A standing guard rather than a live measurement, and the difference is stated here.** With
    // no wrong edge left on the fixture neither clause refuses anything, so both prices are zero and
    // this assertion is an absence. The half that is still live is
    // `the_two_scope_clauses_are_not_one_rule`, which asks about *scope facts* rather than about
    // placements and reads the same either way. A fixture that grows a wrong edge both clauses
    // refuse would bring this one back.
    for (language, directory) in discovered() {
        let (corpus, _scratch, graph) = measured(language, &directory);
        let items: Vec<scope::Item<'_>> = scope::items(&corpus, &graph);
        let labelled: Vec<&scope::Item<'_>> = items.iter().filter(|item| item.claimed).collect();
        for clause in scope::Clause::BOTH {
            let price = scope::Price::of(&labelled, clause, &graph);
            println!(
                "{}: `{}` refuses {} of {} labelled placements ({price})",
                language.as_str(),
                clause.as_str(),
                price.repair + price.damage,
                labelled.len()
            );
            assert_eq!(
                price.damage,
                0,
                "{}: `{}` refuses {} placements the fixture says are right. The clause is only \
                 admissible while that is zero",
                language.as_str(),
                clause.as_str(),
                price.damage
            );
        }
    }
}

#[test]
fn every_decided_and_wrong_row_is_refused_by_a_scope_clause() {
    // The completion signal, and the half that keeps the other two from being vacuously true.
    //
    // **Zero is the claim, and it is the claim that outlives the fix.** While the four edges were
    // still wrong this named every one of them — each was refused by exactly one of the two
    // clauses — and once the clauses are adopted it is empty. A wrong edge that neither clause can
    // see brings it back, and the assertion prints which one, which is the difference between "the
    // gate is green" and "the gate has nothing left to say".
    for (language, directory) in discovered() {
        let (corpus, _scratch, graph) = measured(language, &directory);
        let unexplained = scope::unexplained(&corpus, &graph);
        assert!(
            unexplained.is_empty(),
            "{}: {} decided-and-wrong rows are placed by a rung and explained by neither scope \
             clause, so whatever caused them is not one of the two rules this measurement prices:\n  {}",
            language.as_str(),
            unexplained.len(),
            unexplained.join("\n  ")
        );
    }
}

#[test]
fn print_the_parse_when_asked() {
    // The diagnostic, off unless PEEK_GATE_TREE is set. Two rounds were lost to guessing
    // what tree-sitter-rust calls a field, and this is cheaper than either.
    if std::env::var("PEEK_GATE_TREE").is_err() {
        return;
    }
    for (_, directory) in discovered() {
        let text = std::fs::read_to_string(directory.join("src").join("service.rs"))
            .expect("the fixture reads");
        println!("{}", binding::sexp(&text));
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
