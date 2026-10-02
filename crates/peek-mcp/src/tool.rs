//! The tool catalogue: names, descriptions, and input schemas.
//!
//! # Why the descriptions are written the way they are
//!
//! The only reader of this text is a model choosing between tools, and it is choosing under
//! uncertainty about what each one will cost it and what it will get back. So every description
//! answers three questions in order:
//!
//! 1. **What does this answer?** In one sentence, in the terms the caller already has.
//! 2. **What does it not answer?** Because the failure mode of a tool surface is not a wrong
//!    answer, it is a *right answer to the wrong question* — a model that wanted the neighbourhood
//!    of a function and got its one-hop callers, and read that as the whole story.
//! 3. **What should I use instead?** Named, so the correction is executable.
//!
//! The result is that descriptions are long. That is a deliberate trade: MCP puts the whole
//! catalogue in the client's context at connect time, and a short description is a short
//! description. Two hundred words per tool is cheap next to one wrong tool call.
//!
//! # Why ten tools and not thirty
//!
//! Contract C4 asks the tool count to be justified against real agent workflows, and the
//! justification is the query engine's own: one tool per operation the engine actually has, plus
//! lifecycle. There is no `search` here because the engine has no search — adding one would be
//! advertising a capability the resolver does not have, which is the defect this project exists to
//! remove. There is no `overview` because the engine has no overview. What there is:
//!
//! | | |
//! |---|---|
//! | `index`, `index_status` | the index does not exist until the first of them runs, and nothing else works until it does |
//! | `explain` | "why is this edge here, and how sure are you" |
//! | `callers`, `callees`, `dependents` | the one walk, three parameters |
//! | `context` | the product: a token-budgeted slice of the repository |
//! | `doctor` | "my index looks wrong" |
//! | `watch_start`, `watch_stop` | keeping the index current while the user edits |
//!
//! `callers` and `callees` are kept as separate tools rather than one `walk` with a `direction`
//! because a model that has to choose a direction will sometimes choose wrong, and the wrong
//! direction is a silently inverted answer. `dependents` takes a depth because the depth is a real
//! parameter, not a constant the caller cannot see (D-0007).
//!
//! # Schemas
//!
//! Hand-written, with `additionalProperties: false` on every tool. That is the setting that makes
//! [`crate::params::Args::finish`] able to refuse an argument rather than ignore it, which is the
//! fix for the predecessor's silent parameter degradation (audit D, section F).

use serde::Serialize;
use serde_json::{Value, json};

/// One tool as the client sees it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Tool {
    /// The name a caller passes to `tools/call`.
    pub name: &'static str,
    /// A short human-facing label. `title` in the specification; it is not a substitute for the
    /// description and is not treated as one anywhere.
    pub title: &'static str,
    /// What the tool answers, what it does not, and what to use instead.
    pub description: &'static str,
    /// The argument schema. `additionalProperties` is always `false`.
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

impl Tool {
    /// The schema as a string, for a caller that wants to read it.
    #[must_use]
    pub fn schema_text(&self) -> String {
        serde_json::to_string_pretty(&self.input_schema).unwrap_or_else(|_| "{}".to_owned())
    }
}

/// Every tool this server exposes, in a fixed order.
///
/// Fixed rather than sorted at runtime so that two `tools/list` calls on two builds produce the
/// same bytes when the builds have the same tools, and so that the order a model reads them in is
/// the order a human reading this file reads them in.
#[must_use]
pub fn catalogue() -> Vec<Tool> {
    vec![
        index(),
        index_status(),
        explain(),
        callers(),
        callees(),
        dependents(),
        context(),
        doctor(),
        watch_start(),
        watch_stop(),
    ]
}

/// Every tool name, in catalogue order.
#[must_use]
pub fn names() -> Vec<&'static str> {
    catalogue().iter().map(|tool| tool.name).collect()
}

/// One tool by name.
#[must_use]
pub fn find(name: &str) -> Option<Tool> {
    catalogue().into_iter().find(|tool| tool.name == name)
}

