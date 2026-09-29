//! The context compiler: `peek(target, budget)`.
//!
//! Everything through the resolver is built. This is the part that turns an index into an
//! answer, and it is the part the product actually is: a caller names a target and a token
//! budget, and gets back a slice of a repository that fits inside the budget and says what it
//! left out.
//!
//! # The five promises, and where each one is discharged
//!
//! **The budget is a hard input.** [`peek`] takes `budget_tokens: u64` and never returns a pack
//! that costs more. This is arithmetic rather than intention: the report is *reserved* before a
//! single unit is priced ([`report_reserve`]), so the space available to content is
//! `budget - reserve` and the report fits in what is left. A budget too small to hold even the
//! report is [`QueryError::BudgetTooSmall`], not a silent overrun.
//!
//! **Dropping is reported.** Every candidate the neighbourhood produced is accounted for: included
//! in `units`, or present in `omitted` with a reason and the cost it would have taken. The
//! rendered answer carries a grouped summary, because a pack that spends its whole budget
//! describing what it left out is not an answer; the complete list is on the struct for a
//! consumer that can afford it.
//!
//! **Uncertainty survives.** Every edge in a pack is a whole [`Relation`], so it carries its typed
//! [`ResolutionState`]. An ambiguous edge renders as `ambiguous (3 candidates)` and, by default,
//! names the three. An unresolved edge renders as `unresolved (external)`. Neither is dropped for
//! being unhelpful, and — this is the part that is easy to get wrong — the edges are *ranked
//! ahead of the certain ones*, so that a per-unit edge cap cannot quietly truncate the pack's
//! uncertainties in favour of its certainties.
//!
//! **Selection is explainable.** Every unit carries an [`InclusionReason`], which is both a
//! machine-readable value and a sentence printed in the rendered answer. There is no path by
//! which a unit enters a pack without a stated reason, and [`RankKey`] is a function of that
//! reason, so the rendering order and the justification are the same fact.
//!
//! **Ranking is deterministic.** The ordering key is
//! `(section, distance, path, kind, qualified_name, ordinal)`. The last four components are the
//! entity's identity, so the key is *total*: no two distinct candidates ever compare equal, and
//! the sort never has to break a tie. Nothing in the order depends on a hash seed, a thread
//! schedule, or a query plan.
//!
//! # What a target can be, and what the answer looks like
//!
//! [`resolve_target`] tries three things in a fixed order and reports which one matched in
//! [`Matched`]. A file, a module, a package, a workspace, a repository and a symbol are all
//! valid targets, and they do not give the same answer: a symbol's neighbourhood is the
//! declaration that contains it and what it names and is named by; a file's is everything
//! declared inside it, plus the edges crossing its boundary. Both are built by
//! [`neighbourhood`], whose rules are written out in its own documentation, and the file
//! membership is an *inferred* edge — see [`crate::query::traverse`] for why.
//!
//! # The shape of a pack
//!
//! ```text
//! peek context pack
//! budget: 8000 tokens requested, 6143 spent, 1857 remaining
//! counted as: ceil(utf8 bytes / 3)
//! status: reduced
//! dropped:
//!   3 unit(s) did not fit the budget
//!   12 edge(s) hit the per-unit edge cap
//!
//! function PaymentService.retry — src/payments/service.rs:42
//!   signature: fn retry(&self, attempt: u32) -> Result<Receipt>
//!   included because: it is what you asked for
//!   -> calls Ledger::commit  [resolved (import_binding)]  src/payments/service.rs:57
//!   -> calls settle  [inferred (unique_name): the only `settle` indexed]  src/payments/service.rs:61
//!   -> calls render  [ambiguous (2 candidates)]  src/payments/service.rs:70
//!      between: src/ui/a.rs::render, src/ui/b.rs::render
//!   -> calls std::io::read_to_string  [unresolved (external)]  src/payments/service.rs:74
//! ```
//!
//! Every line of that is a value on the struct, and every one of them was priced. The arithmetic
//! is stated in [`report_reserve`] and checked by a test that recounts the pack from its parts.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::model::entity::{Entity, EntityId, EntityKind};
use crate::model::path::RepoPath;
use crate::model::relation::{Relation, RelationKey, RelationKind, ResolutionState};
use crate::model::span::Span;
use crate::query::Query;
use crate::query::error::QueryError;
use crate::query::tokens::{Cost, TokenCounter};
use crate::query::traverse::membership;

/// How a target string was matched against the index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Matched {
    /// The string read as a repository path, and a file is declared there.
    Path,
    /// Exactly one entity owns this qualified name.
    QualifiedName,
    /// Exactly one entity declares this bare name.
    Name,
}

impl Matched {
    /// A stable, lowercase label for CLI and MCP output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Matched::Path => "path",
            Matched::QualifiedName => "qualified_name",
            Matched::Name => "name",
        }
    }
}

impl fmt::Display for Matched {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What the caller asked for, and what the index said.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    /// The string the caller passed, verbatim.
    pub query: String,
    /// Which of the three lookups matched.
    pub matched: Matched,
    /// The entity it resolved to: a `File` for a path, a declaration for a name.
    pub entity: Entity,
}

impl Target {
    /// The resolved identity.
    #[must_use]
    pub fn id(&self) -> &EntityId {
        &self.entity.id
    }

    /// The resolved kind.
    #[must_use]
    pub fn kind(&self) -> EntityKind {
        self.entity.kind()
    }

    /// Where it is declared.
    #[must_use]
    pub fn path(&self) -> &RepoPath {
        self.entity.path()
    }

    /// Where it starts, when it has a span. A `File` entity does not.
    #[must_use]
    pub fn span(&self) -> Option<Span> {
        self.entity.span
    }
}

/// How a pack relates to the budget it was given.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetStatus {
    /// Everything the neighbourhood held was included, and the budget was not exhausted.
    ///
    /// A caller who asked for 8000 tokens and received 300 needs to know whether the answer is
    /// complete or whether the compiler gave up — and those are very different situations. The
    /// `remaining_tokens` figure is the difference, and it is reported rather than hidden.
    Complete,
    /// Something was left out to fit the budget. The omissions are named in
    /// [`ContextPack::omitted`] and grouped in the rendered report.
    ///
    /// This is the state a smaller budget *should* produce: a smaller, coherent answer rather than
    /// the same answer cut in half. A unit is never partially included, and the fill never skips
    /// a high-ranked unit to make room for a low-ranked one, so a reduced pack is a prefix of the
    /// full ranking rather than an arbitrary subset.
    Reduced,
    /// The target itself does not fit. This is not a context slice; it is a report that the
    /// budget is too small for the question. [`ContextPack::units`] is empty and every candidate
    /// is in [`ContextPack::omitted`]: the ones that cost more than the whole content budget as
    /// [`OmissionReason::ExceedsBudget`], and the cheaper ones as
    /// [`OmissionReason::BudgetExhausted`], because the target ranks first and so the fill
    /// stopped before reaching any of them.
    ///
    /// This is the one state where the compiler returns `Ok` and an empty answer. It is a
    /// refusal rather than a failure because the target *was* resolved and the graph around it
    /// *was* read; what is missing is room, and saying so is more useful to a caller than an
    /// error string it cannot act on differently.
    Insufficient,
}

impl BudgetStatus {
    /// A stable, lowercase label for CLI and MCP output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            BudgetStatus::Complete => "complete",
            BudgetStatus::Reduced => "reduced",
            BudgetStatus::Insufficient => "insufficient",
        }
    }
}

