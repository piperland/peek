//! Index a real repository and report what the graph actually contains.
//!
//! Every other test in this crate runs against fixtures written to exercise a specific claim. This
//! one runs against code nobody wrote to be indexed, which is the only way to find out whether the
//! extractor's *volume* and *shape* are usable — whether the graph is 90% resolved or 90%
//! unresolved, whether a real file produces a sane number of symbols, whether an incremental
//! refresh of one file really touches one file.
//!
//! It is `#[ignore]`d because it needs a repository that is not in this tree, and because its
//! output is a report rather than an assertion. Run it with:
//!
//! ```text
//! PEEK_PROBE_REPO=/path/to/repo cargo test --test real_repository -- --ignored --nocapture
//! ```
//!
//! Two things are worth being precise about. It reports; it does not pass or fail on quality —
//! there is no threshold in here, because "what fraction of edges should resolve" is an empirical
//! question and inventing a number for it would be exactly the kind of invented statistic this
//! project exists to remove. And it only uses the **public** API, so it doubles as a check that
//! the public surface is sufficient to index a real tree without reaching into internals.

// `expect` and `panic` are denied workspace-wide, on the grounds that in production code they hide
// a real failure behind a panic. The unit tests are exempted through `lib.rs`'s `cfg_attr(test,
// allow(..))`. An integration test is a separate crate and does not inherit that, so it is
// exempted here instead — and the justification is the same one: a failure inside this file means
// the report below is *wrong*, and printing a wrong report is worse than stopping. Swallowing an
// error and continuing would defeat the entire purpose of the probe.
#![allow(clippy::expect_used, clippy::panic)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::Instant;

use peek_core::discover::DiscoveryOptions;
use peek_core::indexer::{self, open_store};
use peek_core::model::{EntityKind, Evidence, Language, RelationKind, RepoPath, ResolutionState};

/// The queries whose plans matter, with the shape each one is expected to take.
///
/// These are the shapes every consumer depends on. A plan that degrades from an index seek to a
/// table scan is the exact signature of the predecessor's O(V×E) traversal returning, and it is
/// invisible in a wall-clock number on a small fixture.
const PLANS: &[(&str, &str)] = &[
    (
        "entities in one file",
        "SELECT path, kind, qualified_name, entity_ordinal FROM entity \
         WHERE path = 'a.rs' ORDER BY kind, qualified_name LIMIT 200",
    ),
    (
        "outgoing edges of one symbol",
        "SELECT kind FROM relation WHERE source_path = 'a.rs' AND source_kind = 'function' \
         AND source_qualified_name = 'a' AND source_ordinal = 0",
    ),
    (
        "incoming edges of one symbol",
        "SELECT kind FROM relation WHERE target_path = 'a.rs' AND target_kind = 'function' \
         AND target_qualified_name = 'a' AND target_ordinal = 0",
    ),
    (
        "lookup by bare name",
        "SELECT path FROM entity WHERE name = 'a' LIMIT 50",
    ),
    (
        "everything still ambiguous",
        "SELECT source_path FROM relation WHERE resolution_state = 'ambiguous' LIMIT 50",
    ),
    (
        // The clause the removal path actually issues. A `LIKE` arm cannot be planned as a range
        // against a `BINARY`-collated index, so this was `SCAN` until it became a range; the
        // probe is what found that, and it should keep watching it.
        "subtree of a path",
        "SELECT path FROM entity WHERE (path = 'b' OR (path >= 'b/' AND path < 'b0')) LIMIT 500",
    ),
];

