//! Four tests that state the guarantees the server claims, written so that a failure names the
//! claim rather than the symptom.
//!
//! Each one states a guarantee the crate's own documentation makes in prose. They belong beside the
//! MCP server, not in the engine: they drive the same dispatch loop the other protocol tests drive,
//! and they need the crate to exist to compile.
//!
//! - `a_cancelled_request_id_can_be_reused_by_the_client` — a cancellation names one request, not
//!   one number.
//! - `only_the_binary_names_the_processs_stdout` — one file holds the process's stdout, and it is
//!   the binary.
//! - `the_stdout_scan_fails_a_print_that_is_not_at_the_start_of_a_line` — the source scan prevents
//!   a stray print.
//! - `an_oversized_request_is_answered_on_the_id_it_carried` — the line limit is a bound, and an
//!   over-long request is still answered.

// `expect` and `panic` are denied workspace-wide. An integration test is a separate crate and does
// not inherit the exemption the engine's own unit tests get, so it is exempted here instead, with
// the same justification: a test that fails inside an `expect` has already failed, and a message
// naming what went wrong is worth more than a panic location.
#![allow(clippy::expect_used, clippy::panic)]

use std::io::Cursor;
use std::path::{Path, PathBuf};

use peek_mcp::server::{MAX_LINE_BYTES, serve};
use peek_mcp::session::{Session, SharedLog};
use peek_mcp::writer::ProtocolWriter;
use serde_json::{Value, json};

mod common;

use common::writes_to_stdout;

// ---------------------------------------------------------------------------
// A cancellation is about one request, not about a number
// ---------------------------------------------------------------------------

#[test]
fn a_cancelled_request_id_can_be_reused_by_the_client() {
    // JSON-RPC puts no constraint on request ids beyond "unique among the requests a client has
    // outstanding". A client that cancels, receives the refusal, and then issues a *new* request
    // that happens to pick the same number is a conforming client, and the second request has
    // nothing to do with the first.
    //
    // The specification lets the far end ignore a cancellation naming a request it does not
    // recognise. It does not ask a server to remember the number for ever, and it says nothing
    // about refusing an unrelated request that arrives later.
    let dir = TempDir::new("cancel-reuse");
    let session_bytes = drive(
        dir.path(),
        &[
            notification("notifications/cancelled", json!({ "requestId": 7 })),
            request(7, "ping", json!({})),
            request(7, "ping", json!({})),
        ],
    );
    let replies = replies(&session_bytes);

    assert_eq!(replies.len(), 2, "{replies:?}");
    assert!(
        replies[0].get("error").is_some(),
        "the first request is the one that was cancelled, so refusing it is right: {replies:?}"
    );
    assert_eq!(
        replies[1]["id"], 7,
        "the reply belongs to the request that asked for it"
    );
    assert!(
        replies[1].get("result").is_some(),
        "the second request with the same id is a different request, and a client is free to reuse \
         the number once the first has been answered. Refusing it means one cancellation poisons \
         that id for the rest of the session: {replies:?}"
    );
}

// ---------------------------------------------------------------------------
// One file holds the process's stdout
// ---------------------------------------------------------------------------

#[test]
fn only_the_binary_names_the_processs_stdout() {
    // The claim is that the library never names the process's stdout, so a tool handler has nothing
    // to print to even by accident. The check is on the raw text rather than on the code, which is
    // the point: a module comment that spells the call out while explaining that nothing makes it
    // is exactly what made a file-based guard unreliable, and the crate's own documentation is not
    // exempt from the rule it states.
    let holders: Vec<&str> = common::sources()
        .into_iter()
        .filter(|(_, source)| source.contains("io::stdout"))
        .map(|(name, _)| name)
        .collect();

    assert_eq!(
        holders,
        vec!["src/main.rs"],
        "only the binary may name the process's stdout, and it hands that straight to the protocol \
         writer. Any other file naming it is either a call that has to go or a comment that makes \
         the file-based guard unreliable: {holders:?}"
    );
}

// ---------------------------------------------------------------------------
// The source scan is a prefix check
// ---------------------------------------------------------------------------