/// What to say when a tool is handed an argument it does not have.
///
/// Every entry names the tool that *does* take the argument, because a model that reached for the
/// wrong one has usually understood the question correctly and guessed the interface wrongly. A
/// refusal that only says "no such argument" makes it guess again.
#[must_use]
pub fn hints_for(tool: &str) -> &'static [(&'static str, &'static str)] {
    match tool {
        "index" => &[(
            "paths",
            "`index` takes `paths` only in `refresh` mode; in `full` mode the whole repository is \
             walked and `paths` would be silently ignored",
        )],
        "index_status" => &[
            (
                "target",
                "`index_status` reports the whole index; ask about a target with `context`, \
                 `explain` or `dependents`",
            ),
            (
                "paths",
                "`index_status` reports the whole index; to refresh specific files, call `index` \
                 with `mode` set to `refresh`",
            ),
        ],
        "explain" => &[
            (
                "depth",
                "`explain` takes `chain_depth`, not `depth`; `depth` is the traversal parameter of \
                 `dependents`",
            ),
            (
                "budget_tokens",
                "`explain` returns every edge at the target and is not budgeted; for a \
                 token-bounded slice of the neighbourhood use `context`",
            ),
        ],
        "callers" | "callees" => &[
            (
                "depth",
                "this tool is one hop by definition and takes no depth; use `dependents`, which \
                 walks inbound for a stated number of hops",
            ),
            (
                "direction",
                "the direction is what this tool is for; use `dependents` for inbound traversal \
                 beyond one hop and `callees` for outbound",
            ),
            (
                "budget_tokens",
                "a walk is not budgeted; for a token-bounded slice of the neighbourhood use \
                 `context`",
            ),
        ],
        "dependents" => &[
            (
                "direction",
                "`dependents` walks inbound, which is what the name means; use `callees` for \
                 outbound",
            ),
            (
                "budget_tokens",
                "a walk is not budgeted; for a token-bounded slice of the neighbourhood use \
                 `context`",
            ),
        ],
        "context" => &[
            (
                "depth",
                "a context pack is depth-1 by construction; use `dependents` with a `depth` if you \
                 need to know what depends on something further away",
            ),
            (
                "limit",
                "the budget is `budget_tokens`, and it is a ceiling rather than a target; there is \
                 no other limit",
            ),
        ],
        "doctor" => &[(
            "target",
            "`doctor` diagnoses the whole index, not one target; ask about a target with \
                 `context` or `explain`",
        )],
        "watch_start" => &[(
            "target",
            "`watch_start` watches the whole repository; to refresh specific files once, call \
                 `index` with `mode` set to `refresh`",
        )],
        "watch_stop" => &[(
            "target",
            "`watch_stop` stops the running watch; it takes the `watch_id` that \
                 `watch_start` returned",
        )],
        _ => &[],
    }
}

// ---------------------------------------------------------------------------
// The tools
// ---------------------------------------------------------------------------