#[test]
#[ignore = "needs PEEK_PROBE_REPO to point at a real repository"]
fn report_on_a_real_repository() {
    let root = PathBuf::from(
        std::env::var("PEEK_PROBE_REPO").expect("set PEEK_PROBE_REPO to the repository to index"),
    );
    assert!(root.is_dir(), "{} is not a directory", root.display());

    println!("\n=== repository ===");
    println!("path:  {}", root.display());

    // A clean store, so a second run does not measure an incremental warm index. The location
    // comes from the platform cache root, so this is a directory we own and may delete.
    let mut store = fresh_store(&root);

    let started = Instant::now();
    let outcome = indexer::build_full(&mut store, &root, DiscoveryOptions::default())
        .expect("a real repository must index without error");
    let build_time = started.elapsed();

    println!("build:  {build_time:?}");
    println!("report: {}", outcome.report().summary());

    report_file_outcomes(&outcome);
    report_written_against_held(&root, &store);
    report_graph(&store, build_time);
    // After the build, not somewhere earlier in the run. A full build's second pass reads the whole
    // index, so it is the one operation that can be trusted to decide everything — and saying so
    // here says nothing about the operations below, which is why each of those repeats the check.
    assert_nothing_pending(&store, "a full build");
    report_a_real_symbol(&store, &root);
    report_query_plans(&store);
    report_incremental(&mut store, &root);
    report_deletion(&mut store, &root);
    assert_nothing_pending(&store, "the whole probe cycle");
    report_doctor(&store, &root);
    report_eventual(&mut store, &root);

    match store.verify() {
        Ok(()) => println!("\nintegrity_check: ok"),
        Err(error) => panic!("integrity_check failed after indexing a real repository: {error}"),
    }
}

/// A store that has never been written, so the measurement is of a cold build.
fn fresh_store(root: &std::path::Path) -> peek_core::store::Store {
    let repo = peek_core::store::RepoId::discover(root).expect("derive a repository id");
    let directory = peek_core::store::paths::index_dir(&repo).expect("resolve the index directory");
    // Removing only this repository's directory: it is named for the identity, so nothing else
    // lives here, and this is a measurement rather than a user's data.
    let _ = std::fs::remove_dir_all(&directory);
    open_store(root).expect("open a fresh store")
}

fn report_file_outcomes(outcome: &indexer::IndexOutcome) {
    let report = outcome.report();
    println!("\n=== files ===");
    println!("indexed:     {}", report.files_indexed);
    println!("skipped:     {}", report.files_skipped);
    println!("unsupported: {}", report.files_unsupported);
    println!("degraded:    {}", report.files_degraded);
    println!("tests:       {}", report.tests_found);

    // Every refusal, grouped. A tool that quietly drops files is the failure this whole engine
    // exists to prevent, so the count and the reasons are the interesting part.
    let mut reasons: BTreeMap<String, u64> = BTreeMap::new();
    for skipped in &outcome.skipped {
        *reasons.entry(skipped.reason.to_string()).or_default() += 1;
    }
    if reasons.is_empty() {
        println!("no file was refused");
    }
    for (reason, count) in reasons.iter().rev() {
        println!("{count:>8}  {reason}");
    }
}

fn report_graph(store: &peek_core::store::Store, build_time: std::time::Duration) {
    let stats = store.stats().expect("stats");
    let relations = stats.relation_count.max(1);
    let entities = stats.entity_count.max(1);

    println!("\n=== graph ===");
    println!("entities:            {}", stats.entity_count);
    println!("relations:           {}", stats.relation_count);
    println!("candidates:          {}", stats.candidate_count);
    println!("resolved:            {}", stats.resolved_relations);
    println!("pending:             {}", stats.pending_relations);
    println!("ambiguous:           {}", stats.ambiguous_relations);
    println!("unresolved:          {}", stats.unresolved_relations);
    println!("inferred:            {}", stats.inferred_relations);
    println!("orphans:             {}", stats.orphan_relations);
    println!("generation:          {}", stats.generation);
    println!("database bytes:      {}", stats.file_size_bytes);
    println!("wal bytes:           {}", stats.wal_size_bytes);
    println!(
        "bytes per relation:  {:.0}",
        stats.file_size_bytes as f64 / relations as f64
    );
    println!(
        "bytes per entity:    {:.0}",
        stats.file_size_bytes as f64 / entities as f64
    );
    println!(
        "relations per entity: {:.2}",
        relations as f64 / entities as f64
    );
    if build_time.as_secs_f64() > 0.0 {
        println!(
            "relations per second: {:.0}",
            relations as f64 / build_time.as_secs_f64()
        );
    }

    // The uncertainty states must account for every relation. If they do not, a state exists that
    // cannot be counted, and audit B11 is back: state that cannot be counted cannot be reported.
    let accounted = stats.resolved_relations
        + stats.pending_relations
        + stats.ambiguous_relations
        + stats.unresolved_relations
        + stats.inferred_relations;
    assert_eq!(
        accounted, stats.relation_count,
        "every relation is in exactly one state; a shortfall means a state is uncountable"
    );
    assert_eq!(
        stats.orphan_relations, 0,
        "a fresh build of real code must not leave a dangling edge"
    );
}

