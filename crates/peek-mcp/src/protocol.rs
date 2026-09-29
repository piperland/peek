//! JSON-RPC 2.0 and the subset of MCP this server speaks.
//!
//! # What is in the spec and what is here
//!
//! MCP is JSON-RPC 2.0 over newline-delimited JSON. The methods a **server** must implement are
//! `initialize`, `tools/list` and `tools/call`; `ping` is optional and cheap to honour; two
//! notifications arrive in practice. Resources, prompts, sampling, elicitation, roots, logging and
//! completions are all optional capabilities, and this server advertises none of them — a client
//! that tries them gets a JSON-RPC error naming the ones that exist, which is better than a
//! capability list it has to intersect with a feature matrix.
//!
//! # Why requests are classified by hand
//!
//! A `serde` derive over a struct with `id: Option<Value>` cannot distinguish a request from a
//! notification, and getting that wrong is the single most damaging bug in a JSON-RPC server: a
//! response to a notification desynchronises the client, which is then reading a reply where it
//! expected the next request. So [`Incoming`] is built by looking at the value, and
//! [`Incoming::is_notification`] is the only thing that decides whether a reply is written.
//!
//! # Version negotiation
//!
//! The rule from the specification: if the server supports the version the client asked for, it
//! answers with that version; otherwise it answers with one it does support, preferring the
//! latest. [`negotiate`] implements exactly that, and [`KNOWN_PROTOCOL_VERSIONS`] is the supported
//! set. `2024-11-05` and `2025-03-26` are in the set because a client pinned to one of them is a
//! real client, and refusing to talk to it helps nobody — the shapes this server uses (`tools`,
//! `inputSchema`, text content) are identical across all three.

use serde::Serialize;
use serde_json::{Value, json};

/// The only `jsonrpc` value there is.
pub const JSONRPC_VERSION: &str = "2.0";

/// The version this build implements, and the one it answers with when the client names none it
/// knows.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// Every version this build can speak, oldest first.
///
/// The first two are here because a client pinned to one of them is a real client, and the tool
/// shapes it uses are the same ones. The last is the newest this build has been written against,
/// and it is [`PROTOCOL_VERSION`]: a revision this server has not implemented — tasks, icons, the
/// clarified rule that argument errors are tool execution errors — may not be named here, because a
/// client that is answered with a version believes the server implements that revision. Adding a
/// line here is a claim of conformance, and the difference between a claim and an implementation is
/// the whole subject of the paragraph above.
pub const KNOWN_PROTOCOL_VERSIONS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18"];

/// Parse error: the bytes were not JSON.
pub const CODE_PARSE_ERROR: i64 = -32700;
/// Invalid request: valid JSON that is not a well-formed JSON-RPC request.
pub const CODE_INVALID_REQUEST: i64 = -32600;
/// Method not found.
pub const CODE_METHOD_NOT_FOUND: i64 = -32601;
/// Invalid parameters: a well-formed call to a known method with arguments it cannot use.
pub const CODE_INVALID_PARAMS: i64 = -32602;
/// Internal error: the server failed in a way that is not the caller's fault.
pub const CODE_INTERNAL_ERROR: i64 = -32603;

/// The methods this server implements, listed in error messages so an unknown method is answered
/// with a route rather than a no.
pub const METHODS: &[&str] = &[
    "initialize",
    "notifications/initialized",
    "notifications/cancelled",
    "ping",
    "tools/list",
    "tools/call",
];

/// A message read from the client.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    /// A call that expects a reply.
    Request {
        /// Echoed verbatim in the reply. `Value` because JSON-RPC ids may be a string or a number
        /// and the server must not re-type what the client sent.
        id: Value,
        method: String,
        /// The raw `params`. An object for every method this server has; anything else is refused
        /// with a message that says so.
        params: Value,
    },
    /// A call that expects no reply. **Never** answered — see the module documentation.
    Notification {
        method: String,
        params: Value,
    },
    /// Bytes that are not a request this server can route. Carries the reply to write, if one is
    /// owed, so the caller never has to decide whether an unparseable line was a request.
    Unroutable {
        /// The id to answer, if the message had one. A parse error has none, because nothing was
        /// parsed.
        id: Option<Value>,
        code: i64,
        message: String,
    },
}

