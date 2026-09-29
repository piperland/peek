//! Exit codes, and the one taxonomy of ways a command can decline.
//!
//! # The exit code is the API
//!
//! An agent calls this binary and branches on what came back. Everything else about the output —
//! the prose, the ordering, the column widths — is free to change. The number is not, because
//! changing it silently changes what every caller does. So the codes are few, named, and
//! documented in `--help`.
//!
//! | code | name | when |
//! |---:|---|---|
//! | 0 | ok | the command did what it was asked |
//! | 1 | failed | the engine could not do it: the index, the filesystem or a query failed |
//! | 2 | usage | the command line was not understood |
//! | 3 | refused | the question cannot be answered from this index, and the reason is printed |
//! | 4 | unhealthy | `doctor` found a failing check |
//!
//! # Why "refused" is separate from "failed"
//!
//! Audit D, section F: `--direction sideways` became `Outbound` and `--kind nonsense` became no
//! filter, silently. The same collapse happens to a *result*: an ambiguous target and a target
//! that does not exist are both "no answer", and a caller that cannot tell them apart retries or
//! gives up for reasons that have nothing to do with the fix.
//!
//! So a refusal is a first-class outcome with its own code, and it always carries the thing the
//! caller needs to stop asking: the candidate list for an ambiguity, the minimum for a budget, the
//! reason a target was not found. Code 3 is never returned for something code 1 would cover.
//!
//! # Why `doctor` has its own code
//!
//! `doctor` is the command an agent runs to find out whether it can trust the other commands. If a
//! failing check returned 1, a caller could not distinguish "the diagnosis failed" from "the
//! diagnosis found something broken", and the first is worth retrying while the second is worth
//! re-indexing.

use serde::{Deserialize, Serialize};

/// The command did what it was asked.
pub const EXIT_OK: u8 = 0;
/// The engine could not do it: the index, the filesystem or a query failed.
pub const EXIT_FAILED: u8 = 1;
/// The command line was not understood.
pub const EXIT_USAGE: u8 = 2;
/// The question cannot be answered from this index.
pub const EXIT_REFUSED: u8 = 3;
/// `doctor` found a failing check.
pub const EXIT_UNHEALTHY: u8 = 4;

/// The outcome of a command, and the code it exits with.
///
/// One enum rather than a bare integer, so a code cannot be produced without the name that
/// explains it, and so the JSON mode can carry the name an agent branches on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// The command did what it was asked.
    Ok,
    /// The command line was not understood.
    Usage,
    /// The question cannot be answered from this index.
    Refused,
    /// `doctor` found a failing check.
    Unhealthy,
    /// The engine could not do it.
    Failed,
}

impl Status {
    /// The number the process exits with.
    #[must_use]
    pub const fn exit_code(self) -> u8 {
        match self {
            Status::Ok => EXIT_OK,
            Status::Usage => EXIT_USAGE,
            Status::Refused => EXIT_REFUSED,
            Status::Unhealthy => EXIT_UNHEALTHY,
            Status::Failed => EXIT_FAILED,
        }
    }

    /// A stable lowercase label, for the JSON mode.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Usage => "usage",
            Status::Refused => "refused",
            Status::Unhealthy => "unhealthy",
            Status::Failed => "failed",
        }
    }
}

/// A stable, machine-readable name for a refusal.
///
/// Names rather than numbers so the JSON mode's `refusal.kind` survives a reordering of the enum,
/// and so a caller can match on `"ambiguous_target"` without counting variants.
pub mod kind {
    /// The command line named a flag this build does not have.
    pub const UNKNOWN_FLAG: &str = "unknown_flag";
    /// The command line named a command this build does not have.
    pub const UNKNOWN_COMMAND: &str = "unknown_command";
    /// A flag was repeated, given to a command that does not take it, or given a value it does not
    /// accept.
    pub const BAD_FLAG_USE: &str = "bad_flag_use";
    /// A flag that needs a value and the command line ended.
    pub const MISSING_VALUE: &str = "missing_value";
    /// A flag's value was not the number it has to be.
    pub const NOT_A_NUMBER: &str = "not_a_number";
    /// The wrong number of positional arguments.
    pub const WRONG_ARITY: &str = "wrong_arity";
    /// The argument was not valid UTF-8.
    pub const NOT_UTF8: &str = "not_utf8";
    /// The repository path is not a directory, or cannot be resolved.
    pub const NOT_A_REPOSITORY: &str = "not_a_repository";
    /// The path named a location outside the repository.
    pub const OUTSIDE_REPOSITORY: &str = "outside_repository";
    /// Nothing in the index answers to the target string.
    pub const UNKNOWN_TARGET: &str = "unknown_target";
    /// Several entities answer to it, and the caller must say which.
    pub const AMBIGUOUS_TARGET: &str = "ambiguous_target";
    /// The budget cannot hold the report that would state what was dropped.
    pub const BUDGET_TOO_SMALL: &str = "budget_too_small";
    /// The target itself does not fit the budget.
    pub const BUDGET_INSUFFICIENT: &str = "budget_insufficient";
    /// There is no index for this repository yet.
    pub const NO_INDEX: &str = "no_index";
    /// `context` was asked for without a budget, and this build will not invent one.
    pub const NO_BUDGET: &str = "no_budget";
    /// The index, the filesystem or a query failed.
    pub const ENGINE: &str = "engine";
    /// Watching could not start.
    pub const WATCH_UNAVAILABLE: &str = "watch_unavailable";
    /// One or more batches failed to apply; the index is behind.
    pub const WATCH_INCOMPLETE: &str = "watch_incomplete";
    /// `doctor` found at least one check at `fail`.
    pub const UNHEALTHY: &str = "unhealthy";
}

