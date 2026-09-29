//! `context`: the token-budgeted compiler. The tool an agent will actually call.
//!
//! # What this tool does, and what it refuses to do
//!
//! It hands the target and the budget to [`Query::peek`] and returns the pack. All the arithmetic
//! — reserving the report before pricing a single unit, refusing a budget that cannot hold that
//! report, naming every candidate it dropped and what it would have cost, ranking uncertain edges
//! ahead of certain ones so a truncated pack loses certainties before doubts — is the engine's,
//! tested there. Re-implementing any of it here would be the one thing that could make the MCP
//! answer differ from the CLI answer, which contract D2 forbids.
//!
//! So the work of this module is three things the engine deliberately does not do:
//!
//! 1. **Require the budget.** There is no default. A default is a number nobody chose, and a pack
//!    silently compiled to 4000 tokens because the tool picked that is a pack whose cost the
//!    caller never budgeted for. A missing `budget_tokens` is refused, and the refusal carries the
//!    minimum from the engine.
//! 2. **Map the three budget states onto three outcomes.** `Complete` is `ok`, `Reduced` is
//!    `reduced`, `Insufficient` is `insufficient`. They are three different facts about the
//!    answer and a caller that cannot tell them apart cannot reason about whether it has the whole
//!    neighbourhood.
//! 3. **Refuse a budget the engine will not take, with the number.** [`Query::peek`] returns
//!    `BudgetTooSmall` rather than exceeding; this turns that into a refusal carrying
//!    `minimum_tokens`, because a refusal that says "raise the budget" without saying to what is a
//!    refusal the caller has to guess the rest of.
//!
//! # What the response contains
//!
//! The engine's [`ContextPack`], whole: the target and how it was matched, the budget report, the
//! rendered header, every unit with its inclusion reason and its edges, every omission with its
//! reason and what it would have cost, and every note the pack owes the reader. Nothing is
//! re-derived here, so the cost a caller adds up from the parts is the cost the engine reported.

use serde::Serialize;
use serde_json::Value;

use peek_core::query::{BudgetStatus, ContextPack, Query, QueryError, QueryOptions};

use crate::outcome::{Outcome, ToolError, Verdict};
use crate::params::Args;
use crate::session::Session;
use crate::tools::ToolAnswer;

