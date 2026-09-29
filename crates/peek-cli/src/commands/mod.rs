//! The command bodies.
//!
//! One function per command, each taking the resolved [`Location`] rather than a path, so no
//! command can reach an index that is not the one belonging to the repository it was pointed at.
//! The dispatch is an exhaustive `match` over [`Command`], so a new command cannot be added to the
//! argument table without this file noticing.

mod context;
mod doctor;
mod index;
pub mod query;
mod rm;
mod status;
pub mod watch;

use crate::answer::Answer;
use crate::args::Command;
use crate::exit::{Failure, Refusal, Status, kind};
use crate::paths::Location;
use crate::progress::Progress;
use peek_core::query::WalkRequest;

/// What a command produced, and how it should be reported.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    /// The status, and therefore the exit code.
    pub status: Status,
    /// The refusal, when there is one. `Some` for any status other than `Ok`, and `Some` for a
    /// `context` pack that was refused a budget even though the question resolved.
    pub refusal: Option<Refusal>,
    /// The answer, in full. Present whatever the status, because a refusal that carries no answer
    /// is a refusal the caller cannot inspect.
    pub answer: Answer,
}

impl Outcome {
    /// A command that did what it was asked.
    #[must_use]
    pub fn ok(answer: Answer) -> Self {
        Self {
            status: Status::Ok,
            refusal: None,
            answer,
        }
    }

    /// A command that produced an answer *and* has something to say about its own limits.
    #[must_use]
    pub fn limited(status: Status, refusal: Refusal, answer: Answer) -> Self {
        Self {
            status,
            refusal: Some(refusal),
            answer,
        }
    }
}

/// Run a command.
///
/// `location` is `None` only for `help` and `version`, which read no index. Every command that
/// needs one calls [`need`] rather than unwrapping, so the impossible case is typed rather than
/// asserted.
pub fn run(
    command: &Command,
    location: Option<&Location>,
    progress: &mut dyn Progress,
) -> Result<Outcome, Failure> {
    match command {
        Command::Help => Ok(Outcome::ok(Answer::Help(crate::answer::HelpAnswer {
            usage: crate::args::help_text(),
        }))),
        Command::Version => Ok(Outcome::ok(Answer::Version(crate::answer::VersionAnswer {
            version: peek_core::VERSION.to_owned(),
            engine_version: peek_core::VERSION.to_owned(),
        }))),
        Command::Index { full, .. } => index::run(location, "index", *full, progress),
        Command::Watch {
            quiet_for_ms,
            max_batch_ms,
            ..
        } => watch::run(location, "watch", *quiet_for_ms, *max_batch_ms, progress),
        Command::Explain { target, .. } => query::explain(location, "explain", target),
        Command::Callers { target, .. } => {
            query::walk(location, "callers", target, WalkRequest::callers())
        }
        Command::Callees { target, .. } => {
            query::walk(location, "callees", target, WalkRequest::callees())
        }
        Command::Dependents { target, depth, .. } => query::walk(
            location,
            "dependents",
            target,
            WalkRequest::dependents(*depth),
        ),
        Command::Context { target, budget, .. } => {
            context::run(location, "context", target, *budget)
        }
        Command::Doctor { .. } => doctor::run(location, "doctor"),
        Command::Status { .. } => status::run(location, "status"),
        Command::Remove { path, .. } => rm::run(location, "rm", path),
    }
}

/// The location, or a refusal that says what was missing.
///
/// A `None` here is unreachable — `lib::run` resolves a location for every command but `help` and
/// `version`, and those two never reach this function — so the message is a statement about the
/// program rather than about the user's machine, and it is phrased as one.
pub fn need<'a>(
    location: Option<&'a Location>,
    command: &'static str,
) -> Result<&'a Location, Failure> {
    location.ok_or_else(|| {
        Failure::failed(
            command,
            Refusal::new(
                kind::ENGINE,
                format!(
                    "peek {command} read no repository; this is a defect in peek rather than \
                     something wrong with the command line"
                ),
            ),
        )
    })
}