/// What a command could not do, and what the caller can do about it.
///
/// Always present in the JSON mode when the status is not `ok`, and always printed in the human
/// mode. There is no path by which a command declines without this.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refusal {
    /// One of the names in [`kind`], copied into an owned string.
    ///
    /// Owned rather than `&'static str` because this type is `Deserialize` — a JSON mode that
    /// cannot be read back is a JSON mode nobody has checked — and a borrowed tag is exactly the
    /// field `serde` cannot produce for a plain enum. The constructor still takes a `&'static str`,
    /// so a caller writes a constant and never a `String`.
    pub kind: String,
    /// The statement, in one or two sentences, naming what was tried.
    pub message: String,
    /// Every candidate an ambiguous target could have meant. D-0004: the caller disambiguates,
    /// never this program. Empty for every other kind.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<String>,
    /// The smallest budget that would have been accepted, in tokens. `None` when the question was
    /// not about a budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum_tokens: Option<u64>,
}

impl Refusal {
    /// A refusal with no candidates and no minimum.
    #[must_use]
    pub fn new(kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            kind: kind.to_owned(),
            message: message.into(),
            candidates: Vec::new(),
            minimum_tokens: None,
        }
    }

    /// A refusal that names the candidates a caller could have meant.
    #[must_use]
    pub fn with_candidates(mut self, candidates: Vec<String>) -> Self {
        self.candidates = candidates;
        self
    }

    /// A refusal that states the smallest budget that would have worked.
    #[must_use]
    pub fn with_minimum(mut self, minimum_tokens: u64) -> Self {
        self.minimum_tokens = Some(minimum_tokens);
        self
    }

    /// The human rendering: the message, then the candidates, then the minimum.
    #[must_use]
    pub fn render(&self) -> String {
        let mut text = self.message.clone();
        if !self.candidates.is_empty() {
            text.push_str(&format!(
                "\n{} candidate(s); name one of them:",
                self.candidates.len()
            ));
            for candidate in &self.candidates {
                text.push_str(&format!("\n  {candidate}"));
            }
        }
        if let Some(minimum) = self.minimum_tokens {
            text.push_str(&format!(
                "\nthe smallest budget that would have been accepted is {minimum} token(s)"
            ));
        }
        text
    }
}

/// A command that did not produce an answer, and why.
///
/// Carries the command name so a failure rendered after a later command is still attributable,
/// and the code so `main` does not have to reconstruct it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failure {
    /// The command that failed, as typed.
    pub command: String,
    /// The status, and therefore the code.
    pub status: Status,
    /// What went wrong, and what to do about it.
    pub refusal: Refusal,
}

impl Failure {
    /// The command line was not understood. Exit 2.
    pub fn usage(command: &str, refusal: Refusal) -> Self {
        Self {
            command: command.to_owned(),
            status: Status::Usage,
            refusal,
        }
    }

    /// The question cannot be answered from this index. Exit 3.
    pub fn refused(command: &'static str, refusal: Refusal) -> Self {
        Self {
            command: command.to_owned(),
            status: Status::Refused,
            refusal,
        }
    }

    /// The engine could not do it. Exit 1.
    pub fn failed(command: &'static str, refusal: Refusal) -> Self {
        Self {
            command: command.to_owned(),
            status: Status::Failed,
            refusal,
        }
    }

    /// The code the process exits with.
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        self.status.exit_code()
    }

    /// The human rendering, so a failure reads the same way an answer does.
    #[must_use]
    pub fn render(&self) -> String {
        format!(
            "peek {}: could not answer\n{}",
            self.command,
            self.refusal.render()
        )
    }
}
