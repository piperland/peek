# Review of the MCP server

The Model Context Protocol server is a new workspace crate, `crates/peek-mcp`: about 4,500 lines of
source and 2,750 lines of tests, in 84 tests. It speaks JSON-RPC 2.0 over stdio, exposes ten tools,
and adds no new dependencies — the manifest leaves `Cargo.lock` byte-identical, which is the
strongest possible evidence for a "no new dependencies" claim.

This is a review of that crate and of the places it reaches outside its own directory. The findings
are ordered by how much they should change a decision.

---

## 1. The changes outside `crates/peek-mcp/`

There are three widenings of the engine, one to a commit each, plus a fourth edit in one of those
commits that is not a widening. The five commits after them touch nothing outside the crate, which
is worth saying up front: the discipline is real, and the widening stopped as soon as the engine had
what it needed.

| # | Change | Justified? | Can it be backed out on its own? |
|---|---|---|---|
| 1 | `Query::resolve`, and the `Target` re-export | Yes | Yes, with rework at two call sites |
| 2 | `Serialize` on the five diagnosis and statistics types | Yes | Yes, with worse code at the call sites |
| 3 | `RelationKind::parse`, `ALL_RELATION_KINDS`, `relation_kind_names` | Yes | **No — not mechanically** |
| 4 | The workspace member line, the crate manifest, the crate root | Not a widening | n/a |

### 1. `Query::resolve` — justified, and load-bearing for two tools

A function that was private to the engine's `context` module becomes reachable as
`Query::resolve`, and the `Target` type it returns is re-exported. No existing code changes
behaviour: the body is untouched, and `Target` was previously unreachable from outside the crate,
so nothing that compiled before stops compiling.

This is the change I would argue about, and not because of the widening — because of what has
happened on the main line since. The command-line interface, added after this work branched, had
the same problem and solved it the other way round. `crates/peek-cli/src/commands/query.rs`
resolves a target string by compiling a full context pack at the smallest budget the engine accepts
and taking the identity off the pack, and its module comment says plainly that the resolver is
deliberately not public and that duplicating the three lookups would be worse.

Both routes run the same three lookups inside the engine, in the same order, so the two surfaces
cannot disagree about what a name means. That is the property that matters and it holds either way.
What does not hold is having two ways in with no decision between them: the engine's new entry
point is cheaper, the command line's is the one already reviewed and tested, and the comment
explaining why the cheap one was not used is now wrong.

**Decide this before merging, not after.** If `Query::resolve` lands, the command-line resolver
should be reduced to a call to it and its comment deleted. If it does not, nothing here is wrong —
but then the branch's stated reason for widening the engine ("every surface that takes a target as
text needs it") was written before the second surface existed, and should be re-argued.

### 2. The `Serialize` derives — justified, and the cleanest of the four

Five types gain `Serialize`. No behaviour changes; every variant of the two enums is a unit
variant, so the serialised form is a bare string and is exactly the word `as_str` already prints.
A test in the MCP crate pins the two spellings together, so the wire vocabulary and the terminal
vocabulary cannot drift.

The fallback if it were dropped is a hand-written mirror of each type in the MCP crate, which is
precisely the second definition of a shape that the derive's own comment says will drift. This one
I would not argue with.

### 3. `RelationKind::parse` and the vocabulary list — justified, but not revertable on its own

A true inverse of `as_str`, a list of every kind owned by the enum's own module rather than by a
caller, and a test asserting the list length against a literal so that a new variant without a list
entry fails rather than silently narrowing what a caller can ask for. The design is right.

Two observations. The list's length is checked by asserting a count, which catches "a variant was
added" but not "a variant was renamed and the list was updated to match" — the `parse`-inverts-
`as_str` test covers that, so together they are sound. And only one caller wants this: the
command line has no `--kind` filter at all, so the commit's plural framing ("a surface") is
aspirational rather than current. That is not a reason to reject it; it is a reason not to claim
more than the codebase supports.

**The revertability claim is where this one fails.** It shares a commit with the workspace member
line, the new crate's manifest, and `crates/peek-mcp/src/lib.rs`. That last file is the crate root:
it declares `outcome`, `params`, `protocol`, `server`, `session`, `tool`, `tools` and `writer`.
Reverting the commit removes the crate root, so the five commits stacked on top of it stop
compiling. Splitting the relation-kind change into a commit of its own is the whole fix, and nothing
else has to move.

### 4. The workspace member line — not a boundary change

Adding a crate to a workspace is the minimum required to add a crate. It is not counted here, which
means the "four violations in four commits" framing is off by one in both directions: three
commits, four widenings, and one of the four is not a widening.

---

