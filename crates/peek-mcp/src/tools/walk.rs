//! `callers`, `callees` and `dependents`: one walk, three parameters.
//!
//! # Why they are three tools and not one
//!
//! D-0007 established that the predecessor's `impact()` was reverse structural traversal under a
//! name that promised patch-risk analysis it never did, and audit B12 proves `impact(t, d)` was
//! identical to an inbound dependency walk. There is no second algorithm here, and adding one would
//! recreate exactly the confusion the decision removed.
//!
//! So the three are the same call with different parameters, and each gets its own tool name
//! because a model choosing between them should not also have to choose a `direction` string. The
//! failure mode of a `direction` parameter is a confidently inverted answer: `callees` asked for
//! as `callers` returns the opposite graph and reads correctly as the wrong one.
//!
//! # What comes back
//!
//! Every step carries the whole relation it arrived on, so the state, the evidence and the span
//! survive into the response with nothing projected away. Each step also carries the declaration
//! it reached — kind, path, line, signature — because an identity on its own is not something a
//! caller can act on; it is a thing it has to open the index to interpret.
//!
//! And each step says whether it arrived by inference. `follow_inferred` defaults to true, and
//! the count of steps that used an inferred edge is reported so a caller can check the list
//! against the number rather than trust it.

use serde::Serialize;
use serde_json::Value;

use peek_core::query::{Direction, Query, QueryError, QueryOptions, Step, Walk, WalkRequest};

use crate::outcome::{ToolError, Verdict};
use crate::params::Args;
use crate::session::Session;
use crate::tools::ToolAnswer;

/// Who uses a target, one hop.
pub fn callers(session: &mut Session, arguments: Option<&Value>) -> Result<ToolAnswer, ToolError> {
    walk_tool(
        session,
        arguments,
        "callers",
        Direction::Inbound,
        None,
    )
}

/// What a target depends on, one hop.
pub fn callees(session: &mut Session, arguments: Option<&Value>) -> Result<ToolAnswer, ToolError> {
    walk_tool(
        session,
        arguments,
        "callees",
        Direction::Outbound,
        None,
    )
}

/// Who depends on a target, `depth` hops.
pub fn dependents(
    session: &mut Session,
    arguments: Option<&Value>,
) -> Result<ToolAnswer, ToolError> {
    walk_tool(
        session,
        arguments,
        "dependents",
        Direction::Inbound,
        Some("depth"),
    )
}

/// The one walk, parameterised by which tool asked for it.
///
/// `direction` is passed as a value rather than derived from the tool's name. Deriving it would
/// mean matching on a string, and a typo in that match produces the *opposite graph* under a
/// confident label — the specific failure the three tool names exist to prevent, reintroduced
/// inside the implementation of the thing that prevents it.
///
/// `depth_parameter` is `Some` only for `dependents`, because it is the only one of the three whose
/// definition involves a number of hops. `callers` and `callees` take no `depth`, and a `depth`
/// handed to either is refused with the name of the tool that takes it — the fix for the silent
/// parameter degradation this project exists to remove.
fn walk_tool(
    session: &mut Session,
    arguments: Option<&Value>,
    tool: &'static str,
    direction: Direction,
    depth_parameter: Option<&'static str>,
) -> Result<ToolAnswer, ToolError> {
    let args = Args::new(tool, arguments, crate::tool::hints_for(tool))?;
    let target = args.required_string("target", "a path, a qualified name, or a bare name")?;
    let kind = args.relation_kind("kind")?;
    let follow_inferred = args.optional_bool("follow_inferred")?;
    let depth = match depth_parameter {
        None => None,
        Some(name) => args.optional_u32(name)?,
    };
    let accepted: Vec<&'static str> = match depth_parameter {
        None => vec!["target", "kind", "follow_inferred"],
        Some(_) => vec!["target", "depth", "kind", "follow_inferred"],
    };
    args.finish(&accepted)?;

    let mut options = QueryOptions::default();
    if let Some(follow) = follow_inferred {
        options.follow_inferred = follow;
    }

    let walk = {
        let store = session.reader()?;
        let query = Query::with_options(store, options);
        let resolved = query
            .resolve(&target)
            .map_err(|error| fill_candidates(store, &error))?;
        let request = WalkRequest {
            direction,
            depth: depth.unwrap_or(1),
            kind,
        };
        query
            .walk(resolved.id(), request)
            .map_err(|error| fill_candidates(store, &error))?
    };

    let inferred = walk.followed_inferred;
    session.log(&format!(
        "{tool} {target}: {} step(s) at {} hop(s), {inferred} by inference, {} relation(s) read{}",
        walk.steps.len(),
        walk.max_distance().map_or_else(|| "no".to_owned(), |d| d.to_string()),
        walk.inspected,
        if walk.bounded { ", bound reached" } else { "" }
    ));

    // The declarations are read here rather than inside the walk, so the walk itself stays a pure
    // function of the store and the presentation pass is visibly a second thing. One point lookup
    // per step, each measured at single-digit microseconds in `.agent/DECISIONS.md` D-0002b, so
    // the cost is a rounding error beside the walk that found them.
    let steps: Vec<StepView> = {
        let store = session.reader()?;
        walk.steps
            .iter()
            .map(|step| StepView::of(step, store.entity(&step.id).ok().flatten()))
            .collect()
    };

    let body = WalkBody {
        verdict: Verdict::ok(),
        target: walk.target.clone(),
        request: walk.request,
        headline: walk.headline(),
        steps,
        /// Taken from the walk's own counter rather than recounted here, so the number and the list
        /// come from the same place in the engine and a caller can check them against each other.
        followed_inferred: inferred,
        seeds: walk.seeds.clone(),
        visited: walk.visited,
        inspected: walk.inspected,
        closed: walk.closed,
        revisits: walk.revisits,
        /// True when a limit stopped the walk, which is the difference between "nothing more
        /// depends on this" and "the walk ran out of budget".
        bounded: walk.bounded,
        options: walk.options,
    };
    let text = render(&walk);
    Ok(ToolAnswer::new(text, serde_json::to_value(&body).unwrap_or_else(|error| {
        serde_json::json!({
            "outcome": "failed",
            "reason": format!("the walk could not be encoded: {error}"),
            "advice": Value::Null,
            "candidates": [],
            "steps": [],
        })
    })))
}

