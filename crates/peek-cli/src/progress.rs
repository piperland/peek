//! Where progress narration goes.
//!
//! # Why a sink and not a flag
//!
//! The central rule of this program is that a command states what it could not do. Progress
//! narration is the one thing that is *not* a finding: it is the sound a tool makes while it
//! works, and a caller that asked for JSON does not want it interleaved with the answer.
//!
//! So narration goes through a [`Progress`] sink, the binary sends that sink to standard error, and
//! `--quiet` is a property of the sink rather than of the rendering. The consequence is structural
//! rather than promised: a command cannot suppress a finding by suppressing progress, because a
//! finding is never a progress line in the first place.
//!
//! # The guarantee this buys
//!
//! `Output::progress` records every line whether or not the sink printed it, so the JSON mode
//! always carries the complete narration. And a run with `--quiet` and a run without it produce
//! **byte-identical answers** — which is a testable claim, not a promise.

/// Somewhere for narration to go.
pub trait Progress {
    /// One line of narration. Never a finding, never an answer, never a refusal.
    fn note(&mut self, line: String);
}

/// A sink that discards everything.
///
/// The default for a library caller and for every test: it makes `run` pure with respect to
/// standard error.
#[derive(Debug, Clone, Copy, Default)]
pub struct Silent;

impl Progress for Silent {
    fn note(&mut self, _line: String) {}
}

/// A sink that keeps the lines, so a test can assert on them.
#[derive(Debug, Clone, Default)]
pub struct Collecting {
    /// Everything that was noted, in order.
    pub lines: Vec<String>,
}

impl Collecting {
    /// An empty sink.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The lines joined by newlines, for a substring assertion.
    #[must_use]
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }
}

impl Progress for Collecting {
    fn note(&mut self, line: String) {
        self.lines.push(line);
    }
}

/// Forwards to another sink while keeping a copy.
///
/// This is what [`crate::run`] wraps the caller's sink in, so `Output::progress` is complete
/// whatever the caller's sink chose to do with the lines.
pub struct Recording<'a> {
    inner: &'a mut dyn Progress,
    lines: Vec<String>,
}

impl<'a> Recording<'a> {
    /// Wrap a sink.
    #[must_use]
    pub fn new(inner: &'a mut dyn Progress) -> Self {
        Self {
            inner,
            lines: Vec::new(),
        }
    }

    /// The lines recorded, and the wrapper consumed.
    #[must_use]
    pub fn into_lines(self) -> Vec<String> {
        self.lines
    }
}

impl Progress for Recording<'_> {
    fn note(&mut self, line: String) {
        self.lines.push(line.clone());
        self.inner.note(line);
    }
}