## 2. The claims, checked

| Claim | Verdict | What I found |
|---|---|---|
| stdout carries only the protocol | **True, structurally** | The output stream is a parameter of `serve`, and nothing in the library holds a handle on file descriptor 1. A handler has nothing to print to. |
| The source scan covers every file | **True** | Sixteen files, listed explicitly, and the list matches the tree exactly. Listing rather than globbing is the right call. |
| The source scan prevents a stray print | **False** | It matches a line that, after trimming, *begins* with `println!`, `print!` or `dbg!`. It misses `let _ = println!(..)`, `std::println!(..)`, and `writeln!(io::stdout(), ..)`. |
| A test spawns the real binary | **True** | `tests/transport.rs` runs `CARGO_BIN_EXE_peek-mcp` in a subprocess over real pipes, drives a full session, and parses every line of stdout. Not a mock. |
| Could a print get in through a dependency? | **Possible, and the scan cannot see it** | Nothing in the engine prints to stdout outside its test-only probe module, but a future dependency could. The subprocess test is the only net, and it only covers the paths it drives. |
| The three stdout tests pass | **False** | One of the three is red — see below. |
| A pack never exceeds its budget | **True** | The engine reserves the report before pricing content and checks the reserve is a proven upper bound. If the invariant breaks, the pack says so in a note rather than hiding it. |
| A smaller budget is a coherent subset | **True** | The fill stops at the first unit that does not fit rather than skipping ahead, so a reduced pack is a prefix of the full ranking. Tested by sweeping budgets and comparing unit lists. |
| Omissions are named | **True** | Every omission carries subject, what, reason and cost, in the engine's own vocabulary. |
| A refused budget carries the minimum | **True** | As a number in `minimum_tokens`, not only in the prose. |
| An ambiguous edge arrives with its candidates | **True** | `ResolutionState::Ambiguous` carries the candidate list, and the whole relation is embedded rather than projected. |
| An unresolved edge arrives with its reason | **True** | `ResolutionState::Unresolved` carries a typed reason that serialises as a tag. |
| Nothing is projected in a way that could disagree with the index | **True, with one exception** | Walk steps derive their state string from the relation they carry, and the `uncertain` list is a filter of the edges rather than a second read. The exception is `index_status`; see below. |
| Excluding `index`, `watch_start` and `watch_stop` from determinism is sound | **True** | `index` mutates and reports elapsed time; the two watch tools mutate and report counters. Nothing else does either. |
| `index_status` also varies | **No** | It measures a reader opened once per session and cached, so two calls against an unchanged index are byte-identical. Including it in the determinism test is right. |
| A second `peek-mcp` process is refused | **False** | See below. |

### The red test

`only_the_binary_holds_the_processs_stdout` in `tests/transport.rs` asserts that the only source
file naming `io::stdout` is `src/main.rs`. Three files name it: `src/main.rs`, which means it, and
`src/server.rs` and `src/writer.rs`, which mention it in prose while explaining that they do not
call it. The guard searches the file text, comments included, and compares the result against one
expected name. It fails.

This is worth pausing on, because it is a test that *cannot* have passed and the report says the
suite is green. The fix is small — either strip comments before searching, or assert on the files
that hold a real call. Doing the first without the second leaves a guard that a sentence of
documentation can switch off, which is a worse failure mode than the one being fixed.

### `index_status` reports one generation, not two

`session.rs` and `tools/index.rs` both say the tool "reports the generation the handle was opened at
*and* the generation the store currently records, and says whether they differ". It does not. Both
numbers in the response come from the same cached field, read twice, and `handle_is_stale` is the
comparison of a value with itself — a field that can never be true. Nothing in the crate reads the
generation currently on disk.

The existing test asserts that the two figures are equal and that the flag is false, which pins the
tautology in place and reads as though it were the guarantee. The information is not hard to get:
the watch status already carries the real generation from the last applied refresh, and
`index_status` includes the watch state. Joining the two is a few lines.

### One writer, or one writer per process

Within a process the claim holds and is tested: `index` and `watch_start` are refused while a watch
is running, with the watch id and a sentence saying why. Across processes it does not. The engine
sets a five-second busy timeout on every connection, so a second `peek-mcp` started against the
same repository does not get a refusal; it blocks for up to five seconds per statement and then
fails with SQLite's own error text, which arrives as `outcome: failed` with `isError: true` and no
advice. The honest outcome for "another process is writing" is the same `refused` the in-process
case gets, naming what to stop.

Whether this matters is a question about deployment. One server per repository is the normal shape
and the case does not arise. It arises the first time somebody leaves a `watch` running in one
window and opens a second client in another, and the failure at that moment is a five-second hang
followed by a message that means nothing.