/// Compile a slice of the repository around `target` inside `budget_tokens`.
pub fn context(session: &mut Session, arguments: Option<&Value>) -> Result<ToolAnswer, ToolError> {
    let args = Args::new("context", arguments, crate::tool::hints_for("context"))?;
    let target = args.required_string("target", "a path, a qualified name, or a bare name")?;
    let follow_inferred = args.optional_bool("follow_inferred")?;
    let list_candidates = args.optional_bool("list_ambiguity_candidates")?;

    // The budget is read before it is required, so a refusal can state the minimum. Reading it
    // first is not a validation shortcut: the two are separate decisions and the second needs the
    // first to have failed for a reason worth reporting.
    let budget = args.optional_u64("budget_tokens")?;
    args.finish(&[
        "target",
        "budget_tokens",
        "follow_inferred",
        "list_ambiguity_candidates",
    ])?;

    let mut options = QueryOptions::default();
    if let Some(follow) = follow_inferred {
        options.follow_inferred = follow;
    }
    if let Some(list) = list_candidates {
        options.list_ambiguity_candidates = list;
    }

    // The floor comes from the engine, and it is available without a target — which is the point:
    // a caller can find out what the smallest acceptable budget is before composing one. On a
    // repository with no index it is `None`, and the refusal then says so rather than naming zero.
    let minimum = session.minimum_budget();
    let Some(budget) = budget else {
        let sentence = match minimum {
            Some(floor) => format!(
                "`context` needs a `budget_tokens` argument, and this call did not send one \
                 (the smallest this build accepts is {floor})"
            ),
            None => "`context` needs a `budget_tokens` argument, and this call did not send one \
                     (this repository has no index, so the smallest budget cannot be read yet)"
                .to_owned(),
        };
        let advice = match minimum {
            Some(floor) => format!(
                "a context pack is compiled to a budget rather than truncated to fit one, so the \
                 number has to come from you; {floor} is the floor and anything at or above it is \
                 honoured"
            ),
            None => "run `index` first, then ask again: the compiler reports the smallest budget \
                     it will accept as part of its `initialize` instructions"
                .to_owned(),
        };
        return Err(ToolError::budget(sentence, advice, minimum.unwrap_or(0)));
    };

    let pack = {
        let store = session.reader()?;
        let query = Query::with_options(store, options);
        match query.peek(&target, budget) {
            Ok(pack) => pack,
            Err(error) => {
                let mut mapped = ToolError::from_verdict(&Verdict::from_query_error(&error));
                if let QueryError::BudgetTooSmall { minimum, .. } = error {
                    mapped.minimum_tokens = Some(minimum);
                }
                if !mapped.candidates.is_empty() {
                    let identities: Vec<peek_core::model::EntityId> =
                        mapped.candidates.iter().map(|c| c.id.clone()).collect();
                    mapped.candidates = ToolError::describe_candidates(store, &identities);
                }
                return Err(mapped);
            }
        }
    };

    let outcome = match pack.budget.status {
        BudgetStatus::Complete => Outcome::Ok,
        BudgetStatus::Reduced => Outcome::Reduced,
        BudgetStatus::Insufficient => Outcome::Insufficient,
    };
    let list = options.list_ambiguity_candidates;
    session.log(&format!(
        "context {target} @ {budget} token(s): {} — {}/{} spent, {} unit(s), {} omission(s), \
         {} note(s)",
        pack.budget.status,
        pack.budget.spent_tokens,
        pack.budget.requested_tokens,
        pack.units.len(),
        pack.omitted.len(),
        pack.notes.len()
    ));

    let text = pack.render(list);
    let body = ContextBody {
        verdict: Verdict {
            outcome,
            reason: match outcome {
                Outcome::Insufficient => Some(format!(
                    "the target does not fit a budget of {} token(s) alongside the report, so \
                     this is a refusal rather than a slice",
                    pack.budget.requested_tokens
                )),
                _ => None,
            },
            advice: match minimum {
                Some(floor) if outcome == Outcome::Insufficient => {
                    Some(format!("ask for at least {floor} tokens"))
                }
                _ => None,
            },
            candidates: Vec::new(),
        },
        // The whole pack, as the engine built it. `Option` because a refusal produces no pack, and
        // `null` rather than an absent key so the shape does not depend on the outcome.
        pack: Some(pack.clone()),
        /// The same rendered text, so a caller that only wants prose does not have to re-derive it
        /// from the units — and so a caller can check that the two agree.
        rendered: text.clone(),
        /// How many edges in the pack are undecided, counted from the pack rather than asserted.
        ///
        /// A number a caller can read before deciding how much to trust the pack, which is the
        /// single most useful thing to know about an answer of this shape.
        uncertain_edges: pack
            .edges()
            .filter(|edge| {
                edge.relation.resolution.is_ambiguous() || edge.relation.resolution.is_unresolved()
            })
            .count(),
    };
    Ok(ToolAnswer::new(
        text,
        serde_json::to_value(&body).unwrap_or_else(|error| {
            serde_json::json!({
                "outcome": Outcome::Failed.as_str(),
                "reason": format!("the context pack could not be encoded: {error}"),
                "advice": Value::Null,
                "candidates": [],
                "pack": Value::Null,
            })
        }),
    ))
}

/// The structured body of a compiled pack.
#[derive(Debug, Serialize)]
struct ContextBody {
    #[serde(flatten)]
    verdict: Verdict,
    /// The engine's pack, whole. Never projected: the budget arithmetic is only checkable if the
    /// units, the edges, their costs and the omissions are all present.
    pack: Option<ContextPack>,
    /// The answer as text, identical to the text block of the same MCP result.
    rendered: String,
    /// How many edges in the pack the engine could not decide.
    uncertain_edges: usize,
}
