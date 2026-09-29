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
//!
//! **A command that declined is reported the same way as one that answered.** The failure becomes
//! an [`peek_cli::Output`] whose answer is the refusal, and it is printed on standard output in the
//! mode that was asked for, so a `--json` caller reads the reason instead of finding nothing where
//! an answer should be. The one thing that stays on standard error is the usage text for a command
//! line the parser rejected: that is a help document rather than an answer, and it is for a person
//! at a terminal.

use std::io::Write;

use peek_cli::args::{self, Command, UsageError};
use peek_cli::progress::Progress;
use peek_cli::{Mode, Output, run};

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

/// Print one answer on standard output, in the mode that was asked for.
fn write_answer(output: &Output, mode: &Mode) -> u8 {
    let mut stdout = std::io::stdout();
    let _ = writeln!(stdout, "{}", peek_cli::render(output, mode));
    let _ = stdout.flush();
    output.exit_code
}

/// The output mode a command line asked for, readable from a line that did not parse.
///
/// The parser is what rejects a bad command line, so it cannot also be what reports the mode — and
/// a caller that piped `--json` into a command line the parser refused still wants JSON back, which
/// is the case the JSON mode exists for. Read off the raw arguments, and read the spellings out of
/// the flag table rather than written down here, so a flag that is renamed cannot be honoured under
/// its old name in exactly the case where nothing else can be.
fn mode_of(argv: &[std::ffi::OsString]) -> Mode {
    let asked = |long: &str| {
        args::FLAGS
            .iter()
            .find(|flag| flag.long == long)
            .is_some_and(|flag| {
                argv.iter().any(|argument| {
                    let text = argument.to_string_lossy();
                    text == format!("--{}", flag.long)
                        || flag.short.is_some_and(|letter| text == format!("-{letter}"))
                })
            })
    };
    Mode {
        json: asked("json"),
        quiet: asked("quiet"),
    }
}

/// Report a command line that was not understood, the usage for the command it concerns, and stop.
///
/// The refusal is the answer, so it goes to standard output like any other. The usage text beside
/// it is an aid rather than an answer, so it goes to standard error, and only in the human mode:
/// a JSON consumer asked for a machine-readable result and a help document is not one.
fn usage_failure(error: &UsageError, command: &str, mode: &Mode) -> std::process::ExitCode {
    let output = peek_cli::usage_failed(command, error);
    let code = write_answer(&output, mode);
    if !mode.is_json() {
        let mut stderr = std::io::stderr();
        // The per-command line when the error names a command, and the whole table when it does
        // not — because an unknown flag has no command to be specific *about*, and a reader who
        // typed `peek contex` needs to see the list.
        match error.command() {
            Some(command) => {
                let _ = write!(stderr, "{}", args::command_usage(command));
                let _ = writeln!(stderr, "run `peek --help` for every command");
            }
            None => {
                let _ = write!(stderr, "{}", args::help_text());
            }
        }
    }
    std::process::ExitCode::from(code)
}

/// The process entry point.
fn main() -> std::process::ExitCode {
    let argv: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let invocation = match args::parse(argv.clone()) {
        Ok(invocation) => invocation,
        Err(error) => return usage_failure(&error, &args::named_command(&argv), &mode_of(&argv)),
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
    // other. Only a *refused* command line is reported differently, and it is reported as an
    // answer too — there is no longer a path where a declined command prints nothing to stdout.
    let _ = matches!(invocation.command, Command::Help | Command::Version);

    let mut progress = Stderr {
        quiet: invocation.quiet,
    };
    let code = match run(&invocation, &mut progress) {
        Ok(output) => write_answer(&output, &mode),
        Err(failure) => write_answer(&peek_cli::declined(&failure), &mode),
    };
    std::process::ExitCode::from(code)
}
