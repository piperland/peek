//! What happened, said in a way a caller can act on.
//!
//! # The problem this solves
//!
//! An agent calling a tool cannot tell an empty answer from a failed one. Both arrive as "here is
//! your result" and both contain no symbols. That ambiguity is not a formatting problem; it is the
//! reason an agent confidently concludes that a function has no callers when the truth is that
//! the index could not be opened.
//!
//! So every response this crate produces carries a [`Verdict`], and the verdict is one of a closed
//! set of states rather than an empty array. The states are:
//!
//! | Outcome | What the caller should do |
//! |---|---|
//! | [`Outcome::Ok`] | use the answer |
//! | [`Outcome::Reduced`] | use the answer, and read what was dropped |
//! | [`Outcome::Insufficient`] | raise the budget |
//! | [`Outcome::Refused`] | fix the request, or run the tool the `advice` names |
//! | [`Outcome::AmbiguousTarget`] | choose one of the `candidates` and ask again |
//! | [`Outcome::UnknownTarget`] | check the name; `reason` says which lookups missed |
//! | [`Outcome::NotIndexed`] | run `index` |
//! | [`Outcome::Failed`] | something outside the caller's control; `reason` says what |
//!
//! [`Outcome::UnknownTarget`] and [`Outcome::Ok`] with an empty body are the pair that must never
//! be confusable, and they are not.
//!
//! # Which outcomes set `isError`
//!
//! Only [`Outcome::Failed`]. The MCP field marks a tool call as errored, and several clients
//! surface an errored call as a one-line failure rather than feeding the content to the model —
//! which is exactly wrong for "your name matched three things, here they are", because the model's
//! next move is to pick one and that pick has to be made by the model. A refusal, an ambiguity and
//! an unknown target are all *answers*, and they carry their payload in `structuredContent` and in
//! the text block beside it.

use serde::Serialize;

use peek_core::model::EntityId;

/// What a tool did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// The tool did what it was asked to do. An empty payload is a real answer here, not a
    /// failure: "nothing calls this" is a fact about the code.
    Ok,
    /// The tool did what it was asked to do and left something out to fit a budget. The omissions
    /// are named in the payload.
    Reduced,
    /// The target itself does not fit the budget. Nothing was returned and every candidate is
    /// listed as dropped.
    Insufficient,
    /// The request was understood and the engine declined. `advice` says what would be accepted.
    Refused,
    /// Several indexed entities answer to the target string. `candidates` holds all of them.
    AmbiguousTarget,
    /// Nothing indexed answers to the target string. `reason` names which lookups were tried.
    UnknownTarget,
    /// An identity the caller already held is not in the index — usually because it was built
    /// against a different generation.
    NotIndexed,
    /// The engine could not answer for a reason the caller cannot fix by changing the request.
    Failed,
}

impl Outcome {
    /// The word the text rendering prints, and the word a reader greps for.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::Reduced => "reduced",
            Outcome::Insufficient => "insufficient",
            Outcome::Refused => "refused",
            Outcome::AmbiguousTarget => "ambiguous_target",
            Outcome::UnknownTarget => "unknown_target",
            Outcome::NotIndexed => "not_indexed",
            Outcome::Failed => "failed",
        }
    }

    /// Whether the MCP result should carry `isError: true`.
    ///
    /// See the module documentation: only [`Outcome::Failed`], because every other state is
    /// something the model can act on and an errored tool call is not reliably shown to it.
    #[must_use]
    pub const fn is_error(self) -> bool {
        matches!(self, Outcome::Failed)
    }
}

impl std::fmt::Display for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The header every tool response carries.
///
/// Flattened into each response rather than nested under a key, so `outcome` is at the top level
/// of every response and a consumer reads one field regardless of which tool it called. No field
/// is ever omitted: `reason` is `null` on success and `candidates` is `[]` on success, because a
/// field that appears and disappears is a field whose absence has to be interpreted.
#[derive(Debug, Clone, Serialize)]
pub struct Verdict {
    /// What happened. Never absent.
    pub outcome: Outcome,
    /// A sentence the caller can act on, or `None` when the outcome speaks for itself.
    pub reason: Option<String>,
    /// What to do instead, when there is a specific next step. Named as a tool call wherever one
    /// exists, so the advice is executable rather than advisory.
    pub advice: Option<String>,
    /// Every entity the target could have meant. Empty unless the target was ambiguous, in which
    /// case it is never empty by accident: the engine's own error carries the list.
    pub candidates: Vec<Candidate>,
}