impl fmt::Display for BudgetStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The budget, and what happened to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetReport {
    /// What the caller asked for, in tokens.
    pub requested_tokens: u64,
    /// What the pack costs, in tokens, by [`Self::counter`]. This includes the report, because the
    /// report is part of the answer: a reader who does not notice a dropped unit has been misled,
    /// and the price of telling it is the point.
    pub spent_tokens: u64,
    /// `requested - spent`, saturating. Non-zero alongside [`BudgetStatus::Complete`] means the
    /// neighbourhood was exhausted before the budget ran out.
    pub remaining_tokens: u64,
    /// The counting rule, carried so the arithmetic can be reproduced.
    pub counter: TokenCounter,
    /// How the pack relates to the budget.
    pub status: BudgetStatus,
    /// Candidates the neighbourhood produced and the compiler examined.
    ///
    /// Every one of them, whether it ended up in [`ContextPack::units`] or in
    /// [`ContextPack::omitted`]: the fill stops at the first declaration the budget cannot take, but
    /// it still prices the rest of the ranking so it can name them with what they would have cost.
    /// A pack that stopped early therefore reports the whole neighbourhood, and a caller holding one
    /// of two candidates can account for both.
    pub candidates_considered: u64,
    /// How many of the target's relation reads stopped at the per-entity cap. Each one hides at
    /// least one candidate, so this is a **lower bound** on what was never reached rather than a
    /// count of it: a count is not available without a query the store does not have. Zero unless
    /// a note says so, because a silently-truncated enumeration is the same defect class as a
    /// silently-truncated answer.
    pub candidates_unexamined: u64,
}

impl BudgetReport {
    /// Whether the pack costs no more than it was allowed.
    #[must_use]
    pub fn within_budget(&self) -> bool {
        self.spent_tokens <= self.requested_tokens
    }
}

/// What kind of thing an [`Omission`] is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Omitted {
    /// A declaration that could have been in the pack.
    Unit,
    /// An edge that could have been in the pack.
    Edge,
}

impl Omitted {
    /// A stable, lowercase label for CLI and MCP output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Omitted::Unit => "unit",
            Omitted::Edge => "edge",
        }
    }
}

/// Why something is not in the pack.
///
/// **The wire form is a bare string**, because every variant here is a unit variant and a unit
/// variant needs no tag: `"reason":"budget_exhausted"`. It used to be written as
/// `{"reason":{"reason":"budget_exhausted"}}`, because this enum carried `#[serde(tag = "reason")]`
/// while sitting inside a struct field *also* called `reason`. A private `OmissionReasonWire`
/// accepts both, so an answer serialised by an older build is still readable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", try_from = "OmissionReasonWire")]
pub enum OmissionReason {
    /// It ranked behind the first declaration that did not fit, so the fill stopped rather than
    /// skipping ahead.
    ///
    /// [`BudgetStatus::Reduced`] is a *prefix* of the full ranking, and this is what makes it one:
    /// the fill stops at the first declaration the budget cannot take, so everything behind it is
    /// dropped with it. Skipping to a smaller one would produce a pack with a hole in the middle,
    /// which is harder to reason about than one that is merely short.
    BudgetExhausted,
    /// This one declaration costs more than everything the budget has left for content, so a
    /// different ordering could not have placed it and only a larger budget will.
    ExceedsBudget,
    /// The declaration was included; this edge was not, because one unit prices at most
    /// `QueryOptions::max_edges_per_unit` edges. The cap is a parameter, so what it hid is a count
    /// rather than a shrug.
    EdgeLimit,
}

/// The two shapes an [`OmissionReason`] is found in on the way in.
///
/// A pack is not persisted by the store, but it does cross a process boundary — the MCP server
/// serialises one, and a client holding an answer from an older build reads it — so the wrapped
/// form is out there in the same way. Accepting both costs ten lines; a client that has to be
/// rebuilt to read a value that did not change meaning is the alternative.
#[derive(Deserialize)]
#[serde(untagged)]
enum OmissionReasonWire {
    /// `{"reason":"budget_exhausted"}` — what an older build wrote.
    Wrapped { reason: String },
    /// `"budget_exhausted"` — what this build writes.
    Bare(String),
}

impl TryFrom<OmissionReasonWire> for OmissionReason {
    type Error = String;

    fn try_from(wire: OmissionReasonWire) -> Result<Self, Self::Error> {
        let text = match wire {
            OmissionReasonWire::Wrapped { reason } | OmissionReasonWire::Bare(reason) => reason,
        };
        Self::parse(&text)
            .ok_or_else(|| format!("{text:?} is not an omission reason this build knows"))
    }
}

impl OmissionReason {
    /// A stable, lowercase label for CLI and MCP output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            OmissionReason::BudgetExhausted => "budget_exhausted",
            OmissionReason::ExceedsBudget => "exceeds_budget",
            OmissionReason::EdgeLimit => "edge_limit",
        }
    }

    /// The reason a spelling names, or `None` for a spelling this build does not have.
    ///
    /// The inverse of [`Self::as_str`]. A stored reason this build cannot name is refused rather
    /// than read as the nearest known one, which would be a claim about why something was dropped
    /// that the answer never made.
    fn parse(text: &str) -> Option<Self> {
        match text {
            "budget_exhausted" => Some(OmissionReason::BudgetExhausted),
            "exceeds_budget" => Some(OmissionReason::ExceedsBudget),
            "edge_limit" => Some(OmissionReason::EdgeLimit),
            _ => None,
        }
    }

    /// The clause used in the rendered report for a declaration.
    #[must_use]
    pub const fn unit_phrase(self) -> &'static str {
        match self {
            OmissionReason::BudgetExhausted => "did not fit the budget",
            OmissionReason::ExceedsBudget => "costs more than the entire budget",
            OmissionReason::EdgeLimit => "hit the per-unit edge cap",
        }
    }

    /// The clause used in the rendered report for an edge.
    #[must_use]
    pub const fn edge_phrase(self) -> &'static str {
        match self {
            OmissionReason::BudgetExhausted => "did not fit beside its unit",
            OmissionReason::ExceedsBudget => "cost more than the entire budget",
            OmissionReason::EdgeLimit => "hit the per-unit edge cap",
        }
    }
}

/// Something the compiler found and did not include.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Omission {
    /// The entity, as `EntityId::display()` prints it. A string rather than an `EntityId` because
    /// an edge that was never included may never have had a target.
    pub subject: String,
    /// Whether a declaration or an edge was dropped.
    pub what: Omitted,
    /// Why.
    pub reason: OmissionReason,
    /// What it would have cost. A number, so "it did not fit" is checkable rather than asserted.
    pub cost: Cost,
}

/// Why a declaration is in the pack.
///
/// The variant *is* the justification and the ranking key, so the two cannot disagree — see
/// [`InclusionReason::section`] for the order and [`InclusionReason::describe`] for the sentence
/// printed in the answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum InclusionReason {
    /// It is what the caller asked for.
    Target,
    /// A structural target contains it: a file's declaration, a module's, a package's.
    Member {
        /// The container.
        of: EntityId,
    },
    /// The declaration that lexically contains the target — the `impl` block, the class, the
    /// namespace. Absent for a top-level declaration, which the index also has no edge for: the
    /// extractor emits `Contains` between nested declarations only, so "no container" is a fact
    /// about the index rather than a gap in the traversal.
    Container {
        /// The container.
        of: EntityId,
    },
    /// Something the target calls.
    Callee {
        /// The caller.
        of: EntityId,
        /// Hops from the target. Always 1: the neighbourhood is depth-1 by construction.
        distance: u32,
    },
    /// Something that calls the target.
    Caller {
        /// The callee.
        of: EntityId,
        /// Hops from the target.
        distance: u32,
    },
    /// Something the target names other than by calling it: a type it mentions, a module it
    /// imports, a constant it reads.
    Referenced {
        /// The source of the edge.
        of: EntityId,
        /// The kind of edge that named it.
        kind: RelationKind,
        /// Hops from the target.
        distance: u32,
    },
}

