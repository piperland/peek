//! The dispatch loop: a line in, a line out, until the client goes away.
//!
//! # Why the loop is a function over two streams
//!
//! [`serve`] takes its input and its output as parameters. That is the whole reason stdout can be
//! defended: nothing inside the server holds a handle on the process's file descriptor 1, so a
//! tool handler has nothing to print to even by accident. The alternative — a `serve()` that reads
//! `io::stdin()` and writes `io::stdout()` — is one line shorter and makes the property
//! untestable, untestable being the same as absent.
//!
//! # The loop's rules
//!
//! * **One JSON object per line**, read as bytes so that invalid UTF-8 becomes a parse error rather
//!   than a dead server.
//! * **A notification is never answered.** Answering one desynchronises a client that is waiting
//!   for the reply to its *next* request.
//! * **A line that is not a routable request always produces a reply**, unless it had no id to
//!   reply to. A client that sent something malformed and got silence has no way to know the
//!   server is alive.
//! * **Every reply is flushed before the loop continues.** See [`crate::writer`].
//! * **A cancelled request does not run.** See [`Session::cancel`].
//! * **Shutdown stops the watch.** A client that disconnects without calling `watch_stop` must not
//!   leave a thread holding the SQLite writer.
//!
//! # `tools/call` failures are results, not JSON-RPC errors
//!
//! A tool that could not do its job — an unknown target, an ambiguous one, a budget too small —
//! returns a `CallToolResult` with `isError` set only when the engine failed, and the four
//! standard fields in `structuredContent`. Only a call this server cannot *route* — an unknown
//! method, a missing tool name — is a JSON-RPC error, because those are protocol faults rather than
//! answers.

use std::io::{self, BufRead, Write};

use serde_json::{Value, json};

use crate::protocol::{
    self, CODE_INVALID_PARAMS, CODE_INVALID_REQUEST, CODE_METHOD_NOT_FOUND, Incoming, RpcError,
};
use crate::session::Session;
use crate::tools::{self, ToolAnswer};
use crate::writer::ProtocolWriter;

/// The largest message this server will read, in bytes.
///
/// A bound on memory, not a claim about traffic: an unbounded line is a way for one client to
/// exhaust the process, and a `tools/call` carrying a list of refresh paths or a large budget
/// expression is orders of magnitude smaller than this. A line over the bound is refused and the
/// rest of it is discarded up to the newline, because the alternative is a stream that cannot be
/// resynchronised.
pub const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;

/// Read messages, dispatch them, and write replies until the input ends.
pub fn serve<R: BufRead, W: Write>(
    session: &mut Session,
    input: R,
    output: &mut ProtocolWriter<W>,
) -> io::Result<()> {
    let mut input = input;
    let mut buffer: Vec<u8> = Vec::with_capacity(4096);
    loop {
        buffer.clear();
        let read = match read_line(&mut input, &mut buffer) {
            Ok(read) => read,
            Err(error) => {
                session.log(&format!("the input stream failed: {error}"));
                return Err(error);
            }
        };
        if read == 0 {
            break;
        }
        if buffer.len() > MAX_LINE_BYTES {
            // The line is already read and already over the bound, so the stream is still in sync;
            // there is nothing to discard.
            session.log(&format!(
                "a message of {} bytes was refused: the limit is {MAX_LINE_BYTES}",
                buffer.len()
            ));
            output.send_error_with(
                None,
                CODE_INVALID_REQUEST,
                format!(
                    "that message is {} bytes and this server reads at most {MAX_LINE_BYTES}",
                    buffer.len()
                ),
            )?;
            continue;
        }
        // Lossy rather than strict: a client that sends a byte sequence that is not UTF-8 gets a
        // parse error naming the problem, which is recoverable, rather than a `read_line` failure
        // that would end the session.
        let line = String::from_utf8_lossy(&buffer);
        if let Err(error) = dispatch(session, &line, output) {
            session.log(&format!("a reply could not be written: {error}"));
            return Err(error);
        }
    }
    session.log("the client closed the input stream");
    Ok(())
}

/// Read one line, including its terminator, and report how many bytes arrived.
fn read_line<R: BufRead>(input: &mut R, buffer: &mut Vec<u8>) -> io::Result<usize> {
    input.read_until(b'\n', buffer)
}

