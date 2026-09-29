//! What the stdout guards scan, and how they read it.
//!
//! # Why one module holds the scan
//!
//! Two claims are made about the same property in two files: that nothing in this crate writes to
//! the process's stdout, and that only the binary holds a handle on it. A scan written twice is a
//! scan that can be tightened in one file and not the other, so the file list and the scan live
//! here and both files call them.
//!
//! # Why this scan reads code rather than text
//!
//! A text search over source has two failure modes and they are opposites. It matches the words
//! `io::stdout` in a sentence explaining that nothing calls it, which makes the guard red for a
//! change that broke nothing; and it fails to match `let _ = println!(..)`, which makes the guard
//! green for a change that broke everything. Both come from the same mistake — looking at the file
//! rather than at the program.
//!
//! So [`code_only`] reduces a source file to its **code** first: every comment removed, and every
//! string, raw string and character literal dropped to nothing but its delimiters. The scan then
//! looks for whole identifiers followed by `!` or naming `stdout`. That has two consequences worth
//! stating, because they are the whole point of doing it this way:
//!
//! * **Prose cannot satisfy it.** A doc comment, a line comment, a nested block comment, or the
//!   text of a string literal cannot produce an identifier, so no amount of documentation about
//!   stdout — or about `println!` — can turn either guard red or green.
//! * **A real call cannot escape it.** The macros are matched as identifiers followed by `!`, not
//!   as a prefix at the start of a line, so `let _ = println!(..)`, `std::println!(..)` and a
//!   `writeln!(io::stdout(), ..)` are all found wherever they are written, and the identifier
//!   comparison is exact, so `eprintln!` — stderr, which is this crate's diagnostic channel — is
//!   not a false positive.
//!
//! # What the scan cannot see
//!
//! Bytes written by a *dependency*, or by code reached through a macro, are outside it. That is
//! stated rather than hidden: the subprocess run in `transport.rs` is the net for the paths it
//! drives, and no source scan can be a net for a crate this one does not contain.

// This module is compiled into more than one test binary and each uses a different part of it, so
// a caller that needs only the print scan would get a dead-code warning for the stdout scan. The
// alternative is three copies of the scan, which is the thing this file exists to stop.
#![allow(dead_code)]

/// The printing macros whose invocation writes to stdout. `eprintln!` and `eprint!` are absent on
/// purpose: stderr is the diagnostic channel, and `session::Log` is its correct spelling.
const PRINT_MACROS: [&str; 3] = ["print", "println", "dbg"];

/// Every source file in the crate, as `(path, contents)`.
///
/// Listed rather than discovered, so the scan is over what is in the commit rather than over
/// whatever happens to be on disk when the test runs. A new source file is a new line here, which
/// is the point: a file nobody listed is a file nobody scanned.
pub fn sources() -> Vec<(&'static str, &'static str)> {
    vec![
        ("src/lib.rs", include_str!("../../src/lib.rs")),
        ("src/main.rs", include_str!("../../src/main.rs")),
        ("src/outcome.rs", include_str!("../../src/outcome.rs")),
        ("src/params.rs", include_str!("../../src/params.rs")),
        ("src/protocol.rs", include_str!("../../src/protocol.rs")),
        ("src/server.rs", include_str!("../../src/server.rs")),
        ("src/session.rs", include_str!("../../src/session.rs")),
        ("src/tool.rs", include_str!("../../src/tool.rs")),
        ("src/writer.rs", include_str!("../../src/writer.rs")),
        ("src/tools/mod.rs", include_str!("../../src/tools/mod.rs")),
        ("src/tools/context.rs", include_str!("../../src/tools/context.rs")),
        ("src/tools/doctor.rs", include_str!("../../src/tools/doctor.rs")),
        ("src/tools/explain.rs", include_str!("../../src/tools/explain.rs")),
        ("src/tools/index.rs", include_str!("../../src/tools/index.rs")),
        ("src/tools/target.rs", include_str!("../../src/tools/target.rs")),
        ("src/tools/walk.rs", include_str!("../../src/tools/walk.rs")),
        ("src/tools/watch.rs", include_str!("../../src/tools/watch.rs")),
    ]
}

/// One line of a file whose code writes to the process's stdout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offence {
    /// The one-based line in the file as committed, counting comments as lines.
    pub line: usize,
    /// The line with its comments and its literal contents removed, so a failure message shows the
    /// code rather than the sentence around it.
    pub code: String,
}

/// Every line of `source` whose code calls one of the printing macros.
///
/// The positive form of "this crate does not print", and the check a developer expects to fail
/// first: a banner in `main.rs`, a debug line in a handler, a stray `dbg!` in a loop.
pub fn prints_to_stdout(source: &str) -> Vec<Offence> {
    offences(source, |line| calls_print_macro(line))
}