impl InclusionReason {
    /// The pack's section order, as a small integer.
    ///
    /// A *product* decision rather than a measured threshold, so it is defended as one:
    ///
    /// * `0` — the target. Nothing outranks the thing that was asked for, and nothing may
    ///   displace it.
    /// * `1` — the content of a structural target. For a file, this is the file's declarations,
    ///   which *are* the answer.
    /// * `2` — the declaration that contains the target, so a method arrives with its type.
    /// * `3` — what the target calls. The first question anyone asks about a function.
    /// * `4` — what calls the target. The second.
    /// * `5` — everything else it names.
    #[must_use]
    pub const fn section(&self) -> u8 {
        match self {
            InclusionReason::Target => 0,
            InclusionReason::Member { .. } => 1,
            InclusionReason::Container { .. } => 2,
            InclusionReason::Callee { .. } => 3,
            InclusionReason::Caller { .. } => 4,
            InclusionReason::Referenced { .. } => 5,
        }
    }

    /// Hops from the target, for the second component of [`RankKey`].
    #[must_use]
    pub const fn distance(&self) -> u32 {
        match self {
            InclusionReason::Target => 0,
            InclusionReason::Member { .. } => 1,
            InclusionReason::Container { .. } => 0,
            InclusionReason::Callee { distance, .. }
            | InclusionReason::Caller { distance, .. }
            | InclusionReason::Referenced { distance, .. } => *distance,
        }
    }

    /// One sentence, printed in the rendered answer.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            InclusionReason::Target => "it is what you asked for.".to_owned(),
            InclusionReason::Member { of } => {
                format!("`{}` declares or contains it.", of.display())
            }
            InclusionReason::Container { of } => format!("`{}` contains the target.", of.display()),
            InclusionReason::Callee { of, distance } => {
                format!("`{}` calls it, {distance} hop away.", of.display())
            }
            InclusionReason::Caller { of, distance } => {
                format!("it calls `{}`, {distance} hop away.", of.display())
            }
            InclusionReason::Referenced { of, kind, distance } => {
                format!(
                    "`{}` names it as a `{kind}` target, {distance} hop away.",
                    of.display()
                )
            }
        }
    }
}

/// One edge in a pack.
///
/// A whole [`Relation`], so there is exactly one place the state lives and the output cannot
/// disagree with the index about whether an edge was proven.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextEdge {
    /// The relation, with its typed [`ResolutionState`].
    pub relation: Relation,
    /// What this edge costs in the pack.
    pub cost: Cost,
}

impl ContextEdge {
    /// The resolution state, in the model's own one-line vocabulary.
    #[must_use]
    pub fn state(&self) -> String {
        self.relation.resolution.describe()
    }

    /// The candidates an ambiguous edge was ambiguous between.
    #[must_use]
    pub fn candidates(&self) -> &[EntityId] {
        match &self.relation.resolution {
            ResolutionState::Ambiguous { candidates } => candidates,
            _ => &[],
        }
    }

    /// The rendered form. `unit` decides the arrow, so one edge reads correctly from either end.
    #[must_use]
    pub fn render(&self, unit: &EntityId, list_candidates: bool) -> String {
        render_edge(&self.relation, unit, list_candidates)
    }
}

/// One declaration in a pack, and the edges that justify it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextUnit {
    /// The declaration, whole.
    pub entity: Entity,
    /// Why it is here.
    pub reason: InclusionReason,
    /// What the declaration costs, not counting its edges.
    pub cost: Cost,
    /// The edges at this unit, each carrying its state. Empty for every unit except the target's:
    ///   a neighbour's place in the pack is already justified by the edge recorded on its
    ///   [`InclusionReason`], and printing that edge again inside the neighbour's own block would
    ///   spend the budget twice on one fact.
    pub edges: Vec<ContextEdge>,
    /// What those edges cost together.
    pub edges_cost: Cost,
}

impl ContextUnit {
    /// The rendered declaration, without its edges. This is the exact string that was priced.
    #[must_use]
    pub fn render(&self) -> String {
        render_unit(&self.entity, &self.reason)
    }

    /// The rendered edges, in the order they will be printed.
    #[must_use]
    pub fn render_edges(&self, list_candidates: bool) -> Vec<String> {
        self.edges
            .iter()
            .map(|edge| edge.render(&self.entity.id, list_candidates))
            .collect()
    }
}

/// A compiled slice of a repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextPack {
    /// What was asked for, and what it resolved to.
    pub target: Target,
    /// The budget, and what happened to it.
    pub budget: BudgetReport,
    /// The header every pack opens with. Charged against the budget, because a reader who does
    /// not know what was dropped has been misled.
    pub report: String,
    /// The included declarations, in rank order.
    pub units: Vec<ContextUnit>,
    /// Everything the neighbourhood produced that is not in `units`.
    pub omitted: Vec<Omission>,
    /// Statements the pack owes the reader: a bound that was hit, an enumeration that stopped
    /// early, a target with no neighbourhood at all.
    pub notes: Vec<String>,
}

impl ContextPack {
    /// Every edge in the pack, in order.
    pub fn edges(&self) -> impl Iterator<Item = &ContextEdge> {
        self.units.iter().flat_map(|unit| unit.edges.iter())
    }

    /// The identities in the pack, in order.
    pub fn unit_ids(&self) -> Vec<&EntityId> {
        self.units.iter().map(|unit| &unit.entity.id).collect()
    }

    /// Whether the pack is the whole neighbourhood rather than a subset of it.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        matches!(self.budget.status, BudgetStatus::Complete)
    }

    /// The cost of every edge in the pack, added up independently of the budget report. A test
    /// compares this with [`BudgetReport::spent_tokens`] so the reported figure is checkable
    /// rather than trusted.
    #[must_use]
    pub fn edge_cost(&self) -> Cost {
        self.units
            .iter()
            .fold(Cost::ZERO, |total, unit| total.sum(unit.edges_cost))
    }

    /// The cost of every declaration in the pack, added up independently of the budget report.
    #[must_use]
    pub fn unit_cost(&self) -> Cost {
        self.units
            .iter()
            .fold(Cost::ZERO, |total, unit| total.sum(unit.cost))
    }

    /// The full answer as text: the report, then each unit with its edges.
    #[must_use]
    pub fn render(&self, list_candidates: bool) -> String {
        let mut text = self.report.clone();
        for unit in &self.units {
            text.push('\n');
            text.push_str(&unit.render());
            for edge in unit.render_edges(list_candidates) {
                text.push('\n');
                text.push_str(&edge);
            }
        }
        text
    }
}

// ---------------------------------------------------------------------------
// The compiler
// ---------------------------------------------------------------------------

