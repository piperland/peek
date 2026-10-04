//! The service layer, and every call shape the gate scores.

use crate::model::{entry, Entry};
use crate::report::Sink;
use crate::traits::{Clock, Saveable, Ticks};

/// A handle onto something that needs a callback.
pub struct Runner {
    /// How many attempts have been made.
    pub attempts: u8,
}

impl Runner {
    /// Runs `body` up to `limit` times.
    ///
    /// `body` is called through a parameter, so the call site has no static
    /// target: the gate scores this as a dynamic call and says so rather than
    /// counting it as a resolved edge.
    pub fn run(&self, body: impl Fn() -> usize, limit: u8) -> usize {
        let mut last = 0;
        for attempt in 0..limit {
            last = body();
            if last > 0 {
                break;
            }
            let _ = attempt;
        }
        retry(last)
    }
}

/// Formats one line. **Declared again in `report`**, with a different signature.
pub fn format_line(tag: &str, count: usize) -> String {
    let mut sink = Sink {
        text: String::new(),
    };
    sink.push(tag);
    sink.text.push_str(&count.to_string());
    sink.text
}

/// Calls `format_line` again, so the same-name pair has a caller in this file.
pub fn describe(entry: &Entry) -> String {
    format_line(&entry.label, entry.count as usize)
}

/// Retries `last` once more.
pub fn retry(last: usize) -> usize {
    last + 1
}

/// Runs `body` with a clock, so a trait method call through a concrete receiver
/// and the same call through a trait object can both be labelled.
pub fn with_clock(body: impl FnOnce() -> usize) -> usize {
    let mut ticks = Ticks {
        origin: 7,
    };
    let saved = ticks.save(&entry("tick", 1));
    let _ = saved;
    let _ = Clock::now(&ticks);
    let _ = Sink { text: String::new() }.text;
    body()
}