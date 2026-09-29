//! The real binary, over a real pipe, in a real subprocess.
//!
//! # Why this file exists
//!
//! `protocol.rs` proves the dispatch loop is clean when it is handed two in-memory streams. That is
//! a proof about the *code path*, and it is the stronger of the two claims — but it cannot catch a
//! `println!` in `main.rs`, because `main.rs` is not on that path, and `main.rs` is exactly where
//! one appears when somebody adds a startup banner.
//!
//! So this file drives the compiled binary over a pipe and asserts that every line it writes to
//! **stdout** is a JSON-RPC message, that the session did real work, and that the **stderr**
//! transcript is where the diagnostics went. Plus a source scan, which is the one that prevents the
//! regression rather than detecting it.
//!
//! # Cost
//!
//! One fixture, no network, no filesystem events, no sleeping. There is no timing in this file, so
//! there is nothing in it to be flaky; it stays on the default test path.

// `expect` and `panic` are denied workspace-wide, on the grounds that in production code they hide
// a real failure behind a panic. `peek-core`'s unit tests are exempted through that crate's
// `lib.rs`; an integration test is a separate crate and does not inherit that, so it is exempted
// here instead. The justification is the same one: a test that fails inside an `expect` has
// already failed, and a message naming what went wrong is worth more than a panic location.
#![allow(clippy::expect_used, clippy::panic)]

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, Command, Stdio};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use peek_mcp::Session;
use peek_mcp::session::SharedLog;

mod common;

use common::{Offence, names_stdout, prints_to_stdout, sources, writes_to_stdout};
// Every source file in the crate is listed in `common::sources`, together with the scan both
// stdout guards use. They live in one place because they are one claim, and a guard written twice
// is a guard that can be tightened in one file and not the other.

/// A child server, and the pipes it speaks through.
struct Server {
    child: Child,
    input: std::process::ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
    stderr: Arc<Mutex<String>>,
    /// The thread draining the child's stderr. Joined before the transcript is read, so the
    /// transcript is complete rather than merely written so far.
    stderr_thread: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    /// Start `peek-mcp` over `root`.
    fn start(root: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_peek-mcp"))
            .arg("--root")
            .arg(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("the binary under test is built before its integration tests run");

        let input = child.stdin.take().expect("stdin was piped");
        let stdout = BufReader::new(child.stdout.take().expect("stdout was piped"));
        let stderr = child.stderr.take().expect("stderr was piped");

        // Read stderr on its own thread. A pipe that nobody drains fills at a fixed size and blocks
        // the writer, so a session that logs a lot would otherwise hang — which would look like a
        // protocol failure rather than a test artefact.
        let transcript = Arc::new(Mutex::new(String::new()));
        let sink = Arc::clone(&transcript);
        let stderr_thread = std::thread::spawn(move || {
            let mut pipe: ChildStderr = stderr;
            let mut buffer = Vec::new();
            if pipe.read_to_end(&mut buffer).is_ok() {
                let mut slot = sink.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                slot.push_str(&String::from_utf8_lossy(&buffer));
            }
        });

        Self {
            child,
            input,
            stdout,
            stderr: transcript,
            stderr_thread: Some(stderr_thread),
        }
    }

    /// Send a `tools/call` request and read its reply.
    fn call(&mut self, id: u64, name: &str, arguments: Value) -> Value {
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": { "name": name, "arguments": arguments }
        }));
        self.read()
    }

    /// Send an arbitrary request and read its reply.
    fn send_and_read(&mut self, message: Value) -> Value {
        self.send(message);
        self.read()
    }

    fn send(&mut self, message: Value) {
        writeln!(self.input, "{message}").expect("write a request");
        self.input.flush().expect("flush the request");
    }

    /// Read one line of stdout, asserting it is a JSON-RPC message.
    ///
    /// **This is the stdout test.** Every byte the process writes to file descriptor 1 arrives
    /// here, and a line that does not parse is a corrupted session in the field. The assertion
    /// prints the offending line, because "parse error at line 1 column 34" does not.
    fn read(&mut self) -> Value {
        let mut line = String::new();
        let read = self
            .stdout
            .read_line(&mut line)
            .expect("read a line of the protocol stream");
        assert!(
            read > 0,
            "the server closed stdout before answering. stderr so far:\n{}",
            self.transcript()
        );
        serde_json::from_str(line.trim_end()).unwrap_or_else(|error| {
            panic!("a line on the protocol stream that is not JSON ({error}): {line:?}")
        })
    }

    fn transcript(&self) -> String {
        transcript_of(&self.stderr)
    }

    /// Close stdin, wait for the process, and return its stderr transcript.
    fn finish(self) -> String {
        // Taken apart rather than closed field by field, because closing stdin *moves* the handle
        // out of `self`, and a `self` with a field already moved out of it has no transcript left
        // to read afterwards. The parts this does not need are dropped where they stand.
        let Self {
            mut child,
            input,
            stderr,
            mut stderr_thread,
            ..
        } = self;
        drop(input);
        let status = child.wait().expect("wait for the server to exit");
        assert!(status.success(), "the server exited with {status}");
        if let Some(handle) = stderr_thread.take() {
            handle.join().expect("the stderr reader thread finished");
        }
        transcript_of(&stderr)
    }
}