/// Compile a pack. See [`crate::query::Query::peek`].
pub(crate) fn peek(
    query: &Query<'_>,
    target_query: &str,
    budget_tokens: u64,
) -> Result<ContextPack, QueryError> {
    let options = *query.options();
    let counter = options.counter;
    let reserve = report_reserve(counter);
    if budget_tokens < reserve {
        return Err(QueryError::BudgetTooSmall {
            requested: budget_tokens,
            minimum: reserve,
        });
    }
    let available = budget_tokens - reserve;

    let target = resolve_target(query, target_query)?;
    let (mut candidates, mut notes, unexamined) = neighbourhood(query, &target)?;

    let mut units: Vec<ContextUnit> = Vec::new();
    let mut omitted: Vec<Omission> = Vec::new();
    let mut spent: u64 = 0;
    let mut considered: u64 = 0;
    let mut target_included = false;
    let mut stopped = false;

    // Indexed rather than a `for` loop, because the `!fits` arm has to reach the ranking after the
    // current candidate to account for it. The declaration is taken out of its slot with
    // `Option::take` rather than cloned, because a pack is priced from rendered text and nothing
    // here reads the same entity twice.
    let mut rank = 0;
    while rank < candidates.len() {
        let candidate = &mut candidates[rank];
        let Some(entity) = candidate.entity.take() else {
            notes.push(missing_entity_note(&candidate.id));
            rank += 1;
            continue;
        };
        considered += 1;
        let cost = Cost::of(&render_unit(&entity, &candidate.reason), counter);

        if !fits(spent, cost.tokens, available) {
            // **The first refusal ends the fill.** The ranking is a total order and the walk
            // follows it, so admitting a cheaper unit from further down would leave a hole in the
            // middle of the pack: the caller would be given a worse-ranked declaration while
            // being told a better-ranked one was dropped for want of room, which is the opposite of
            // what the ranking means. `spent` does not grow here, so the old behaviour of setting
            // `stopped` and continuing was re-testing every later candidate against budget nothing
            // had been spent from.
            stopped = true;
            omitted.push(unit_omission(&entity, cost, available));

            // Every candidate behind the refusal is named too, each with what it would have cost.
            // A bare `break` would drop the tail silently, and silently dropping is the same class
            // of defect as admitting a later item: the caller is told less than the truth. The
            // refused candidate itself is already named above, so the walk resumes after it.
            for rest in &candidates[rank + 1..] {
                let Some(entity) = &rest.entity else {
                    notes.push(missing_entity_note(&rest.id));
                    continue;
                };
                considered += 1;
                let cost = Cost::of(&render_unit(entity, &rest.reason), counter);
                omitted.push(unit_omission(entity, cost, available));
            }
            break;
        }
        spent = spent.saturating_add(cost.tokens);
        let is_target = matches!(candidate.reason, InclusionReason::Target);
        if is_target {
            target_included = true;
        }

        let (edges, edges_cost, omitted_edges) = price_edges(
            counter,
            &options,
            &entity,
            &candidate.edges,
            is_target,
            spent,
            available,
        );
        spent = spent.saturating_add(edges_cost.tokens);
        omitted.extend(omitted_edges);

        units.push(ContextUnit {
            entity,
            reason: candidate.reason.clone(),
            cost,
            edges,
            edges_cost,
        });
        rank += 1;
    }

    let status = if !target_included {
        BudgetStatus::Insufficient
    } else if omitted.is_empty() {
        BudgetStatus::Complete
    } else {
        BudgetStatus::Reduced
    };
    if !target_included {
        notes.push(format!(
            "the target does not fit a budget of {budget_tokens} token(s) alongside the \
             {reserve}-token report; this pack is a refusal, not a slice"
        ));
    }
    if stopped {
        notes.push(
            "the fill stopped at the first declaration that did not fit rather than skipping to a \
             smaller one, so this pack is a prefix of the full ranking; every declaration behind \
             that one is named in `omitted` with what it would have cost"
                .to_owned(),
        );
    }
    if status == BudgetStatus::Complete {
        notes.push(format!(
            "the neighbourhood was exhausted with {} token(s) of budget unspent; there was \
             nothing left worth adding",
            available.saturating_sub(spent)
        ));
    }

    let report = report_text(counter, budget_tokens, spent, status, &omitted);
    let report_cost = Cost::of(&report, counter);
    let total = spent.saturating_add(report_cost.tokens);
    if total > budget_tokens {
        // Unreachable while the reserve is a proven upper bound on the report, which is what
        // `report_reserve` exists for. Reported rather than hidden, because the one thing
        // `BudgetReport::spent_tokens` promises is that it is not a lie.
        notes.push(
            "the report cost more than the budget reserved for it, so this pack exceeds its \
             budget; the reserve is documented as an upper bound, so this is a defect"
                .to_owned(),
        );
    }

    Ok(ContextPack {
        target,
        budget: BudgetReport {
            requested_tokens: budget_tokens,
            spent_tokens: total,
            remaining_tokens: budget_tokens.saturating_sub(total),
            counter,
            status,
            candidates_considered: considered,
            candidates_unexamined: unexamined,
        },
        report,
        units,
        omitted,
        notes,
    })
}

/// Whether `cost` more tokens still fit in `available`.
fn fits(spent: u64, cost: u64, available: u64) -> bool {
    spent.saturating_add(cost) <= available
}

/// The omission for a declaration the budget could not take.
///
/// The two reasons are claims about different things. [`OmissionReason::ExceedsBudget`] is a fact
/// about this declaration alone: it costs more than the whole content budget, so it would not have
/// fitted however the fill were ordered. Anything else was affordable and is not in the pack
/// because the fill had already stopped by the time the walk reached it — which is exactly what
/// makes a reduced pack a prefix rather than a subset.
fn unit_omission(entity: &Entity, cost: Cost, available: u64) -> Omission {
    Omission {
        subject: entity.id.display(),
        what: Omitted::Unit,
        reason: if cost.tokens > available {
            OmissionReason::ExceedsBudget
        } else {
            OmissionReason::BudgetExhausted
        },
        cost,
    }
}

/// The note for a candidate the index names but holds no row for.
///
/// A note rather than an omission because it was never priced: it has no cost, and an omission
/// carrying a cost of zero would claim the declaration was free.
fn missing_entity_note(id: &EntityId) -> String {
    format!(
        "`{}` is named by a relation in the index but has no entity row, so it was not offered \
         as context",
        id.display()
    )
}

/// Price a unit's edges, taking a prefix of the ranked ones and naming every one that does not
/// fit.
///
/// The same stopping rule as the declaration fill, and for the same reason. `ordered` is ranked
/// with the undecided edges first (`edge_rank`), so skipping a priced-out edge to fit a cheaper one
/// further down would spend the budget on the edges a reader most needs to discount last — the
/// exact inversion of why they are ranked first. The tail is named rather than dropped, so what the
/// answer left out is visible in full.
fn price_edges(
    counter: TokenCounter,
    options: &crate::query::QueryOptions,
    entity: &Entity,
    edges: &[Relation],
    is_target: bool,
    spent: u64,
    available: u64,
) -> (Vec<ContextEdge>, Cost, Vec<Omission>) {
    // Only the target's edges are printed. See `ContextUnit::edges`.
    if !is_target {
        return (Vec::new(), Cost::ZERO, Vec::new());
    }
    let mut ordered: Vec<&Relation> = edges.iter().collect();
    ordered.sort_by_key(|edge| edge_rank(edge));
    ordered.dedup_by(|a, b| a.natural_key() == b.natural_key());

    let mut packed: Vec<ContextEdge> = Vec::new();
    let mut cost = Cost::ZERO;
    let mut omitted: Vec<Omission> = Vec::new();
    let mut running = spent;

    for (rank, edge) in ordered.iter().enumerate() {
        let price = edge_cost(counter, options, edge, &entity.id);
        if !fits(running, price.tokens, available) {
            // The first refusal ends the fill here too, and everything behind it is accounted for.
            for rest in &ordered[rank..] {
                omitted.push(edge_omission(rest, edge_cost(counter, options, rest, &entity.id)));
            }
            break;
        }
        running = running.saturating_add(price.tokens);
        cost = cost.sum(price);
        packed.push(ContextEdge {
            relation: (*edge).clone(),
            cost: price,
        });
    }
    (packed, cost, omitted)
}