/// Every line of `source` whose code names the process's stdout at all.
///
/// Holding a handle and printing to it are different acts, and the second is a subset of the first
/// for every spelling in the standard library — `io::stdout()`, `std::io::stdout()`, a `use` of
/// either, a `writeln!` to the result. Matching the identifier catches all of them and cannot be
/// satisfied by a file that merely mentions the word.
pub fn names_stdout(source: &str) -> Vec<Offence> {
    offences(source, |line| has_identifier(line, "stdout"))
}

/// Every line of `source` whose code would put bytes on the protocol channel.
///
/// The union of the two, because that is the question the rule is actually about: either a macro
/// that prints, or a route to the process's standard output. A line that does both is reported
/// once.
pub fn writes_to_stdout(source: &str) -> Vec<Offence> {
    offences(source, |line| calls_print_macro(line) || has_identifier(line, "stdout"))
}

/// Whether [`code_only`] read the whole of `source`.
///
/// A lexer that lost the tail of a file would report it clean, which is the worst failure this one
/// can have: a guard that goes quiet over the half of a file it did not understand. So every scan
/// is paired with this, and a file whose line count does not survive the round trip fails the test
/// rather than passing it.
///
/// The count is all that is checked, because that is the property that matters and the only one
/// that can be checked without writing a Rust parser: a literal that swallowed the rest of the
/// file loses lines, and a correctly-read file loses none.
pub fn scanned_every_line(source: &str) -> bool {
    code_only(source).lines().count() == source.lines().count()
}

/// The lines of `source` that satisfy `is_an_offence`, with their comments already removed.
fn offences(source: &str, is_an_offence: fn(&str) -> bool) -> Vec<Offence> {
    code_only(source)
        .lines()
        .enumerate()
        .filter(|(_, line)| is_an_offence(line))
        .map(|(number, line)| Offence {
            line: number + 1,
            code: line.trim().to_owned(),
        })
        .collect()
}

/// Whether `line` calls one of [`PRINT_MACROS`].
///
/// The macro name is matched as a whole identifier followed by `!`, never as a prefix at the start
/// of a line, so the call is found wherever it is written and `eprintln!` is not mistaken for
/// `println!`.
fn calls_print_macro(line: &str) -> bool {
    let bytes = line.as_bytes();
    identifiers(line).any(|(start, word)| {
        PRINT_MACROS.contains(&word)
            && bytes.get(start + word.len()) == Some(&b'!')
            // `print != x` is a comparison between two identifiers, not an invocation.
            && bytes.get(start + word.len() + 1) != Some(&b'=')
    })
}

/// Whether `line` contains `name` as a whole identifier.
fn has_identifier(line: &str, name: &str) -> bool {
    identifiers(line).any(|(_, word)| word == name)
}

/// Every identifier in `line`, as `(byte offset, spelling)`.
fn identifiers(line: &str) -> Vec<(usize, &str)> {
    let bytes = line.as_bytes();
    let mut found = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if !is_identifier_start(bytes[index]) {
            index += 1;
            continue;
        }
        let start = index;
        index += 1;
        while index < bytes.len() && is_identifier_continue(bytes[index]) {
            index += 1;
        }
        found.push((start, &line[start..index]));
    }
    found
}

fn is_identifier_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