### Cancellation

A cancellation is recorded in a set that is never pruned. The consequence is that a cancelled
request id is refused for the rest of the session, including for a request the client issues later
with the same number. Reusing small integers is what most clients do, so this fires the first time
anybody cancels anything. Removing the id when the refusal is written is the fix; the accompanying
test is in `crates/peek-mcp/tests/audit.rs`.

The specification allows a receiver to ignore a cancellation naming a request it does not
recognise. It does not ask a server to remember the number for ever, and it says nothing about
refusing an unrelated later request.

### The line limit

`MAX_LINE_BYTES` is documented as a bound on memory. The line is read in full first and its length
inspected afterwards, so the allocation the bound exists to prevent has already happened. The
refusal is then sent with a null id, so a client whose request was too long is owed an answer on
its own id and receives one it cannot match — it will wait for a reply that is not coming. Both are
in the audit tests.

---

## 3. The hand-written protocol layer

**I agree with the decision, and disagree with one of the four reasons for it.**

Checked against crates.io, the author's factual claims hold:

- `rmcp` 3.5.0 is Apache-2.0 and declares a minimum of Rust 1.88. Correct.
- Every transport feature needs `tokio` and `tokio-util` (`transport-io` depends on
  `transport-async-rw`, which pulls `tokio/io-util` and `tokio-util/codec`, and the default feature
  set includes `server`, which needs it). There is no feature set that gives a stdio server without
  an async runtime. Correct, and the conclusion survives: a runtime in front of every
  `peek doctor` and every context compile is a bad trade.
- `mcp-sdk` has not been published since January 2025 and is at 0.0.3. Correct.

Where I part company:

**The minimum-version argument is being spent on a constraint nobody has checked.** The workspace
declares 1.85 and pins 1.98.1; CI only ever builds 1.98.1. So "1.88 against a declared 1.85" is a
hypothetical. It is currently costing a real, hand-maintained protocol layer, which is a bad trade
against a number that has never been tested. Either add a CI job that builds at the declared
minimum, or stop citing it. The same applies to the branch's careful avoidance of let-chains, which
is good discipline for a constraint that is not real until it is gated.

**The licence argument is not an argument.** Depending on an Apache-2.0 crate from an MIT project
is unremarkable and common. Listing it among the reasons reads as padding and invites a reviewer to
discount the two that matter.

**The real cost is not the three message shapes.** It is the unknown unknowns, and there is a
published answer to those that is not being taken. The protocol project ships a conformance suite,
and it contains a scenario whose stated requirement is that a server reject JSON-RPC batch
requests. Running the suite against this server would have turned several of the findings in
section 2 from "I read the code and found this" into "the suite says this", and would have found the
rest. The argument in the crate documentation is written as though writing the protocol is the only
option; the alternative that was not considered is *testing* the protocol.

### Version negotiation

Formally correct against both the 2025-06-18 and 2025-11-25 revisions: echo the client's version
when the server speaks it, otherwise answer with one it does. The rule is unchanged between those
revisions.

Substantively, it claims more than the code supports. `KNOWN_PROTOCOL_VERSIONS` contains
`2025-11-25`, so a client asking for it is told `2025-11-25` — a claim of conformance to a revision
whose differences (tasks, icons, the clarified rule that argument errors are tool execution errors)
this server implements none of. The constant the crate names as the version it implements is
`2025-06-18`, and the comment on the list says the last entry is the newest this build has been
written against, which contradicts it. When the client asks for something unrecognised, the server
answers `2025-06-18` — which is also not the newest version the list claims to support.

The existing test only checks that the answer is *a member of the list*, so it passes. Either drop
`2025-11-25` from the list, or move `PROTOCOL_VERSION` up to it and implement the differences. The
first is a one-line change and is almost certainly the right one.

### Batching

Refusing a batch with `-32600` is right, and the spec is on the author's side: batching was removed
in 2025-06-18, so a conforming client never sends one, and the conformance suite expects a server
to reject it. There is a theoretical objection from strict JSON-RPC 2.0, which permits batches and
expects a response; it was raised against the specification change and the specification kept its
decision anyway. Refusing is also the more useful behaviour, because the refusal names the shape it
received.

### Cancellation during a long call

The single-threaded reader is real, and a `notifications/cancelled` that arrives while `index` is
running sits in the pipe until the run finishes. The author's account of this is accurate.

