//! `explain`: why an edge exists, and how sure the engine is.
//!
//! # What this tool adds to the engine's answer
//!
//! Almost nothing, and that is deliberate. `Query::explain` already returns the resolution state
//! as a typed value, the evidence class, the basis, the span and the chain; all this does is carry
//! them across and name the two things an agent most often needs and the engine does not put in
//! one place: **how much of the answer is decided**, and **which edges the engine could not
//! decide**.
//!
//! The second is the reason this tool exists as a separate surface. `uncertain` in the response is
//! every ambiguous and unresolved edge at the target, listed on its own. A caller that wants to
//! know "what does this engine *not* know about this symbol" gets it in one field rather than by
//! filtering, and a caller that wants to be sure it has seen the doubt does not have to check that
//! it read the list correctly.
//!
//! # Why the target is a string
//!
//! A caller has a name read off a screen, not an `EntityId`. The three lookups — path, qualified
//! name, bare name, in that order — are the engine's, reached through [`crate::tools::target`], so
//! there is exactly one resolution path in the product. An ambiguous name comes back as
//! `ambiguous_target` with every candidate; it is never resolved by picking the first.

use serde::Serialize;
use serde_json::Value;

use peek_core::query::{ChainStep, ExplainedEdge, Explanation, Query, QueryError, QueryOptions};

use crate::outcome::{Outcome, ToolError, Verdict};
use crate::params::Args;
use crate::session::Session;
use crate::tools::ToolAnswer;
use crate::tools::target;

/// Report every edge at a target, and the provenance chain to it.
pub fn explain(session: &mut Session, arguments: Option<&Value>) -> Result<ToolAnswer, ToolError> {
    let args = Args::new("explain", arguments, crate::tool::hints_for("explain"))?;
    let target = args.required_string("target", "a path, a qualified name, or a bare name")?;
    let chain_depth = args.optional_u32("chain_depth")?;
    let follow_inferred = args.optional_bool("follow_inferred")?;
    args.finish(&["target", "chain_depth", "follow_inferred"])?;

    let mut options = QueryOptions::default();
    if let Some(depth) = chain_depth {
        options.chain_depth = depth;
    }
    if let Some(follow) = follow_inferred {
        options.follow_inferred = follow;
    }

    let resolved = {
        let store = session.reader()?;
        let query = Query::with_options(store, options);
        let identity = target::resolve(store, options, &target)
            .map_err(|error| resolve_error(store, &error))?;
        query
            .explain(&identity)
            .map_err(|error| resolve_error(store, &error))?
    };

    let uncertain = resolved.uncertain().count();
    let decided = resolved.edges.len().saturating_sub(uncertain);
    session.log(&format!(
        "explain {target}: {} edge(s), {decided} decided, {uncertain} undecided, chain {} hop(s)",
        resolved.edges.len(),
        resolved.chain.len()
    ));
    Ok(ToolAnswer::new(
        resolved.render(),
        body(&resolved, &Verdict::ok()),
    ))
}

/// Turn a `QueryError` into the verdict the caller sees, with its candidates filled in from the
/// index where there are any.
///
/// Without the lookup the candidates would be bare identities, and a list of identities is not a
/// choice — it is a thing the model has to guess at. The rule is that an ambiguity is only
/// actionable if the ambiguity names what each option *is*.
fn resolve_error(store: &peek_core::store::Store, error: &QueryError) -> ToolError {
    let mut mapped = ToolError::from_verdict(&Verdict::from_query_error(error));
    if !mapped.candidates.is_empty() {
        let identities: Vec<peek_core::model::EntityId> =
            mapped.candidates.iter().map(|c| c.id.clone()).collect();
        mapped.candidates = ToolError::describe_candidates(store, &identities);
    }
    mapped
}

/// The structured body.
#[derive(Debug, Serialize)]
struct ExplainBody {
    #[serde(flatten)]
    verdict: Verdict,
    /// The subject, described the way the engine describes it.
    subject: String,
    /// The identity at the centre of the explanation, so a caller can pass it straight back to
    /// another tool without parsing the sentence.
    entity: peek_core::model::EntityId,
    /// Every edge at the subject, each carrying its typed resolution state inside `relation`.
    edges: Vec<ExplainedEdge>,
    /// The edges the engine could not decide, listed on their own.
    ///
    /// A projection of `edges`, not a separate read: the list is derived so it cannot disagree
    /// with the edges it names.
    uncertain: Vec<ExplainedEdge>,
    /// How many edges are decided, as a claim about the answer above and not as a separate
    /// measurement.
    decided_edges: usize,
    /// One chosen edge per hop, each recording how many alternatives it passed over.
    chain: Vec<ChainStep>,
    /// The depth the chain was allowed, whether or not it used it.
    chain_depth: u32,
    /// The options in force, so the answer says what it was allowed to read.
    options: QueryOptions,
    /// Statements the answer owes the reader.
    notes: Vec<String>,
}

fn body(explanation: &Explanation, verdict: &Verdict) -> Value {
    let uncertain: Vec<ExplainedEdge> = explanation.uncertain().cloned().collect();
    let body = ExplainBody {
        verdict: verdict.clone(),
        subject: explanation.subject.describe(),
        entity: explanation.entity().clone(),
        decided_edges: explanation.edges.len().saturating_sub(uncertain.len()),
        edges: explanation.edges.clone(),
        uncertain,
        chain: explanation.chain.clone(),
        chain_depth: explanation.chain_depth,
        options: explanation.options,
        notes: explanation.notes.clone(),
    };
    serde_json::to_value(&body).unwrap_or_else(|error| {
        serde_json::json!({
            "outcome": Outcome::Failed.as_str(),
            "reason": format!("the explanation could not be encoded: {error}"),
            "advice": Value::Null,
            "candidates": [],
            "edges": [],
            "uncertain": [],
            "chain": [],
        })
    })
}
