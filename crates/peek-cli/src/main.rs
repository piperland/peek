//! The `peek` binary.
//!
//! Five things and nothing else: apply `--index-dir`, parse argv, run, print, exit with the code
//! the library chose. Everything a test would want to assert on lives in `peek_cli`, which is why
//! this file is this short.
//!
//! **Standard output carries the answer; standard error carries narration.** A caller that pipes
//! stdout into a file or a JSON parser must not find a progress line in it, so `--quiet` gates the
//! narration sink and nothing else — the answer is byte-identical either way, and that is a test
//! in the library rather than a promise here.

use std::io::Write;

use peek_cli::args::{self, Command, UsageError};
use peek_cli::progress::Progress;
use peek_cli::{Mode, run};

/// Print each narration line to standard error, unless `--quiet`.
struct Stderr {
    quiet: bool,
}

impl Progress for Stderr {
    fn note(&mut self, line: String) {
        if !self.quiet {
            let _ = writeln!(std::io::stderr(), "{line}");
        }
    }
}

/// Print a usage error, the usage for the command it concerns, and stop.
///
/// The per-command line when the error names a command, and the whole table when it does not —
/// because an unknown flag has no command to be specific *about*, and a reader who typed
/// `peek contex` needs to see the list.
fn usage_failure(error: &UsageError) -> std::process::ExitCode {
    let mut stderr = std::io::stderr();
    let _ = writeln!(stderr, "peek: {}\n", error.message());
    match error.command() {
        Some(command) => {
            let _ = write!(stderr, "{}", args::command_usage(command));
            let _ = writeln!(stderr, "run `peek --help` for every command");
        }
        None => {
            let _ = write!(stderr, "{}", args::help_text());
        }
    }
    std::process::ExitCode::from(peek_cli::exit::EXIT_USAGE)
}

/// The process entry point.
fn main() -> std::process::ExitCode {
    let argv: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let invocation = match args::parse(argv) {
        Ok(invocation) => invocation,
        Err(error) => return usage_failure(&error),
    };

    // Applied before the library runs, because the index root is process-global in
    // `peek_core::store::paths` and the library deliberately does not mutate global state on a
    // caller's behalf. The value is used verbatim, and an empty one is ignored by the engine.
    if let Some(index_dir) = &invocation.index_dir {
        peek_core::store::paths::set_root_override(Some(index_dir.clone()));
    }

    let mode = Mode {
        json: invocation.json,
        quiet: invocation.quiet,
    };
    // `--help` and `--version` are answers rather than usage errors, so they go to stdout like any
    // other. Only a *refused* command line goes to stderr.
    let _ = matches!(
        invocation.command,
        Command::Help | Command::Version
    );

    let mut progress = Stderr {
        quiet: invocation.quiet,
    };
    match run(&invocation, &mut progress) {
        Ok(output) => {
            let mut stdout = std::io::stdout();
            let _ = writeln!(stdout, "{}", peek_cli::render(&output, &mode));
            let _ = stdout.flush();
            std::process::ExitCode::from(output.exit_code)
        }
        Err(failure) => {
            let mut stderr = std::io::stderr();
            let _ = writeln!(stderr, "{}", failure.render());
            std::process::ExitCode::from(failure.exit_code())
        }
    }
}