/// The child's stderr, read out of the slot the reader thread writes into.
fn transcript_of(stderr: &Mutex<String>) -> String {
    stderr
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// A repository with two Rust files, removed when it goes out of scope.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "peek-mcp-subprocess-{label}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(path.join("src")).expect("create a temporary repository");
        std::fs::write(
            path.join("src/lib.rs"),
            "pub fn helper() -> u32 {\n    1\n}\n",
        )
        .expect("write a source file");
        std::fs::write(
            path.join("src/main.rs"),
            "pub fn entry() -> u32 {\n    helper()\n}\n",
        )
        .expect("write a source file");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // The index lives outside the repository, under the OS cache root, named for the
        // repository's identity. Asking the library where that is, rather than reaching for the
        // engine's paths module, keeps this file a test of the MCP surface and nothing else.
        // A throwaway session, used only for its answer about where the index lives.
        let session = Session::new(&self.0, Box::new(SharedLog::new()));
        if let Some(path) = session.index_path() {
            let _ = std::fs::remove_dir_all(path.parent().unwrap_or(path));
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn the_binary_speaks_only_protocol_on_stdout_and_diagnostics_on_stderr() {
    // The test that prevents the most damaging possible bug in an MCP server. A single stray
    // `println!` puts a non-JSON line into the middle of the stream and the client dies with a
    // parse error that names nothing.
    let repository = TempDir::new("session");
    let mut server = Server::start(repository.path());

    // A full session. Every `read` above asserts that the line it received is JSON, so each of
    // these steps is a check that stdout carried nothing else.
    let initialize = server.send_and_read(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": { "protocolVersion": "2025-06-18" }
    }));
    assert_eq!(initialize["result"]["serverInfo"]["name"], "peek");

    // A notification, which must produce no line at all. If one appeared, the next read below
    // would take it as the answer to the request after it, and the id assertion would fail.
    server.send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));

    let listed = server.send_and_read(json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }));
    assert_eq!(
        listed["result"]["tools"].as_array().map(Vec::len),
        Some(peek_mcp::tool::catalogue().len()),
        "the binary advertises the catalogue the library has"
    );

    let refused = server.call(3, "index_status", json!({}));
    assert_eq!(
        refused["result"]["structuredContent"]["outcome"], "not_indexed",
        "a repository that has never been indexed says so: {refused}"
    );
    assert_eq!(
        refused["result"]["isError"],
        json!(false),
        "and it is an answer the model can act on, not an errored call: {refused}"
    );

    let built = server.call(4, "index", json!({ "mode": "full" }));
    assert_eq!(
        built["result"]["structuredContent"]["report"]["states_partition"],
        json!(true),
        "the build partitioned: {built}"
    );

    let status = server.call(5, "index_status", json!({}));
    assert_eq!(
        status["result"]["structuredContent"]["states_partition"],
        json!(true),
        "and so did the index: {status}"
    );

    let pack = server.call(
        6,
        "context",
        json!({ "target": "entry", "budget_tokens": 4000 }),
    );
    let content = &pack["result"]["content"][0];
    assert_eq!(content["type"], "text", "a text block is present: {pack}");
    assert!(
        content["text"]
            .as_str()
            .is_some_and(|text| !text.is_empty()),
        "and it says something: {pack}"
    );
    let structured = &pack["result"]["structuredContent"];
    assert!(
        structured["pack"]["units"].is_array(),
        "the pack is structured as well as rendered: {pack}"
    );
    assert!(
        structured["pack"]["budget"]["spent_tokens"]
            .as_u64()
            .is_some_and(|spent| spent <= 4000),
        "the budget is a ceiling over a real pipe too: {pack}"
    );

    // A malformed call, to prove the error path also writes nothing but JSON.
    let unknown = server.send_and_read(json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": "no/such/method"
    }));
    assert_eq!(unknown["error"]["code"], -32601);

    let ping = server.send_and_read(json!({ "jsonrpc": "2.0", "id": 8, "method": "ping" }));
    assert_eq!(
        ping["id"], 8,
        "the stream is still in step after the refusal: {ping}"
    );

    let stderr = server.finish();
    assert!(
        !stderr.trim().is_empty(),
        "the session did real work, so the diagnostics went somewhere; stderr is empty, which means \
         they went to stdout and the JSON above would not have parsed"
    );
    assert!(
        stderr.contains("session open"),
        "and the transcript says what the server was pointed at: {stderr}"
    );
}