/// What one edge costs in its unit's block, as the exact string that is printed.
fn edge_cost(
    counter: TokenCounter,
    options: &crate::query::QueryOptions,
    edge: &Relation,
    unit: &EntityId,
) -> Cost {
    Cost::of(
        &render_edge(edge, unit, options.list_ambiguity_candidates),
        counter,
    )
}

/// The omission for an edge that did not fit beside the rest of its unit's block.
///
/// Always [`OmissionReason::BudgetExhausted`], including for an edge that costs more than the
/// whole content budget. The engine does not draw that distinction for edges today — it never has,
/// which is why `OmissionReason::ExceedsBudget`'s edge clause has never been printed. Introducing
/// it here would change every report carrying an edge omission, which is a separate decision.
fn edge_omission(edge: &Relation, cost: Cost) -> Omission {
    Omission {
        subject: edge_label(edge),
        what: Omitted::Edge,
        reason: OmissionReason::BudgetExhausted,
        cost,
    }
}

/// How an edge is named in an omission, falling back to the name as written when it has no
/// target.
fn edge_label(edge: &Relation) -> String {
    match edge.target.as_ref() {
        Some(target) => target.display(),
        None => edge.target_name.clone(),
    }
}

/// The order edges are priced in: the undecided ones first.
///
/// This is the part that is easy to get wrong. A pack that truncates its own uncertainties in
/// favour of its certainties is precisely the failure D-0009 describes: a consumer handed a
/// confident answer and no indication that the engine had also been unsure. Ranking uncertainty
/// first means the budget spends itself on the edges a reader most needs to discount.
fn edge_rank(edge: &Relation) -> (u8, RelationKey) {
    let uncertainty = match &edge.resolution {
        ResolutionState::Ambiguous { .. } => 0,
        ResolutionState::Unresolved { .. } => 1,
        ResolutionState::Pending { .. } => 2,
        ResolutionState::Inferred { .. } => 3,
        ResolutionState::Resolved { .. } => 4,
    };
    (uncertainty, edge.natural_key())
}

/// The rendered form of a declaration. This is the exact string that is priced.
fn render_unit(entity: &Entity, reason: &InclusionReason) -> String {
    let mut text = match entity.span {
        Some(span) => format!(
            "{} {} — {}:{}",
            entity.kind(),
            entity.id.qualified_name(),
            entity.path(),
            span.start_line
        ),
        None => format!(
            "{} {} — {}",
            entity.kind(),
            entity.id.qualified_name(),
            entity.path()
        ),
    };
    if let Some(signature) = &entity.signature {
        text.push_str(&format!("\n  signature: {signature}"));
    }
    if let Some(doc) = &entity.doc {
        // The whole doc comment, never a prefix of one. A unit is included whole or not at all;
        // half a doc comment is a claim about the code that the code does not make.
        text.push_str(&format!("\n  doc: {doc}"));
    }
    text.push_str(&format!("\n  included because: {}", reason.describe()));
    text
}

/// The rendered form of an edge. The exact string that is priced.
fn render_edge(edge: &Relation, unit: &EntityId, list_candidates: bool) -> String {
    let arrow = if &edge.source == unit { "->" } else { "<-" };
    let place = match edge.target.as_ref() {
        Some(target) => target.display(),
        None => edge.target_name.clone(),
    };
    let mut text = format!(
        "  {arrow} {} {}  [{}]  {}:{}",
        edge.kind,
        place,
        edge.resolution.describe(),
        edge.source.path(),
        edge.span.start_line
    );
    if let ResolutionState::Inferred { basis, .. } | ResolutionState::Pending { basis, .. } =
        &edge.resolution
    {
        text.push_str(&format!("\n      because: {basis}"));
    }
    if edge.resolution.is_ambiguous() {
        let names: Vec<String> = match &edge.resolution {
            ResolutionState::Ambiguous { candidates } => {
                candidates.iter().map(EntityId::display).collect()
            }
            _ => Vec::new(),
        };
        let detail = if names.is_empty() {
            "no candidate was recorded".to_owned()
        } else if list_candidates {
            names.join(", ")
        } else {
            format!("{} candidate(s), not listed", names.len())
        };
        text.push_str(&format!("\n      between: {detail}"));
    }
    text
}

// ---------------------------------------------------------------------------
// Target resolution
// ---------------------------------------------------------------------------

/// Resolve a caller's string to exactly one entity, or say why it could not be done.
///
/// Three lookups, in a fixed order, each recorded in [`Matched`]:
///
/// 1. **A repository path.** `RepoPath::new` normalises and validates, and `entities_in_file`
///    decides whether a file is actually indexed. Both are the store's own view; nothing is
///    assumed about how a file entity is named.
/// 2. **A qualified name**, through `entity_by_qualified_name`. Exactly one match, or an
///    [`QueryError::AmbiguousTarget`] carrying all of them.
/// 3. **A bare name**, through `entity_by_name` — but with `File` entities held back unless there
///    is nothing else. Audit B21 is exactly what happens without that filter: a file whose name
///    matches a symbol wins the lookup and produces a well-formed empty answer instead of a
///    reported ambiguity. Holding files back is a preference for declarations over containers, and
///    it is stated here because it is a choice.
///
/// A string that matches nothing is [`QueryError::UnknownTarget`], and the message says which of
/// the three it was not.
///
/// Private, and deliberately so. A caller that has only a string compiles the smallest pack the
/// engine accepts and takes the identity off it, which runs these same three lookups in this same
/// order; `peek-core` does not need a second entry point that returns the identity alone, and a
/// public one would be a second way in for a rule that already has a way.
fn resolve_target(query: &Query<'_>, target: &str) -> Result<Target, QueryError> {
    let store = query.store();
    let limit = query.options().entities_per_file;

    if let Some(path) = RepoPath::new(target) {
        let declared = store.entities_in_file(&path, limit)?;
        if let Some(file) = declared
            .iter()
            .find(|entity| entity.kind() == EntityKind::File)
        {
            return Ok(Target {
                query: target.to_owned(),
                matched: Matched::Path,
                entity: file.clone(),
            });
        }
    }

    let by_qualified = store.entities_with_qualified_name(target, limit)?;
    match by_qualified.as_slice() {
        [only] => {
            return Ok(Target {
                query: target.to_owned(),
                matched: Matched::QualifiedName,
                entity: only.clone(),
            });
        }
        [_, _, ..] => {
            return Err(QueryError::AmbiguousTarget {
                query: target.to_owned(),
                matched: Matched::QualifiedName,
                candidates: by_qualified.iter().map(|e| e.id.clone()).collect(),
            });
        }
        [] => {}
    }

    let by_name = store.entities_named(target, limit)?;
    let declarations: Vec<&Entity> = by_name
        .iter()
        .filter(|entity| entity.kind() != EntityKind::File)
        .collect();
    let chosen: Vec<&Entity> = if declarations.is_empty() {
        by_name.iter().collect()
    } else {
        declarations
    };
    match chosen.as_slice() {
        [only] => {
            // `Entity::clone` spelled out rather than `.clone()`: the binding is a `&&Entity`, and
            // the explicit form leaves nothing to infer.
            let entity = Entity::clone(*only);
            return Ok(Target {
                query: target.to_owned(),
                matched: Matched::Name,
                entity,
            });
        }
        [_, _, ..] => {
            return Err(QueryError::AmbiguousTarget {
                query: target.to_owned(),
                matched: Matched::Name,
                candidates: chosen.iter().map(|e| e.id.clone()).collect(),
            });
        }
        [] => {}
    }

    Err(QueryError::UnknownTarget {
        query: target.to_owned(),
        detail: match RepoPath::new(target) {
            Some(path) => {
                format!("it reads as the repository path `{path}`, which no indexed file declares")
            }
            None => "it is not a repository path, and no entity owns that qualified name or \
                    declares that bare name"
                .to_owned(),
        },
    })
}

