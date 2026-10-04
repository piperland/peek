//! Rendering, and the import shapes the gate scores: a grouped import, a glob,
//! and a re-export.

pub use crate::model::Severity;
use crate::model::Alias as Renamed;
use crate::model::{entry, Entry};
use crate::traits::*;

/// Where a rendered report ends up.
pub struct Sink {
    /// The text written so far.
    pub text: String,
}

impl Sink {
    /// Appends a line.
    pub fn push(&mut self, line: &str) {
        self.text.push_str(line);
    }
}

/// Formats one line. **The same name is declared in `service`**, so a bare-name
/// lookup has two candidates.
pub fn format_line(tag: &str, count: usize) -> String {
    let owned = entry(tag, count as u32);
    format_line_inner(&owned)
}

fn format_line_inner(entry: &Entry) -> String {
    entry.label.clone()
}

/// Writes every entry into `out`.
///
/// `Sink` comes from the glob import, so this body depends on the glob having
/// resolved for the field access and the method call to land anywhere sensible.
pub fn render(entries: &[Entry], out: &mut String) -> String {
    let mut sink = Sink {
        text: String::new(),
    };
    for entry in entries {
        sink.push(&format_line(&entry.label, entry.count as usize));
    }
    let mut text = sink.text;
    text.push_str(&format_line("total", entries.len()));
    let mut clock = Ticks { origin: 0 };
    let _now = clock.now();
    let _receipt = clock.receipt();
    out.push_str(&text);
    text
}

/// Names a severity, for a call whose target is a variant rather than a value.
pub fn loud() -> Severity {
    Severity::Loud
}

/// Builds a value through an **aliased** import, so the gate has an alias to
/// resolve and not merely a name.
pub fn renamed() -> Renamed {
    entry("renamed", 1)
}