#[test]
fn the_binary_writes_its_help_to_stderr_so_stdout_stays_a_protocol_channel() {
    // `--help` is the one place a person rather than a client is the caller, and it still goes to
    // stderr: an exception to the stdout rule is how the rule dies.
    let output = Command::new(env!("CARGO_BIN_EXE_peek-mcp"))
        .arg("--help")
        .output()
        .expect("run the binary");
    assert!(output.status.success());
    assert!(
        output.stdout.is_empty(),
        "`--help` wrote {} bytes to the protocol channel: {:?}",
        output.stdout.len(),
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("USAGE"),
        "the usage text is on stderr: {stderr}"
    );
    assert!(
        stderr.contains("context"),
        "and it lists the tools, so a person can see the surface: {stderr}"
    );
}

#[test]
fn the_binary_refuses_an_option_it_does_not_take() {
    // The same rule the tools follow: a misspelt flag that is ignored is a session that behaves
    // differently from the one that was asked for.
    let output = Command::new(env!("CARGO_BIN_EXE_peek-mcp"))
        .arg("--reposiory")
        .arg(".")
        .output()
        .expect("run the binary");
    assert_eq!(
        output.status.code(),
        Some(2),
        "a bad option is a non-zero exit, not a session that runs anyway"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--reposiory"),
        "the refusal quotes what was sent: {stderr}"
    );
    assert!(
        output.stdout.is_empty(),
        "and stdout is untouched: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn no_source_file_in_the_crate_prints_to_stdout() {
    // The regression guard, rather than the detection. `protocol.rs` proves the dispatch loop is
    // clean; this fails the moment somebody writes a banner into `main.rs` or a debug line into a
    // handler. It is a plain string search over sources compiled into the test, so it cannot itself
    // be broken by a build configuration.
    //
    // The search is over the file's **code**, not its text: comments and the contents of string
    // literals are removed first, and what is left is searched for a printing macro called as a
    // whole identifier. That is what lets this test say something it means — a sentence about
    // `println!` in a doc comment is not a print, and a `println!` after `let _ = ` is.
    //
    // Only stdout is scanned. stderr is the diagnostic channel and `eprintln!` is its correct
    // spelling, so scanning for it would be scanning for the thing the rule asks for.
    for (name, source) in sources() {
        assert!(
            common::scanned_every_line(source),
            "{name} did not survive the round trip through the scanner, so the scan below would be \
             reporting on part of it. A guard that goes quiet over the half it did not read is \
             worse than one that is red"
        );
        assert_eq!(
            prints_to_stdout(source),
            Vec::<Offence>::new(),
            "{name} writes to stdout. stdout is the protocol channel; use `session::Log` for \
             diagnostics."
        );
    }
}

#[test]
fn only_the_binary_holds_the_processs_stdout() {
    // The positive form of the same rule: not "it does not print" but "it cannot". The library
    // never names the process's standard output, so a tool handler has nothing to print to even by
    // accident. Every spelling counts — `io::stdout()`, `std::io::stdout()`, a `use` of either, a
    // `writeln!` to the result — because the scan matches the identifier rather than one of them.
    let holders: Vec<&str> = sources()
        .into_iter()
        .filter(|(_, source)| !names_stdout(source).is_empty())
        .map(|(name, _)| name)
        .collect();
    assert_eq!(
        holders,
        vec!["src/main.rs"],
        "only the binary may hold the process's stdout, and it hands that straight to the protocol \
         writer: {holders:?}"
    );
}

#[test]
fn the_stdout_scan_reads_code_and_cannot_be_satisfied_by_prose() {
    // The scan is a lexer over the file, not a search through it, and this is the test for the
    // difference. Every input below is a sentence *about* stdout or about a printing macro,
    // written the way this crate's own documentation writes them, and not one of them may be
    // reported: a guard that a paragraph of documentation can switch off is worse than the one it
    // replaced.
    let prose = vec![
        (
            "a line comment",
            "//! Nothing here names the process's standard output, or calls a printing macro.\n",
        ),
        (
            "a nested block comment",
            "/* outer /* inner */ and still says println! in prose */\n",
        ),
        (
            "a string literal",
            "let note = \"text naming a printing macro, and a // that is not a comment\";\n",
        ),
        (
            "a raw string",
            "let note = r#\"text naming io::stdout() in prose\"#;\n",
        ),
        (
            "a lifetime",
            "let note: &'static str = \"text naming a printing macro\";\n",
        ),
        ("a character literal", "let brace = '{';\n"),
    ];
    for (label, source) in prose {
        assert_eq!(
            writes_to_stdout(source),
            Vec::<Offence>::new(),
            "prose must not be able to satisfy or break the scan: {label}"
        );
    }

    // And the other direction: a real call is found wherever it is written, not only at the start
    // of a line. Each of these really does put bytes on the protocol channel.
    for (label, source) in [
        ("a bare call", "    println!(\"ready\");\n"),
        ("a discarded call", "    let _ = println!(\"ready\");\n"),
        ("a qualified call", "    std::println!(\"ready\");\n"),
        (
            "a write to a handle",
            "    writeln!(io::stdout(), \"ready\").ok();\n",
        ),
        ("a bare handle", "    let out = std::io::stdout();\n"),
        ("a use of the module", "use std::io::stdout;\n"),
    ] {
        assert_eq!(
            writes_to_stdout(source).len(),
            1,
            "a real call cannot escape the scan: {label}"
        );
    }
}

#[test]
fn the_binary_never_opens_a_socket() {
    // Contract M1 and M3. Stdio has no address to bind, so this is a structural claim rather than
    // a policy one, and the cheapest way to keep it structural is to assert that nothing in the
    // crate reaches for a network API.
    for (name, source) in sources() {
        for forbidden in [
            "TcpListener",
            "TcpStream",
            "UdpSocket",
            "reqwest",
            "hyper::",
        ] {
            assert!(
                !source.contains(forbidden),
                "{name} names `{forbidden}`; this server has no network socket and M1/M3 are \
                 satisfied by the transport, not by a configuration"
            );
        }
    }
}