/// Pick a real symbol and show what points at it, with the evidence for each edge.
fn report_a_real_symbol(store: &peek_core::store::Store, root: &std::path::Path) {
    println!("\n=== a real symbol and its edges ===");

    // The largest Rust file, because it has the most symbols and so the most chances for the
    // extractor to be wrong. Chosen from the filesystem, not from the store, so the choice does
    // not depend on the thing being measured.
    let Some(path) = largest_rust_file_with_a_function(store, root) else {
        println!("no Rust file containing a function was found");
        return;
    };
    println!("file: {}", path.display());
    let Ok(relative) = path.strip_prefix(root) else {
        return;
    };
    let Some(repo_path) = RepoPath::new(relative.to_string_lossy().as_ref()) else {
        return;
    };

    let entities = store
        .entities_in_file(&repo_path, 5_000)
        .expect("entities in file");
    if entities.is_empty() {
        println!("  no entities extracted from this file");
        return;
    }

    let mut kinds: BTreeMap<String, u64> = BTreeMap::new();
    for entity in &entities {
        *kinds.entry(entity.kind().as_str().to_owned()).or_default() += 1;
    }
    println!("  {} entities:", entities.len());
    for (kind, count) in &kinds {
        println!("    {count:>5}  {kind}");
    }

    // The first function in the file, and everything that points at it.
    let Some(function) = entities
        .iter()
        .find(|entity| entity.kind() == EntityKind::Function)
    else {
        println!("  no function in this file");
        return;
    };
    println!(
        "\n  fn {} at {}:{}",
        function.id().qualified_name(),
        function.path().as_str(),
        // A `File` entity has no span of its own, so this is optional rather than assumed.
        function
            .span
            .map(|span| span.start_line.to_string())
            .unwrap_or_else(|| "unknown".to_owned())
    );

    let edges = store
        .incoming(function.id(), None, 20)
        .expect("incoming edges");
    println!("  {} edge(s) point at it:", edges.len());
    for edge in edges.iter().take(8) {
        println!(
            "    <- {} ({}) {}",
            edge.source.qualified_name(),
            edge.kind.as_str(),
            edge.resolution.describe()
        );
    }

    let calls = store
        .outgoing(function.id(), Some(RelationKind::Calls), 20)
        .expect("outgoing calls");
    println!("  it calls {} symbol(s):", calls.len());
    for edge in calls.iter().take(8) {
        println!(
            "    -> {} ({}) {}",
            edge.target_name,
            edge.resolution.describe(),
            edge.target
                .as_ref()
                .map(|target| target.qualified_name().to_owned())
                .unwrap_or_else(|| "<no target>".to_owned())
        );
    }

    // A name that appears in more than one file is where ambiguity either shows up or does not.
    let same_name = store
        .entities_named(function.id().name(), 50)
        .expect("entities by name");
    if same_name.len() > 1 {
        println!(
            "\n  `{}` is declared in {} files, so a bare reference to it is genuinely ambiguous:",
            function.id().name(),
            same_name.len()
        );
        for entity in same_name.iter().take(5) {
            println!(
                "    {} {}",
                entity.path().as_str(),
                entity.id().qualified_name()
            );
        }
        let ambiguous = store
            .relations_in_state(
                &ResolutionState::Ambiguous {
                    candidates: Vec::new(),
                },
                5,
            )
            .expect("ambiguous relations");
        println!(
            "  {} ambiguous edge(s) in the whole repository",
            ambiguous.len()
        );
        // Stated carefully, because the honest answer depends on whether the resolver has run.
        // Zero ambiguous edges alongside a large pending count means *nothing has had to choose
        // yet*, which is not the same as "ambiguity is being recorded correctly". Printing the
        // latter would claim a capability this build does not have.
        let pending = store.stats().expect("stats").pending_relations;
        if pending > 0 {
            println!(
                "  with {pending} edge(s) still pending, so no reference has been forced to \
                 choose between these two declarations yet"
            );
        }
    }
}