impl Verdict {
    /// The verdict for a successful call.
    #[must_use]
    pub fn ok() -> Self {
        Self {
            outcome: Outcome::Ok,
            reason: None,
            advice: None,
            candidates: Vec::new(),
        }
    }

    /// A verdict with a state and a sentence and nothing else.
    #[must_use]
    pub fn of(outcome: Outcome, reason: impl Into<String>) -> Self {
        Self {
            outcome,
            reason: Some(reason.into()),
            advice: None,
            candidates: Vec::new(),
        }
    }

    /// A verdict naming the tool that would fix it.
    #[must_use]
    pub fn advising(outcome: Outcome, reason: impl Into<String>, advice: impl Into<String>) -> Self {
        Self {
            outcome,
            reason: Some(reason.into()),
            advice: Some(advice.into()),
            candidates: Vec::new(),
        }
    }

    /// A verdict listing the entities a name could have meant.
    #[must_use]
    pub fn ambiguous(reason: impl Into<String>, candidates: Vec<Candidate>) -> Self {
        Self {
            outcome: Outcome::AmbiguousTarget,
            reason: Some(reason.into()),
            advice: Some(
                "call the same tool again with `target` set to one candidate's `id` field verbatim"
                    .to_owned(),
            ),
            candidates,
        }
    }

    /// Turn an engine failure into the verdict the caller sees.
    ///
    /// One function, so every tool maps the same `QueryError` to the same words. A per-tool
    /// translation is how two tools end up disagreeing about what `BudgetTooSmall` means.
    #[must_use]
    pub fn from_query_error(error: &peek_core::query::QueryError) -> Self {
        use peek_core::query::QueryError;
        match error {
            QueryError::UnknownTarget { query, detail } => Self::advising(
                Outcome::UnknownTarget,
                format!("no indexed entity matches `{query}`: {detail}"),
                "the target is looked up three ways, in order: as a repository path, as a \
                 qualified name, and as a bare name; the detail above says which of them it was not",
            ),
            QueryError::AmbiguousTarget {
                query,
                matched,
                candidates,
            } => Self {
                outcome: Outcome::AmbiguousTarget,
                reason: Some(format!(
                    "`{query}` is {matched} for {} indexed entities",
                    candidates.len()
                )),
                advice: Some(
                    "call the same tool again with `target` set to one candidate's `id` field \
                     verbatim"
                        .to_owned(),
                ),
                candidates: candidates.iter().map(Candidate::of).collect(),
            },
            QueryError::NotIndexed { query } => Self::advising(
                Outcome::NotIndexed,
                format!("no entity is indexed as {query}"),
                "the index moved under the caller; run `index` and read the generation it reports",
            ),
            QueryError::BudgetTooSmall { requested, minimum } => Self::advising(
                Outcome::Refused,
                format!(
                    "a budget of {requested} token(s) cannot hold the {minimum}-token report that \
                     would say what was dropped"
                ),
                format!("ask for at least {minimum} tokens; the report is charged against the \
                         budget, so a smaller one cannot be honoured without lying about the cost"),
            ),
            QueryError::Store(inner) => Self::of(Outcome::Failed, format!("the index store: {inner}")),
        }
    }
}
/// One thing a name might have meant.
///
/// The identity as the engine spells it, plus the three fields a caller needs to choose: what it
/// is, which file, which line. An identity alone is not a choice — it is an id a model has to
/// guess at — so the extra fields are the point of the type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Candidate {
    /// The identity, exactly as `EntityId` serialises. Reuse it verbatim as `target`.
    pub id: EntityId,
    /// `path::qualified_name`, the compact form for logs and for a human reading a transcript.
    pub display: String,
    /// The declaration's kind, lowercased.
    pub kind: String,
    /// Which repository-relative file declares it.
    pub path: String,
    /// Where it starts, when it has a span. `null` for a file or other structural entity.
    pub start_line: Option<u32>,
    /// The declared signature, when the extractor read one. Never synthesised.
    pub signature: Option<String>,
}

impl Candidate {
    /// A candidate described only by its identity.
    ///
    /// Used when the entity row could not be read — which is itself a fact, and is reported as
    /// absent fields rather than as a fabricated zero line number.
    #[must_use]
    pub fn of(id: &EntityId) -> Self {
        Self {
            id: id.clone(),
            display: id.display(),
            kind: id.kind().as_str().to_owned(),
            path: id.path().as_str().to_owned(),
            start_line: None,
            signature: None,
        }
    }