#[test]
fn the_stdout_scan_fails_a_print_that_is_not_at_the_start_of_a_line() {
    // A guard described as the thing that "prevents the regression" has to fail on a `println!`
    // wherever it appears. A rule that matches a macro only at the start of a line is a rule
    // against one style of mistake: it misses a discarded result, a qualified path, and a
    // `write!` to a handle, and each of those really does put bytes on the protocol channel.
    //
    // This drives the scan the crate's own tests use rather than a restatement of it, so a fix
    // that loosens the scan cannot leave this test green.
    let offenders: [&str; 4] = [
        r#"    println!("peek-mcp: ready");"#,
        r#"    let _ = println!("peek-mcp: ready");"#,
        r#"    std::println!("peek-mcp: ready");"#,
        r#"    writeln!(io::stdout(), "peek-mcp: ready").ok();"#,
    ];
    let mut missed: Vec<&str> = Vec::new();
    for line in offenders {
        if writes_to_stdout(line).is_empty() {
            missed.push(line);
        }
    }

    assert!(
        missed.is_empty(),
        "the source scan missed a print written in these forms: {missed:?}. A guard that matches a \
         macro only at the start of a line is a guard against one style of mistake"
    );
}

// ---------------------------------------------------------------------------
// The line limit
// ---------------------------------------------------------------------------

#[test]
fn an_oversized_request_is_answered_on_the_id_it_carried() {
    // The limit is a bound on memory, and refusing an over-long line is right. Two things are
    // wrong with how it used to be refused.
    //
    // The line used to be read in full before its length was looked at, so the allocation the limit
    // is supposed to prevent had already happened by the time anything was refused.
    //
    // And the refusal used to be sent with a null id. The message that arrived carried `id: 42`, so
    // the client is owed an answer addressed to 42; instead it got an error it cannot match to
    // anything, and the reply to its request never comes.
    let dir = TempDir::new("oversize-id");
    let padding = "a".repeat(MAX_LINE_BYTES + 1);
    let arguments = json!({ "name": "index_status", "arguments": { "padding": padding } });
    let session_bytes = drive(
        dir.path(),
        &[
            request(42, "tools/call", arguments),
            request(1, "ping", json!({})),
        ],
    );
    let replies = replies(&session_bytes);

    assert_eq!(replies.len(), 2, "{replies:?}");
    let refusal = &replies[0];
    assert_eq!(
        refusal["error"]["code"], -32600,
        "an over-long line is an invalid request: {refusal}"
    );
    assert_eq!(
        refusal["id"], 42,
        "the refusal is owed to the request that was refused. Answering with a null id leaves the \
         client waiting for a reply to 42 that is never coming: {refusal}"
    );
    assert_eq!(replies[1]["id"], 1, "and the session is still in step");
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// A directory that removes itself, so a failing test leaves nothing behind.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        // The label plus the process id: two tests in one binary must not share a directory, and a
        // stale one from a previous run must not be reused.
        let name = format!("peek-mcp-audit-{label}-{}", std::process::id());
        let path = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create a temporary directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Send `messages` to a server over `root`, and return every byte it wrote.
fn drive(root: &Path, messages: &[Value]) -> Vec<u8> {
    let mut input = String::new();
    for message in messages {
        input.push_str(&serde_json::to_string(message).expect("encode a message"));
        input.push('\n');
    }
    let mut session = Session::new(root, Box::new(SharedLog::new()));
    let mut output = ProtocolWriter::new(Vec::new());
    serve(&mut session, Cursor::new(input.into_bytes()), &mut output).expect("serve one session");
    output.into_inner()
}

/// A request with an id.
fn request(id: u64, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

/// A notification, which has no id and must never be answered.
fn notification(method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "method": method, "params": params })
}

/// Every line the server wrote, parsed.
///
/// The parse is the assertion: a reply nobody parses is not a test, and a line that is not JSON is
/// the failure this whole file is downstream of.
fn replies(stdout: &[u8]) -> Vec<Value> {
    String::from_utf8(stdout.to_vec())
        .expect("the protocol stream is UTF-8")
        .lines()
        .map(|line| serde_json::from_str(line).expect("every protocol line is a JSON value"))
        .collect()
}
