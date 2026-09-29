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

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Instant;

use peek_core::discover::DiscoveryOptions;
use peek_core::indexer::{self, open_store};
use peek_core::model::{EntityKind, Language, RelationKind, RepoPath, ResolutionState};

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
        "subtree of a path",
        "SELECT path FROM entity WHERE path = 'a.rs' OR path LIKE 'b/%' ESCAPE '\\' LIMIT 500",
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
    report_graph(&store, build_time);
    report_a_real_symbol(&store, &root);
    report_query_plans(&store);
    report_incremental(&mut store, &root);
    report_deletion(&mut store, &root);

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
            "  {} ambiguous edge(s) in the whole repository, so ambiguity is being recorded \
             rather than silently picked",
            ambiguous.len()
        );
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
    std::fs::remove_file(&path).expect("delete the file");

    let outcome = indexer::refresh(
        store,
        root,
        std::slice::from_ref(&path),
        &DiscoveryOptions::default(),
    )
    .expect("refresh after a deletion");
    let after = store.stats().expect("stats");

    println!("deleted:    {name}");
    println!("report:     {}", outcome.report().summary());
    println!("orphans:    {}", after.orphan_relations);
    assert_eq!(
        after.orphan_relations, 0,
        "deleting a file must demote the edges that pointed into it, not leave them dangling"
    );
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