Is it acceptable? For the stated use, yes. Indexing is the only slow call, it is not usually
cancelled, and a client that gives up and stops waiting is no worse off than one that receives a
refusal forty seconds later. The limitation is real, and the *documentation* of it is the problem:
it appears in a module comment in `server.rs` and in a summary written for a reviewer, and nowhere
a user will meet it. The `index` tool's description — the only text a model reads when deciding
whether to call it — does not say that a call cannot be interrupted, nor that cancellation takes
effect only against a request that has not started. That is a one-paragraph addition to the
description and it is where it belongs.

### `isError`

The server marks a call as errored only when the engine failed for a reason the caller cannot fix.
A bad argument, an unknown target, an ambiguous name and a budget below the floor all come back
with `isError: false` and a populated payload.

The stated reason is that some clients render an errored call as a one-line failure and never show
the model the content, which is exactly wrong for "your name matched three things, here they are".
That is a real client behaviour and the reasoning is sound. But it is a deliberate divergence from
the direction the specification is moving: the 2025-11-25 revision clarifies that input validation
errors should be returned as tool execution errors, precisely so a model can self-correct. There is
also a second-order cost the crate does not name — some clients gate an automatic retry on
`isError`, so a refused call looks like a call that succeeded and is answered with the same
question. The design is defensible; it should be recorded as a divergence rather than presented as
neutral framing.

---

## 4. The highest-value missing test

**`index_status` must report the generation the store currently records, and say so when it differs
from the one the session's reader was opened at.**

The guarantee is stated twice in the module documentation and is unobservable in the response: the
two numbers are the same number and the flag that is supposed to compare them is always false. The
test that should exist starts a watch, writes a file, waits for the refresh to be applied, and then
asserts that the store's generation has moved on and that the response says the handle is behind.
It would fail today, and the existing test — which asserts the opposite on a session where nothing
has changed — would need to be replaced rather than supplemented.

This is the highest-value one because it is the only place where a caller is told something about
staleness and gets a reassurance instead. Everywhere else, either the guarantee holds or the field
is absent.

Three smaller gaps worth naming:

- **A `println!` that is not at the start of a line.** The scan is the only thing standing between
  a stray print and a dead session, and it matches one style of mistake. There is a test in
  `crates/peek-mcp/tests/audit.rs`.
- **Two processes writing the same index.** Nothing covers the cross-process case, which is the one
  the in-process refusal does not handle.
- **A client that reuses a request id.** Covered in `crates/peek-mcp/tests/audit.rs`.

---

## 5. Before merging

Not as it stands. Five things, in this order:

1. **Fix the red test.** One assertion in `tests/transport.rs` is failing, and it is failing because
   it counts comments as calls. Strip comments, or assert on a real call. The report says the suite
   is green; it is not, and that is worth correcting wherever the report is kept.
2. **Split the relation-kind change into its own commit** so that each widening is separately
   revertable, which is the property the history claims and only two of the three currently have.
   This is a rebase and a one-line message.
3. **Drop `2025-11-25` from the supported versions, or implement it.** One line, and it stops the
   server telling a client it speaks something it does not.
4. **Decide the `Query::resolve` question with the command-line interface in the room.** The
   widening is defensible; having it arrive without anyone noticing that the second surface already
   solved the same problem differently is not.
5. **Rebase.** The branch is 52 commits behind the main line, 15 of which touch the engine. One of
   those added a second binary, so the root manifest now names three workspace members where this
   branch names two, and the one-line member change has to be re-applied by hand. Cheap now, and
   the source of a merge conflict later if it waits.

None of these is large. Three are single-line or single-commit changes, one is a rebase, and one is
a decision rather than work. The rest of the crate is in good order: the budget contract is real
and enforced in the engine rather than restated, uncertainty reaches the caller as typed state, the
resolution path is the engine's own in both surfaces, the source-scan file list is complete, the
subprocess test is a real subprocess, and the answer shapes are uniform enough that a caller can
branch on one field.

---

## 6. Tests added by this review

`crates/peek-mcp/tests/audit.rs` holds four tests, one per claim this review finds to be false or
unobservable. All four fail against the code as it stands, and each is written so that a fix turns
it into the test that keeps the fix:

| Test | Claim it falsifies |
|---|---|
| `a_cancelled_request_id_can_be_reused_by_the_client` | A cancellation is about one request, not one number |
| `only_the_binary_names_the_processs_stdout` | One file holds stdout, and it is the binary |
| `the_stdout_scan_fails_a_print_that_is_not_at_the_start_of_a_line` | The source scan prevents a stray print |
| `an_oversized_request_is_answered_on_the_id_it_carried` | The line limit is a bound, and an over-long request is still answered |

They belong in the crate's test directory and compile only where the crate exists, so they land
after the server does. A red suite is the intended state of these four until the matching defect is
fixed; deleting one without fixing the defect throws away the evidence.

