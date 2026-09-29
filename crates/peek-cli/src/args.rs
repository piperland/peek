//! The argument table, and the one function that turns argv into a request.
//!
//! # Why a table and not a parser framework
//!
//! Audit D, section F: `--direction sideways` silently became `Outbound`, `--kind nonsense`
//! silently became no filter, and a `find-symbol` with no arguments silently returned fifty
//! arbitrary symbols. Three silent-wrong-answer bugs in the product's core value proposition, all
//! of them the same shape: a value that was not understood was replaced by a default, and nothing
//! said so.
//!
//! The fix is structural rather than careful. There is **one** function, [`parse`], and it is
//! exhaustive over a [`FLAGS`] table and a [`COMMANDS`] table. Every way an argument can be wrong
//! is a variant of [`UsageError`], so an unknown flag, a repeated flag, a flag used on the wrong
//! command, a value that is not a number, and the wrong number of positionals are five branches of
//! one `match` — not five behaviours spread across five commands.
//!
//! The tables are also the documentation. `--help` is generated from them, so it cannot describe a
//! flag that does not exist or omit one that does.
//!
//! The pattern is [`peek_core::extract::spec`]: a `&'static [Row]` of plain data, a lookup by
//! name, and no per-case string matching anywhere else.
//!
//! # Where the repository comes from
//!
//! Two places, and never both for the same command, so there is no precedence rule to get wrong:
//!
//! * `index` and `watch` name the repository as their positional `[PATH]`, defaulting to the
//!   working directory.
//! * every other command takes the repository from the global `--root`, also defaulting to the
//!   working directory.
//!
//! The defaults are the *same* `.`, so `peek status` in a checkout and `peek index` in the same
//! checkout agree about which tree is meant — and both are canonicalised before anything reads
//! them, so `.`, `./` and an absolute path to the same directory are one repository and one index.

use std::ffi::OsString;
use std::path::PathBuf;

/// One flag the command line accepts.
///
/// `value` is `Some(placeholder)` for a flag that takes a value and `None` for one that does not,
/// which is the only thing the parser needs to know to decide whether the next token is an
/// argument or a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Flag {
    /// The spelling without dashes, e.g. `index-dir`.
    pub long: &'static str,
    /// The single-letter spelling, if there is one.
    pub short: Option<char>,
    /// The placeholder to print in help, e.g. `DIR`. `None` means the flag is a switch.
    pub value: Option<&'static str>,
    /// One line of help, generated into the usage text from this same row.
    pub help: &'static str,
}

const fn flag(
    long: &'static str,
    short: Option<char>,
    value: Option<&'static str>,
    help: &'static str,
) -> Flag {
    Flag {
        long,
        short,
        value,
        help,
    }
}

/// Every flag, in the order `--help` prints them.
///
/// The order is the help text's, so it is a presentation decision stated once here rather than
/// sorted at print time.
pub const FLAGS: &[Flag] = &[
    flag("help", Some('h'), None, "print this help and exit"),
    flag("version", Some('V'), None, "print the version and exit"),
    flag(
        "json",
        None,
        None,
        "emit the answer as JSON on stdout instead of prose",
    ),
    flag(
        "quiet",
        Some('q'),
        None,
        "suppress progress narration; never suppress a finding",
    ),
    flag(
        "root",
        None,
        Some("DIR"),
        "the repository to read or write; defaults to the working directory",
    ),
    flag(
        "index-dir",
        None,
        Some("DIR"),
        "keep this repository's index in DIR instead of the OS cache root",
    ),
    flag(
        "full",
        None,
        None,
        "`index`: clear the existing index and build it again",
    ),
    flag(
        "budget",
        None,
        Some("N"),
        "`context`: the token ceiling for the answer. Required",
    ),
    flag(
        "depth",
        None,
        Some("N"),
        "`dependents`: how many hops to walk. Defaults to 1",
    ),
    flag(
        "quiet-for",
        None,
        Some("MS"),
        "`watch`: how long a batch stays open without a new event. Defaults to 200",
    ),
    flag(
        "max-batch",
        None,
        Some("MS"),
        "`watch`: close a batch after this long even without a quiet gap",
    ),
];