/// Route one line. Never writes a reply for a notification.
fn dispatch<W: Write>(
    session: &mut Session,
    line: &str,
    output: &mut ProtocolWriter<W>,
) -> io::Result<()> {
    let Some(message) = protocol::classify(line) else {
        return Ok(());
    };
    match message {
        Incoming::Notification { method, params } => {
            session.log(&format!("notification: {method}"));
            match method.as_str() {
                "notifications/initialized" => session.log("the client finished initialising"),
                "notifications/cancelled" => {
                    if let Some(id) = params.get("requestId") {
                        session.cancel(id);
                    } else {
                        session.log("a cancellation arrived with no requestId, so it names nothing");
                    }
                }
                other => session.log(&format!(
                    "notification `{other}` is not one this server acts on; it is not an error, \
                     because a notification is a statement rather than a question"
                )),
            }
            Ok(())
        }
        Incoming::Unroutable { id, code, message } => {
            let data = json!({ "supported_methods": protocol::METHODS });
            output.send_error(id, RpcError::new(code, message).with_data(data))
        }
        Incoming::Request { id, method, params } => {
            if session.is_cancelled(&id) {
                session.log(&format!("request {id} was cancelled and is not being run"));
                return output.send_error(
                    Some(id),
                    RpcError::new(CODE_INVALID_REQUEST, "this request was cancelled"),
                );
            }
            route(session, id, &method, &params, output)
        }
    }
}

/// Route one request to a method.
fn route<W: Write>(
    session: &mut Session,
    id: Value,
    method: &str,
    params: &Value,
    output: &mut ProtocolWriter<W>,
) -> io::Result<()> {
    match method {
        "initialize" => {
            let requested = params.get("protocolVersion").and_then(Value::as_str);
            let instructions = session.instructions();
            session.log(&format!(
                "initialize: client asked for {:?}, answering {:?}",
                requested,
                protocol::negotiate(requested)
            ));
            output.send_result(id, protocol::initialize_result(requested, &instructions))
        }
        "ping" => output.send_result(id, json!({})),
        "tools/list" => {
            let catalogue = crate::tool::catalogue();
            session.log(&format!("tools/list: {} tool(s)", catalogue.len()));
            output.send_result(id, json!({ "tools": catalogue }))
        }
        "tools/call" => call(session, id, params, output),
        other => {
            session.log(&format!("no method named {other}"));
            output.send_error(
                Some(id),
                RpcError::new(
                    CODE_METHOD_NOT_FOUND,
                    format!("there is no method named `{other}`"),
                )
                .with_data(json!({ "supported_methods": protocol::METHODS })),
            )
        }
    }
}

/// Run one tool and wrap the answer.
fn call<W: Write>(
    session: &mut Session,
    id: Value,
    params: &Value,
    output: &mut ProtocolWriter<W>,
) -> io::Result<()> {
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return output.send_error(
            Some(id),
            RpcError::new(
                CODE_INVALID_PARAMS,
                "`tools/call` needs a `name`: the name of the tool to run",
            )
            .with_data(json!({ "tools": crate::tool::names() })),
        );
    };
    let arguments = params.get("arguments");
    let name = name.to_owned();
    let answer = match tools::dispatch(session, &name, arguments) {
        Ok(answer) => answer,
        Err(error) => {
            // The tool was found and could not do the job, which is a result the model has to
            // read, not a protocol fault.
            session.log(&format!("{name} refused: {}", error.verdict_reason));
            tools::refusal(&name, &error)
        }
    };
    let ToolAnswer {
        text,
        structured,
        is_error,
    } = answer;
    output.send_result(
        id,
        serde_json::to_value(protocol::CallToolResult::answered(text, structured, is_error))
            .unwrap_or_else(|error| {
                json!({
                    "content": [{
                        "type": "text",
                        "text": format!("the answer could not be encoded: {error}"),
                    }],
                    "isError": true
                })
            }),
    )
}

/// A one-line summary of the whole tool surface, for a human reading `--help`.
///
/// Not sent to a client: `tools/list` carries the schemas and the descriptions, and a second
/// description of the surface in two places is a second thing to keep in step.
#[must_use]
pub fn surface_summary() -> String {
    crate::tool::catalogue()
        .iter()
        .map(|tool| format!("  {:<14} {}", tool.name, tool.title))
        .collect::<Vec<_>>()
        .join("\n")
}
