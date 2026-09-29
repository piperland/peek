//! The `peek-mcp` binary: an MCP server over stdio.
//!
//! # What this process does, and does not, print
//!
//! **stdout is the protocol channel and this binary writes nothing else to it, ever.** Every byte
//! it emits there is one JSON-RPC message, written by [`peek_mcp::writer::ProtocolWriter`] and by
//! nothing else. There is no banner, no version line and no startup message, because a client's
//! first read has to be the response to its `initialize` and a banner would be a parse error.
//!
//! That includes `--help` and `--version`, which go to **stderr**. A person running the binary sees
//! them in a terminal either way, and putting them on stdout would mean the invariant above has an
//! exception somebody has to remember — and an exception is how the invariant dies. A test in
//! `tests/transport.rs` scans this crate's sources for `print!` and `println!` and fails if either
//! appears outside the writer; that test can only be honest if the rule has no exceptions.
//!
//! # Nothing leaves the machine
//!
//! There is no socket, no listener, no client, and no environment variable this binary reads to
//! find one. Contract M1 ("no source leaves the machine by default") and M3 ("MCP/daemon bind
//! safely — loopback-only unless explicitly configured") are satisfied here by the transport itself
//! rather than by a policy: stdio has no address to bind. That is a large part of why the transport
//! is hand-written rather than taken from a general-purpose SDK — see the crate documentation.
//!
//! # Usage
//!
//! ```text
//! peek-mcp [--root <path>] [--help] [--version]
//! ```
//!
//! `--root` defaults to the working directory, which is where an agent launches its servers from.
//! The repository is identified once, at startup, and every tool answers about that one.

use std::io::BufReader;
use std::path::PathBuf;
use std::process::ExitCode;

use peek_mcp::Session;
use peek_mcp::server::serve;
use peek_mcp::writer::ProtocolWriter;
use peek_mcp::StderrLog;

const USAGE: &str = "\
peek-mcp — the Peek MCP server, over stdio

USAGE:
    peek-mcp [--root <path>] [--help] [--version]

OPTIONS:
    --root <path>    The repository to answer about. Defaults to the working directory.
                     Identified once at startup; the index lives under the OS cache directory and
                     is never written inside the repository.
    --help           Print this and exit.
    --version        Print the version and exit.

    This text goes to stderr, not stdout: stdout is the protocol channel, and the one rule this
    binary keeps is that nothing but a protocol message is ever written there.

PROTOCOL:
    JSON-RPC 2.0 over newline-delimited JSON on stdin and stdout, per the Model Context Protocol.

TOOLS:
";

fn main() -> ExitCode {
    let options = match Options::parse(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("peek-mcp: {message}");
            eprint!("\n{USAGE}");
            return ExitCode::from(2);
        }
    };

    if options.help {
        eprint!("{USAGE}{}\n", peek_mcp::server::surface_summary());
        return ExitCode::SUCCESS;
    }
    if options.version {
        eprintln!("peek-mcp {}", peek_mcp::VERSION);
        return ExitCode::SUCCESS;
    }

    let Some(root) = options.root_or_cwd() else {
        return ExitCode::from(2);
    };

    let mut session = Session::new(&root, Box::new(StderrLog));
    let stdin = BufReader::new(std::io::stdin());
    // The one handle on stdout this process creates. Nothing else in the crate can reach it, which
    // is the property `tests/transport.rs` checks by running this binary and parsing every byte.
    let mut output = ProtocolWriter::new(std::io::stdout().lock());

    match serve(&mut session, stdin, &mut output) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("peek-mcp: the session ended: {error}");
            ExitCode::FAILURE
        }
    }
}

/// What the binary was asked to do.
struct Options {
    root: Option<PathBuf>,
    help: bool,
    version: bool,
}

impl Options {
    /// Read the arguments.
    ///
    /// An unknown argument is an error rather than a shrug — the same rule the tools follow, and
    /// for the same reason: a misspelt flag that is ignored is a session that behaves differently
    /// from the one that was asked for. A `--root` that is not a directory is refused at startup
    /// rather than producing a server whose every tool says the repository could not be read.
    fn parse<I: Iterator<Item = String>>(arguments: I) -> Result<Self, String> {
        let mut options = Self {
            root: None,
            help: false,
            version: false,
        };
        let mut arguments = arguments.peekable();
        while let Some(argument) = arguments.next() {
            match argument.as_str() {
                "--help" | "-h" => options.help = true,
                "--version" | "-V" => options.version = true,
                "--root" => {
                    let value = arguments
                        .next()
                        .ok_or_else(|| "`--root` needs a path after it".to_owned())?;
                    options.root = Some(PathBuf::from(value));
                }
                other => {
                    return Err(format!(
                        "`{other}` is not an option this server takes; it accepts `--root`, \
                         `--help` and `--version`"
                    ));
                }
            }
        }
        if let Some(root) = &options.root
            && !root.is_dir()
        {
            return Err(format!("{} is not a directory", root.display()));
        }
        Ok(options)
    }

    /// The repository to answer about: the one named, or the working directory.
    ///
    /// Reports its own failure on stderr and yields `None`, because there is nowhere else to say it
    /// and a process that starts a session with no repository answers every question with a
    /// failure the client did not cause.
    fn root_or_cwd(&self) -> Option<PathBuf> {
        if let Some(root) = &self.root {
            return Some(root.clone());
        }
        match std::env::current_dir() {
            Ok(root) => Some(root),
            Err(error) => {
                eprintln!(
                    "peek-mcp: neither `--root` nor the working directory names a repository, and \
                     the working directory could not be read: {error}"
                );
                eprint!("\n{USAGE}");
                None
            }
        }
    }
}