/// The flags every command accepts.
pub const GLOBAL_FLAGS: &[&str] = &["help", "version", "json", "quiet", "root", "index-dir"];

/// Where a command reads the repository from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootSource {
    /// The first positional, spelled `[PATH]`. Only `index` and `watch`, where the path *is* the
    /// thing being operated on.
    Positional,
    /// The global `--root`.
    Flag,
}

/// One command, with the shape of its arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandSpec {
    /// The name as typed, e.g. `context`.
    pub name: &'static str,
    /// The positional arguments, with the optional ones in brackets.
    pub positionals: &'static [&'static str],
    /// How many of them must be present.
    pub required: usize,
    /// Where the repository comes from.
    pub root: RootSource,
    /// The non-global flags this command accepts. A flag outside this list is an error, so
    /// `peek status --budget 10` is refused rather than ignored.
    pub flags: &'static [&'static str],
    /// One line for the command list.
    pub summary: &'static str,
}

const fn spec(
    name: &'static str,
    positionals: &'static [&'static str],
    required: usize,
    root: RootSource,
    flags: &'static [&'static str],
    summary: &'static str,
) -> CommandSpec {
    CommandSpec {
        name,
        positionals,
        required,
        root,
        flags,
        summary,
    }
}

/// Every command, in the order the contract names them.
pub const COMMANDS: &[CommandSpec] = &[
    spec(
        "index",
        &["[PATH]"],
        0,
        RootSource::Positional,
        &["full"],
        "build or refresh the index",
    ),
    spec(
        "watch",
        &["[PATH]"],
        0,
        RootSource::Positional,
        &["quiet-for", "max-batch"],
        "watch the repository and apply each batch",
    ),
    spec(
        "explain",
        &["<TARGET>"],
        1,
        RootSource::Flag,
        &[],
        "why this declaration exists and what it points at",
    ),
    spec(
        "callers",
        &["<TARGET>"],
        1,
        RootSource::Flag,
        &[],
        "what reaches this, one hop",
    ),
    spec(
        "callees",
        &["<TARGET>"],
        1,
        RootSource::Flag,
        &[],
        "what this reaches, one hop",
    ),
    spec(
        "dependents",
        &["<TARGET>"],
        1,
        RootSource::Flag,
        &["depth"],
        "what depends on this, N hops",
    ),
    spec(
        "context",
        &["<TARGET>"],
        1,
        RootSource::Flag,
        &["budget"],
        "a token-budgeted slice of the repository",
    ),
    spec(
        "doctor",
        &[],
        0,
        RootSource::Flag,
        &[],
        "diagnose the index and say what is wrong with it",
    ),
    spec(
        "status",
        &[],
        0,
        RootSource::Flag,
        &[],
        "generation, counts, index location and durability",
    ),
    spec(
        "rm",
        &["<PATH>"],
        1,
        RootSource::Flag,
        &[],
        "remove a file or a subtree from the index",
    ),
];

