//! The protocol layer, driven directly.
//!
//! These tests never touch a file descriptor. They hand [`serve`] two in-memory streams and read
//! the bytes back, which is what makes "the protocol stream stays clean" a claim about the code
//! rather than about a run that happened not to print anything. The subprocess run that proves the
//! same thing about the real binary is in `transport.rs`.

// `expect` and `panic` are denied workspace-wide, on the grounds that in production code they hide
// a real failure behind a panic. `peek-core`'s unit tests are exempted through that crate's
// `lib.rs`; an integration test is a separate crate and does not inherit that, so it is exempted
// here instead. The justification is the same one: a test that fails inside an `expect` has
// already failed, and a message naming what went wrong is worth more than a panic location.
#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::io::Cursor;

use peek_mcp::server::{MAX_LINE_BYTES, serve};
use peek_mcp::session::{Session, SharedLog, StderrLog};
use peek_mcp::writer::ProtocolWriter;
use serde_json::{Value, json};

/// A directory that removes itself, so a failing test leaves nothing behind.
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        // The label plus the process id: two tests in one binary must not share a directory, and a
        // stale one from a previous run must not be reused.
        let path = std::env::temp_dir().join(format!("peek-mcp-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create a temporary directory");
        Self(path)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// What a driven session produced.
struct Driven {
    /// Every byte the server wrote to the protocol stream.
    stdout: Vec<u8>,
    /// The diagnostics it logged instead.
    log: SharedLog,
}

/// Send `messages` to a server over `root`, and return what came back.
fn drive(root: &std::path::Path, messages: &[Value]) -> Driven {
    let mut input = String::new();
    for message in messages {
        input.push_str(&serde_json::to_string(message).expect("encode a request"));
        input.push('\n');
    }
    let log = SharedLog::new();
    let mut session = Session::new(root, Box::new(log.clone()));
    let mut output = ProtocolWriter::new(Vec::new());
    serve(&mut session, Cursor::new(input.into_bytes()), &mut output).expect("serve one session");
    Driven {
        stdout: output.into_inner(),
        log,
    }
}

/// Send raw `bytes` to a server, for the framing cases `drive` cannot express.
fn drive_raw(root: &std::path::Path, bytes: Vec<u8>) -> Vec<u8> {
    let mut session = Session::new(root, Box::new(StderrLog));
    let mut output = ProtocolWriter::new(Vec::new());
    serve(&mut session, Cursor::new(bytes), &mut output)
        .expect("a bad line does not end a session");
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
/// The parse is the assertion: a response nobody parses is not tested, and a line that is not JSON
/// is the exact failure this module exists to prevent.
fn replies(stdout: &[u8]) -> Vec<Value> {
    String::from_utf8(stdout.to_vec())
        .expect("the protocol stream is UTF-8")
        .lines()
        .map(|line| serde_json::from_str(line).expect("every protocol line is a JSON value"))
        .collect()
}

#[test]
fn initialize_states_the_version_the_server_implements_and_its_own_name() {
    let dir = TempDir::new("initialize");
    let driven = drive(
        dir.path(),
        &[request(
            1,
            "initialize",
            json!({ "protocolVersion": "2025-06-18" }),
        )],
    );
    let replies = replies(&driven.stdout);
    assert_eq!(replies.len(), 1, "one request, one reply: {replies:?}");
    let result = &replies[0]["result"];
    assert_eq!(replies[0]["jsonrpc"], "2.0");
    assert_eq!(replies[0]["id"], 1);
    assert_eq!(result["protocolVersion"], "2025-06-18");
    assert_eq!(result["serverInfo"]["name"], "peek");
    assert_eq!(
        result["serverInfo"]["version"],
        peek_mcp::VERSION,
        "the version in `initialize` is the library's, not a second constant written here"
    );
    assert!(
        result["capabilities"]["tools"]["listChanged"].is_boolean(),
        "the tool capability is declared: {result}"
    );
    assert!(
        result["instructions"]
            .as_str()
            .is_some_and(|text| !text.is_empty()),
        "a server that can be misused should say how to use it: {result}"
    );
    assert!(
        driven.log.mentions("session open"),
        "starting up is logged to the diagnostic stream, not the protocol one: {:?}",
        driven.log.lines()
    );
}

#[test]
fn the_version_answered_is_the_one_the_client_asked_for_when_it_is_known() {
    let dir = TempDir::new("negotiate-known");
    let driven = drive(
        dir.path(),
        &[request(
            1,
            "initialize",
            json!({ "protocolVersion": "2024-11-05" }),
        )],
    );
    assert_eq!(
        replies(&driven.stdout)[0]["result"]["protocolVersion"],
        "2024-11-05",
        "the rule is: answer with the client's version when this build speaks it"
    );
}

#[test]
fn the_version_answered_is_one_this_build_implements_when_the_client_asks_for_another() {
    // The negotiation rule is only half the claim. The other half is that the answer is a version
    // this server has actually been written against: a client that asks for a revision this build
    // does not implement must be answered with the one it does, never with an echo of what it
    // asked for and never with a different unimplemented revision.
    for asked in ["2099-01-01", "2025-11-25", "not a version at all", ""] {
        let dir = TempDir::new("negotiate-unknown");
        let driven = drive(
            dir.path(),
            &[request(
                1,
                "initialize",
                json!({ "protocolVersion": asked }),
            )],
        );
        let answered = replies(&driven.stdout)[0]["result"]["protocolVersion"]
            .as_str()
            .expect("a version is always answered")
            .to_owned();
        assert_eq!(
            answered,
            peek_mcp::protocol::PROTOCOL_VERSION,
            "a client asking for {asked:?} is answered with the version this build implements"
        );
        assert_ne!(
            answered, asked,
            "and never with an echo of what it asked for"
        );
    }
}

#[test]
fn every_version_this_build_claims_is_one_it_implements() {
    // A version in the list is a promise to a client, so the list and the constant this build was
    // written against cannot drift apart. When they do, a client asking for the newer revision is
    // told the server speaks it, and it does not.
    assert_eq!(
        peek_mcp::protocol::KNOWN_PROTOCOL_VERSIONS.last(),
        Some(&peek_mcp::protocol::PROTOCOL_VERSION),
        "the newest version claimed is the newest one implemented; a version this build has not \
         been written against may not be in the list"
    );
    assert!(
        peek_mcp::protocol::KNOWN_PROTOCOL_VERSIONS.contains(&peek_mcp::protocol::PROTOCOL_VERSION),
        "and the version it does implement is in the list, so a client asking for it is echoed"
    );
}

#[test]
fn a_notification_is_never_answered() {
    // The single most damaging bug in a JSON-RPC server: a reply to a notification desynchronises
    // the client, which then reads it as the answer to its *next* request.
    let dir = TempDir::new("notification");
    let driven = drive(
        dir.path(),
        &[
            notification("notifications/initialized", json!({})),
            notification("notifications/cancelled", json!({ "requestId": 99 })),
            request(1, "ping", json!({})),
        ],
    );
    let replies = replies(&driven.stdout);
    assert_eq!(
        replies.len(),
        1,
        "two notifications and one request produce exactly one reply: {replies:?}"
    );
    assert_eq!(
        replies[0]["id"], 1,
        "the reply is the request's, not a notification's"
    );
}

#[test]
fn an_unknown_method_names_the_methods_that_exist() {
    let dir = TempDir::new("unknown-method");
    let driven = drive(dir.path(), &[request(1, "resources/list", json!({}))]);
    let reply = &replies(&driven.stdout)[0];
    assert_eq!(reply["error"]["code"], -32601);
    let message = reply["error"]["message"].as_str().expect("a sentence");
    assert!(
        message.contains("resources/list"),
        "the refusal names what was asked for: {message}"
    );
    let supported = reply["error"]["data"]["supported_methods"]
        .as_array()
        .expect("the supported methods are listed");
    assert!(
        supported.iter().any(|name| name == "tools/call"),
        "a refusal that does not say what to use instead is a dead end: {supported:?}"
    );
}

#[test]
fn a_line_that_is_not_json_is_a_parse_error_with_no_id() {
    let dir = TempDir::new("parse-error");
    let mut input = b"{not json at all\n".to_vec();
    input.extend_from_slice(&serde_json::to_vec(&request(1, "ping", json!({}))).unwrap());
    input.push(b'\n');

    let replies = replies(&drive_raw(dir.path(), input));
    assert_eq!(
        replies.len(),
        2,
        "the bad line and then the good one: {replies:?}"
    );
    assert_eq!(replies[0]["error"]["code"], -32700);
    assert_eq!(
        replies[0]["id"],
        Value::Null,
        "a parse error has no id to answer, and the member is still present"
    );
    assert_eq!(
        replies[1]["id"], 1,
        "the session continued: a malformed line is recoverable, not fatal"
    );
}

#[test]
fn a_message_without_a_jsonrpc_member_is_refused_with_the_reason() {
    let dir = TempDir::new("no-version");
    let driven = drive(dir.path(), &[json!({ "id": 1, "method": "ping" })]);
    let reply = &replies(&driven.stdout)[0];
    assert_eq!(reply["error"]["code"], -32600);
    assert!(
        reply["error"]["message"]
            .as_str()
            .is_some_and(|text| text.contains("jsonrpc")),
        "the refusal names the missing member: {reply}"
    );
}

#[test]
fn a_message_with_the_wrong_jsonrpc_version_is_refused_with_the_version_it_had() {
    let dir = TempDir::new("wrong-version");
    let driven = drive(
        dir.path(),
        &[json!({ "jsonrpc": "1.0", "id": 1, "method": "ping" })],
    );
    let reply = &replies(&driven.stdout)[0];
    assert_eq!(reply["error"]["code"], -32600);
    assert!(
        reply["error"]["message"]
            .as_str()
            .is_some_and(|text| text.contains("1.0")),
        "the refusal quotes what was sent rather than only what was wanted: {reply}"
    );
}

#[test]
fn a_message_that_is_not_an_object_is_refused_and_named() {
    let dir = TempDir::new("not-an-object");
    let driven = drive(dir.path(), &[json!([1, 2, 3])]);
    let reply = &replies(&driven.stdout)[0];
    assert_eq!(reply["error"]["code"], -32600);
    assert!(
        reply["error"]["message"]
            .as_str()
            .is_some_and(|text| text.contains("list")),
        "the refusal says what arrived: {reply}"
    );
}

#[test]
fn a_blank_line_is_ignored_rather_than_answered() {
    // A client that writes a trailing newline after its last message produces one. Answering it
    // with a parse error would be complaining about a framing detail of a message already handled.
    let dir = TempDir::new("blank-line");
    let mut input = b"   \n\n".to_vec();
    input.extend_from_slice(&serde_json::to_vec(&request(1, "ping", json!({}))).unwrap());
    input.push(b'\n');
    assert_eq!(replies(&drive_raw(dir.path(), input)).len(), 1);
}

#[test]
fn a_final_line_without_a_newline_is_still_answered() {
    // A client that closes its pipe without a trailing newline is not making a mistake about the
    // protocol; refusing to answer its last request would be ours.
    let dir = TempDir::new("no-trailing-newline");
    let input = serde_json::to_vec(&request(1, "ping", json!({}))).unwrap();
    assert!(
        input.last().is_some_and(|byte| *byte != b'\n'),
        "this test is about a line with no terminator, so it must not have one"
    );
    let replies = replies(&drive_raw(dir.path(), input));
    assert_eq!(
        replies.len(),
        1,
        "the last request was answered: {replies:?}"
    );
    assert_eq!(replies[0]["id"], 1);
}

#[test]
fn a_message_over_the_line_limit_is_refused_and_the_stream_continues() {
    // A bound on memory, and the point of the test is that it is one *before* the length is looked
    // at: the line is read in bounded chunks and the rest of it is discarded, so a client cannot
    // make this process allocate whatever it likes. The stream is still in step afterwards, which
    // is the property that makes refusing an over-long line safe at all.
    //
    // These bytes are not JSON at all, so there is no id to answer on and the reply carries a
    // null one. That is the honest answer for a line with nothing in it to answer.
    let dir = TempDir::new("line-limit");
    let mut input = vec![b'x'; MAX_LINE_BYTES + 1];
    input.push(b'\n');
    input.extend_from_slice(&serde_json::to_vec(&request(1, "ping", json!({}))).unwrap());
    input.push(b'\n');

    let replies = replies(&drive_raw(dir.path(), input));
    assert_eq!(
        replies.len(),
        2,
        "the refusal and then the answer: {replies:?}"
    );
    assert_eq!(replies[0]["error"]["code"], -32600);
    assert_eq!(
        replies[0]["id"],
        Value::Null,
        "there is no id in a line of `x`, so the null id is the only honest one"
    );
    assert!(
        replies[0]["error"]["message"]
            .as_str()
            .is_some_and(|text| text.contains(&MAX_LINE_BYTES.to_string())),
        "the refusal states the limit it applied: {replies:?}"
    );
    assert_eq!(replies[1]["id"], 1, "the session continued in step");
}

#[test]
fn an_over_long_message_is_refused_on_the_id_it_carried() {
    // The id is where the client says the answer belongs, and an over-long message is still a
    // message. Two things have to hold for the recovery to be worth anything, and both are here:
    //
    // * the id has to be *inside* the bound, or there is nothing to recover — so the bulk of the
    //   line comes after it, not before;
    // * a member named `id` inside `params` is the caller's own argument, not the request's
    //   identity, and a recovery that searched for the word would answer on `"not the request's
    //   id"`.
    let dir = TempDir::new("line-limit-id");
    let mut input = br#"{"jsonrpc":"2.0","method":"tools/call","params":{"id":"not the request's id","name":"index_status","arguments":{"padding":""#.to_vec();
    input.extend(vec![b'a'; 1024]);
    input.extend_from_slice(br#""}},"id":99,"note":""#);
    input.extend(vec![b'b'; MAX_LINE_BYTES]);
    input.extend_from_slice(br#""}}"#);
    input.push(b'\n');
    input.extend_from_slice(&serde_json::to_vec(&request(1, "ping", json!({}))).unwrap());
    input.push(b'\n');

    let replies = replies(&drive_raw(dir.path(), input));
    assert_eq!(
        replies.len(),
        2,
        "the refusal and then the answer: {replies:?}"
    );
    assert_eq!(replies[0]["error"]["code"], -32600);
    assert_eq!(
        replies[0]["id"],
        json!(99),
        "the client is owed an answer addressed to 99 and gets one it can match, not the argument \
         of the same name inside `params`: {replies:?}"
    );
    assert_eq!(replies[1]["id"], 1, "and the session is still in step");
}

#[test]
fn a_cancelled_request_is_not_run() {
    let dir = TempDir::new("cancelled");
    let driven = drive(
        dir.path(),
        &[
            notification("notifications/cancelled", json!({ "requestId": 7 })),
            request(7, "ping", json!({})),
        ],
    );
    let replies = replies(&driven.stdout);
    assert_eq!(replies.len(), 1);
    assert!(
        replies[0]["error"]["message"]
            .as_str()
            .is_some_and(|text| text.contains("cancelled")),
        "a cancelled request is refused with that word, not silently dropped: {replies:?}"
    );
    assert!(
        driven.log.mentions("cancelled"),
        "the cancellation is logged, so a caller can see why its request did not run: {:?}",
        driven.log.lines()
    );
}

#[test]
fn the_tool_catalogue_lists_every_tool_with_a_schema_that_refuses_extras() {
    let dir = TempDir::new("catalogue");
    let driven = drive(dir.path(), &[request(1, "tools/list", json!({}))]);
    let tools = replies(&driven.stdout)[0]["result"]["tools"]
        .as_array()
        .expect("tools/list returns an array")
        .clone();

    let names: Vec<&str> = tools
        .iter()
        .map(|tool| tool["name"].as_str().expect("a name"))
        .collect();
    assert_eq!(
        names,
        peek_mcp::tool::names(),
        "the catalogue and the dispatch table are the same list, in the same order"
    );
    for tool in &tools {
        let name = tool["name"].as_str().expect("a name");
        assert_eq!(
            tool["inputSchema"]["additionalProperties"],
            json!(false),
            "`{name}` must refuse an argument it does not have, which needs this to be false"
        );
        assert!(
            tool["inputSchema"]["properties"].is_object(),
            "`{name}` declares its arguments: {tool}"
        );
        let description = tool["description"].as_str().expect("a description");
        assert!(
            description.len() > 200,
            "`{name}`'s description is {} characters, too short to say what it does not answer and \
             what to use instead",
            description.len()
        );
    }
}

#[test]
fn tools_call_without_a_name_is_a_protocol_error_naming_the_tools() {
    let dir = TempDir::new("call-no-name");
    let driven = drive(dir.path(), &[request(1, "tools/call", json!({}))]);
    let reply = &replies(&driven.stdout)[0];
    assert_eq!(
        reply["error"]["code"], -32602,
        "the method exists, so this is a parameter fault rather than a missing method: {reply}"
    );
    assert!(
        reply["error"]["data"]["tools"]
            .as_array()
            .is_some_and(|tools| !tools.is_empty()),
        "the refusal lists what could have been called: {reply}"
    );
}

#[test]
fn a_tool_result_names_every_member_the_specification_names() {
    // The other tests in this file reach the wire through a whole session, which is the right place
    // to check behaviour and the wrong place to check spelling: a member spelled `is_error` still
    // arrives, still parses, and still leaves a client reading a call that failed as a call that
    // worked. This asserts the names themselves, off the serialised value, so the rename cannot be
    // undone by somebody renaming the field back.
    //
    // Three members of this payload are camel-cased on the wire and snake-cased in the struct. They
    // were not all renamed together, which is why the assertion lists all three rather than the one
    // that happened to be caught.
    let result = peek_mcp::protocol::CallToolResult::answered(
        "an answer",
        json!({ "outcome": "ok" }),
        true,
    );
    let wire = serde_json::to_value(&result).expect("a result is serialisable");

    assert_eq!(
        wire["isError"],
        json!(true),
        "the field that marks a failed call is spelled the way the specification spells it, because \
         a client that cannot find it treats the failure as a success: {wire}"
    );
    assert!(
        wire.get("is_error").is_none(),
        "and there is no second spelling of it to read instead: {wire}"
    );
    assert_eq!(
        wire["structuredContent"],
        json!({ "outcome": "ok" }),
        "the data half travels under the name the specification gives it: {wire}"
    );
    assert_eq!(
        wire["content"][0]["type"],
        json!("text"),
        "and so does the discriminator on a content block: {wire}"
    );
    assert!(
        wire["content"][0].get("kind").is_none(),
        "the internal name is not the wire name for that one either: {wire}"
    );
}

#[test]
fn every_tool_dispatch_rejects_a_name_that_is_not_a_tool() {
    // Dispatch and catalogue are two `match`es over the same set of names. A name in one and not
    // the other is the shape of a bug that only shows up to a client.
    let dir = TempDir::new("no-such-tool");
    let driven = drive(
        dir.path(),
        &[request(
            1,
            "tools/call",
            json!({ "name": "search", "arguments": {} }),
        )],
    );
    let reply = &replies(&driven.stdout)[0];
    let structured = &reply["result"]["structuredContent"];
    assert_eq!(structured["outcome"], "refused");
    assert!(
        structured["reason"]
            .as_str()
            .is_some_and(|text| text.contains("search")),
        "the refusal names the tool that was asked for: {structured}"
    );
    assert!(
        structured["advice"]
            .as_str()
            .is_some_and(|text| text.contains("context")),
        "the refusal names a real tool to use instead: {structured}"
    );
    assert_eq!(
        reply["result"]["isError"],
        json!(false),
        "a refusal the model can act on is not an errored tool call: {reply}"
    );
}