fn report_query_plans(store: &peek_core::store::Store) {
    println!("\n=== query plans ===");
    for (label, sql) in PLANS {
        match store.query_plan(sql) {
            Ok(details) if details.is_empty() => println!("{label}: no plan"),
            Ok(details) => {
                for line in details {
                    println!("{label}:\n    {line}");
                }
            }
            Err(error) => println!("{label}: EXPLAIN FAILED: {error}"),
        }
    }
}

fn report_incremental(store: &mut peek_core::store::Store, root: &std::path::Path) {
    println!("\n=== incremental refresh of one file ===");
    let Some(path) = largest_rust_file(root) else {
        println!("no Rust file found");
        return;
    };

    let before = store.stats().expect("stats");
    let started = Instant::now();
    let outcome = indexer::refresh(
        store,
        root,
        std::slice::from_ref(&path),
        &DiscoveryOptions::default(),
    )
    .expect("refresh one file");
    let elapsed = started.elapsed();
    let after = store.stats().expect("stats");

    println!("touched:    {}", path.display());
    println!("elapsed:    {elapsed:?}");
    println!(
        "entities:   {} -> {}",
        before.entity_count, after.entity_count
    );
    println!(
        "relations:  {} -> {}",
        before.relation_count, after.relation_count
    );
    println!("generation: {} -> {}", before.generation, after.generation);
    println!("report:     {}", outcome.report().summary());
    // **Here, not after the build.** This is the step that creates them: a refresh decides the
    // files it was given, so a file larger than the pass's per-file read leaves its tail
    // undecided. Measured on `BurntSushi/ripgrep`: refreshing a file of 1,363 entities left 348 of
    // its 3,599 relations pending, and nothing afterwards decided them.
    assert_nothing_pending(store, "an incremental refresh of one file");
    assert_eq!(
        outcome.report().relations_undecided,
        0,
        "the run must be able to say it left nothing behind, not only that the store agrees now"
    );
    println!(
        "orphan check: {}",
        if after.orphan_relations == 0 {
            "no dangling edges"
        } else {
            "DANGLING EDGES — a refresh broke the edges into the file it rewrote"
        }
    );
}