impl Incoming {
    /// Whether this message must not be answered.
    #[must_use]
    pub const fn is_notification(&self) -> bool {
        matches!(self, Incoming::Notification { .. })
    }

    /// The method, where there is one.
    #[must_use]
    pub fn method(&self) -> Option<&str> {
        match self {
            Incoming::Request { method, .. } | Incoming::Notification { method, .. } => {
                Some(method)
            }
            Incoming::Unroutable { .. } => None,
        }
    }
}

/// Read one line of the stream and classify it.
///
/// Returns `None` for a line that is empty or only whitespace. An empty line is not an error: a
/// client that writes a trailing newline after its last message produces one, and answering it with
/// a parse error would be the server complaining about a framing detail of a message it already
/// answered.
pub fn classify(line: &str) -> Option<Incoming> {
    if line.trim().is_empty() {
        return None;
    }
    let value: Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(error) => {
            return Some(Incoming::Unroutable {
                id: None,
                code: CODE_PARSE_ERROR,
                message: format!("that line is not JSON: {error}"),
            });
        }
    };

    // A `match` rather than a `let ... else`, because the else arm needs to name what arrived and
    // a let-else's scrutinee is not reliably readable from its else block.
    let object = match value {
        Value::Object(object) => object,
        other => {
            return Some(Incoming::Unroutable {
                id: None,
                code: CODE_INVALID_REQUEST,
                message: format!(
                    "a JSON-RPC message is an object; this line is {}",
                    describe(&other)
                ),
            });
        }
    };

    // The id is read before the version, because an id is the only thing a reply can be addressed
    // to and losing it means the client waits forever.
    let id = object.get("id").cloned().filter(|value| !value.is_null());

    let unroutable = |code: i64, message: String| Incoming::Unroutable {
        id: id.clone(),
        code,
        message,
    };

    match object.get("jsonrpc") {
        Some(Value::String(version)) if version == JSONRPC_VERSION => {}
        Some(Value::String(version)) => {
            return Some(unroutable(
                CODE_INVALID_REQUEST,
                format!("`jsonrpc` must be the string \"{JSONRPC_VERSION}\"; this line said {version:?}"),
            ));
        }
        Some(_) => {
            return Some(unroutable(
                CODE_INVALID_REQUEST,
                format!("`jsonrpc` must be the string \"{JSONRPC_VERSION}\""),
            ));
        }
        None => {
            return Some(unroutable(
                CODE_INVALID_REQUEST,
                format!("`jsonrpc` is missing; every message must say \"{JSONRPC_VERSION}\""),
            ));
        }
    }

    let method = match object.get("method") {
        Some(Value::String(method)) => method.clone(),
        Some(other) => {
            return Some(unroutable(
                CODE_INVALID_REQUEST,
                format!("`method` must be a string; this line holds {}", describe(other)),
            ));
        }
        None => {
            return Some(unroutable(
                CODE_INVALID_REQUEST,
                "`method` is missing; this is a JSON-RPC response, and this server is a server, so \
                 it has nothing to respond to"
                    .to_owned(),
            ));
        }
    };

    let params = object
        .get("params")
        .cloned()
        .unwrap_or(Value::Object(serde_json::Map::new()));

    match id {
        // A message with no `id` is a notification, and a notification is never answered. This is
        // the branch a derived struct gets wrong.
        None => Some(Incoming::Notification { method, params }),
        Some(id) => Some(Incoming::Request { id, method, params }),
    }
}

/// A successful reply.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Response {
    /// Always `"2.0"`.
    pub jsonrpc: &'static str,
    /// The request's id, verbatim.
    pub id: Value,
    /// The result.
    pub result: Value,
}

impl Response {
    /// A reply carrying `result`.
    #[must_use]
    pub fn new(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            id,
            result,
        }
    }
}

/// A failed reply.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    /// Extra context. Present whenever there is something useful to add, and its absence is itself
    /// meaningful: a bare code with no data is a failure this server did not think about.
    pub data: Option<Value>,
}