/// What was asked for.
///
/// Every variant carries the repository root as a `PathBuf` rather than a `String`, because a
/// path on disk is not required to be UTF-8 and refusing to open a repository over one undecodable
/// byte in its name would be a defect, not a safety property. Targets and `rm` arguments *are*
/// `String`, because a qualified name and a repository-relative path are both `String` in the
/// model and there is nothing a non-UTF-8 target could mean.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Print the usage text.
    Help,
    /// Print the version.
    Version,
    /// Build or refresh the index for a repository.
    Index {
        /// The repository root.
        root: PathBuf,
        /// Clear the index before building.
        full: bool,
    },
    /// Watch a repository and apply each batch.
    Watch {
        /// The repository root.
        root: PathBuf,
        /// How long a batch stays open without a new event.
        quiet_for_ms: u64,
        /// The oldest a batch may get before it is closed regardless of the quiet period.
        max_batch_ms: u64,
    },
    /// Explain a target.
    Explain {
        /// The repository root.
        root: PathBuf,
        /// A repository path, a qualified name, or a bare name.
        target: String,
    },
    /// One hop inbound.
    Callers {
        /// The repository root.
        root: PathBuf,
        /// The target string, verbatim.
        target: String,
    },
    /// One hop outbound.
    Callees {
        /// The repository root.
        root: PathBuf,
        /// The target string, verbatim.
        target: String,
    },
    /// N hops inbound.
    Dependents {
        /// The repository root.
        root: PathBuf,
        /// The target string, verbatim.
        target: String,
        /// Hops to walk.
        depth: u32,
    },
    /// Compile a token-budgeted context pack.
    Context {
        /// The repository root.
        root: PathBuf,
        /// The target string, verbatim.
        target: String,
        /// The requested ceiling in tokens, or `None` when no `--budget` was given. Kept distinct
        /// from a budget of zero so the refusal can say which mistake was made.
        budget: Option<u64>,
    },
    /// Diagnose the index.
    Doctor {
        /// The repository root.
        root: PathBuf,
    },
    /// Report the index's own numbers.
    Status {
        /// The repository root.
        root: PathBuf,
    },
    /// Remove a path from the index.
    Remove {
        /// The repository root.
        root: PathBuf,
        /// The path, as typed: relative to the working directory, or absolute.
        path: String,
    },
}

impl Command {
    /// The name this command is invoked by, for output and for error messages.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Command::Help => "help",
            Command::Version => "version",
            Command::Index { .. } => "index",
            Command::Watch { .. } => "watch",
            Command::Explain { .. } => "explain",
            Command::Callers { .. } => "callers",
            Command::Callees { .. } => "callees",
            Command::Dependents { .. } => "dependents",
            Command::Context { .. } => "context",
            Command::Doctor { .. } => "doctor",
            Command::Status { .. } => "status",
            Command::Remove { .. } => "rm",
        }
    }

    /// The repository root, or `None` for the two commands that read no index.
    pub fn root(&self) -> Option<&PathBuf> {
        match self {
            Command::Help | Command::Version => None,
            Command::Index { root, .. }
            | Command::Watch { root, .. }
            | Command::Explain { root, .. }
            | Command::Callers { root, .. }
            | Command::Callees { root, .. }
            | Command::Dependents { root, .. }
            | Command::Context { root, .. }
            | Command::Doctor { root }
            | Command::Status { root }
            | Command::Remove { root, .. } => Some(root),
        }
    }
}

/// A parsed command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    /// `--json`, if given.
    pub json: bool,
    /// `--index-dir`, if given. Applied by the binary before the library runs, because the index
    /// location is process-global in [`peek_core::store::paths`].
    pub index_dir: Option<PathBuf>,
    /// Suppress progress narration. Affects narration and nothing else.
    pub quiet: bool,
    /// What to do.
    pub command: Command,
}

/// Everything that can be wrong with a command line.
///
/// Every variant names the argument, the value, and what was tried. A usage error that only says
/// "invalid arguments" costs the caller a round trip to find out which one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UsageError {
    /// A flag that is not in the table.
    UnknownFlag {
        /// As typed, without the dashes.
        flag: String,
        /// The flags that are.
        known: Vec<&'static str>,
    },
    /// A flag that takes a value, given without one.
    MissingValue {
        /// The flag.
        flag: &'static str,
    },
    /// A switch given `--flag=value`.
    UnexpectedValue {
        /// The flag.
        flag: &'static str,
    },
    /// The same flag twice. Last-wins would be a silent parameter degradation.
    RepeatedFlag {
        /// The flag.
        flag: &'static str,
    },
    /// A flag that belongs to a different command.
    FlagNotValidHere {
        /// The flag.
        flag: &'static str,
        /// The command it was given to.
        command: &'static str,
        /// The commands that take it.
        belongs_to: Vec<&'static str>,
    },
    /// A value that is not a number, or is out of range.
    NotANumber {
        /// The flag.
        flag: &'static str,
        /// The value as typed.
        value: String,
        /// What would have been acceptable.
        expected: &'static str,
    },
    /// A command that is not in the table.
    UnknownCommand {
        /// As typed.
        name: String,
        /// The closest command, if one is close.
        suggestion: Option<&'static str>,
        /// The commands that are.
        known: Vec<&'static str>,
    },
    /// Fewer positionals than the command needs.
    MissingArgument {
        /// The command.
        command: &'static str,
        /// What it needed.
        expected: &'static str,
    },
    /// More positionals than the command accepts.
    TooManyArguments {
        /// The command.
        command: &'static str,
        /// How many it accepts.
        accepts: usize,
    },
    /// A target or a `rm` argument that is not valid UTF-8.
    NotUtf8 {
        /// The command.
        command: &'static str,
        /// Which argument.
        argument: &'static str,
    },
}