fn is_identifier_continue(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// The source with every comment removed and every literal's contents dropped.
///
/// The result is the same file with everything a search could mistake for code taken out: a line
/// of it is the code on the corresponding line of the file, so an [`Offence`] can point at a line
/// a reader can open, and a literal that has been emptied is a pair of quotes with nothing inside.
///
/// A small lexer rather than a regular expression, because the four things a search must not be
/// fooled by are all lexical: a nested block comment, a `//` inside a string, a lifetime, and an
/// apostrophe in prose.
pub fn code_only(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut code: Vec<u8> = Vec::with_capacity(source.len());
    let mut index = 0;
    while index < bytes.len() {
        if let Some(end) = literal_end(bytes, index, &mut code) {
            index = end;
            continue;
        }
        if bytes[index..].starts_with(b"//") {
            while index < bytes.len() && bytes[index] != b'\n' {
                index += 1;
            }
            continue;
        }
        if bytes[index..].starts_with(b"/*") {
            index = block_comment_end(bytes, index, &mut code);
            continue;
        }
        code.push(bytes[index]);
        index += 1;
    }
    // Assembled from bytes rather than pushed a character at a time, so a multi-byte character in
    // the source survives as itself instead of being taken apart. Lossy for the rest: a file that
    // is not UTF-8 is a file this crate cannot be built from in the first place.
    String::from_utf8_lossy(&code).into_owned()
}

/// The end of the literal starting at `index`, or `None` when no literal starts there.
///
/// `code` is where the newlines the literal swallows are put back, so that a line number in the
/// result is still a line number in the file.
fn literal_end(bytes: &[u8], index: usize, code: &mut Vec<u8>) -> Option<usize> {
    // A prefix may only begin an identifier, so the `b` of `sub` is not a byte-string marker.
    if index > 0 && is_identifier_continue(bytes[index - 1]) {
        return None;
    }
    let mut cursor = index;
    let mut raw = false;
    if bytes.get(cursor) == Some(&b'b') {
        cursor += 1;
    }
    if bytes.get(cursor) == Some(&b'r') {
        raw = true;
        cursor += 1;
    }
    let mut hashes = 0;
    while bytes.get(cursor) == Some(&b'#') {
        hashes += 1;
        cursor += 1;
    }
    match bytes.get(cursor).copied() {
        Some(quote) if quote == b'"' => {
            Some(close_quoted(bytes, cursor + 1, quote, hashes, raw, code))
        }
        Some(b'\'') if !raw => char_literal_end(bytes, cursor + 1),
        _ => None,
    }
}

/// The end of a `"`-delimited literal, whose body starts at `body`.
///
/// A plain string literal may hold bare newlines — this crate writes its usage text and its tool
/// descriptions that way — so the scan runs to the closing quote rather than to the end of the
/// line. The newlines it passes over go back into `code`, which is what keeps an [`Offence`]'s
/// line number a line number in the file rather than a line number in a truncated copy of it.
fn close_quoted(
    bytes: &[u8],
    body: usize,
    quote: u8,
    hashes: usize,
    raw: bool,
    code: &mut Vec<u8>,
) -> usize {
    let mut index = body;
    while index < bytes.len() {
        if raw {
            // A raw literal ends at its quote followed by exactly as many `#` as it opened with.
            if bytes[index] == quote && hashes_after(bytes, index + 1) == hashes {
                return index + 1 + hashes;
            }
        } else if bytes[index] == b'\\' {
            // The escaped byte, whatever it is. A backslash before a newline is Rust's line
            // continuation, and the newline it swallows is put back with everything else.
            if bytes.get(index + 1) == Some(&b'\n') {
                code.push(b'\n');
            }
            index += 2;
            continue;
        } else if bytes[index] == quote {
            return index + 1;
        }
        if bytes[index] == b'\n' {
            code.push(b'\n');
        }
        index += 1;
    }
    bytes.len()
}

/// The end of a character literal, or `None` when the apostrophe was a lifetime.
///
/// A lifetime is the reason this is not a one-line rule: `&'static str` is far more common in this
/// crate than `'\n'` is, and a lexer that reads every apostrophe as the start of a literal deletes
/// the rest of the file. A character literal is an escape or exactly one character between two
/// apostrophes, and nothing else qualifies.
fn char_literal_end(bytes: &[u8], body: usize) -> Option<usize> {
    match bytes.get(body) {
        Some(b'\\') => {
            // The longest escape is four bytes, so a closing apostrophe further away than that
            // belongs to a later lifetime.
            let limit = body.saturating_add(10).min(bytes.len());
            (body + 1..limit).find(|index| bytes[*index] == b'\'').map(|index| index + 1)
        }
        Some(_) if bytes.get(body + 1) == Some(&b'\'') => Some(body + 2),
        _ => None,
    }
}

/// The number of `#` immediately after `index`.
fn hashes_after(bytes: &[u8], index: usize) -> usize {
    if index > bytes.len() {
        return 0;
    }
    bytes[index..]
        .iter()
        .take_while(|byte| **byte == b'#')
        .count()
}

/// The end of the block comment starting at `index`, counting the nesting Rust allows.
///
/// The newlines inside it go back into `code`, so a block comment in the middle of a file does not
/// shift every line number after it.
fn block_comment_end(bytes: &[u8], index: usize, code: &mut Vec<u8>) -> usize {
    let mut depth = 0usize;
    let mut cursor = index;
    while cursor < bytes.len() {
        if bytes[cursor..].starts_with(b"/*") {
            depth += 1;
            cursor += 2;
            continue;
        }
        if bytes[cursor..].starts_with(b"*/") {
            depth -= 1;
            cursor += 2;
            if depth == 0 {
                return cursor;
            }
            continue;
        }
        if bytes[cursor] == b'\n' {
            code.push(b'\n');
        }
        cursor += 1;
    }
    bytes.len()
}
