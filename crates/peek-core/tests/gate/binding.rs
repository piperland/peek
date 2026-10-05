//! Which rule for a local binding the labelled population supports, and what each costs.
//!
//! # The question
//!
//! Two of the six decided-and-wrong edges name a name that a function body binds with a
//! `let` — `summarise` names `out`, `Runner.run` names `last` — and the fixture says no
//! entity is the referent. Four name something that does exist. Two rules were on the
//! table for the first pair, and **which one is right was not established**:
//!
//! * **P, positional.** A name inside a function body is local.
//! * **Q, a binding.** A name bound by a binder the index holds **no entity** for is local.
//!
//! The two are not close, and the difference is the whole cost. P's population is every
//! use of a name in a body, which includes every field read and every parameter. Q's
//! population is the uses of a name no declaration introduced. A rule that picks the wrong
//! one does not fix two edges; it unresolves a class of real ones, and
//! `resolution_correctness` goes **up** while the graph loses edges — the move this file
//! exists to make visible before anything is made to do it.
//!
//! # How it is decided here
//!
//! Not by reading the fixture and asserting what one expects to find. Both rules are run
//! as classifiers over the **whole `references` population the index holds**, and each is
//! priced in the only currency that matters:
//!
//! | | refused decided-and-right | refused decided-and-wrong | refused already-undecided |
//! |---|---|---|---|
//! | over the labelled population | the damage | the repair | a no-op |
//! | over every relation | the damage | the repair | a no-op |
//!
//! A rule is admissible only when its damage is zero. That is the discriminator, and it is
//! arithmetic rather than taste.
//!
//! # Why this reads the tree and the index separately
//!
//! The syntactic half comes from the fixture's own source: *what binds this name at this
//! byte*. The half about what the index holds comes from the index: whether the relation is
//! decided and whether the fixture says where it must point. Neither half can answer the
//! other's question, and the join between them is on the file and the byte — because a name
//! is not enough. `Entry.label` and `entry.label` are two declarations of one name, and the
//! placement dimensions tell them apart by qualified name while a question about the source
//! can only be told by going back to the byte.
//!
//! [`measure`] **panics** if a relation in the index has no identifier at its span, and
//! that is deliberate: a measurement that silently covered a subset of the population would
//! report a clean result and be worth nothing.

use std::collections::BTreeMap;

use tree_sitter::{Node, Parser};
use tree_sitter_rust::LANGUAGE;

use super::expect::{Corpus, Key};
use super::measure::Graph;
use peek_core::extract::LanguageSpec;
use peek_core::model::Language;

/// The relation class this file measures.
///
/// The two rows the question is about are `references`, and the class is named rather
/// than inferred so that adding a second measurement to `calls` is a decision rather than
/// an accident.
pub const CLASS: &str = "references";

/// Node types whose subtree introduces a name the index holds **no entity** for.
///
/// **A per-language list, and that is the first finding.** A `let` binding is not a
/// declaration: the extractor emits no entity for one, so nothing in the graph can name
/// it. Recognising it therefore needs the grammar, which means the same kind of
/// declarative table as `scope_nodes`, `type_scope_nodes` and `module_nodes` — it cannot
/// live in the shared walker, for exactly the reason `mod` does not.
///
/// Spelled as the measurement needs them and no more. `let_declaration` and
/// `for_expression` carry their name in a `pattern` field, and a `closure_expression` in
/// `parameters`. A node type here that the grammar does not have never fires, and both
/// `every_reference_relation_is_joined_to_exactly_one_occurrence` and
/// `a_binding_is_classified_entityless_at_least_once` then fail — so a wrong spelling
/// cannot read as a clean measurement.
const ENTITYLESS_BINDERS: &[&str] = &["let_declaration", "for_expression", "closure_expression"];

/// What the fixture says about one labelled relation's referent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Claim {
    /// The fixture names an entity the relation must point at.
    Referent(Key),
    /// The fixture says no entity is the referent.
    Nothing,
}

/// What the source says about the occurrence, read from the tree.
#[derive(Debug, Clone)]
pub struct Occurrence {
    /// P: the occurrence is inside a `block`, which is what "inside a function body" means.
    pub in_body: bool,
    /// Q: the node type of the binder that makes this occurrence local — the one it
    /// **introduces**, or the one whose scope it **sits inside**. `None` when no such
    /// binder is in reach.
    pub entityless: Option<String>,
    /// Whether that binder is the one this occurrence is writing. The two cases are the
    /// same claim about the graph and different claims about the source, and the report
    /// prints them apart because the second is the one that is easy to miss.
    pub introduces: bool,
}