impl UsageError {
    /// The command the error is about, when the error is about one.
    ///
    /// `None` for an unknown flag or an unknown command, because in both cases there is no command
    /// to be specific *about* — the whole usage text is the useful response. Present for a bad
    /// flag use, a bad number, or the wrong arity, so the caller can be shown the one line they
    /// needed rather than the whole table.
    #[must_use]
    pub fn command(&self) -> Option<&'static str> {
        match self {
            UsageError::FlagNotValidHere { command, .. }
            | UsageError::MissingArgument { command, .. }
            | UsageError::TooManyArguments { command, .. }
            | UsageError::NotUtf8 { command, .. } => Some(*command),
            // `NotANumber` and `MissingValue` carry no command, and deliberately so: they can be
            // raised before a command has been identified. They answer `None` here, which means
            // the caller is shown the whole usage table — and that is the useful response for a
            // mistyped value, because the accepted range is in the flag's help text.
            UsageError::UnknownFlag { .. }
            | UsageError::RepeatedFlag { .. }
            | UsageError::UnexpectedValue { .. }
            | UsageError::UnknownCommand { .. }
            | UsageError::NotANumber { .. }
            | UsageError::MissingValue { .. } => None,
        }
    }

    /// The one-sentence message, with the closest thing to a fix.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            UsageError::UnknownFlag { flag, known } => format!(
                "unknown flag `--{flag}`; this build accepts {}",
                list(known)
            ),
            UsageError::MissingValue { flag } => {
                format!("`--{flag}` needs a value and the command line ended")
            }
            UsageError::UnexpectedValue { flag } => {
                format!("`--{flag}` is a switch and takes no value")
            }
            UsageError::RepeatedFlag { flag } => format!(
                "`--{flag}` was given more than once; this build refuses to choose between them"
            ),
            UsageError::FlagNotValidHere {
                flag,
                command,
                belongs_to,
            } => format!(
                "`--{flag}` is not an option of `{command}`; it belongs to {}",
                list(belongs_to)
            ),
            UsageError::NotANumber {
                flag,
                value,
                expected,
            } => format!("`--{flag} {value}` is not {expected}"),
            UsageError::UnknownCommand {
                name,
                suggestion,
                known,
            } => match suggestion {
                Some(near) => format!(
                    "unknown command `{name}`; did you mean `{near}`? this build accepts {}",
                    list(known)
                ),
                None => format!(
                    "unknown command `{name}`; this build accepts {}",
                    list(known)
                ),
            },
            UsageError::MissingArgument { command, expected } => {
                format!("`{command}` needs {expected}")
            }
            UsageError::TooManyArguments { command, accepts } => format!(
                "`{command}` accepts at most {accepts} positional argument(s) and was given more"
            ),
            UsageError::NotUtf8 { command, argument } => format!(
                "the {argument} given to `{command}` is not valid UTF-8; a target is a name or a \
                 path, and neither can be one"
            ),
        }
    }
}

/// A comma-separated list with "and" before the last, so a message reads as a sentence.
fn list(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [only] => (*only).to_owned(),
        [first, middle @ .., last] => format!("{first}, {}, and {last}", middle.join(", ")),
    }
}

/// The default quiet period for `watch`, in milliseconds.
///
/// Not this crate's number: it is the one
/// [`peek_core::watch::native::WatchOptions`] defines, so the command and the engine cannot
/// disagree about it.
pub const DEFAULT_QUIET_FOR_MS: u64 = 200;

