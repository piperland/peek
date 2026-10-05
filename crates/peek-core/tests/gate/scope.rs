//! Which rule the surviving wrong edges support, and what each candidate rule costs.
//!
//! # The question
//!
//! Four decided-and-wrong edges survived the local-binding work, and they look like one defect:
//! **a candidate the source's own scope can see beat a candidate it cannot.** Two were placed by
//! the import rung on a declaration in another file, and two by the same-file rung on a parameter of
//! a *different* function in the same file.
//!
//! The naive reading — "the source's own scope wins" — is one rule, and it is not the rule. It says
//! nothing at all about the second pair, because in those the wrong target is in the source's own
//! **file** while the source's own scope does not declare the name anywhere. Applied to them the
//! rule does not fail; it endorses the answer. That is the trap, and it is invisible from the four
//! rows alone: all four read as "a rung answered over something nearer", so the naive rule looks
//! like it covers all four.
//!
//! # What this measures
//!
//! Two clauses, priced separately and over the **whole** relation population rather than over the
//! wrong edges, because a measure computed after the fact is a post-hoc explanation:
//!
//! | clause | the claim |
//! |---|---|
//! | **own scope** | the occurrence's own lexical scope declares the name, so a rung's candidate outside that scope cannot be it |
//! | **foreign binding** | a binding declared inside an unrelated declaration is not in scope where the use is written |
//!
//! Each is a predicate over the graph, evaluated over **every relation the index holds**, labelled
//! or not. The four numbers are the four `binding.rs` publishes, and the fourth is the one that
//! stops a clause looking free: a row no placement label claims has no known correctness, and
//! calling it a repair reports a saving nobody measured.
//!
//! The decision is read from three things, and only the first two are counts:
//!
//! * **damage is zero**, for both clauses;
//! * **the two clauses do not cover the same labels**, so one is not a special case of the other;
//! * **every decided-and-wrong row is refused by at least one clause**, so the pair explains the
//!   damage rather than a part of it.
//!
//! [`covers`] answers the second and [`unexplained`] the third. Both are whole-population questions
//! about *scope facts* rather than about where the engine placed anything, so they read the same
//! before and after a fix — which is what lets a measurement of a defect outlive the fix.

use std::collections::{BTreeMap, BTreeSet};

use super::expect::{Bind, Corpus, Key};
use super::measure::{Graph, RelationRow};

/// The two candidate clauses, as a reader would state them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Clause {
    /// The occurrence's own lexical scope declares the name.
    ///
    /// The whole of the naive rule, and the clause whose coverage the measurement is
    /// about.
    OwnScope,
    /// The name is a binding declared inside an unrelated declaration.
    ///
    /// The other half of "same file is not the same scope": a parameter belongs to the
    /// function that declares it and means nothing anywhere else.
    ForeignBinding,
}

impl Clause {
    /// Both, in the order the report prints them.
    pub const BOTH: [Clause; 2] = [Clause::OwnScope, Clause::ForeignBinding];

    /// The clause as a reader would state it.
    pub fn as_str(self) -> &'static str {
        match self {
            Clause::OwnScope => "own scope: the use's own scope declares the name",
            Clause::ForeignBinding => {
                "foreign binding: the name is a binding of a declaration the use is not inside"
            }
        }
    }

    /// Whether this clause refuses the placement the engine made on `row`.
    ///
    /// **A predicate over the graph, not over the label.** It never mentions whether the placement
    /// happens to be right, because a rule whose predicate reads the answer is the finding restated
    /// and would accept every edge it is meant to reject.
    pub fn refuses(self, graph: &Graph, bind: &Bind, row: &RelationRow) -> bool {
        if !row.is_decided() {
            return false;
        }
        let Some(placed) = row.target.as_ref() else {
            return false;
        };
        let scope = graph.enclosing_scope(&key_of_source(bind));
        let in_scope = scope.contains(&placed.key().render());
        match self {
            Clause::OwnScope => !own_scope_declares(graph, bind).is_empty() && !in_scope,
            Clause::ForeignBinding => placed.is_binding() && !in_scope,
        }
    }
}

fn key_of_source(bind: &Bind) -> Key {
    Key::new(&bind.path, &bind.kind, &bind.subject)
}