fn index() -> Tool {
    Tool {
        name: "index",
        title: "Build or refresh the index",
        description: "\
Build a complete index of the repository, or refresh the files named in `paths`. Use this \
before any other Peek tool: every one of them answers from the index, and an index that does \
not exist produces `outcome: not_indexed` rather than an empty answer.

This is the only tool that writes, and it is the slowest — on a large repository a full build \
takes minutes. Nothing here is an estimate of what the index will contain: the response carries \
the count of every resolution state, so `relations_ambiguous` and `relations_unresolved` are \
measurements of what the engine could not decide, and `files_skipped`, `files_unsupported` and \
the `skipped` list say which files did not make it in and why.

**`paths` names files inside this repository and nothing else.** A path spelled in full is \
accepted only if the repository contains it; one that is outside is a **refusal**, not a skipped \
file, because this server will read it and put its contents in the index. One unusable entry \
refuses the whole request rather than the rest of it running without that entry: the request is \
either honoured for every path you named or it does not run at all, so a successful answer \
always means the index covers the list you sent.

Do not use it to ask a question. It does not answer anything about a target. Do not use `mode: \
refresh` for a file you have not changed: a refresh re-extracts and re-resolves, which is \
correct but is not free, and `index_status` is the cheaper way to ask what is already indexed.",
        input_schema: object(
            vec![
                (
                    "mode",
                    property(
                        "string",
                        "`full` walks the whole repository. `refresh` re-extracts only `paths` and \
                         re-resolves the relations that point into them. Defaults to `full`.",
                        &[("enum", json!(["full", "refresh"]))],
                    ),
                ),
                (
                    "paths",
                    property(
                        "array",
                        "Files to refresh, as paths inside this repository and `/`-separated. \
                         Required in `refresh` mode and refused in `full` mode, where it would be \
                         ignored. A path outside this repository is refused rather than skipped, \
                         and one such path refuses the whole request: nothing is indexed, and \
                         the refusal names every offending entry.",
                        &[("items", json!({ "type": "string" }))],
                    ),
                ),
            ],
            &[],
        ),
    }
}

fn index_status() -> Tool {
    Tool {
        name: "index_status",
        title: "What the index holds",
        description: "\
Report what the index currently contains, measured rather than remembered: the generation, the \
schema version, the file on disk and its size, the number of entities and relations, and the \
count in each of the five resolution states. The five counts are asserted to be a partition of \
`relation_count` and the response says whether they were, so a mismatch is visible rather than \
inferred from arithmetic that does not add up.

Use this to decide whether to run `index`, to check whether a `watch_start` has kept up, and to \
find out where the index lives — it is under the OS cache directory, never inside your \
repository.

Do not use it to ask about a symbol; it has no `target` and answers nothing about one. Do not \
use it as a substitute for `doctor`: it reports what the index holds, not what is wrong with it, \
and a corrupt index reports counts as cheerfully as a sound one.",
        input_schema: object(vec![], &[]),
    }
}

fn explain() -> Tool {
    Tool {
        name: "explain",
        title: "Why an edge exists, and how sure the engine is",
        description: "\
Report every edge at a target — the ones leaving it and the ones arriving — with each edge's \
resolution state, its evidence class, the basis the resolver wrote, and its span, plus one \
provenance chain from the target back towards whatever made it reachable.

The point of this tool is the uncertainty. An edge that was proven says so and names the evidence \
(`import_binding`, `same_file`, `unique_name`); an edge that was inferred says so *and* carries \
the sentence that was inferred and from what; an ambiguous edge is listed with the candidates it \
was ambiguous between, and an unresolved edge with the reason it could not be placed. An \
ambiguous edge is never presented as a fact, and there are no numeric confidence scores anywhere \
in the answer — only named evidence classes.

Do not use it to find callers. It lists every edge of every kind, which is not the same question \
as `callers`, and the two disagree on purpose. Do not use it when you need a bounded amount of \
context: it is not budgeted and a highly connected declaration will produce a long answer. Use \
`context` for that.",
        input_schema: object(
            vec![
                (
                    "target",
                    required_string(
                        "A repository path, a qualified name such as `Type.method`, or a \
                                    bare name. Looked up in that order; an ambiguous name returns \
                                    every candidate rather than picking one.",
                    ),
                ),
                (
                    "chain_depth",
                    property(
                        "integer",
                        "Hops the provenance chain may walk. Defaults to 4. A chain is one chosen \
                         path, not every path: each hop records how many alternatives it passed \
                         over.",
                        &[("minimum", json!(0))],
                    ),
                ),
                (
                    "follow_inferred",
                    property(
                        "boolean",
                        "Whether the chain may follow edges that were inferred rather than proven. \
                         Defaults to true. Set it false to see only stored edges.",
                        &[],
                    ),
                ),
            ],
            &["target"],
        ),
    }
}