/// How many quiet periods a batch may span before it is closed anyway.
///
/// A **product decision, not a measured threshold**, and defended as one. The failure it prevents
/// is real and does not need a benchmark: a build that writes continuously produces events faster
/// than the quiet period elapses, so the batch never closes and the index falls arbitrarily far
/// behind while the watcher looks healthy. Twenty quiet periods of the default 200 ms is four
/// seconds — behind enough for a person editing code to notice, short enough that catching up is
/// cheap. Nothing here has been measured; the number is the smallest one that makes the failure
/// mode bounded rather than absent.
pub const MAX_BATCH_QUIET_PERIODS: u64 = 20;

/// Turn argv into an invocation.
///
/// The only entry point. Every argument error in the program is produced here.
pub fn parse<I, S>(args: I) -> Result<Invocation, UsageError>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let tokens: Vec<OsString> = args.into_iter().map(Into::into).collect();
    let scan = scan(&tokens)?;
    let flags = scan.flags;

    let shell = |command: Command| Invocation {
        json: has_flag(&flags, "json"),
        index_dir: flag_path(&flags, "index-dir"),
        quiet: has_flag(&flags, "quiet"),
        command,
    };

    // A command line with no command at all prints the help. Chosen over a usage error because
    // `peek` with no arguments is a request for orientation, and refusing to answer it would be
    // the opposite of helpful. `peek <nonsense>` is a different thing and is refused below.
    let Some(name) = scan.positionals.first() else {
        return Ok(shell(Command::Help));
    };
    let name = text(name, "peek", "command name")?;
    if name == "help" {
        return Ok(shell(Command::Help));
    }
    if name == "version" {
        return Ok(shell(Command::Version));
    }

    let Some(spec) = COMMANDS.iter().find(|spec| spec.name == name) else {
        return Err(UsageError::UnknownCommand {
            name,
            suggestion: suggest(&name),
            known: COMMANDS.iter().map(|spec| spec.name).collect(),
        });
    };
    check_flags(spec, &flags)?;

    let given = &scan.positionals[1..];
    if given.len() < spec.required {
        return Err(UsageError::MissingArgument {
            command: spec.name,
            expected: spec.positionals[spec.required - 1],
        });
    }
    if given.len() > spec.positionals.len() {
        return Err(UsageError::TooManyArguments {
            command: spec.name,
            accepts: spec.positionals.len(),
        });
    }

    // The repository, from exactly one place per command. See the module documentation: two
    // sources and no precedence rule, so there is nothing to get wrong.
    let (root, rest): (PathBuf, &[OsString]) = match spec.root {
        RootSource::Positional => match given.first() {
            Some(given) => (PathBuf::from(given), given.get(1..).unwrap_or_default()),
            None => (PathBuf::from("."), given),
        },
        RootSource::Flag => (
            flag_path(&flags, "root").unwrap_or_else(|| PathBuf::from(".")),
            given,
        ),
    };

    // The one non-root positional, when the command has one. Resolved here rather than inside each
    // arm so there is a single place that converts a token to a target and a single place that can
    // report a non-UTF-8 one.
    let argument_label: &'static str = if spec.name == "rm" { "path" } else { "target" };
    let argument: Option<String> = if spec.root == RootSource::Flag {
        match rest.first() {
            Some(value) => Some(text(value, spec.name, argument_label)?),
            None => {
                return Err(UsageError::MissingArgument {
                    command: spec.name,
                    expected: argument_label,
                });
            }
        }
    } else {
        None
    };
    // Only the `RootSource::Flag` commands have one, and each of them requires it, so this is never
    // a missing value; `unwrap_or_default` rather than `expect`, because a panic in a library is
    // worse than an empty string in a branch that cannot be reached.
    let argument = argument.unwrap_or_default();

    let command = match spec.name {
        "index" => Command::Index {
            root,
            full: has_flag(&flags, "full"),
        },
        "watch" => {
            let quiet_for_ms = number(&flags, "quiet-for", DEFAULT_QUIET_FOR_MS)?;
            Command::Watch {
                root,
                quiet_for_ms,
                max_batch_ms: number(
                    &flags,
                    "max-batch",
                    quiet_for_ms.saturating_mul(MAX_BATCH_QUIET_PERIODS),
                )?,
            }
        }
        "explain" => Command::Explain {
            root,
            target: argument,
        },
        "callers" => Command::Callers {
            root,
            target: argument,
        },
        "callees" => Command::Callees {
            root,
            target: argument,
        },
        "dependents" => {
            let hops = number(&flags, "depth", 1)?;
            let depth = u32::try_from(hops).map_err(|_| UsageError::NotANumber {
                flag: "depth",
                value: hops.to_string(),
                expected: "a whole number of hops between 0 and 4294967295",
            })?;
            Command::Dependents {
                root,
                target: argument,
                depth,
            }
        }
        "context" => Command::Context {
            root,
            target: argument,
            // Required, and refused by the command with the minimum rather than defaulted here. A
            // budget nobody wrote down is a number this program will not invent.
            budget: optional_number(&flags, "budget")?,
        },
        "doctor" => Command::Doctor { root },
        "status" => Command::Status { root },
        "rm" => Command::Remove {
            root,
            path: argument,
        },
        other => {
            // Every name in `COMMANDS` is handled above. A row added without an arm here is a
            // compile error rather than a command that quietly does nothing.
            return Err(UsageError::UnknownCommand {
                name: other.to_owned(),
                suggestion: None,
                known: COMMANDS.iter().map(|spec| spec.name).collect(),
            });
        }
    };

    Ok(shell(command))
}

