//! The dispatch loop: a line in, a line out, until the client goes away.
//!
//! # Why the loop is a function over two streams
//!
//! [`serve`] takes its input and its output as parameters. That is the whole reason the protocol
//! channel can be defended: nothing inside the server holds a handle on the process's file
//! descriptor 1, so a tool handler has nothing to print to even by accident. The alternative —
//! a `serve()` that reached for the process's own standard input and standard output itself — is
//! one line shorter and makes the property untestable, untestable being the same as absent.
//!
//! # The loop's rules
//!
//! * **One JSON object per line**, read as bytes so that invalid UTF-8 becomes a parse error rather
//!   than a dead server.
//! * **A notification is never answered.** Answering one desynchronises a client that is waiting
//!   for the reply to its *next* request.
//! * **A line that is not a routable request always produces a reply**, unless it had no id to
//!   reply to. A client that sent something malformed and got silence has no way to know the
//!   server is alive. A line too long to read whole is the one case where the id may be past the
//!   bound, and it is recovered from what was read rather than dropped.
//! * **Every reply is flushed before the loop continues.** See [`crate::writer`].
//! * **A cancelled request does not run, and the number is then free.** See [`Session::cancel`].
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

// `Read` is not imported: `Lines` is bounded by `BufRead`, whose supertrait is `Read`, and the
// read inside `Lines::next` goes through that bound rather than through a concrete handle.
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
/// A bound on memory, and a real one: a line is read in chunks of [`READ_CHUNK_BYTES`] and at most
/// this many bytes of it are kept, so the allocation a client would otherwise be able to force
/// never happens. A bound applied after the line has been read in full is a number in a constant,
/// not a bound.
///
/// A line over the bound is refused and the rest of it is discarded up to the newline, because the
/// alternative is a stream that cannot be resynchronised. It is refused **on the id it carried**:
/// these bytes are a prefix of a message the client sent, and the id is usually inside that
/// prefix, so there is one more statement between a client and the answer it is owed. Where the id
/// is not in the prefix — because the padding came first and pushed it past the bound — the reply
/// carries `null` and the diagnostic stream says why, because a client waiting on a number can do
/// nothing at all with an answer it cannot match.
pub const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;

/// How much of a line is read at a time.
///
/// A constant rather than a literal because it is the granularity of the discard, not the limit: a
/// small one means an over-long line is dropped in bounded steps rather than in a single read.
const READ_CHUNK_BYTES: usize = 64 * 1024;

/// Read messages, dispatch them, and write replies until the input ends.
pub fn serve<R: BufRead, W: Write>(
    session: &mut Session,
    input: R,
    output: &mut ProtocolWriter<W>,
) -> io::Result<()> {
    let mut lines = Lines::new(input);
    let mut buffer: Vec<u8> = Vec::with_capacity(4096);
    loop {
        buffer.clear();
        let (read, truncated) = match lines.next(&mut buffer) {
            Ok(outcome) => outcome,
            Err(error) => {
                session.log(&format!("the input stream failed: {error}"));
                return Err(error);
            }
        };
        if read == 0 {
            break;
        }
        if truncated {
            session.log(&format!(
                "a message of more than {MAX_LINE_BYTES} bytes was refused and the rest of the \
                 line was discarded; the limit is {MAX_LINE_BYTES}"
            ));
            // Recovered from the prefix rather than answered with a null id unconditionally, so a
            // client whose request was too long is told on its own id and is not left waiting for a
            // reply that is never coming.
            let id = id_in_prefix(&buffer);
            if id.is_none() {
                session.log(
                    "the refused message carried no id within the bytes that were read, so the \
                     refusal has to go out with a null id",
                );
            }
            output.send_error(
                id,
                RpcError::new(
                    CODE_INVALID_REQUEST,
                    format!(
                        "that message is longer than {MAX_LINE_BYTES} bytes and this server reads \
                         at most {MAX_LINE_BYTES}"
                    ),
                ),
            )?;
            continue;
        }
        // Lossy rather than strict: a client that sends a byte sequence that is not UTF-8 gets a
        // parse error naming the problem, which is recoverable, rather than a read failure that
        // would end the session.
        let line = String::from_utf8_lossy(&buffer);
        if let Err(error) = dispatch(session, &line, output) {
            session.log(&format!("a reply could not be written: {error}"));
            return Err(error);
        }
    }
    session.log("the client closed the input stream");
    Ok(())
}