/// One `references` relation, with the source's reading of it and the engine's answer.
pub struct Row {
    pub relation: String,
    pub claim: Option<Claim>,
    pub decided: bool,
    pub correct: bool,
    pub occurrence: Occurrence,
    /// The occurrence's first byte, and the source text at it.
    ///
    /// Printed because a relation whose natural key ignores its span appears several times
    /// with an identical render, and "four rows refuse this rule" is not a reading — "these
    /// four bytes do" is.
    pub at: String,
}

impl Row {
    /// Whether this rule would refuse the row.
    pub fn refuses_under(&self, rule: Rule) -> bool {
        match rule {
            Rule::Positional => self.occurrence.in_body,
            Rule::Entityless => self.occurrence.entityless.is_some(),
        }
    }

    /// The row as a reader wants to hear it: what it names, what the fixture claims, what
    /// the engine did, and what the rule would do about it.
    pub fn describe(&self, rule: Rule) -> String {
        format!(
            "{} at {} | the fixture claims {} | {} | {} | this rule would {}",
            self.relation,
            self.at,
            match &self.claim {
                None => "nothing at all".to_owned(),
                Some(Claim::Referent(key)) => key.render(),
                Some(Claim::Nothing) => "no referent".to_owned(),
            },
            if self.decided {
                "decided"
            } else {
                "undecided"
            },
            if self.correct { "right" } else { "wrong" },
            if self.refuses_under(rule) {
                "refuse it"
            } else {
                "keep it"
            },
        )
    }
}

/// The two candidate rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    /// A name inside a function body is local.
    Positional,
    /// A name bound by a binder the index holds no entity for is local.
    Entityless,
}

impl Rule {
    /// Both rules, in the order the report prints them.
    pub const BOTH: [Rule; 2] = [Rule::Positional, Rule::Entityless];

    /// The rule as a reader would state it.
    pub fn as_str(self) -> &'static str {
        match self {
            Rule::Positional => "P: a name inside a function body is local",
            Rule::Entityless => "Q: a name bound by a binder with no entity is local",
        }
    }
}

/// What one rule would cost and what it would buy, as counts.
///
/// **Four numbers, and the fourth is the one that stops a rule looking free.** An earlier
/// reading counted a refused row with no placement claim as a repair, because "not correct"
/// and "correct" are the only two answers a `correct` flag can give. That is false: a row
/// the fixture never claimed has no known correctness at all, and calling it a repair
/// reports a saving that has not been measured. It is counted as unknown, and the decision
/// is read from the damage alone.
#[derive(Debug, Default, Clone, Copy)]
pub struct Price {
    /// Refused, decided, and the fixture says it must point at an entity — and it does not.
    /// **The damage.**
    pub damage: u64,
    /// Refused, decided, and the fixture says no entity is the referent. **The repair.**
    pub repair: u64,
    /// Refused, and already undecided. A no-op.
    pub no_op: u64,
    /// Refused, decided, and no placement claim. **Whether it was right is not known.**
    pub unknown: u64,
}

impl Price {
    fn of(rows: &[&Row], rule: Rule) -> Price {
        let mut price = Price::default();
        for row in rows.iter().filter(|row| row.refuses_under(rule)) {
            if !row.decided {
                price.no_op += 1;
            } else if row.claim.is_none() {
                price.unknown += 1;
            } else if row.correct {
                price.damage += 1;
            } else {
                price.repair += 1;
            }
        }
        price
    }

    fn render(self) -> String {
        format!(
            "{} repair, {} DAMAGE, {} already undecided, {} decided with no placement claim",
            self.repair, self.damage, self.no_op, self.unknown
        )
    }
}

// ---------------------------------------------------------------------------
// The syntactic half
// ---------------------------------------------------------------------------

/// A parser for the fixture's own language.
fn parser() -> Parser {
    let mut parser = Parser::new();
    parser
        .set_language(&LANGUAGE.into())
        .expect("the rust grammar loads");
    parser
}