/// One `--name [value]` as it appeared, with its index in [`FLAGS`].
#[derive(Debug, Clone, PartialEq, Eq)]
struct Seen {
    index: usize,
    value: Option<OsString>,
}

/// The raw shape of a command line, before any command is known.
struct Scan {
    flags: Vec<Seen>,
    positionals: Vec<OsString>,
}

/// Walk the tokens once, splitting flags from positionals.
///
/// A flag is `--name`, `--name=value`, or `-x`. Everything after a bare `--` is positional, which
/// is what lets a target begin with a dash.
fn scan(tokens: &[OsString]) -> Result<Scan, UsageError> {
    let mut flags: Vec<Seen> = Vec::new();
    let mut positionals: Vec<OsString> = Vec::new();
    let mut after_terminator = false;
    let mut index = 0;

    while index < tokens.len() {
        let token = &tokens[index];
        index += 1;
        let raw = token.to_string_lossy().into_owned();

        if after_terminator {
            positionals.push(token.clone());
            continue;
        }
        if raw == "--" {
            after_terminator = true;
            continue;
        }
        if raw == "-" || !raw.starts_with('-') {
            positionals.push(token.clone());
            continue;
        }

        let body = raw.trim_start_matches('-');
        if body.is_empty() {
            // A bare `---` is not a flag; it is nonsense, and saying so beats treating it as one.
            return Err(UsageError::UnknownFlag {
                flag: raw,
                known: FLAGS.iter().map(|flag| flag.long).collect(),
            });
        }
        let (name, inline) = match body.split_once('=') {
            Some((name, value)) => (name, Some(value.to_owned())),
            None => (body, None),
        };

        // One character after one dash is a short flag; a longer one is a misspelled long flag
        // rather than a cluster. Clusters are not supported and are not silently accepted.
        let found = if raw.starts_with("--") {
            FLAGS.iter().position(|flag| flag.long == name)
        } else {
            match name.chars().next() {
                Some(letter) if name.chars().count() == 1 => {
                    FLAGS.iter().position(|flag| flag.short == Some(letter))
                }
                _ => None,
            }
        };
        let Some(found) = found else {
            return Err(UsageError::UnknownFlag {
                flag: name.to_owned(),
                known: FLAGS.iter().map(|flag| flag.long).collect(),
            });
        };
        let spec = FLAGS[found];

        let value = match (spec.value, inline) {
            (_, Some(_)) if spec.value.is_none() => {
                return Err(UsageError::UnexpectedValue { flag: spec.long });
            }
            (Some(_), Some(value)) => Some(OsString::from(value)),
            (Some(_), None) => {
                let next = tokens
                    .get(index)
                    .cloned()
                    .ok_or(UsageError::MissingValue { flag: spec.long })?;
                index += 1;
                Some(next)
            }
            (None, None) => None,
        };

        if flags.iter().any(|seen| seen.index == found) {
            return Err(UsageError::RepeatedFlag { flag: spec.long });
        }
        flags.push(Seen {
            index: found,
            value,
        });
    }

    Ok(Scan { flags, positionals })
}