/// One line at a time out of a byte stream, with a bound on what it will hold for one.
///
/// # Why the leftover is kept rather than dropped
///
/// A read is larger than one line, almost always. The bytes after the newline in the block that
/// read returned are the beginning of the **next** message, and a reader that discards them loses
/// it — silently, because a pipe will not give them back and the client is left waiting for a reply
/// that was never going to be produced. So the unconsumed tail stays in [`Lines::chunk`] and the
/// next call starts there, and the only way a read and a line line up is when the client sent one
/// message and waited.
///
/// # Why there are two buffers
///
/// `chunk` is fixed at [`READ_CHUNK_BYTES`] and holds at most one read's worth. `buffer` is the
/// caller's, grows to at most [`MAX_LINE_BYTES`], and is what the line is read out of. Neither can
/// be made large by a client, and neither is a copy of the other.
struct Lines<R> {
    input: R,
    /// The bytes one read returned, of which `chunk[start..end]` is not yet part of a line.
    chunk: Vec<u8>,
    start: usize,
    end: usize,
}

impl<R: BufRead> Lines<R> {
    fn new(input: R) -> Self {
        Self {
            input,
            chunk: vec![0_u8; READ_CHUNK_BYTES],
            start: 0,
            end: 0,
        }
    }

    /// Read the next line into `buffer`, and say whether the bound discarded any of it.
    ///
    /// `(0, false)` is the end of the input. The terminator is consumed either way, so the stream
    /// is in step for the next message — that is the property that makes refusing an over-long line
    /// safe at all. `buffer` is appended to, never cleared, so a caller reusing one buffer across
    /// messages pays for the allocation once.
    fn next(&mut self, buffer: &mut Vec<u8>) -> io::Result<(usize, bool)> {
        let mut read = 0_usize;
        let mut truncated = false;
        loop {
            let newline = self.chunk[self.start..self.end]
                .iter()
                .position(|byte| *byte == b'\n');
            let Some(at) = newline else {
                let held = self.end - self.start;
                truncated |= keep(buffer, &self.chunk[self.start..self.end]);
                read += held;
                self.start = 0;
                self.end = 0;
                match self.input.read(&mut self.chunk) {
                    Ok(0) => {
                        // End of input with no terminator: the last line is still a line, and
                        // refusing to answer it would be refusing a request because the client
                        // closed its pipe.
                        return Ok((read, truncated));
                    }
                    Ok(count) => self.end = count,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error),
                }
                continue;
            };
            // The `|=` is the point: a line can be brought over the bound by the very block that
            // carries its terminator, and a bound that is not noticed there is not a bound.
            let terminator = self.start + at;
            truncated |= keep(buffer, &self.chunk[self.start..terminator]);
            read += at + 1;
            // Past the newline, not onto it: the terminator belongs to this line.
            self.start = terminator + 1;
            return Ok((read, truncated));
        }
    }
}

/// Append as much of `bytes` as the bound allows, and report whether the bound was reached.
fn keep(buffer: &mut Vec<u8>, bytes: &[u8]) -> bool {
    let room = MAX_LINE_BYTES.saturating_sub(buffer.len());
    if bytes.len() <= room {
        buffer.extend_from_slice(bytes);
        return false;
    }
    buffer.extend_from_slice(&bytes[..room]);
    true
}