impl RpcError {
    /// An error with a code and a sentence.
    #[must_use]
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    /// An error with structured context beside the sentence.
    #[must_use]
    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }
}

/// A failed reply.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ErrorResponse {
    /// Always `"2.0"`.
    pub jsonrpc: &'static str,
    /// The request's id, or `null` when the request had none or could not be parsed. JSON-RPC
    /// requires the member to be present even when it is null, and a client that is waiting on an
    /// id cannot be told anything by a reply that omits it.
    pub id: Value,
    pub error: RpcError,
}

impl ErrorResponse {
    /// A reply carrying an error.
    #[must_use]
    pub fn new(id: Option<Value>, error: RpcError) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            id: id.unwrap_or(Value::Null),
            error,
        }
    }
}

/// One block of a tool's result.
///
/// Text only, deliberately. The engine's answers are text and structured data, and neither is an
/// image or an embedded resource, so the other variants would be a schema this server cannot
/// honestly fill.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TextContent {
    /// The discriminator the specification requires. Named `r#type` internally and spelled `type`
    /// on the wire, because `type` is a keyword.
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub text: String,
}

impl TextContent {
    /// A text block.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            kind: "text",
            text: text.into(),
        }
    }
}

/// The result of `tools/call`.
///
/// Both halves matter and neither is sufficient. `content` is what a client shows, and a client
/// that ignores `structuredContent` entirely still gets a readable answer. `structuredContent` is
/// what a program parses, and a model reading a rendered report has to infer structure from prose
/// unless it is given some.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CallToolResult {
    /// The answer, as text.
    pub content: Vec<TextContent>,
    /// The answer, as data. Omitted when a tool produced none, which is a framing choice about a
    /// protocol field rather than about a payload: every payload this crate builds has a stable
    /// shape with no fields omitted.
    #[serde(rename = "structuredContent", skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<Value>,
    /// True only when the engine could not answer for a reason the caller cannot fix. See
    /// [`crate::outcome::Outcome::is_error`].
    pub is_error: bool,
}

impl CallToolResult {
    /// A successful result.
    #[must_use]
    pub fn ok(text: impl Into<String>, structured: Value) -> Self {
        Self {
            content: vec![TextContent::new(text)],
            structured_content: Some(structured),
            is_error: false,
        }
    }

    /// A result the model should read but which is not a failure.
    #[must_use]
    pub fn answered(text: impl Into<String>, structured: Value, is_error: bool) -> Self {
        Self {
            content: vec![TextContent::new(text)],
            structured_content: Some(structured),
            is_error,
        }
    }
}

/// The version to answer `initialize` with.
///
/// The rule from the specification, in order: the client's version if this build speaks it, and
/// otherwise the newest version this build does speak. Never a version outside
/// [`KNOWN_PROTOCOL_VERSIONS`], because a client that receives one it does not know may refuse to
/// continue, and a server that claims a version it has not been written against is the same class
/// of claim as a benchmark nobody ran.
///
/// "Newest this build does speak" is [`PROTOCOL_VERSION`], and a test in `tests/protocol.rs`
/// asserts it is the last entry of the list. That assertion is the point: if the list ever grows
/// past the version this build implements, the fallback starts answering with something this
/// server has not been written against, and no test of the negotiation itself would notice.
#[must_use]
pub fn negotiate(requested: Option<&str>) -> String {
    match requested {
        Some(version) if KNOWN_PROTOCOL_VERSIONS.contains(&version) => (*version).to_owned(),
        _ => PROTOCOL_VERSION.to_owned(),
    }
}

/// The result of `initialize`.
#[must_use]
pub fn initialize_result(requested: Option<&str>, instructions: &str) -> Value {
    json!({
        "protocolVersion": negotiate(requested),
        "capabilities": {
            "tools": { "listChanged": false }
        },
        "serverInfo": {
            "name": "peek",
            "version": crate::VERSION
        },
        "instructions": instructions
    })
}

/// A sentence naming a JSON value's shape, for an error message.
fn describe(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "a list",
        Value::Object(_) => "an object",
    }
}