/// Reject a flag that the chosen command does not take.
///
/// This is the check that makes the flag table worth having: without it, `--budget` on `status`
/// is a value nobody reads, and a caller that believes it took effect is wrong.
fn check_flags(spec: &CommandSpec, flags: &[Seen]) -> Result<(), UsageError> {
    for seen in flags {
        let flag = &FLAGS[seen.index];
        if GLOBAL_FLAGS.contains(&flag.long) || spec.flags.contains(&flag.long) {
            continue;
        }
        return Err(UsageError::FlagNotValidHere {
            flag: flag.long,
            command: spec.name,
            belongs_to: COMMANDS
                .iter()
                .filter(|other| other.flags.contains(&flag.long))
                .map(|other| other.name)
                .collect(),
        });
    }
    Ok(())
}

/// Whether a switch was given.
fn has_flag(flags: &[Seen], long: &str) -> bool {
    let Some(index) = FLAGS.iter().position(|flag| flag.long == long) else {
        return false;
    };
    flags.iter().any(|seen| seen.index == index)
}

/// The value of a flag that carries a path.
fn flag_path(flags: &[Seen], long: &str) -> Option<PathBuf> {
    let index = FLAGS.iter().position(|flag| flag.long == long)?;
    flags
        .iter()
        .find(|seen| seen.index == index)
        .and_then(|seen| seen.value.clone())
        .map(PathBuf::from)
}

/// A numeric flag, or `default` when it was not given.
fn number(flags: &[Seen], long: &'static str, default: u64) -> Result<u64, UsageError> {
    Ok(optional_number(flags, long)?.unwrap_or(default))
}

/// A numeric flag, or `None` when it was not given.
///
/// A missing flag and a flag whose value is zero are different mistakes, so the two must be
/// distinguishable here rather than collapsed into a default.
fn optional_number(flags: &[Seen], long: &'static str) -> Result<Option<u64>, UsageError> {
    let Some(index) = FLAGS.iter().position(|flag| flag.long == long) else {
        return Ok(None);
    };
    let Some(seen) = flags.iter().find(|seen| seen.index == index) else {
        return Ok(None);
    };
    let Some(value) = seen.value.as_ref() else {
        return Ok(None);
    };
    let text = value.to_str().ok_or_else(|| UsageError::NotANumber {
        flag: long,
        value: value.to_string_lossy().into_owned(),
        expected: "a decimal number",
    })?;
    // `parse` rather than a lenient conversion, because a negative, a float or trailing prose must
    // all be refused rather than coerced into something the caller did not write.
    text.parse::<u64>()
        .map(Some)
        .map_err(|_| UsageError::NotANumber {
            flag: long,
            value: text.to_owned(),
            expected: "a whole number of 0 or more",
        })
}

/// An argument that must be text.
fn text(
    value: &OsString,
    command: &'static str,
    argument: &'static str,
) -> Result<String, UsageError> {
    value
        .to_str()
        .map(str::to_owned)
        .ok_or(UsageError::NotUtf8 { command, argument })
}

/// The closest command name, if one is close enough to be worth suggesting.
///
/// Prefix either way, then substring. A full edit distance would also be fine; this is chosen
/// because it cannot suggest a name the user did not nearly type, and because a wrong suggestion
/// is worse than none.
fn suggest(name: &str) -> Option<&'static str> {
    let lowered = name.to_ascii_lowercase();
    let names: Vec<&'static str> = COMMANDS.iter().map(|spec| spec.name).collect();
    names
        .iter()
        .find(|candidate| candidate.starts_with(&lowered) || lowered.starts_with(*candidate))
        .or_else(|| names.iter().find(|candidate| candidate.contains(&lowered)))
        .copied()
}