fn callers() -> Tool {
    Tool {
        name: "callers",
        title: "Who uses this, one hop",
        description: "\
List everything that depends on a target, one hop inbound. A caller here means *anything with \
a followable edge into the target* — a function that invokes it, a type that mentions it, a \
module that imports it — not only a `calls` edge. Narrow that with `kind` if you need the \
invocations alone.

Every result carries the distance from the target and the whole relation it arrived on, so an \
arrival that rests on an inference is visibly an inference and an edge that could not be \
followed is counted rather than quietly dropped. An edge with no proven target is read, counted \
in `inspected`, and **not** returned as a result: the answer cannot list something the engine \
does not know the identity of.

Do not use it for more than one hop. One hop is what the word means; use `dependents` with a \
`depth` for a wider question, and expect a larger answer. Do not use it when you want the \
declaration itself — this returns identities and edges, not source.",
        input_schema: walk_schema(
            vec![(
                "target",
                required_string(
                    "A repository path, a qualified name such as `Type.method`, or a \
                                bare name.",
                ),
            )],
            true,
        ),
    }
}

fn callees() -> Tool {
    Tool {
        name: "callees",
        title: "What this uses, one hop",
        description: "\
List everything a target depends on, one hop outbound. The mirror of `callers` with the same \
definition of dependency: a followable edge of any kind counts, and `kind` narrows it.

The result is a set of identities, not source. If you want to read what it calls, take a name \
from here and call `context` with a budget; that is the intended two-step and it is cheaper than \
asking for everything at once.

Do not use it for a wider question — use `dependents` for inbound, and note that there is no \
multi-hop outbound tool, because a depth-2 call graph of a large repository is not an answer \
anyone can act on.",
        input_schema: walk_schema(
            vec![(
                "target",
                required_string(
                    "A repository path, a qualified name such as `Type.method`, or a \
                                bare name.",
                ),
            )],
            true,
        ),
    }
}

fn dependents() -> Tool {
    Tool {
        name: "dependents",
        title: "Who depends on this, N hops",
        description: "\
Walk inbound from a target for a stated number of hops and return everything reached, each \
with its distance from the target. This is the tool for \"what breaks if I change this\" and for \
mapping how far a change propagates.

`depth` is a real parameter and is passed straight to the walk: `depth: 1` is the same question \
as `callers`, `depth: 3` reaches three hops out. A depth of `0` returns nothing on purpose — the \
target is never listed as its own dependent, because self-inclusion destroys the distance \
information that is the whole value of the answer. There is no upper limit on `depth`, and none \
is needed: the work is bounded by the engine's visit limit, and when that limit is reached the \
response sets `bounded: true` and reports how much was read, so a short answer is never mistaken \
for a complete one.

Use this rather than `callers` whenever you care about reach. Do not use it to read code; it \
returns identities and edges, not source.",
        input_schema: walk_schema(
            vec![
                (
                    "target",
                    required_string(
                        "A repository path, a qualified name such as `Type.method`, or a \
                                    bare name.",
                    ),
                ),
                (
                    "depth",
                    property(
                        "integer",
                        "Hops to walk inbound. `0` returns nothing. The same question at `1` is \
                         `callers`.",
                        &[("minimum", json!(0))],
                    ),
                ),
            ],
            false,
        ),
    }
}

