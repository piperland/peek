//! The Peek MCP server: the same engine, spoken to over the Model Context Protocol.
//!
//! # What this crate is, and is not
//!
//! It is an **adapter**. Every tool here resolves an argument, calls one function in
//! [`peek_core`], and serialises what came back. There is no query logic, no ranking policy and no
//! resolution rule in this crate, because a second copy of any of those is a second place for the
//! engine's answers to stop matching its own behaviour. Contract C3 says CLI and MCP route to the
//! same engine module; this crate is the proof of that claim, and the way it is proven is that the
//! only interesting thing in it is the framing.
//!
//! # Why the protocol is written out rather than depended on
//!
//! The obvious choice is `rmcp`, the official Rust SDK. It is Apache-2.0 and it requires `tokio`
//! unconditionally — `tokio` and `tokio-util` are non-optional dependencies at version 3.4.1, and
//! there is no feature set that removes them, because a server is a `Service`, and a `Service` is
//! a future. This engine is entirely synchronous: `rusqlite`, Tree-sitter, `ignore` and `notify`
//! are all blocking, and every one of them is called from a plain function today. Adopting `rmcp`
//! would put a multi-threaded runtime, a scheduler, and roughly ninety crates in front of every
//! `peek doctor` and every context compile, in exchange for three message shapes and a framing
//! loop. That is a bad trade for a tool whose selling point is that it is small, local and
//! deterministic.
//!
//! So the JSON-RPC layer is [`protocol`] and [`writer`], over `serde_json`, which the workspace
//! already depends on. What is *not* hand-rolled is the engine, the budget arithmetic, or the
//! uncertainty model — the parts that have to be right are the parts that already are.
//!
//! # The four rules this crate exists to keep
//!
//! 1. **stdout is the protocol channel and nothing else.** The server never has a handle on the
//!    process's stdout except the one it was handed in [`writer::ProtocolWriter`]. Diagnostics go
//!    to stderr through [`session::Log`]. A single stray `println!` corrupts the stream and the
//!    client dies with a parse error that names nothing; three separate tests in `tests/` defend
//!    this, the strictest being a scan of this crate's own code for a called printing macro or a
//!    handle on the process's standard output.
//! 2. **A budget is a hard input.** See [`tools::context`].
//! 3. **Uncertainty reaches the caller.** Every edge in a response is the engine's own
//!    [`peek_core::model::Relation`], carrying its typed
//!    [`peek_core::model::ResolutionState`], and an ambiguous target returns candidates rather
//!    than a pick.
//! 4. **Two identical calls produce identical bytes.** Every response is built from structs, never
//!    from a hash map, so the serialisation order is a property of the type rather than of a seed.
//!
//! # The tool surface
//!
//! Nine tools, and the reasoning for each is in [`tool`]. They are thin by construction:
//!
//! | Tool | Answers |
//! |---|---|
//! | [`index`](tools::index::index) | build or refresh the index, with all five resolution states |
//! | [`index_status`](tools::index::status) | what the index holds, measured |
//! | [`explain`](tools::explain::explain) | why an edge exists and how sure the engine is |
//! | [`callers`](tools::walk::callers) | who uses this, one hop |
//! | [`callees`](tools::walk::callees) | what this uses, one hop |
//! | [`dependents`](tools::walk::dependents) | who depends on this, `depth` hops |
//! | [`context`](tools::context::context) | a token-budgeted slice of the repository |
//! | [`doctor`](tools::doctor::doctor) | what is wrong with the index |
//! | [`watch_start`](tools::watch::start) | refresh the index as files change |
//! | [`watch_stop`](tools::watch::stop) | stop it, flushing what is pending |

pub mod outcome;
pub mod params;
pub mod protocol;
pub mod server;
pub mod session;
pub mod tool;
pub mod tools;
pub mod writer;

pub use outcome::{Outcome, ToolError, Verdict};
pub use server::serve;
// `StderrLog` beside `Log` because the binary needs the two together: it opens a session, and
// where that session's diagnostics go is not a choice the binary should have to reach into
// `session` to make.
pub use session::{Log, Session, StderrLog};
pub use tool::Tool;

/// The engine version this server reports in `initialize`.
///
/// From the same constant the library reports, so a client that sees two version strings is
/// looking at two builds rather than at two places that name the same build differently.
pub const VERSION: &str = peek_core::VERSION;