/// Whether the use's own scope declares `name`, read off the containment chain.
pub fn own_scope_declares(graph: &Graph, bind: &Bind) -> Vec<Key> {
    graph.declared_inside(&graph.enclosing_scope(&key_of_source(bind)), &bind.name)
}

/// Every carrier of `name` that is a binding the use is not written inside.
pub fn foreign_bindings(graph: &Graph, bind: &Bind) -> Vec<Key> {
    let scope = graph.enclosing_scope(&key_of_source(bind));
    graph
        .named(&bind.name)
        .into_iter()
        .filter(|row| row.is_binding() && !scope.contains(&row.key().render()))
        .map(|row| row.key())
        .collect()
}

/// The placement labels each clause's *scope fact* covers.
///
/// **About scope, not about placement**, and that is what makes it readable on either side of a
/// fix. `describe | entry` is covered by both clauses whatever the engine did with it: the name is
/// declared in its own scope, and it is also a binding of three other functions elsewhere in the
/// fixture. `render | count` is covered by the second and not the first, and stays covered after the
/// fix, while the first clause has nothing at all to say about it.
pub fn covers(corpus: &Corpus, graph: &Graph, clause: Clause) -> BTreeSet<String> {
    corpus
        .binds
        .iter()
        .filter(|bind| {
            match clause {
                Clause::OwnScope => !own_scope_declares(graph, bind).is_empty(),
                Clause::ForeignBinding => !foreign_bindings(graph, bind).is_empty(),
            }
        })
        .map(Bind::key)
        .collect()
}

/// One relation row, with the label that identifies it and what that label claims.
pub struct Item<'a> {
    /// A placement label for the row: the five identifying fields, plus the claim.
    ///
    /// Built for every row rather than only for labelled ones, because the price is read over the
    /// whole population and a row nothing labels still has to be priced — as **unknown**, which is
    /// a number and not an omission.
    pub bind: Bind,
    /// Whether a `binds` line claims this row at all, as opposed to claiming that no entity is the
    /// referent. The two are different facts and the price counts them apart.
    pub claimed: bool,
    pub row: &'a RelationRow,
}

/// Every relation the index holds that a placement label could name, with its label and claim.
pub fn items<'a>(corpus: &Corpus, graph: &'a Graph) -> Vec<Item<'a>> {
    let claims = claims_of(corpus);
    every_row(graph)
        .into_iter()
        .map(|row| {
            let key = row_key(row);
            let claim = claims.get(&key);
            Item {
                bind: Bind {
                    class: row.kind.to_owned(),
                    path: row.source.path.clone(),
                    kind: row.source.kind.clone(),
                    subject: row.source.qualified_name.clone(),
                    name: row.target_name.clone(),
                    target: claim.cloned().flatten(),
                },
                claimed: claim.is_some(),
                row,
            }
        })
        .collect()
}

/// Every relation the index holds that a placement label could name.
///
/// Structural classes are left out: they are settled at extraction, they never carry a name lookup,
/// and pricing a rule against them would measure the filter rather than the rule.
fn every_row(graph: &Graph) -> Vec<&RelationRow> {
    graph
        .relations
        .iter()
        .filter(|row| !matches!(row.kind, "contains" | "defines" | "owns"))
        .collect()
}

/// The identity this file gives a row, and the one `Bind::key` gives a label.
fn row_key(row: &RelationRow) -> String {
    format!(
        "{}|{}|{}|{}|{}",
        row.kind,
        row.source.path,
        row.source.kind,
        row.source.qualified_name,
        row.target_name
    )
}

/// The claim each label makes, keyed the way [`row_key`] identifies a row.
fn claims_of(corpus: &Corpus) -> BTreeMap<String, Option<Key>> {
    let mut claims = BTreeMap::new();
    for bind in &corpus.binds {
        claims.insert(bind.key(), bind.target.clone());
    }
    claims
}

/// What one clause would cost and what it would buy, as counts.
///
/// The same four numbers `binding::Price` publishes, for the same reason.
#[derive(Debug, Default, Clone, Copy)]
pub struct Price {
    /// Refused, decided, and the fixture says the placement is not the referent.
    /// **The repair.**
    pub repair: u64,
    /// Refused, decided, and the placement is the referent. **The damage.**
    pub damage: u64,
    /// Refused, and already undecided. A no-op.
    pub no_op: u64,
    /// Refused, decided, and no placement claim. Correctness unknown.
    pub unknown: u64,
}