fn context() -> Tool {
    Tool {
        name: "context",
        title: "A token-budgeted slice of the repository",
        description: "\
Compile a slice of the indexed repository around one target and return it inside a token budget \
you state. This is the tool to reach for when you need to read code you have not read.

**The budget is a hard ceiling, not a target.** The answer never costs more than `budget_tokens`, \
counted as `ceil(utf8 bytes / 3)` — deliberately pessimistic, so a pack tends to under-fill \
rather than overrun. `budget.status` is one of three states and they mean different things: \
`complete` (the whole neighbourhood fitted), `reduced` (something was left out — read \
`budget.omitted`, which names every dropped unit and edge with its reason and what it would have \
cost), and `insufficient` (even the target does not fit, so the pack is a refusal rather than a \
slice). A budget too small to hold the report that would explain the omissions is refused outright \
with the minimum, never exceeded.

**Uncertainty survives into the answer.** Every edge is the engine's own relation record with its \
typed resolution state, so an ambiguous edge appears as `ambiguous` with its candidate list and an \
unresolved one as `unresolved` with its reason. The engine ranks uncertain edges *ahead* of \
certain ones when the budget is tight, so a truncated pack loses certainties before it loses \
doubts.

What you get is depth-1 by construction: the target, the container it sits in if it has one, and \
one hop of callers, callees and named references. That is a deliberate ceiling. Use `dependents` \
for reach, and do not expect this to return a call graph.

Do not use it to find a symbol you cannot name — there is no search here, and a target that \
matches nothing comes back as `unknown_target` naming the three lookups that missed. Do not use \
it without a budget: the number is required, and refusing to guess one is the point.",
        input_schema: object(
            vec![
                (
                    "target",
                    required_string(
                        "A repository path, a qualified name such as `Type.method`, or a \
                                    bare name.",
                    ),
                ),
                (
                    "budget_tokens",
                    property(
                        "integer",
                        "The ceiling, in tokens, for the whole answer including the report of what \
                         was dropped. Required: there is no default, because a default is a \
                         number nobody chose.",
                        &[("minimum", json!(0))],
                    ),
                ),
                (
                    "follow_inferred",
                    property(
                        "boolean",
                        "Whether edges inferred rather than proven may bring a neighbour into the \
                         pack. They are included and marked either way. Defaults to true.",
                        &[],
                    ),
                ),
                (
                    "list_ambiguity_candidates",
                    property(
                        "boolean",
                        "Whether an ambiguous edge names its candidates rather than only counting \
                         them. Defaults to true; set it false to save tokens when you will not \
                         act on the ambiguity.",
                        &[],
                    ),
                ),
            ],
            &["target", "budget_tokens"],
        ),
    }
}

fn doctor() -> Tool {
    Tool {
        name: "doctor",
        title: "Diagnose the index",
        description: "\
Run every diagnostic the engine has over the index and return the findings with their severity, \
the measurement each was derived from, and what to do about it.

The rule this tool follows is that **a check that could not be performed reports that it could \
not be performed**. It never passes a check it skipped, because a green answer that silently \
omitted the check would convert an unknown into an assurance — which is the exact failure the \
predecessor had, reporting a healthy install over a corrupt index. So `index_openable: fail` \
means the index is broken rather than that the check was skipped.

Use it when an answer looks wrong, when the index is older than the code, or before blaming the \
engine. It is also the right first call on a repository that has never been indexed: it will say \
so and name the tool that fixes it.

Do not use it to ask about a symbol. It takes no `target`, and it inspects the index rather than \
the graph.",
        input_schema: object(vec![], &[]),
    }
}

fn watch_start() -> Tool {
    Tool {
        name: "watch_start",
        title: "Keep the index current as files change",
        description: "\
Start a background watcher that re-indexes the repository as files are edited, and return \
immediately with what it is watching. The call does not block on a file event: it waits only for \
the operating system to confirm the watch, up to `ready_timeout_ms`, and returns whether that \
succeeded. If it did not, the response says why.

Events are coalesced over `quiet_for_ms` (200 ms by default) so one editor save produces one \
refresh rather than five, and each refresh is scoped to the files that changed plus the relations \
pointing into them, so a refresh does not cost a rebuild. Every applied refresh appears in the \
next `index_status` and in the next `watch_stop`, and the counters say how many were applied and \
how many reports were not delivered — a report can be lost, and the loss is a number rather than a \
silence.

Requires an existing index; it will not build one. Only one watch runs at a time, because SQLite \
admits one writer, and a second `watch_start` is refused with the id of the running one.

To stop it, call `watch_stop` with the `watch_id` this returns — the response includes that call \
verbatim so you do not have to reconstruct it. Leaving a watch running keeps a process alive; \
stop it when you are done reading.",
        input_schema: object(
            vec![
                (
                    "quiet_for_ms",
                    property(
                        "integer",
                        "How long a batch of file events stays open without a new one. Defaults \
                         to 200. Longer for a network filesystem, shorter to feel immediate.",
                        &[("minimum", json!(1))],
                    ),
                ),
                (
                    "ready_timeout_ms",
                    property(
                        "integer",
                        "How long to wait for the operating system to confirm the watch before \
                         reporting that it could not be started. Defaults to 10000. This bounds the \
                         call, not the watch.",
                        &[("minimum", json!(1))],
                    ),
                ),
            ],
            &[],
        ),
    }
}

