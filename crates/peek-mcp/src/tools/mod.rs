//! The tools, and the one place a name becomes a call.
//!
//! # Dispatch is a match and nothing else
//!
//! [`dispatch`] maps a tool name to a handler and returns what the handler produced. There is no
//! registry, no reflection, no macro, and no per-tool middleware — because every one of those is a
//! place where a tool could acquire behaviour its author did not write, and the whole claim of
//! this crate is that it adds framing to the engine and nothing else. A tool that is not in the
//! match does not exist, and a name that is not in the catalogue is refused by name.
//!
//! # The shape every answer takes
//!
//! A handler returns a [`ToolAnswer`]: a readable text block and a structured value, plus the
//! error flag the MCP result needs. A handler that cannot do the job returns a [`ToolError`], and
//! [`refusal`] turns that into a [`ToolAnswer`] of the same shape — so a caller reading only
//! `structuredContent` sees the same four fields whether the tool worked or not, and a caller
//! reading only the text sees a sentence either way.

pub mod context;
pub mod doctor;
pub mod explain;
pub mod index;
pub mod target;
pub mod walk;
pub mod watch;

use serde::Serialize;
use serde_json::Value;

use crate::outcome::{ToolError, Verdict};
use crate::session::Session;
use crate::tool;

/// What a handler produced.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToolAnswer {
    /// The answer as text, for a client that only renders text.
    pub text: String,
    /// The answer as data, for a client that parses.
    pub structured: Value,
    /// Whether the MCP result should be marked as an error. See [`crate::Outcome::is_error`].
    pub is_error: bool,
}

impl ToolAnswer {
    /// A successful answer.
    pub fn new(text: impl Into<String>, structured: Value) -> Self {
        Self {
            text: text.into(),
            structured,
            is_error: false,
        }
    }
}

/// The structured body of a tool that could not do its job.
///
/// Every field a success has, plus the tool that failed. Serialised rather than free prose so a
/// program can branch on `outcome` without parsing a sentence.
#[derive(Debug, Clone, Serialize)]
struct Refusal {
    /// Which tool refused.
    tool: &'static str,
    /// The four standard fields.
    #[serde(flatten)]
    verdict: Verdict,
    /// The smallest budget that would be accepted, when the refusal was about a budget.
    ///
    /// Present and `null` otherwise rather than omitted, so the shape of a refusal does not depend
    /// on what went wrong.
    minimum_tokens: Option<u64>,
}

/// Render a [`ToolError`] as an answer, so a refusal reaches the caller in the same shape a
/// success does.
pub fn refusal(tool: &'static str, error: &ToolError) -> ToolAnswer {
    let body = Refusal {
        tool,
        verdict: error.verdict(),
        minimum_tokens: error.minimum_tokens,
    };
    let mut text = format!("{tool} did not answer.\n");
    text.push_str(&format!("outcome: {}\n", error.outcome.as_str()));
    // Unconditional, unlike the two lines below it, because there is no state in which the reason
    // exists in one and not the other: `ToolError::verdict` puts this same sentence into the
    // structured body as `reason`. A reason that came and went with the outcome would be a field a
    // reader had to interpret.
    // Positional, because an inline capture takes a plain identifier and not a field access — so
    // `{error.verdict_reason}` is not a shorthand, it is a format string the compiler rejects.
    // `uninlined_format_args` does not apply: it fires on a bare variable, not on a field.
    text.push_str(&format!("reason: {0}\n", error.verdict_reason));
    if let Some(advice) = &error.advice {
        text.push_str(&format!("do this instead: {advice}\n"));
    }
    if let Some(minimum) = error.minimum_tokens {
        text.push_str(&format!("smallest budget accepted: {minimum} tokens\n"));
    }
    for candidate in &error.candidates {
        text.push_str(&format!(
            "  candidate: {} ({}, {}:{})\n",
            candidate.display,
            candidate.kind,
            candidate.path,
            candidate
                .start_line
                .map_or_else(|| "-".to_owned(), |line| line.to_string())
        ));
    }
    ToolAnswer {
        text,
        structured: serde_json::to_value(&body).unwrap_or_else(|_| {
            // A body of four owned types and a string cannot fail to encode; the fallback exists
            // so this function can hand back an answer rather than panic, and it is a valid
            // JSON-RPC payload in its own right.
            serde_json::json!({
                "tool": tool,
                "outcome": "failed",
                "reason": "the refusal could not be encoded",
                "advice": Value::Null,
                "candidates": []
            })
        }),
        is_error: error.outcome.is_error(),
    }
}

/// Run one tool.
pub fn dispatch(
    session: &mut Session,
    name: &str,
    arguments: Option<&Value>,
) -> Result<ToolAnswer, ToolError> {
    match name {
        "index" => index::index(session, arguments),
        "index_status" => index::status(session, arguments),
        "explain" => explain::explain(session, arguments),
        "callers" => walk::callers(session, arguments),
        "callees" => walk::callees(session, arguments),
        "dependents" => walk::dependents(session, arguments),
        "context" => context::context(session, arguments),
        "doctor" => doctor::doctor(session, arguments),
        "watch_start" => watch::start(session, arguments),
        "watch_stop" => watch::stop(session, arguments),
        other => Err(ToolError::refused(
            format!("there is no tool named `{other}`"),
            format!("the tools are: {}", tool::names().join(", ")),
        )),
    }
}