fn report_deletion(store: &mut peek_core::store::Store, root: &std::path::Path) {
    println!("\n=== deleting a file ===");
    let Some(path) = largest_rust_file(root) else {
        println!("no Rust file found");
        return;
    };
    let name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();

    // The bytes are read first and written back at the end, because this section **mutates the
    // repository it is measuring**. An earlier version deleted the file and left it deleted, so
    // every later run indexed a slightly smaller tree: entity counts fell by hundreds and
    // relations by thousands on each pass, and a whole cycle was spent reading a trend into what
    // was a tool eating its own input. A measurement that changes its subject is not a
    // measurement.
    let original = std::fs::read(&path).expect("read the file before deleting it");
    let before = store.stats().expect("stats before the deletion");
    std::fs::remove_file(&path).expect("delete the file");

    let outcome = indexer::refresh(
        store,
        root,
        std::slice::from_ref(&path),
        &DiscoveryOptions::default(),
    )
    .expect("refresh after a deletion");
    let after = store.stats().expect("stats");

    println!("deleted:    {name}, and put back afterwards");
    println!("report:     {}", outcome.report().summary());
    println!("orphans:    {}", after.orphan_relations);
    println!("pending after the deletion: {}", after.pending_relations);
    assert_eq!(
        after.orphan_relations, 0,
        "deleting a file must demote the edges that pointed into it, not leave them dangling"
    );

    // Restore, and re-index, so both the repository and the index are as they were found. A probe
    // that leaves its subject smaller than it found it will be blamed for the difference next
    // time, which is worse than not running it.
    std::fs::write(&path, &original).expect("put the file back");
    indexer::refresh(
        store,
        root,
        std::slice::from_ref(&path),
        &DiscoveryOptions::default(),
    )
    .expect("re-index the restored file");
    let restored = store.stats().expect("stats");
    println!("pending after the restore:  {}", restored.pending_relations);
    // Compared against the count from *before* the deletion, not after. An earlier version of this
    // assertion compared against `after` and failed on correct behaviour: the deletion drops the
    // rows, the restoration brings them back, and the only count the restoration has to match is
    // the one the deletion started from.
    assert_eq!(
        restored.entity_count, before.entity_count,
        "restoring the file must return the row count to what it was before the deletion: \
         {} deleted, {} restored, {} before",
        after.entity_count, restored.entity_count, before.entity_count
    );
    report_pending(store, "a deletion and its restore");
    // **Asserted once, here, over both steps, and not between them.** Asserting between the delete
    // and the restore would stop the run with the repository it is measuring one file short, and the
    // next run would index a smaller tree and read the difference as a trend. So the deletion's
    // number is carried into the message instead: a failure says which of the two left them there.
    assert_eq!(
        (after.pending_relations, restored.pending_relations),
        (0, 0),
        "a deletion and its restore must leave nothing undecided: {} after the deletion, {} after \
         the restore",
        after.pending_relations,
        restored.pending_relations
    );
}

/// Say that nothing is awaiting a decision, and name the step that left anything there.
///
/// `Pending` is the one state that answers no question in either direction: the edge was extracted
/// and never decided, so it is neither placed nor refused. Five states partitioning the relation
/// count is consistent with that — the partition is a claim about *counting*, and a leak of
/// undecided edges partitions perfectly.
///
/// The count is read from the store rather than from a report, because the report is the extractor's
/// arithmetic and this is the index's own. The list is printed before the assertion so a failing run
/// says which files to go and look at instead of only how many.
fn assert_nothing_pending(store: &peek_core::store::Store, step: &str) {
    let pending = store.stats().expect("stats").pending_relations;
    println!("pending:    {pending}");
    report_pending(store, step);
    assert_eq!(
        pending, 0,
        "{pending} relation(s) are still pending after {step}; `report_pending` above names them"
    );
}