// ---------------------------------------------------------------------------
// The neighbourhood
// ---------------------------------------------------------------------------

/// One thing the compiler might include.
#[derive(Debug, Clone)]
struct Candidate {
    id: EntityId,
    /// `None` when a relation named it but the index holds no row for it, which is a real state
    /// rather than one to paper over.
    entity: Option<Entity>,
    reason: InclusionReason,
    /// The edges that reach this candidate. Only the target's are printed; see
    /// [`ContextUnit::edges`].
    edges: Vec<Relation>,
}

/// The sort key. A total order, so the ranking never has to break a tie.
///
/// The last four components are the entity's identity, which is unique; the first two are the
/// [`InclusionReason`]'s section and distance. Together they make the key unique across distinct
/// candidates, which is what makes "two runs over an unchanged index produce identical output" a
/// structural fact rather than a hope.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct RankKey {
    section: u8,
    distance: u32,
    path: String,
    kind: EntityKind,
    qualified_name: String,
    ordinal: u32,
}

impl RankKey {
    fn of(id: &EntityId, reason: &InclusionReason) -> Self {
        Self {
            section: reason.section(),
            distance: reason.distance(),
            path: id.path().as_str().to_owned(),
            kind: id.kind(),
            qualified_name: id.qualified_name().to_owned(),
            ordinal: id.ordinal(),
        }
    }
}

/// Everything one hop from the target, ranked.
///
/// The neighbourhood is **depth 1 and a fixed shape**, so its enumeration terminates and its cost
/// is a function of the target's own degree rather than of the repository's size:
///
/// 1. the target;
/// 2. for a structural target, everything it contains — a file's declarations, a module's, a
///    package's — found through [`membership`];
/// 3. for a symbol, the declaration that lexically contains it, found from an inbound structural
///    edge. Absent for a top-level declaration, which the index has no edge for either;
/// 4. every entity the target reaches through one followable dependency edge, in either direction,
///    split into `Callee` for an outbound `calls`, `Caller` for an inbound `calls`, and
///    `Referenced` for any other followable outbound edge;
/// 5. nothing else.
///
/// The target's own file is deliberately *not* a unit. It would be the eighth thing on a unit line
/// that already says `path:line`, so it buys nothing and costs a unit's worth of budget; a `File`
/// entity carries a name and a path and no source.
///
/// Depth 1 is a deliberate ceiling rather than an unfinished implementation. A pack is a *slice*:
/// the caller asked for a budget, not for a closure of the graph, and a depth-2 walk of a
/// repository's call graph is a thing the caller cannot express a budget for.
fn neighbourhood(
    query: &Query<'_>,
    target: &Target,
) -> Result<(Vec<Candidate>, Vec<String>, u64), QueryError> {
    let store = query.store();
    let options = *query.options();
    let mut found: BTreeMap<EntityId, Candidate> = BTreeMap::new();
    let mut notes: Vec<String> = Vec::new();
    let mut unexamined: u64 = 0;
    let id = target.id().clone();

    offer(
        &mut found,
        id.clone(),
        Some(target.entity.clone()),
        InclusionReason::Target,
        None,
    );

    let mut incident: Vec<Relation> = Vec::new();
    match store.incoming(&id, None, options.relations_per_entity) {
        Ok(edges) => {
            if edges.len() >= options.relations_per_entity {
                unexamined += 1;
                notes.push(format!(
                    "only the first {} relations of the target were read; at least one candidate \
                     it could have led to was never seen",
                    options.relations_per_entity
                ));
            }
            incident.extend(edges);
        }
        Err(error) => {
            notes.push(format!(
                "the relations of {} could not be read: {error}",
                id.display()
            ));
        }
    }
    match store.outgoing(&id, None, options.relations_per_entity) {
        Ok(edges) => {
            if edges.len() >= options.relations_per_entity {
                unexamined += 1;
                notes.push(format!(
                    "only the first {} outgoing relations of the target were read; at least one \
                     candidate it could have led to was never seen",
                    options.relations_per_entity
                ));
            }
            incident.extend(edges);
        }
        Err(error) => {
            notes.push(format!(
                "the outgoing relations of {} could not be read: {error}",
                id.display()
            ));
        }
    }

    let is_container = structural_target(target.kind());
    if is_container {
        // What a file, module or package *holds*. See `traverse::membership` for why half of
        // these edges are derived rather than recorded.
        for edge in membership(store, &options, &id)? {
            let Some(other) = edge.target.as_ref() else {
                continue;
            };
            if other == &id {
                continue;
            }
            offer(
                &mut found,
                other.clone(),
                store.entity(other)?,
                InclusionReason::Member { of: id.clone() },
                Some(edge),
            );
        }
    } else {
        // The declaration that encloses a symbol. `incident` holds every edge at the target, so
        // the edges that *contain* it are the ones whose source is somebody else.
        for edge in &incident {
            if !edge.kind.is_structural() || !edge.is_followable() || edge.source == id {
                continue;
            }
            offer(
                &mut found,
                edge.source.clone(),
                store.entity(&edge.source)?,
                InclusionReason::Container {
                    of: edge.source.clone(),
                },
                Some(edge.clone()),
            );
        }
    }

    for edge in &incident {
        if !followable(edge, &options) {
            continue;
        }
        let (other, reason) = match edge.target.as_ref() {
            Some(reached) if edge.source == id => (
                reached.clone(),
                match edge.kind {
                    RelationKind::Calls => InclusionReason::Callee {
                        of: id.clone(),
                        distance: 1,
                    },
                    kind => InclusionReason::Referenced {
                        of: id.clone(),
                        kind,
                        distance: 1,
                    },
                },
            ),
            _ => (
                edge.source.clone(),
                InclusionReason::Caller {
                    of: id.clone(),
                    distance: 1,
                },
            ),
        };
        if other == id {
            continue;
        }
        // Borrowed for the lookup, then moved into the offer. The order matters: the row is read
        // before the id is handed over, because a `store.entity` call needs the id it is given and
        // `other` is the one being consumed.
        let row = store.entity(&other)?;
        offer(&mut found, other, row, reason, Some(edge.clone()));
    }

    // The target's block shows *its* edges — every edge at it, in both directions — rather than
    // the edge that brought it here, so a reader sees the undecided ones as well as the decided
    // ones. A neighbour's block shows nothing: its place is already justified by the edge recorded
    // on its `InclusionReason`, and repeating that edge would price one fact twice. See
    // `ContextUnit::edges`.
    if let Some(entry) = found.get_mut(&id) {
        entry.edges = incident;
    }

    let mut ranked: Vec<(RankKey, EntityId)> = found
        .iter()
        .map(|(id, candidate)| (RankKey::of(id, &candidate.reason), id.clone()))
        .collect();
    ranked.sort();

    // The cap is applied *after* ranking, so what it drops is the lowest-ranked part of the
    // neighbourhood rather than an arbitrary prefix of its discovery order. The count is reported
    // because a cap that says nothing is a silent truncation wearing a parameter.
    if ranked.len() > options.max_units {
        let total = ranked.len();
        ranked.truncate(options.max_units);
        unexamined = unexamined.saturating_add((total - options.max_units) as u64);
        notes.push(format!(
            "the neighbourhood holds {total} candidate(s) and at most {} were ranked; the rest \
             were not, and they are the lowest-ranked",
            options.max_units
        ));
    }

    let mut ordered: Vec<Candidate> = Vec::with_capacity(ranked.len());
    for (_, id) in ranked {
        if let Some(candidate) = found.get(&id) {
            ordered.push(candidate.clone());
        }
    }
    Ok((ordered, notes, unexamined))
}