/// The `id` of the JSON-RPC message these bytes are the start of, or `None`.
///
/// A tolerant scan rather than a parse, because a truncated line does not parse — that is what
/// truncation means. Three things make it a scan of the *envelope* rather than a search for a word:
///
/// * **Depth.** A member named `id` inside `params` is the caller's own argument, not the request's
///   identity, so only a member of the top-level object counts.
/// * **Position.** Only a string where a member's *name* belongs counts, so a string *value* that
///   happens to read `"id"` is not taken for a key.
/// * **Completion.** A string that does not close inside the bytes available means the prefix ends
///   inside it, and there is nothing after it; the scan stops rather than reading on into a string
///   that is still open.
///
/// The value itself is read with a real JSON parser, so an unterminated number or string yields
/// `None` rather than half of one. `None` is an answer and not a failure: it means the id was not
/// in the bytes that were read, and the caller says so on the diagnostic stream before the
/// null-id refusal goes out.
fn id_in_prefix(bytes: &[u8]) -> Option<Value> {
    if bytes.first() != Some(&b'{') {
        return None;
    }
    // The opening brace is consumed above rather than counted, so a member of the top-level object
    // sits at depth zero and anything inside `params` is already below it.
    let mut index = 1_usize;
    let mut depth = 0_i32;
    // Whether the next token is a member's name. Set by the comma and the colon that follow one and
    // cleared by consuming a name, so a string in a value position is never read as a key.
    let mut name_next = true;
    while index < bytes.len() {
        match bytes[index] {
            b'{' | b'[' => {
                depth += 1;
                index += 1;
            }
            b'}' | b']' => {
                depth -= 1;
                index += 1;
                if depth < 0 {
                    return None;
                }
            }
            b',' | b':' if depth == 0 => {
                name_next = true;
                index += 1;
            }
            b'"' => {
                let (text, next) = json_string(bytes, index)?;
                index = next;
                if depth == 0 && name_next && text == "id" {
                    let mut after = index;
                    while after < bytes.len() && bytes[after].is_ascii_whitespace() {
                        after += 1;
                    }
                    if bytes.get(after) != Some(&b':') {
                        return None;
                    }
                    return json_value(bytes, after + 1).map(|(value, _)| value);
                }
                name_next = false;
            }
            _ => index += 1,
        }
    }
    None
}

/// One JSON string starting at `open`, as its text, and the index after its closing quote.
///
/// `None` when the string does not close within the bytes available, which is the ordinary answer
/// for a prefix that ends inside one.
fn json_string(bytes: &[u8], open: usize) -> Option<(String, usize)> {
    let mut index = open + 1;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index += 2,
            b'"' => {
                let text = std::str::from_utf8(&bytes[open + 1..index]).ok()?;
                return Some((text.to_owned(), index + 1));
            }
            _ => index += 1,
        }
    }
    None
}

/// One complete JSON value starting at `open`, and where it ended.
fn json_value(bytes: &[u8], open: usize) -> Option<(Value, usize)> {
    let text = std::str::from_utf8(&bytes[open..]).ok()?;
    let mut de = serde_json::Deserializer::from_str(text).into_iter::<Value>();
    let value = de.next()?.ok()?;
    Some((value, open + de.byte_offset()))
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
                        session
                            .log("a cancellation arrived with no requestId, so it names nothing");
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
            // Taken, not read: a cancellation is about one request, so the number it used is free
            // again the moment the refusal goes out. See [`Session::take_cancelled`].
            if session.take_cancelled(&id) {
                session.log(&format!(
                    "request {id} was cancelled and is not being run; the number is now free for \
                     the client to reuse"
                ));
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
    // `name` is used after `arguments` is taken out of the same object. That is two shared borrows
    // of one value, which is fine; an owned copy here would be a clone nobody asked for.
    let answer = match tools::dispatch(session, name, arguments) {
        Ok(answer) => answer,
        Err(error) => {
            // The tool was found and could not do the job, which is a result the model has to
            // read, not a protocol fault.
            session.log(&format!("{name} refused: {}", error.verdict_reason));
            tools::refusal(tool_name(name), &error)
        }
    };
    let ToolAnswer {
        text,
        structured,
        is_error,
    } = answer;
    output.send_result(
        id,
        serde_json::to_value(protocol::CallToolResult::answered(
            text, structured, is_error,
        ))
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

/// The `'static` name of a tool, so a refusal can say which tool refused.
///
/// The name arrived from a client as a borrowed string, but every tool in the catalogue is a
/// compile-time constant, so this returns the constant rather than leaking the borrow. A name that
/// is *not* in the catalogue comes back as `"unknown"`, which is the truth about it: `dispatch`
/// already refused it and `refusal` is only ever reached for a name the client sent.
fn tool_name(name: &str) -> &'static str {
    crate::tool::names()
        .into_iter()
        .find(|known| *known == name)
        .unwrap_or("unknown")
}
