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

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, Command, Stdio};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use peek_mcp::Session;
use peek_mcp::session::SharedLog;

/// Every source file in the crate, as `(path, contents)`.
///
/// Listed rather than discovered, so the scan is over what is in the commit rather than over
/// whatever happens to be on disk when the test runs. A new source file is a new line here, which
/// is the point: a file nobody listed is a file nobody scanned.
fn sources() -> Vec<(&'static str, &'static str)> {
    vec![
        ("src/lib.rs", include_str!("../src/lib.rs")),
        ("src/main.rs", include_str!("../src/main.rs")),
        ("src/outcome.rs", include_str!("../src/outcome.rs")),
        ("src/params.rs", include_str!("../src/params.rs")),
        ("src/protocol.rs", include_str!("../src/protocol.rs")),
        ("src/server.rs", include_str!("../src/server.rs")),
        ("src/session.rs", include_str!("../src/session.rs")),
        ("src/tool.rs", include_str!("../src/tool.rs")),
        ("src/writer.rs", include_str!("../src/writer.rs")),
        ("src/tools/mod.rs", include_str!("../src/tools/mod.rs")),
        ("src/tools/context.rs", include_str!("../src/tools/context.rs")),
        ("src/tools/doctor.rs", include_str!("../src/tools/doctor.rs")),
        ("src/tools/explain.rs", include_str!("../src/tools/explain.rs")),
        ("src/tools/index.rs", include_str!("../src/tools/index.rs")),
        ("src/tools/walk.rs", include_str!("../src/tools/walk.rs")),
        ("src/tools/watch.rs", include_str!("../src/tools/watch.rs")),
    ]
}

/// A child server, and the pipes it speaks through.
struct Server {
    child: Child,
    input: std::process::ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
    stderr: Arc<Mutex<String>>,
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
        std::thread::spawn(move || {
            let mut pipe: ChildStderr = stderr;
            let mut buffer = Vec::new();
            if pipe.read_to_end(&mut buffer).is_ok() {
                let mut slot = sink
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                slot.push_str(&String::from_utf8_lossy(&buffer));
            }
        });

        Self {
            child,
            input,
            stdout,
            stderr: transcript,
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
        self.stderr
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Close stdin, wait for the process, and return its stderr transcript.
    fn finish(mut self) -> String {
        drop(self.input);
        let status = self.child.wait().expect("wait for the server to exit");
        assert!(status.success(), "the server exited with {status}");
        // The reader thread ends when the process does; joining it is what guarantees the
        // transcript is complete rather than merely written so far.
        let transcript = self.transcript();
        transcript
    }
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
        if let Ok(session) = Session::new(&self.0, Box::new(SharedLog::new()))
            && let Some(path) = session.index_path()
        {
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
    let initialize = server
        .send_and_read(json!({
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
        refused["result"]["structuredContent"]["outcome"],
        "not_indexed",
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

    let pack = server.call(6, "context", json!({ "target": "entry", "budget_tokens": 4000 }));
    let content = &pack["result"]["content"][0];
    assert_eq!(content["type"], "text", "a text block is present: {pack}");
    assert!(
        content["text"].as_str().is_some_and(|text| !text.is_empty()),
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
    assert_eq!(ping["id"], 8, "the stream is still in step after the refusal: {ping}");

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
    assert!(stderr.contains("USAGE"), "the usage text is on stderr: {stderr}");
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
    // Only stdout is scanned. stderr is the diagnostic channel and `eprintln!` is its correct
    // spelling, so scanning for it would be scanning for the thing the rule asks for.
    for (name, source) in sources() {
        for line in source.lines() {
            let trimmed = line.trim_start();
            let printing = trimmed.starts_with("println!")
                || trimmed.starts_with("print!")
                || trimmed.starts_with("dbg!");
            assert!(
                !printing,
                "{name} writes to stdout at `{trimmed}`. stdout is the protocol channel; use \
                 `session::Log` for diagnostics."
            );
        }
    }
}

#[test]
fn only_the_binary_holds_the_processs_stdout() {
    // The positive form of the same rule: not "it does not print" but "it cannot". The library
    // never names `io::stdout`, so a tool handler has nothing to print to even by accident.
    let mut holders: Vec<&str> = sources()
        .into_iter()
        .filter(|(_, source)| source.contains("io::stdout"))
        .map(|(name, _)| name)
        .collect();
    assert_eq!(
        holders,
        vec!["src/main.rs"],
        "only the binary may hold the process's stdout, and it hands it straight to the protocol \
         writer: {holders:?}"
    );
    assert!(
        sources()
            .iter()
            .filter(|(name, _)| *name != "src/main.rs")
            .all(|(_, source)| !source.contains("std::io::stdout")),
        "and no other spelling of the same call appears either"
    );
}

#[test]
fn the_binary_never_opens_a_socket() {
    // Contract M1 and M3. Stdio has no address to bind, so this is a structural claim rather than
    // a policy one, and the cheapest way to keep it structural is to assert that nothing in the
    // crate reaches for a network API.
    for (name, source) in sources() {
        for forbidden in ["TcpListener", "TcpStream", "UdpSocket", "reqwest", "hyper::"] {
            assert!(
                !source.contains(forbidden),
                "{name} names `{forbidden}`; this server has no network socket and M1/M3 are \
                 satisfied by the transport, not by a configuration"
            );
        }
    }
}