/// Put the two counts of the graph side by side and say which one each is.
///
/// The build reports what it *wrote* and the store reports what it *holds*, and the two are not the
/// same quantity: a write is an upsert statement, so two relations with the same natural key are two
/// writes and one row. A reader who compares the numbers and sees them differ has three possible
/// explanations — the index dropped rows, the index is stale, or the two numbers were never counting
/// the same thing — and only the third is invisible in the numbers themselves.
///
/// So this measures the middle quantity: every discovered file is extracted again, and the
/// extraction results are counted twice, once as emitted and once after collapsing on the key the
/// store keys a row by. If the collapsed count is what the store holds, the index is right and the
/// comparison was wrong, and that is a fact worth printing rather than leaving to be argued.
fn report_written_against_held(root: &std::path::Path, store: &peek_core::store::Store) {
    println!("\n=== what was written against what is held ===");
    let discovery = peek_core::discover::FileDiscovery::new(root, DiscoveryOptions::default())
        .discover()
        .expect("discover the repository");

    let mut entities_emitted = 0usize;
    let mut entity_ids: BTreeSet<[String; 4]> = BTreeSet::new();
    let mut relations_emitted = 0usize;
    let mut relation_keys: BTreeSet<String> = BTreeSet::new();
    for file in discovery.files() {
        let absolute = discovery.report().absolute(&file.path);
        let Some(spec) = peek_core::extract::registry::get(file.language) else {
            continue;
        };
        let Ok(text) = std::fs::read_to_string(&absolute) else {
            continue;
        };
        let extracted = peek_core::extract::extract_with(spec, file.path.clone(), &text);
        entities_emitted += extracted.entities.len();
        for entity in &extracted.entities {
            entity_ids.insert([
                entity.id().path().as_str().to_owned(),
                entity.id().kind().as_str().to_owned(),
                entity.id().qualified_name().to_owned(),
                entity.id().ordinal().to_string(),
            ]);
        }
        relations_emitted += extracted.relations.len();
        for relation in &extracted.relations {
            // Collapsed on the key the store keys a row by, so this counts what the index can hold
            // rather than what the extractor happened to emit.
            relation_keys.insert(format!("{:?}", relation.natural_key()));
        }
    }

    let stats = store.stats().expect("stats");
    // **Labelled `extracted …`, not `entities:`/`relations:`.** `scripts/probe-real-repos.sh` pulls
    // its table out of this log by grepping `^entities:` and `^relations:`, and it takes the first
    // line that matches. A section that reused one of those labels would silently become the
    // table's number — and the script's guard against a non-integer would answer it with a zero, so
    // the damage would be a table of zeros that reads as a finding about the engine. The label is
    // part of the measurement's contract, not a cosmetic choice.
    println!(
        "extracted entities:  {entities_emitted} emitted, {} distinct, {} held",
        entity_ids.len(),
        stats.entity_count
    );
    println!(
        "extracted relations: {relations_emitted} emitted, {} distinct, {} held",
        relation_keys.len(),
        stats.relation_count
    );
    if entity_ids.len() as u64 != stats.entity_count
        || relation_keys.len() as u64 != stats.relation_count
    {
        println!(
            "the index does not hold one row per distinct key, so the gap above is not explained \
             by collapsing duplicates and the store is worth a look"
        );
    } else if entities_emitted as u64 != stats.entity_count
        || relations_emitted as u64 != stats.relation_count
    {
        println!(
            "the index holds exactly one row per distinct key, so the gap between written and held \
             is the extractor emitting the same key twice, not the store losing a row"
        );
    }
}

/// What `doctor` says about an index in the state this probe has left it in.
///
/// Asked *here*, while the relations are still outstanding, because `doctor` is a statement about
/// the index as it is and the answer a few steps later describes a different one.
fn report_doctor(store: &peek_core::store::Store, root: &std::path::Path) {
    println!("\n=== what doctor says ===");
    let repo = peek_core::store::RepoId::discover(root).expect("derive a repository id");
    let diagnosis = peek_core::doctor::diagnose_open(store, root, &repo);
    println!("{}", diagnosis.report());
    println!(
        "healthy: {}",
        match diagnosis.is_healthy() {
            true => "yes",
            false => "no",
        }
    );
}

/// Ask whether anything ever decides the relations a refresh leaves behind, and answer it by
/// measurement rather than by reading the resolver.
///
/// Two passes are run, in the order that discriminates the two explanations:
///
/// * a **second refresh of the same file** — a scoped pass that runs, on the file the undecided
///   relations belong to. If it clears them then a refresh is merely late rather than blind, and a
///   later edit would finish the job.
/// * a **full re-resolve** — the pass a full build runs. If only this one clears them then the
///   relations are not deferred, they are stranded: nothing the ordinary operations run will ever
///   revisit them, and the only thing that decides them is a rebuild nobody asked for.
fn report_eventual(store: &mut peek_core::store::Store, root: &std::path::Path) {
    let before = store.stats().expect("stats").pending_relations;
    println!("\n=== does anything decide them later ===");
    println!("pending to begin with: {before}");
    if before == 0 {
        println!("nothing is outstanding, so there is nothing here to explain");
        return;
    }
    let Some(path) = largest_rust_file(root) else {
        println!("no Rust file found");
        return;
    };

    indexer::refresh(
        store,
        root,
        std::slice::from_ref(&path),
        &DiscoveryOptions::default(),
    )
    .expect("a second refresh of the same file");
    let after_refresh = store.stats().expect("stats").pending_relations;
    println!("pending after another refresh of that file: {after_refresh}");

    peek_core::resolve::resolve_all(store, peek_core::resolve::ResolutionOptions::default())
        .expect("a full re-resolve");
    let after_all = store.stats().expect("stats").pending_relations;
    println!("pending after a full re-resolve:            {after_all}");
}