    /// A candidate described by its entity row, when one is available.
    #[must_use]
    pub fn from_entity(entity: &peek_core::model::Entity) -> Self {
        Self {
            id: entity.id.clone(),
            display: entity.id.display(),
            kind: entity.id.kind().as_str().to_owned(),
            path: entity.id.path().as_str().to_owned(),
            start_line: entity.span.map(|span| span.start_line),
            signature: entity.signature.clone(),
        }
    }
}

/// A tool call that could not produce its payload.
///
/// Not an `anyhow` and not a bare string: the *state* is the part a caller branches on, and a
/// string cannot be branched on safely.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{verdict_reason}")]
pub struct ToolError {
    /// The state the caller sees.
    pub outcome: Outcome,
    /// A sentence, and the whole of the `Display` output.
    pub verdict_reason: String,
    /// What to do instead.
    pub advice: Option<String>,
    /// The entities a name could have meant.
    pub candidates: Vec<Candidate>,
    /// The smallest budget that would have been accepted, when the refusal was about a budget.
    ///
    /// Carried on the error rather than formatted into the advice sentence so it survives into
    /// `structuredContent` as a number a program can branch on. A refusal that says "raise the
    /// budget" without saying to what is a refusal the caller has to guess the rest of.
    pub minimum_tokens: Option<u64>,
}

impl ToolError {
    /// A refusal: understood, not done.
    #[must_use]
    pub fn refused(reason: impl Into<String>, advice: impl Into<String>) -> Self {
        Self {
            outcome: Outcome::Refused,
            verdict_reason: reason.into(),
            advice: Some(advice.into()),
            candidates: Vec::new(),
            minimum_tokens: None,
        }
    }

    /// A failure the caller cannot fix by changing the request.
    #[must_use]
    pub fn failed(reason: impl Into<String>) -> Self {
        Self {
            outcome: Outcome::Failed,
            verdict_reason: reason.into(),
            advice: None,
            candidates: Vec::new(),
            minimum_tokens: None,
        }
    }

    /// A refusal that names the smallest budget that would have worked.
    #[must_use]
    pub fn budget(reason: impl Into<String>, advice: impl Into<String>, minimum: u64) -> Self {
        Self {
            outcome: Outcome::Refused,
            verdict_reason: reason.into(),
            advice: Some(advice.into()),
            candidates: Vec::new(),
            minimum_tokens: Some(minimum),
        }
    }

    /// No index yet.
    #[must_use]
    pub fn not_indexed(root: &std::path::Path) -> Self {
        Self {
            outcome: Outcome::NotIndexed,
            verdict_reason: format!(
                "{} has never been indexed, so there is nothing to ask about it",
                root.display()
            ),
            advice: Some("run the `index` tool first".to_owned()),
            candidates: Vec::new(),
            minimum_tokens: None,
        }
    }

    /// A bad or absent argument.
    #[must_use]
    pub fn argument(reason: impl Into<String>, advice: impl Into<String>) -> Self {
        Self::refused(reason, advice)
    }

    /// The same error as a [`Verdict`], for a caller that reached the verdict first.
    ///
    /// One translation from an engine error to caller-facing words, held in
    /// [`Verdict::from_query_error`]; this is the other direction, so a tool that builds a verdict
    /// by hand and then wants to return it as an error does not write a second sentence.
    #[must_use]
    pub fn from_verdict(verdict: &Verdict) -> Self {
        Self {
            outcome: verdict.outcome,
            verdict_reason: verdict
                .reason
                .clone()
                .unwrap_or_else(|| format!("the request was refused ({})", verdict.outcome)),
            advice: verdict.advice.clone(),
            candidates: verdict.candidates.clone(),
            minimum_tokens: None,
        }
    }

    /// The verdict to put in the response.
    #[must_use]
    pub fn verdict(&self) -> Verdict {
        Verdict {
            outcome: self.outcome,
            reason: Some(self.verdict_reason.clone()),
            advice: self.advice.clone(),
            candidates: self.candidates.clone(),
        }
    }

    /// Enrich a candidate list with entity rows, leaving it untouched where a row is missing.
    ///
    /// A row that cannot be read is not a candidate that does not exist; the identity is still
    /// returned, with the fields that needed the row left `null`.
    #[must_use]
    pub fn describe_candidates(
        store: &peek_core::store::Store,
        candidates: &[EntityId],
    ) -> Vec<Candidate> {
        candidates
            .iter()
            .map(|id| match store.entity(id) {
                Ok(Some(entity)) => Candidate::from_entity(&entity),
                _ => Candidate::of(id),
            })
            .collect()
    }
}