/// The parse of one source, as the grammar's own shape.
///
/// **A diagnostic, not a claim.** Guessing what a grammar calls a field cost two rounds
/// here: `for_expression`'s `value` was first assumed to hold the iterable alone and then
/// to hold the iterable and the body, and both assumptions were wrong. Printing the tree
/// is cheaper than either.
pub fn sexp(text: &str) -> String {
    parser().parse(text, None).expect("the source parses").root_node().to_sexp()
}

/// The node types the engine's own reference rule matches.
///
/// **Read from the specification rather than written out here.** A copy would be a
/// second place to forget `scoped_identifier`, and forgetting it does not reduce this
/// measurement's population — it makes [`measure`] panic on the nine rows `super::…`
/// writes, which is loud, but loud is not the same as right. `peek_core::extract` is
/// public, so the measurement can ask the same table the walker asks.
fn reference_node_types() -> &'static [&'static str] {
    let spec = LanguageSpec::for_language(Language::Rust).expect("rust has a spec");
    spec.references
        .as_ref()
        .expect("the rust spec declares a reference rule")
        .node_types
}

/// Every occurrence of a reference node in one file, keyed by the byte it starts at.
fn occurrences(text: &str) -> BTreeMap<u32, Occurrence> {
    let mut found = BTreeMap::new();
    let tree = parser().parse(text, None).expect("the fixture parses");
    collect(tree.root_node(), text.as_bytes(), &mut found);
    found
}

fn collect(node: Node<'_>, source: &[u8], found: &mut BTreeMap<u32, Occurrence>) {
    // A name that is the **field** of something is a field, whatever it is read through.
    //
    // This clause is the direct answer to the objection that a name inside a body may still
    // be a field of something, and it was added because the measurement found the case
    // rather than because the argument suggested it: `render` reads `sink.text` and
    // `self.text.push_str(..)`, the local is `sink`, and the name `text` resolves correctly
    // to `Sink.text` today. Without this clause a rule about which names are local refuses
    // those two edges and unresolves them — a wrong edge replaced by a gap, which no
    // published column would show and the gate would score as neither.
    //
    // So a field name is not classified by the binder of the receiver. The receiver is a
    // local; the field is a member of the thing the local holds, and naming it is the one
    // thing about it that reaches a declaration.
    //
    // Recorded with no binder rather than skipped, because the join in `measure` is over
    // every reference relation the index holds and a missing entry would fail the
    // coverage check rather than price the clause.
    if reference_node_types().contains(&node.kind()) {
        let name = String::from_utf8_lossy(&source[node.byte_range()]).into_owned();
        let introduces = introduces_itself(node, source, &name);
        found.insert(
            node.start_byte() as u32,
            Occurrence {
                in_body: has_ancestor_of_kind(node, "block"),
                entityless: (!is_a_field_name(node))
                    .then(|| {
                        introduces
                            .clone()
                            .or_else(|| entityless_binder_in_scope(node, source, &name))
                    })
                    .flatten(),
                introduces: introduces.is_some(),
            },
        );
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect(child, source, found);
    }
}

/// Whether this occurrence **is** a binder's own pattern, rather than sitting inside one.
///
/// # Why the second clause is not the same rule as the first
///
/// "Bound by a binder with no entity" finds `out` in `out.push_str(..)`. It does not find
/// `out` in `let mut out = ..` — and that occurrence is not a weaker case, it is the
/// declaration itself. The index holds no entity for a `let` binding, so nothing can be
/// its referent either, and it resolves today to the very parameter `render.out` the other
/// occurrence is refused for. Ten of the fixture's `binds_nothing` rows are of this shape:
/// one per local binding declared.
///
/// **It is stated as a separate clause rather than folded into the first, because it is
/// separately falsifiable.** `let size = size + 1` is the near miss — the value occurrence
/// is textually after the pattern and must not be caught by it — and folding the two
/// clauses together is how that near miss gets introduced.
///
/// **Not measured by this fixture:** a destructuring pattern, where
/// `let Entry { count, .. } = e` makes `count` a local the index also has no entity for.
/// The clause reads the identifiers inside the pattern, so it should hold, and "should" is
/// not a measurement. It is named here rather than counted.
fn introduces_itself(node: Node<'_>, source: &[u8], name: &str) -> Option<String> {
    let mut current = Some(node);
    while let Some(candidate) = current {
        if ENTITYLESS_BINDERS.contains(&candidate.kind())
            && let Some(pattern) = binding_pattern(candidate)
            && inside_field(&candidate, node, "pattern")
            && binds(&pattern, source, name)
        {
            return Some(candidate.kind().to_owned());
        }
        current = candidate.parent();
    }
    None
}