/// Whether a path names an extension this build extracts.
///
/// The same rule the indexer applies, read from the same registry, so a watcher cannot decide to
/// ignore a file the indexer would have indexed.
/// [`peek_core::watch::native::WatchOptions`]'s own default is `rs`-only, which is honest for a
/// module with no discovery policy and far too narrow for a repository.
///
/// A `fn` item rather than a closure because that is the type `WatchOptions` carries, and a
/// function item coerces to it without a cast at the call site.
pub fn is_indexable(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .and_then(peek_core::model::Language::from_extension)
        .is_some_and(|language| peek_core::extract::registry::get(language).is_some())
}

/// The command's positional arguments, joined, e.g. `<TARGET> [PATH]`.
fn positionals_of(spec: &CommandSpec) -> String {
    if spec.positionals.is_empty() {
        String::new()
    } else {
        format!(" {}", spec.positionals.join(" "))
    }
}

/// The usage text, generated from the tables above.
///
/// Nothing here is written twice: the command list comes from [`COMMANDS`] and the flag list from
/// [`FLAGS`], so a flag that exists is documented and a flag that is documented exists.
pub fn help_text() -> String {
    let mut text = String::new();
    text.push_str(&format!(
        "peek {}\n\n\
         Local codebase intelligence for coding agents.\n\n\
         usage:\n  peek <COMMAND> [ARGUMENTS] [OPTIONS]\n\n\
         The repository defaults to the working directory. `index` and `watch` name it as their \
         [PATH] argument; every other command takes --root DIR.\n\ncommands:\n",
        peek_core::VERSION
    ));
    for spec in COMMANDS {
        text.push_str(&format!(
            "  {:<11}{:<14}{}\n",
            spec.name,
            positionals_of(spec),
            spec.summary
        ));
    }
    text.push_str("\noptions:\n");
    for flag in FLAGS {
        let spelling = match (flag.short, flag.value) {
            (Some(short), Some(value)) => format!("-{short}, --{} {value}", flag.long),
            (Some(short), None) => format!("-{short}, --{}", flag.long),
            (None, Some(value)) => format!("    --{} {value}", flag.long),
            (None, None) => format!("    --{}", flag.long),
        };
        text.push_str(&format!("  {spelling:<24}{}\n", flag.help));
    }
    text.push_str(&format!(
        "\nexit codes:\n  \
         {ok}  the command did what it was asked\n  \
         {failed}  the engine could not do it: the index, the filesystem or a query failed\n  \
         {usage}  the command line was not understood\n  \
         {refused}  refused: the question cannot be answered from this index, and the reason is printed\n  \
         {unhealthy}  `doctor` found a failing check\n\n\
         Every command prints what it could not do, in both output modes. Progress narration goes to \
         standard error and is silenced by --quiet; findings never are.\n",
        ok = crate::exit::EXIT_OK,
        failed = crate::exit::EXIT_FAILED,
        usage = crate::exit::EXIT_USAGE,
        refused = crate::exit::EXIT_REFUSED,
        unhealthy = crate::exit::EXIT_UNHEALTHY,
    ));
    text
}

/// The per-command usage, printed alongside a usage error.
pub fn command_usage(command: &str) -> String {
    let Some(spec) = COMMANDS.iter().find(|spec| spec.name == command) else {
        return String::from("usage: peek <COMMAND>\n");
    };
    let options = if spec.flags.is_empty() {
        String::from("(none beyond the global options)")
    } else {
        spec.flags
            .iter()
            .map(|name| format!("--{name}"))
            .collect::<Vec<String>>()
            .join(", ")
    };
    let root = match spec.root {
        RootSource::Positional => "the [PATH] argument, or the working directory",
        RootSource::Flag => "--root DIR, or the working directory",
    };
    format!(
        "usage: peek {}{}\nrepository: {root}\noptions: {options}\n",
        spec.name,
        positionals_of(spec)
    )
}