/// Insert a candidate, keeping the strongest reason and every distinct edge.
fn offer(
    found: &mut BTreeMap<EntityId, Candidate>,
    id: EntityId,
    entity: Option<Entity>,
    reason: InclusionReason,
    edge: Option<Relation>,
) {
    let entry = found.entry(id.clone()).or_insert_with(|| Candidate {
        id: id.clone(),
        entity: None,
        reason: reason.clone(),
        edges: Vec::new(),
    });
    if entry.entity.is_none() {
        entry.entity = entity;
    }
    if reason.section() < entry.reason.section() {
        entry.reason = reason;
    }
    if let Some(edge) = edge {
        if !entry
            .edges
            .iter()
            .any(|held| held.natural_key() == edge.natural_key())
        {
            entry.edges.push(edge);
        }
    }
}

/// Whether an edge may bring a neighbour into the neighbourhood.
///
/// The same rule a walk uses — a proven target, a dependency kind, and the `follow_inferred`
/// policy — so a pack and a walk can never disagree about what the engine believes. An ambiguous
/// or unresolved edge brings no unit into the pack, but it *is* printed: the fact that the engine
/// tried and could not decide is more useful to a reader than its absence.
fn followable(edge: &Relation, options: &crate::query::QueryOptions) -> bool {
    if !edge.kind.is_dependency() || !edge.is_followable() {
        return false;
    }
    options.follow_inferred || !matches!(edge.resolution, ResolutionState::Inferred { .. })
}

/// Whether a target of this kind contains other entities, so its neighbourhood is its content.
///
/// A `File` is included because a file is exactly that. `Module`, `Package`, `Workspace` and
/// `Repository` are the same question asked at a coarser level, defined today so the answer is
/// not invented when the extractor starts emitting them (R-007).
#[must_use]
pub fn structural_target(kind: EntityKind) -> bool {
    matches!(
        kind,
        EntityKind::File
            | EntityKind::Module
            | EntityKind::Package
            | EntityKind::Workspace
            | EntityKind::Repository
    )
}

// ---------------------------------------------------------------------------
// The budget report
// ---------------------------------------------------------------------------

/// The largest the report can be, and the space reserved for it before the fill begins.
///
/// **Why a reserve at all.** The report is part of the answer — a reader who does not know what
/// was dropped has been misled — so it must be inside the budget. But the report's *content*
/// depends on what the fill did, and the fill's budget depends on the reserve. Reserving the
/// largest the report could be breaks the circle: content is priced against `budget - reserve`,
/// and the report is then written and charged against what is left.
///
/// **Why this is an upper bound.** The only variable-width things in the report are numbers, and
/// [`u64::MAX`] is the widest a number in it can be. Every status and every omission phrase is a
/// fixed string, and the skeleton below lists each reason under both headings even though at most
/// one of the two can carry it. So the real report is never wider than this one. If that ever
/// stops being true, the compiler says so in a note rather than letting a pack quietly exceed its
/// budget.
pub(crate) fn report_reserve(counter: TokenCounter) -> u64 {
    counter.count(&report_skeleton(counter))
}

/// The report with every number at its widest and every reason listed under both headings.
fn report_skeleton(counter: TokenCounter) -> String {
    let widest = u64::MAX;
    let mut text = format!(
        "peek context pack\n\
         budget: {widest} tokens requested, {widest} spent, {widest} remaining\n\
         counted as: {}\n\
         status: insufficient\n",
        counter.rule()
    );
    for heading in ["unit", "edge"] {
        for reason in [
            OmissionReason::BudgetExhausted,
            OmissionReason::ExceedsBudget,
            OmissionReason::EdgeLimit,
        ] {
            let phrase = match heading {
                "unit" => reason.unit_phrase(),
                _ => reason.edge_phrase(),
            };
            text.push_str(&format!("  {widest} {heading}(s) {phrase}\n"));
        }
    }
    text
}

