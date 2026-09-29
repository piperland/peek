//! The `peek` command-line interface.
//!
//! # What this crate is
//!
//! A thin adapter over [`peek_core`]. It parses arguments, calls one engine function per command,
//! and renders the result. It contains no traversal, no ranking, no budget arithmetic and no
//! diagnostic of its own — every one of those lives in the engine and is tested there, which is
//! what contract C3 means by "CLI and MCP are adapters over ONE typed core".
//!
//! A library plus a thin `main`, so the argument parsing and the command bodies are testable
//! without spawning a process. [`run`] takes an [`Invocation`] and a [`Progress`] sink and returns
//! an [`Output`] or a [`Failure`]; `main` does the few things a binary has to do and nothing else.
//!
//! # The three rules this crate is built around
//!
//! ## 1. The exit code is the API
//!
//! [`exit`] defines five codes and every command returns one of them. An agent calls this binary
//! and branches on the number, so the number is the part of the interface that cannot change
//! quietly. The human text, the ordering and the column widths are free to move.
//!
//! ## 2. Every command states what it could not do
//!
//! This is the project's central rule, and a CLI is where it is easiest to break: a formatter that
//! prints a summary and drops the caveats is worse than no formatter, because it looks like an
//! answer. So every [`Answer`] carries a `did_not` list, printed in the human mode and carried in
//! the JSON mode under the same name. A refused budget prints the minimum. An ambiguous target
//! prints every candidate. A skipped file is counted *and named*. A `context` pack that
//! under-fills says the answer was complete.
//!
//! ## 3. One place turns arguments into a request
//!
//! [`args`] holds the flag table, the command table, and a single [`args::parse`]. An unknown flag
//! is one error path, not a behaviour per command; a flag given to a command that does not take it
//! is refused rather than ignored. The tables are also the `--help` text, so the documentation
//! cannot describe a flag that does not exist.
//!
//! # Path handling is a correctness property
//!
//! [`paths`] resolves the repository, canonicalises it, and refuses anything outside it. A
//! relative path, an absolute path, a symlinked root and a path outside the tree each behave the
//! way the contract says, and each has a test that says so.
//!
//! # The one thing this build cannot do
//!
//! **`peek watch` does not survive Ctrl-C.** A process killed by a signal runs no destructor, so
//! the final flush does not run and a batch open at that instant is not in the index. Detecting a
//! signal needs a dependency this crate does not have. Rather than take one silently, every watch
//! run says so in its `did_not` list. See the `watch` module for the two shutdowns that do work.
//!
//! # Example
//!
//! ```no_run
//! # use peek_cli::{Mode, Silent, args, run};
//! # fn main() {
//! let invocation = match args::parse(["status", "--json"]) {
//!     Ok(invocation) => invocation,
//!     Err(error) => { eprintln!("{}", error.message()); return; }
//! };
//! let mut progress = Silent;
//! match run(&invocation, &mut progress) {
//!     Ok(output) => println!("{}", crate::render(&output, &Mode::json())),
//!     Err(failure) => eprintln!("{}", failure.render()),
//! }
//! # }
//! ```

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod answer;
pub mod args;
pub mod commands;
pub mod exit;
pub mod paths;
pub mod progress;

use std::path::Path;

use serde::{Deserialize, Serialize};

pub use answer::Answer;
pub use args::{Command, Invocation};
pub use exit::{Failure, Refusal, Status};
pub use progress::{Collecting, Progress, Silent};

/// How to render an answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Mode {
    /// Emit JSON rather than prose.
    pub json: bool,
    /// Suppress progress narration. **Never** suppresses a finding, a refusal or a note, and the
    /// two are separated structurally rather than by a filter: narration goes to the [`Progress`]
    /// sink and the answer does not.
    pub quiet: bool,
}

impl Mode {
    /// Prose, with narration.
    #[must_use]
    pub const fn human() -> Self {
        Self {
            json: false,
            quiet: false,
        }
    }

    /// Prose, without narration.
    #[must_use]
    pub const fn quiet() -> Self {
        Self {
            json: false,
            quiet: true,
        }
    }

    /// JSON, with narration. Narration goes to the sink, never to stdout: a consumer reading
    /// standard output must get nothing but JSON.
    #[must_use]
    pub const fn json() -> Self {
        Self {
            json: true,
            quiet: false,
        }
    }

    /// Whether this mode is JSON. Named because `Mode::json()` reads as a constructor and the
    /// field is what the renderer asks.
    #[must_use]
    pub const fn is_json(self) -> bool {
        self.json
    }