impl Price {
    /// What the clause costs over `items`, as the four numbers.
    pub fn of(items: &[&Item<'_>], clause: Clause, graph: &Graph) -> Self {
        let mut price = Price::default();
        for item in items {
            if !clause.refuses(graph, &item.bind, item.row) {
                continue;
            }
            match (item.row.is_decided(), item.claimed) {
                (false, _) => price.no_op += 1,
                (true, false) => price.unknown += 1,
                (true, true) if placement_holds(item.bind.target.as_ref(), item.row) => {
                    price.damage += 1;
                }
                (true, true) => price.repair += 1,
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

/// Whether a placement is the one the label names.
///
/// `None` is the `binds_nothing` reading and it is **not** the same as the other `None`: a label
/// saying no entity is the referent is satisfied by no decided edge at all, so a decided row under
/// one is a false claim and not a gap.
pub fn placement_holds(want: Option<&Key>, row: &RelationRow) -> bool {
    match (want, &row.target) {
        (Some(want), Some(got)) => &got.key() == want,
        (None, None) => true,
        _ => false,
    }
}

/// Every decided-and-wrong row in the labelled population that neither clause refuses.
///
/// **Zero is the claim, and it is the claim that survives a fix.** Before either clause was adopted
/// this named every surviving wrong edge; afterwards it is empty, and a future defect neither clause
/// can see brings it back. A measure that only read the wrong edges would go quiet instead.
pub fn unexplained(corpus: &Corpus, graph: &Graph) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for bind in &corpus.binds {
        for row in graph.rows_for(bind) {
            if !row.is_decided() || placement_holds(bind.target.as_ref(), row) {
                continue;
            }
            if Clause::BOTH
                .into_iter()
                .any(|clause| clause.refuses(graph, bind, row))
            {
                continue;
            }
            found.push(format!(
                "{} | the {} rung placed it on {} and neither scope clause sees it",
                row.describe_decided(),
                row.evidence,
                match &row.target {
                    None => "<nothing>".to_owned(),
                    Some(target) => target.render(),
                }
            ));
        }
    }
    found.sort();
    found.dedup();
    found
}

/// Print the priced outcome of both clauses. The numbers the decision is read from are printed
/// rather than asserted, because a reader who wants to check the arithmetic needs the arithmetic.
pub fn report(corpus: &Corpus, graph: &Graph) {
    let all = items(corpus, graph);
    let labelled: Vec<&Item<'_>> = all.iter().filter(|item| item.claimed).collect();
    let unexplained = unexplained(corpus, graph);

    println!("\n--- what a candidate placement rule costs, priced ---");
    println!(
        "  {} relations in the index, {} of them carrying a placement claim",
        all.len(),
        labelled.len()
    );
    for clause in Clause::BOTH {
        println!("  {}", clause.as_str());
        println!(
            "    labelled:   {}",
            Price::of(&labelled, clause, graph).render()
        );
        println!("    every row:  {}", Price::of(&all, clause, graph).render());
        for item in labelled
            .iter()
            .filter(|item| clause.refuses(graph, &item.bind, item.row))
        {
            println!(
                "      {} | {} | placed on {}",
                item.bind.key(),
                item.row.evidence,
                placed_on(item.row)
            );
        }
    }

    let own = covers(corpus, graph, Clause::OwnScope);
    let foreign = covers(corpus, graph, Clause::ForeignBinding);
    println!(
        "  scope facts: {} labels have the name declared in the use's own scope, {} have a binding \
         of an unrelated declaration competing for it, {} have both",
        own.len(),
        foreign.len(),
        own.intersection(&foreign).count()
    );
    println!(
        "  {} labelled sites have the second and not the first, so the first clause has nothing to \
         say about them:",
        foreign.difference(&own).count()
    );
    for key in foreign.difference(&own) {
        println!("    {key}");
    }
    println!(
        "  decided-and-wrong rows neither clause refuses: {}",
        unexplained.len()
    );
    for line in &unexplained {
        println!("    {line}");
    }
}

fn placed_on(row: &RelationRow) -> String {
    match &row.target {
        None => "<nothing>".to_owned(),
        Some(target) => target.render(),
    }
}