/// The header of a pack: what was asked for, what it cost, and what was left behind.
fn report_text(
    counter: TokenCounter,
    requested: u64,
    spent: u64,
    status: BudgetStatus,
    omitted: &[Omission],
) -> String {
    let mut units: BTreeMap<OmissionReason, u64> = BTreeMap::new();
    let mut edges: BTreeMap<OmissionReason, u64> = BTreeMap::new();
    for entry in omitted {
        let bucket = match entry.what {
            Omitted::Unit => &mut units,
            Omitted::Edge => &mut edges,
        };
        *bucket.entry(entry.reason).or_default() += 1;
    }

    let mut text = format!(
        "peek context pack\n\
         budget: {requested} tokens requested, {spent} spent, {} remaining\n\
         counted as: {}\n\
         status: {status}\n",
        requested.saturating_sub(spent),
        counter.rule()
    );
    if units.is_empty() && edges.is_empty() {
        text.push_str("dropped: nothing\n");
        return text;
    }
    text.push_str("dropped:\n");
    for (reason, count) in &units {
        text.push_str(&format!("  {count} unit(s) {}\n", reason.unit_phrase()));
    }
    for (reason, count) in &edges {
        text.push_str(&format!("  {count} edge(s) {}\n", reason.edge_phrase()));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::{
        BudgetStatus, Cost, InclusionReason, Matched, Omission, OmissionReason, Omitted, RankKey,
        report_reserve, report_text,
    };
    use crate::model::entity::{EntityId, EntityKind};
    use crate::model::path::RepoPath;
    use crate::model::relation::RelationKind;
    use crate::query::tokens::TokenCounter;

    fn id(qname: &str) -> EntityId {
        EntityId::new(
            RepoPath::new("src/a.rs").expect("path"),
            EntityKind::Function,
            qname,
            0,
        )
    }

    fn omission(what: Omitted, reason: OmissionReason) -> Omission {
        Omission {
            subject: "x".to_owned(),
            what,
            reason,
            cost: Cost::ZERO,
        }
    }

    #[test]
    fn the_reserve_is_an_upper_bound_on_the_report() {
        // The whole reserve argument rests on this: whatever the report says, it must cost no more
        // than the skeleton it was measured against, at the widest numbers it could contain and
        // with every reason present under both headings.
        let counter = TokenCounter::default();
        let reserve = report_reserve(counter);
        let widest = report_text(
            counter,
            u64::MAX,
            u64::MAX,
            BudgetStatus::Insufficient,
            &[
                omission(Omitted::Unit, OmissionReason::BudgetExhausted),
                omission(Omitted::Unit, OmissionReason::ExceedsBudget),
                omission(Omitted::Unit, OmissionReason::EdgeLimit),
                omission(Omitted::Edge, OmissionReason::BudgetExhausted),
                omission(Omitted::Edge, OmissionReason::ExceedsBudget),
                omission(Omitted::Edge, OmissionReason::EdgeLimit),
            ],
        );
        let cost = counter.count(&widest);
        assert!(
            cost <= reserve,
            "a report listing every reason under both headings cost {cost} but the reserve is \
             {reserve}"
        );
    }

    #[test]
    fn the_rank_key_is_total_so_the_ranking_never_breaks_a_tie() {
        // Two candidates with the same identity are the same candidate; two with different
        // identities must order somehow, and the identity components guarantee they do.
        let target = RankKey::of(&id("a"), &InclusionReason::Target);
        let later = RankKey::of(
            &id("b"),
            &InclusionReason::Callee {
                of: id("a"),
                distance: 1,
            },
        );
        assert!(target < later, "the target outranks everything");
        assert_ne!(target, later);
    }

    #[test]
    fn every_inclusion_reason_states_itself_in_one_sentence() {
        // A unit with no readable reason is a unit `explain` would contradict.
        let reasons = [
            InclusionReason::Target,
            InclusionReason::Member { of: id("f") },
            InclusionReason::Container { of: id("Service") },
            InclusionReason::Callee {
                of: id("a"),
                distance: 1,
            },
            InclusionReason::Caller {
                of: id("a"),
                distance: 1,
            },
            InclusionReason::Referenced {
                of: id("a"),
                kind: RelationKind::UsesType,
                distance: 1,
            },
        ];
        for reason in reasons {
            let described = reason.describe();
            assert!(!described.is_empty(), "{reason:?} describes nothing");
            assert!(
                described.ends_with('.'),
                "{reason:?} reads as a fragment: {described}"
            );
        }
    }

    #[test]
    fn a_member_is_one_hop_away_and_the_target_is_not() {
        // The distance component of the rank key depends on this: a member of a file is content
        // of the target and sits at distance 1, while the target itself is distance 0.
        assert_eq!(InclusionReason::Target.distance(), 0);
        assert_eq!(InclusionReason::Member { of: id("f") }.distance(), 1);
        assert_eq!(
            InclusionReason::Callee {
                of: id("a"),
                distance: 1
            }
            .distance(),
            1
        );
    }

    #[test]
    fn the_target_outranks_every_other_section() {
        for reason in [
            InclusionReason::Member { of: id("f") },
            InclusionReason::Container { of: id("s") },
            InclusionReason::Callee {
                of: id("a"),
                distance: 1,
            },
            InclusionReason::Caller {
                of: id("a"),
                distance: 1,
            },
            InclusionReason::Referenced {
                of: id("a"),
                kind: RelationKind::Imports,
                distance: 1,
            },
        ] {
            assert!(
                reason.section() > InclusionReason::Target.section(),
                "{reason:?} may not outrank the thing that was asked for"
            );
        }
    }

    #[test]
    fn an_omission_reason_travels_as_a_bare_string() {
        // The counterpart to `an_unresolved_reason_travels_as_a_bare_string` on the model side: the
        // reader's model says an omission is `{"reason":"budget_exhausted"}`, and the old encoding
        // made it `{"reason":{"reason":"budget_exhausted"}}` because this enum carried
        // `#[serde(tag = "reason")]` inside a struct whose own field is called `reason`.
        let omission = Omission {
            subject: "src/a.rs::x".to_owned(),
            what: Omitted::Unit,
            reason: OmissionReason::BudgetExhausted,
            cost: Cost::ZERO,
        };
        let json = serde_json::to_string(&omission).expect("serialise");
        assert!(
            json.contains(r#""reason":"budget_exhausted""#),
            "the reason is the bare string the reader's model names: {json}"
        );
        assert!(
            !json.contains(r#""reason":{"#),
            "and nothing wraps it in a second object: {json}"
        );
    }

    #[test]
    fn an_omission_reason_from_before_the_flat_form_is_still_read() {
        // A pack is not persisted by the store, but it does cross a process boundary, so an answer
        // a client holds from an older build arrives wrapped. Reading one is ten lines; making a
        // client discard it to read a value that never changed meaning is not a trade anyone
        // should have to make. This is the deserialiser `Omission::reason` is built from, so a
        // whole record written by an older build decodes through it too.
        for (payload, expected) in [
            (
                r#"{"reason":{"reason":"budget_exhausted"}}"#,
                OmissionReason::BudgetExhausted,
            ),
            (
                r#"{"reason":{"reason":"edge_limit"}}"#,
                OmissionReason::EdgeLimit,
            ),
            (
                r#"{"reason":{"reason":"exceeds_budget"}}"#,
                OmissionReason::ExceedsBudget,
            ),
        ] {
            let read: OmissionReason = serde_json::from_str(payload)
                .unwrap_or_else(|error| panic!("{payload} must still decode: {error}"));
            assert_eq!(read, expected, "{payload} decoded to the wrong reason");
        }
    }

    #[test]
    fn an_inclusion_reason_keeps_its_tag_because_its_variants_carry_payloads() {
        // The one enum in this file that must *not* be flattened. `Callee { of, distance }` has no
        // meaning without its fields, so the tag is not a second copy of the word "reason" — it is
        // what says there are fields. Flattening this one because its neighbours were flattened
        // would throw the payload away.
        let reason = InclusionReason::Callee {
            of: id("a"),
            distance: 1,
        };
        let json = serde_json::to_string(&reason).expect("serialise");
        assert!(
            json.starts_with(r#"{"reason":"callee","#),
            "an inclusion reason is tagged and carries its fields: {json}"
        );
        let back: InclusionReason = serde_json::from_str(&json).expect("deserialise");
        assert_eq!(back, reason);
    }

    #[test]
    fn an_omission_reason_spelling_this_build_does_not_have_is_refused() {
        // The same refusal as on the model side: a spelling from a newer build is not read as the
        // nearest known one, because that would be a claim about why something was dropped.
        let outcome: Result<OmissionReason, _> = serde_json::from_str(r#""ran_out""#);
        assert!(outcome.is_err(), "an unknown omission reason must not decode");
        let wrapped: Result<OmissionReason, _> = serde_json::from_str(r#"{"reason":"ran_out"}"#);
        assert!(
            wrapped.is_err(),
            "and it must not decode in the wrapped shape either"
        );
    }

    #[test]
    fn a_file_is_a_container_and_a_function_is_not() {
        // The distinction decides whether a walk or a pack expands a target to its members, so it
        // is asserted where it is decided rather than only in prose.
        assert!(super::structural_target(EntityKind::File));
        assert!(super::structural_target(EntityKind::Module));
        assert!(super::structural_target(EntityKind::Package));
        assert!(!super::structural_target(EntityKind::Function));
        assert!(!super::structural_target(EntityKind::Method));
    }

    #[test]
    fn a_drop_nothing_report_is_shorter_than_the_reserve() {
        let counter = TokenCounter::default();
        let text = report_text(counter, 8000, 400, BudgetStatus::Complete, &[]);
        assert!(
            text.contains("dropped: nothing"),
            "a complete pack must say so: {text}"
        );
        assert!(
            counter.count(&text) <= report_reserve(counter),
            "the empty-drop report is still within the reserve"
        );
    }

    #[test]
    fn the_status_and_match_labels_are_stable() {
        assert_eq!(Matched::Path.as_str(), "path");
        assert_eq!(Matched::QualifiedName.as_str(), "qualified_name");
        assert_eq!(Matched::Name.as_str(), "name");
        assert_eq!(BudgetStatus::Complete.to_string(), "complete");
        assert_eq!(BudgetStatus::Reduced.to_string(), "reduced");
        assert_eq!(BudgetStatus::Insufficient.to_string(), "insufficient");
        assert_eq!(OmissionReason::BudgetExhausted.as_str(), "budget_exhausted");
        assert_eq!(OmissionReason::ExceedsBudget.as_str(), "exceeds_budget");
        assert_eq!(OmissionReason::EdgeLimit.as_str(), "edge_limit");
        assert_eq!(Omitted::Unit.as_str(), "unit");
        assert_eq!(Omitted::Edge.as_str(), "edge");
    }
}