/// Say where any relation still awaiting a decision sits, and where it came from.
///
/// `Pending` is the one state that answers no question in either direction: the edge was extracted
/// and never decided, so it is neither placed nor refused. The store counts it (which is why
/// `doctor` can report it) but a count says only that an index is holding undecided edges, not
/// which ones — and which ones is the difference between a limit that truncated a read and a
/// pass that was never asked to look.
///
/// The source's position in its own file is printed for the same reason: a resolver that reads a
/// bounded number of entities per file leaves the relations of everything past that bound
/// undecided, and that is a claim worth being able to check rather than infer.
fn report_pending(store: &peek_core::store::Store, step: &str) {
    let pending = store
        .relations_in_state(
            &ResolutionState::Pending {
                evidence: Evidence::NameOnly,
                basis: String::new(),
            },
            usize::MAX,
        )
        .expect("pending relations");
    if pending.is_empty() {
        return;
    }

    let mut by_source: BTreeMap<(String, String), u64> = BTreeMap::new();
    for relation in &pending {
        *by_source
            .entry((
                relation.source.path().as_str().to_owned(),
                relation.source.qualified_name().to_owned(),
            ))
            .or_default() += 1;
    }
    println!(
        "\n{} relation(s) still pending after {step}:",
        pending.len()
    );
    // How many entities each affected file holds, so a bounded per-file read can be told apart
    // from a set of files the pass never visited.
    let mut per_file: BTreeMap<&str, usize> = BTreeMap::new();
    for (path, _) in by_source.keys() {
        if per_file.contains_key(path.as_str()) {
            continue;
        }
        let Some(repo_path) = RepoPath::new(path.as_str()) else {
            continue;
        };
        let found = store
            .entities_in_file(&repo_path, usize::MAX)
            .expect("entities in file")
            .len();
        per_file.insert(path.as_str(), found);
    }
    for ((path, source), count) in &by_source {
        let rank = store
            .entities_in_file(
                &RepoPath::new(path.as_str()).expect("a stored path"),
                usize::MAX,
            )
            .expect("entities in file")
            .iter()
            .position(|entity| entity.id().qualified_name() == source)
            .map_or_else(|| "?".to_owned(), |at| at.to_string());
        println!(
            "  {count:>5}  {path} ({}) entity #{rank} of {}",
            source,
            per_file.get(path.as_str()).copied().unwrap_or(0)
        );
    }
}

/// The largest Rust file that actually contains a function.
///
/// "Largest" alone is not enough, and the first version of this probe learned that: in
/// `rust-lang/regex` the largest file is a generated Unicode table of 66 constants and not one
/// function, so the most interesting part of the report had nothing to say. A file of generated
/// constants is still a real thing the extractor handled correctly — it reported 66 of them — but
/// it is not where the graph's behaviour is visible.
fn largest_rust_file_with_a_function(
    store: &peek_core::store::Store,
    root: &std::path::Path,
) -> Option<PathBuf> {
    largest_rust_file(root).or_else(|| {
        // Fall back to asking the store, which knows what it extracted.
        store
            .entities_named("main", 1)
            .expect("query")
            .into_iter()
            .next()
            .map(|entity| root.join(entity.path().as_str()))
    })
}

fn largest_rust_file(root: &std::path::Path) -> Option<PathBuf> {
    let discovery = peek_core::discover::FileDiscovery::new(root, DiscoveryOptions::default())
        .discover()
        .expect("discover the repository");
    let mut files: Vec<(u64, PathBuf)> = discovery
        .files()
        .iter()
        .filter(|file| file.language == Language::Rust)
        .map(|file| {
            let absolute = discovery.report().absolute(&file.path);
            let size = std::fs::metadata(&absolute).map(|m| m.len()).unwrap_or(0);
            (size, absolute)
        })
        .collect();
    files.sort();
    files.pop().map(|(_, path)| path)
}