/// The structured body of a walk.
#[derive(Debug, Serialize)]
struct WalkBody {
    #[serde(flatten)]
    verdict: Verdict,
    /// What the walk started from.
    target: peek_core::model::EntityId,
    /// The exact parameters, echoed so a caller can see what was asked for.
    request: WalkRequest,
    /// One line naming the relationship, the work done and the reach.
    headline: String,
    steps: Vec<StepView>,
    /// How many steps arrived by inference. Equal to the number of steps whose `inferred` is true.
    followed_inferred: usize,
    /// The members a structural target expanded to before the walk began.
    seeds: Vec<peek_core::model::EntityId>,
    /// Entities whose edges were read.
    visited: usize,
    /// Relations read, of every resolution state — including the ones that were not followed,
    /// because "we read it and could not follow it" is information.
    inspected: usize,
    /// Followable edges declined because the other end had already been reached. Non-zero means the
    /// neighbourhood is not a tree.
    closed: usize,
    /// Of `closed`, the ones that reached an entity already found.
    revisits: usize,
    /// A limit stopped the walk, so this answer may be incomplete.
    bounded: bool,
    /// The options in force, so the answer says what it was allowed to do.
    options: QueryOptions,
}

/// One entity the walk reached.
#[derive(Debug, Serialize)]
struct StepView {
    /// The entity reached.
    id: peek_core::model::EntityId,
    /// `path::qualified_name`, for a reader rather than a parser.
    display: String,
    /// Hops from the target. Always at least 1.
    distance: u32,
    /// Whether this arrival rests on an inference.
    inferred: bool,
    /// The resolution state, in the model's own one-line vocabulary. `ambiguous (3 candidates)`
    /// reads as ambiguous, which is the point.
    state: String,
    /// The edge that led here, in full.
    via: peek_core::model::Relation,
    /// The declaration reached, when the walk could read its row.
    declaration: Option<Declaration>,
}

impl StepView {
    fn of(step: &Step, entity: Option<peek_core::model::Entity>) -> Self {
        Self {
            id: step.id.clone(),
            display: step.id.display(),
            distance: step.distance,
            inferred: step.is_inferred(),
            state: step.state(),
            via: step.via.clone(),
            declaration: entity.as_ref().map(Declaration::of),
        }
    }
}

/// A declaration, with the three fields a caller needs to open it.
#[derive(Debug, Serialize)]
struct Declaration {
    kind: String,
    qualified_name: String,
    path: String,
    start_line: Option<u32>,
    end_line: Option<u32>,
    signature: Option<String>,
    is_test: bool,
}

impl Declaration {
    fn of(entity: &peek_core::model::Entity) -> Self {
        Self {
            kind: entity.kind().as_str().to_owned(),
            qualified_name: entity.id.qualified_name().to_owned(),
            path: entity.path().as_str().to_owned(),
            start_line: entity.span.map(|span| span.start_line),
            end_line: entity.span.map(|span| span.end_line),
            signature: entity.signature.clone(),
            is_test: entity.is_test,
        }
    }
}

fn render(walk: &Walk) -> String {
    let mut text = walk.headline();
    for step in &walk.steps {
        text.push_str(&format!(
            "\n  {} hop(s) away: {}{}",
            step.distance,
            step.id.display(),
            match &step.via.resolution {
                peek_core::model::ResolutionState::Inferred { basis, .. } => {
                    format!("  [inferred: {basis}]")
                }
                _ => String::new(),
            }
        ));
    }
    if walk.bounded {
        text.push_str(
            "\nnote: a limit stopped this walk, so the list above may be incomplete; `bounded` in \
             the response says so and `inspected` says how much was read",
        );
    }
    text
}

/// Fill an ambiguity's candidates with entity rows, so the caller can choose rather than guess.
fn fill_candidates(store: &peek_core::store::Store, error: &QueryError) -> ToolError {
    let mut mapped = ToolError::from_verdict(&crate::outcome::Verdict::from_query_error(error));
    if !mapped.candidates.is_empty() {
        let identities: Vec<peek_core::model::EntityId> =
            mapped.candidates.iter().map(|c| c.id.clone()).collect();
        mapped.candidates = ToolError::describe_candidates(store, &identities);
    }
    mapped
}