fn has_ancestor_of_kind(node: Node<'_>, kind: &str) -> bool {
    let mut current = node.parent();
    while let Some(parent) = current {
        if parent.kind() == kind {
            return true;
        }
        current = parent.parent();
    }
    false
}

/// Whether this occurrence is written in the **field** slot of a field access or a field
/// initialiser.
///
/// Two slots, not one: `sink.text` is a `field_expression`'s `field`, and `Sink {
/// text: String::new() }` is a `field_initializer`'s `field`. The second is not a read, but
/// it names the same member and refusing it on the grounds that the enclosing local has no
/// entity would be refusing the local's own declaration.
fn is_a_field_name(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    matches!(parent.kind(), "field_expression" | "field_initializer")
        && parent
            .child_by_field_name("field")
            .is_some_and(|field| field.id() == node.id())
}

/// The nearest entityless binder in scope that introduces `name`, if there is one.
///
/// # A binder is a sibling, not an ancestor
///
/// `let mut out = ...; out.push_str(..)` binds `out` for the second line and the binder is
/// in a **different statement**. Walking the ancestors finds nothing at all, which is not
/// a subtle miss: it classifies nothing, and Q's damage then reads as zero for entirely
/// the wrong reason. So each enclosing block is scanned over its own statements, and only
/// the ones that begin before the occurrence, and the walk continues outwards.
///
/// # When a binding is in force, and the grammar's `value` field is not the answer
///
/// * **Forward.** `let x = ...;` names `x` for what comes after the pattern. An
///   occurrence at or before the end of the pattern is not bound by it, which is what
///   keeps the pattern's own occurrence out.
/// * **Not in the initialiser.** `let size = size + 1` reads the `size` that existed
///   before it, so an occurrence inside the initialiser is not bound by the declaration
///   even though it is textually after the pattern.
/// * **A `for` loop binds its body, and the field that says so is not `value`.** In
///   tree-sitter-rust the iterable *and the block* are both under `for_expression`'s
///   `value` field, so the "not in the initialiser" rule applied to a `for` removes the
///   body as well — which is how `attempt` inside `for attempt in 0..limit { .. attempt
///   .. }` reads as unbound by its own binder. The body has its own field, and the rule
///   is stated per node type rather than as one rule for the three.
fn entityless_binder_in_scope(node: Node<'_>, source: &[u8], name: &str) -> Option<String> {
    let mut current = Some(node);
    while let Some(candidate) = current {
        if candidate.kind() == "block" {
            let mut cursor = candidate.walk();
            for statement in candidate.named_children(&mut cursor) {
                if statement.start_byte() as u32 >= node.start_byte() as u32 {
                    break;
                }
                let Some(binder) = as_binder(statement) else {
                    continue;
                };
                let Some(pattern) = binding_pattern(binder) else {
                    continue;
                };
                if in_force(binder, node, pattern) && binds(&pattern, source, name) {
                    return Some(binder.kind().to_owned());
                }
            }
        }
        current = candidate.parent();
    }
    None
}