fn watch_stop() -> Tool {
    Tool {
        name: "watch_stop",
        title: "Stop the watcher",
        description: "\
Stop the running watch, apply anything still pending, and return what the last refresh did. \
Shutting down flushes the open batch rather than dropping it, so changes made in the moment \
before you stopped are in the index rather than missing from it.

Pass the `watch_id` that `watch_start` returned. An id that is not running is refused with the \
ids that are, so a stale id produces a correction rather than a silent success. Calling this when \
nothing is watching is reported as such rather than treated as done.

After this returns, `index_status` and `context` read the index as it stands.",
        input_schema: object(
            vec![(
                "watch_id",
                property(
                    "integer",
                    "The id `watch_start` returned. Optional: the most recently started watch is \
                     stopped when it is absent, and the response says which one that was.",
                    &[("minimum", json!(1))],
                ),
            )],
            &[],
        ),
    }
}

// ---------------------------------------------------------------------------
// Schema construction
// ---------------------------------------------------------------------------

/// The shared shape of a walk tool: `target` plus the two optional filters.
///
/// Built from a per-tool list so the three walk tools cannot drift apart on how they spell `kind`
/// or `follow_inferred`. `with_kind` is false for `dependents`, which already declares its own
/// arguments, and true for the two that share this shape.
fn walk_schema(mut properties: Vec<(&'static str, Value)>, with_kind: bool) -> Value {
    if with_kind {
        properties.push((
            "kind",
            property(
                "string",
                "Restrict to one relation kind, such as `calls` or `imports`. Omit for every \
                 dependency kind, which is the question a walk normally answers. A kind this build \
                 does not have is refused with the list, not ignored.",
                &[],
            ),
        ));
    }
    properties.push((
        "follow_inferred",
        property(
            "boolean",
            "Whether edges that were inferred rather than proven may be followed. They are \
             followed by default and every arrival that used one says so. Set false for stored \
             edges only.",
            &[],
        ),
    ));
    object(properties, &["target"])
}

/// A schema for an object with no arguments.
fn object(properties: Vec<(&'static str, Value)>, required: &[&'static str]) -> Value {
    let properties: serde_json::Map<String, Value> = properties
        .into_iter()
        .map(|(name, schema)| (name.to_owned(), schema))
        .collect();
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        // The setting that lets a tool refuse an argument it does not have instead of ignoring
        // it. See the module documentation.
        "additionalProperties": false
    })
}

/// One property, with whatever constraints it has.
///
/// `extra` is a list of key/value pairs rather than a pre-built object, so a caller writes a plain
/// name and a plain value at each call site instead of nesting `json!` inside `json!`. A property
/// with no constraints passes an empty slice.
fn property(kind: &str, description: &str, extra: &[(&str, Value)]) -> Value {
    let mut schema = serde_json::Map::new();
    schema.insert("type".to_owned(), json!(kind));
    schema.insert("description".to_owned(), json!(description));
    for (key, value) in extra {
        schema.insert((*key).to_owned(), value.clone());
    }
    Value::Object(schema)
}

/// A required string property, which is the shape of every `target` in the catalogue.
fn required_string(description: &str) -> Value {
    json!({ "type": "string", "description": description })
}
