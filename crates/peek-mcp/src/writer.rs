//! The one place in this crate that writes to the protocol stream.
//!
//! # Why this module is separate
//!
//! On a stdio transport, stdout **is** the wire. A single stray `println!` — from a dependency's
//! initialisation, from a `Drop` impl, from a debug statement somebody left behind — puts a line
//! that is not JSON into the middle of the stream, and the client dies with a parse error that
//! names a byte offset and nothing else. There is no partial credit: the session is over.
//!
//! The defence is structural rather than a code review. This type takes the output stream **as a
//! parameter**. Nothing in this crate holds a `Stdout` handle, asks the standard library for the
//! process's standard output, or otherwise has a route to file descriptor 1; the binary constructs
//! the writer once in [`crate::main`] and hands it down. A tool handler physically cannot print,
//! because the only thing it can reach is a `&Store`.
//!
//! Two tests back that up, in `tests/transport.rs`:
//!
//! * a **source scan** over every file in this crate for a *called* printing macro and for the
//!   process's standard output. It reads each file's code rather than its text, so neither a
//!   paragraph of documentation about `println!` nor a `let _ = println!(..)` can hide from it;
//! * a **subprocess run** of the real binary over a real session, asserting that every byte on
//!   stdout parses as a JSON-RPC message and that diagnostics arrived on stderr instead.
//!
//! # Flushing
//!
//! Every message is flushed before the call returns. A server that buffers and waits for a full
//! block will hang against a client that has sent one request and is waiting for one reply, and the
//! symptom is a client that times out with nothing in the log. One flush per message is the price
//! of never deadlocking, and at the rate a human or a model drives this — a few calls a second —
//! the syscall is not measurable.
//!
//! # When serialisation fails
//!
//! It is handled rather than propagated, because a failure here means the client gets nothing at
//! all and waits. Every type this crate serialises derives `Serialize`, so a failure is a bug
//! rather than a condition, and the fallback is a static internal-error frame that a client can
//! parse and a human can read. The `io::Error` from a genuine write failure *is* propagated,
//! because that one means the pipe is gone and there is nothing left to say.

use std::io::{self, Write};

use serde::Serialize;
use serde_json::Value;

use crate::protocol::{CODE_INTERNAL_ERROR, ErrorResponse, RpcError};

/// Writes protocol messages, one per line, to a stream it was given.
pub struct ProtocolWriter<W: Write> {
    out: W,
}

impl<W: Write> ProtocolWriter<W> {
    /// Wrap a stream.
    pub fn new(out: W) -> Self {
        Self { out }
    }

    /// A successful reply.
    pub fn send_result(&mut self, id: Value, result: Value) -> io::Result<()> {
        self.send(&crate::protocol::Response::new(id, result))
    }

    /// A failed reply.
    pub fn send_error(&mut self, id: Option<Value>, error: RpcError) -> io::Result<()> {
        self.send(&ErrorResponse::new(id, error))
    }

    /// Hand the stream back, so a caller that owns it can close it.
    pub fn into_inner(self) -> W {
        self.out
    }

    /// Serialise one message, write it, terminate the line, and flush.
    ///
    /// The serialisation is done into a buffer first so that a message which cannot be serialised
    /// produces a well-formed fallback rather than a half-written line. A half-written line is the
    /// worst outcome available: the next read gets it as a parse error and the real message is
    /// gone.
    fn send<T: Serialize>(&mut self, message: &T) -> io::Result<()> {
        let bytes = match serde_json::to_vec(message) {
            Ok(bytes) => bytes,
            Err(error) => {
                let fallback = ErrorResponse::new(
                    None,
                    RpcError::new(
                        CODE_INTERNAL_ERROR,
                        format!("this server built a response it could not encode: {error}"),
                    ),
                );
                // Encoded with `to_vec` on a type of plain strings and integers, so if even this
                // fails there is nothing further to try and the stream is already lost.
                let bytes = serde_json::to_vec(&fallback).unwrap_or_else(|_| {
                    br#"{"jsonrpc":"2.0","id":null,"error":{"code":-32603,"message":"response encoding failed","data":null}}"#.to_vec()
                });
                self.out.write_all(&bytes)?;
                self.out.write_all(b"\n")?;
                return self.out.flush();
            }
        };
        self.out.write_all(&bytes)?;
        self.out.write_all(b"\n")?;
        self.out.flush()
    }
}