/// The binder a statement is, or the binder it wraps.
///
/// **A `for` loop written as a statement is an `expression_statement` around a
/// `for_expression`.** Looking for the binder among the block's direct children therefore
/// never finds one, and the only symptom is that the rule classifies nothing — the same
/// quiet no-op as a misspelt node type, and the reason
/// `every_relation_the_fixture_says_names_nothing_is_bound_by_an_entityless_binder` exists
/// rather than a damage count of zero being taken at face value.
///
/// One level is all the unwrapping that does: a binder nested deeper than this is not a
/// shape the fixture has, and a rule that grew a recursive search here would be guessing
/// about grammars nobody has measured.
fn as_binder(statement: Node<'_>) -> Option<Node<'_>> {
    if ENTITYLESS_BINDERS.contains(&statement.kind()) {
        return Some(statement);
    }
    let mut cursor = statement.walk();
    statement
        .named_children(&mut cursor)
        .find(|child| ENTITYLESS_BINDERS.contains(&child.kind()))
}

/// Whether a binder is in force at `node`.
fn in_force(binder: Node<'_>, node: Node<'_>, pattern: Node<'_>) -> bool {
    if node.start_byte() as u32 <= pattern.end_byte() as u32 {
        return false;
    }
    match binder.kind() {
        "let_declaration" => !inside_field(&binder, node, "value"),
        "for_expression" => !inside_iterable(&binder, &pattern, node),
        "closure_expression" => !inside_field(&binder, node, "return_type"),
        _ => true,
    }
}

/// Whether `node` sits inside the part of a `for` that is evaluated **before** the loop
/// variable exists: the iterable.
///
/// **Written as a sibling test rather than as `child_by_field_name("value")`, because the
/// field does not mean what the name suggests.** An earlier reading assumed `for`'s
/// `value` covered the iterable alone and then that it covered iterable-and-body; neither
/// is safe to assume, and both were written down here and had to be taken out again. The
/// iterable is whichever named child follows the pattern, whatever the grammar calls the
/// field it sits in, and the body is the child after that — which is in the loop variable's
/// scope.
fn inside_iterable(binder: &Node<'_>, pattern: &Node<'_>, node: Node<'_>) -> bool {
    let mut cursor = binder.walk();
    let mut after_pattern = false;
    for child in binder.named_children(&mut cursor) {
        if child.id() == pattern.id() {
            after_pattern = true;
            continue;
        }
        if after_pattern {
            return node.start_byte() >= child.start_byte() && node.end_byte() <= child.end_byte();
        }
    }
    false
}

/// Whether `node` sits inside the named field of `parent`.
///
/// A containment test on the byte range rather than a walk, because a field's subtree is a
/// range and the question is whether one byte is in it.
fn inside_field(parent: &Node<'_>, node: Node<'_>, field: &str) -> bool {
    parent.child_by_field_name(field).is_some_and(|value| {
        node.start_byte() >= value.start_byte() && node.end_byte() <= value.end_byte()
    })
}

/// The subtree of a binding node that carries the names it introduces.
fn binding_pattern(node: Node<'_>) -> Option<Node<'_>> {
    node.child_by_field_name("pattern")
        .or_else(|| node.child_by_field_name("parameters"))
}

/// Whether a binding subtree introduces `name`.
///
/// Taken as the identifiers the pattern holds, which is exact for a binding pattern and
/// over-broad only for a destructuring one — and an over-broad answer here can only make
/// the classifier call *more* names local, which is the direction the damage count guards.
/// Nothing is read from the text beyond the identifier nodes themselves, so a name inside
/// a comment or a string literal cannot satisfy it.
///
/// A named-children walk rather than `descendants`, because the cursor's borrow cannot be
/// threaded through a helper that also needs the node's own lifetime.
fn binds(pattern: &Node<'_>, source: &[u8], name: &str) -> bool {
    if pattern.kind() == "identifier"
        && String::from_utf8_lossy(&source[pattern.byte_range()]) == name
    {
        return true;
    }
    let mut cursor = pattern.walk();
    pattern
        .named_children(&mut cursor)
        .any(|child| binds(&child, source, name))
}

// ---------------------------------------------------------------------------
// The measurement
// ---------------------------------------------------------------------------

/// What the fixture says about one site, keyed the way a relation is keyed.
type Claims = BTreeMap<(String, String, String, String), Claim>;

fn claims_of(corpus: &Corpus) -> Claims {
    let mut claims = Claims::new();
    for bind in &corpus.binds {
        if bind.class != CLASS {
            continue;
        }
        claims.insert(
            (
                bind.path.clone(),
                bind.kind.clone(),
                bind.subject.clone(),
                bind.name.clone(),
            ),
            match &bind.target {
                None => Claim::Nothing,
                Some(key) => Claim::Referent(key.clone()),
            },
        );
    }
    claims
}

/// Every `references` relation in the index, joined to the source and to the fixture.
pub fn measure(corpus: &Corpus, graph: &Graph) -> Vec<Row> {
    let claims = claims_of(corpus);
    let mut trees: BTreeMap<String, BTreeMap<u32, Occurrence>> = BTreeMap::new();
    let mut sources: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut rows = Vec::new();

    for relation in graph.relations.iter().filter(|row| row.kind == CLASS) {
        let by_byte = trees
            .entry(relation.source.path.clone())
            .or_insert_with(|| {
                let file = corpus.directory.join(&relation.source.path);
                let text = std::fs::read_to_string(&file)
                    .unwrap_or_else(|error| panic!("cannot read {}: {error}", file.display()));
                occurrences(&text)
            });
        let source = sources
            .entry(relation.source.path.clone())
            .or_insert_with(|| {
                let file = corpus.directory.join(&relation.source.path);
                std::fs::read(&file)
                    .unwrap_or_else(|error| panic!("cannot read {}: {error}", file.display()))
            });
        let Some(occurrence) = by_byte.get(&relation.start_byte) else {
            panic!(
                "{} starts at byte {} of {}, where the source has no identifier; the join \
                 between the index and the fixture is broken, so every count below would be \
                 over a population nobody chose",
                relation.render(),
                relation.start_byte,
                relation.source.path
            );
        };
        let key = (
            relation.source.path.clone(),
            relation.source.kind.clone(),
            relation.source.qualified_name.clone(),
            relation.target_name.clone(),
        );
        let claim = claims.get(&key).cloned();
        rows.push(Row {
            relation: relation.render(),
            at: snippet(source, relation.start_byte),
            decided: relation.is_decided(),
            // A `binds_nothing` label is satisfied by no decided relation at all, so a
            // decided row under one is wrong — the same reading `score_placement` uses.
            correct: match (&claim, &relation.target) {
                (Some(Claim::Referent(want)), Some(got)) => got.key() == *want,
                _ => false,
            },
            claim,
            occurrence: occurrence.clone(),
        });
    }
    rows
}

/// The source at and around one byte, as a reader can check it against the file.
///
/// Thirty characters either side, on one line, with newlines flattened — enough to see
/// whether the occurrence is `sink.text` or `text.push_str(..)` and not enough to bury the
/// count.
fn snippet(source: &[u8], at: u32) -> String {
    let start = at.saturating_sub(30) as usize;
    let end = ((at as usize) + 30).min(source.len());
    let text = String::from_utf8_lossy(&source[start..end]).replace('\n', " ");
    format!("byte {at}: ...{text}...")
}

/// How many rows a rule refuses, over the whole population.
pub fn refusals(rows: &[Row], rule: Rule) -> usize {
    rows.iter().filter(|row| row.refuses_under(rule)).count()
}

/// Every labelled site the fixture says names **a referent**, and which rules would refuse
/// it. This is the population a rule's damage is counted over.
pub fn named_referents(rows: &[Row]) -> Vec<(String, Rule)> {
    rows.iter()
        .filter(|row| matches!(row.claim, Some(Claim::Referent(_))))
        .flat_map(|row| {
            Rule::BOTH
                .into_iter()
                .filter(move |rule| row.refuses_under(*rule))
                .map(move |rule| (row.relation.clone(), rule))
        })
        .collect()
}

/// Every labelled site the fixture says names **nothing**, with the binder that makes it
/// local and whether that binder is the one the occurrence writes.
///
/// The tuple rather than a sentence, because the assertion that uses it is "no entityless
/// binder reaches this name", and matching a phrase to find that out is how a reading gets
/// attached to the wrong row.
pub fn named_locals(rows: &[Row]) -> Vec<(String, Option<String>, bool)> {
    rows.iter()
        .filter(|row| matches!(row.claim, Some(Claim::Nothing)))
        .map(|row| {
            (
                row.relation.clone(),
                row.occurrence.entityless.clone(),
                row.occurrence.introduces,
            )
        })
        .collect()
}

/// Print the priced outcome of both rules. The numbers the decision is read from are
/// printed rather than asserted, because a reader who wants to check the arithmetic needs
/// the arithmetic.
pub fn report(rows: &[Row]) {
    let labelled: Vec<&Row> = rows.iter().filter(|row| row.claim.is_some()).collect();
    let whole: Vec<&Row> = rows.iter().collect();

    println!("\n--- what treating a local as local costs, priced ---");
    println!(
        "  {} `{CLASS}` relations in the index, {} of them carrying a placement claim",
        rows.len(),
        labelled.len()
    );
    for rule in Rule::BOTH {
        println!("  {}", rule.as_str());
        println!("    labelled:   {}", Price::of(&labelled, rule).render());
        println!("    every row:  {}", Price::of(&whole, rule).render());
        for row in labelled
            .iter()
            .filter(|row| row.refuses_under(rule) && row.decided)
        {
            println!("      {}", row.describe(rule));
        }
        for row in whole
            .iter()
            .filter(|row| row.refuses_under(rule) && row.decided && row.claim.is_none())
        {
            println!(
                "      {} (no placement claim: correctness unknown)",
                row.describe(rule)
            );
        }
    }
}