    /// Whether this mode suppresses narration.
    #[must_use]
    pub const fn is_quiet(self) -> bool {
        self.quiet
    }
}

/// Everything a command produced, whatever the outcome.
///
/// One value, both modes. The human mode is a formatting of it and the JSON mode is a
/// serialisation of it, so the two cannot disagree about what happened — which is the CLI's share
/// of contract D2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Output {
    /// The engine version that answered, so an answer can be traced to a build.
    pub version: String,
    /// The command that ran.
    pub command: String,
    /// The status, and therefore the exit code.
    pub status: Status,
    /// The code, repeated so a JSON consumer does not have to know the mapping.
    pub exit_code: u8,
    /// The repository that was read.
    pub root: String,
    /// The index that was read.
    pub index_path: String,
    /// Whether the index existed before this command ran. `Store::open` creates one, so a command
    /// that opened a store may have made the index it is now reporting on — which is a fact the
    /// caller needs and cannot otherwise infer.
    pub index_existed: bool,
    /// The answer, in full.
    pub answer: Answer,
    /// The refusal, when there is one. `None` on a clean success, and **not** an error: a `doctor`
    /// that found a failing check exits non-zero and still carries its whole diagnosis here.
    pub refusal: Option<Refusal>,
    /// Every progress line, whether or not the sink printed any of them.
    ///
    /// Complete in both modes. `--quiet` silences the sink, never this list, so the JSON mode
    /// carries the same narration whether or not the caller asked for it to be quiet.
    pub progress: Vec<String>,
}

impl Output {
    /// The human rendering: the answer, then the refusal if there is one.
    ///
    /// A refusal is printed **after** the answer rather than instead of it, because an agent that
    /// ran `peek doctor` needs the findings even when the command exits non-zero, and a caller that
    /// ran `peek context` with too small a budget needs the pack that explains the refusal.
    #[must_use]
    pub fn render(&self) -> String {
        let mut text = self.answer.render();
        if let Some(refusal) = &self.refusal {
            text.push_str(&format!(
                "\n\npeek {}: {}\n{}",
                self.command,
                refusal.kind,
                refusal.render()
            ));
        }
        text
    }

    /// The JSON rendering.
    ///
    /// A serialisation failure produces a hand-written object rather than nothing at all: a caller
    /// that received no output could not distinguish "the answer was empty" from "the answer could
    /// not be written", and the second is worth reporting loudly.
    #[must_use]
    pub fn render_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|error| {
            let command = serde_json::to_string(&self.command)
                .unwrap_or_else(|_| "\"unknown\"".to_owned());
            format!(
                "{{\"command\":{command},\"status\":\"failed\",\"exit_code\":1,\
                 \"refusal\":{{\"kind\":\"engine\",\"message\":{}}}}}",
                serde_json::to_string(&format!(
                    "the answer could not be serialised: {error}"
                ))
                .unwrap_or_else(|_| "\"unserialisable\"".to_owned())
            )
        })
    }
}

/// Run a command.
///
/// The single entry point the binary and the tests both go through. `progress` receives narration;
/// pass [`Silent`] to discard it, or [`Collecting`] to inspect it. The lines are recorded into
/// [`Output::progress`] whatever the sink does with them.
pub fn run(invocation: &Invocation, progress: &mut dyn Progress) -> Result<Output, Failure> {
    let command = invocation.command.clone();
    let mut recording = progress::Recording::new(progress);
    let (outcome, root, index_path, index_existed) = match &command {
        Command::Help | Command::Version => (
            commands::run(&command, None, &mut recording)?,
            String::new(),
            String::new(),
            false,
        ),
        _ => {
            let given: &Path = command
                .root()
                .map_or(Path::new("."), |root| root.as_path());
            let location = paths::locate(given, command.name())?;
            let outcome = commands::run(&command, Some(&location), &mut recording)?;
            (
                outcome,
                location.root_text(),
                location.index_text(),
                location.index_existed,
            )
        }
    };
    let narration = recording.into_lines();
    Ok(Output {
        version: peek_core::VERSION.to_owned(),
        command: command.name().to_owned(),
        status: outcome.status,
        exit_code: outcome.status.exit_code(),
        root,
        index_path,
        index_existed,
        answer: outcome.answer,
        refusal: outcome.refusal,
        progress: narration,
    })
}

/// Render a command's result, in whichever mode was asked for.
#[must_use]
pub fn render(output: &Output, mode: &Mode) -> String {
    if mode.is_json() {
        output.render_json()
    } else {
        output.render()
    }
}
