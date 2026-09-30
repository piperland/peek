# What Peek is allowed to trust about a path

This is the answer to one question:

> When a command is asked whether a user-supplied path belongs to the selected repository, and
> that path may not exist, what is Peek allowed to treat as evidence?

It governs `crates/peek-cli/src/paths.rs`, and the one function there that implements it is
`relative_to`, through `locate_within`. Nothing else in the crate decides containment.

## The invariant, stated

> **Peek trusts the repository root to be where `canonicalize` said it was. For the path the user
> supplied, Peek trusts the base it was anchored to and the letters in it, and asks the filesystem
> for exactly two reasons — to follow a symlink out of the tree, and to say whether a spelling that
> reads as outside is one directory under another name. A filesystem that cannot answer is never
> treated as a permission.**

Broken into the four claims that are actually testable:

1. **The base is the root, not the working directory.** Once `--root` has named a tree, a relative
   path means a path in that tree. The same command line therefore means the same thing whatever
   directory the user is standing in. The root is the single exception, because it is the only path
   with no other candidate to be relative to.
2. **Containment is decidable without the filesystem.** A path that does not exist is still
   classifiable. Being unable to look something up is a fact about *existence*, and must never be
   promoted into a fact about *location*. This is why the refusal at step 3 is still reached without
   any I/O — a share that is not answering costs no round trip — even though step 4 may go on to ask
   about a spelling it has already refused.
3. **A resolution may add a refusal, and may remove one it produced itself — but only ever by naming
   a location.** Lexical containment is necessary but not sufficient: a symlink inside the tree that
   points out of it is lexically inside and physically outside, which only the filesystem can see.
   It is equally not sufficient on its own, because letters are not always a location. So the
   filesystem is consulted once, and its answer is believed when it is a location. A walk that
   resolved nothing answers with the spelling it was handed, which is the very thing the lexical
   test already rejected, so **a resolution cannot overturn a refusal by declining to answer**.
4. **A missing tail is trusted as a name, and that trust is a risk, not a proof.** Resolving
   `<root>/a/b/c.rs` when only `<root>/a` exists means trusting that `b` and `c.rs` are plain names
   under a directory whose real location has been verified. See *The risk accepted* below.

## The four notions, and why three of them are not usable

These are not variations on a theme. They answer different questions, they need different inputs,
and they disagree on real inputs. The previous implementation used all three of the first ones
stacked on top of each other, which is why its contract could not be written down.

| notion | what it needs | what it can decide |
|---|---|---|
| **lexical containment** | nothing but the two paths | where the path *would* be if it existed |
| **filesystem-resolved containment** | every component to exist | where the path *is* |
| **trusted-ancestor containment** | the deepest component that exists | where the path *is*, given a trusted missing tail |
| **canonicalized containment** | the whole path to exist | where the path *is*, now |

**Canonicalized containment is unusable**, because it has no answer for a path that does not exist,
and a nonexistent path is the case `peek rm` exists to serve. A file deleted between the last index
and the command that repairs the index is a file the user must still be able to name. Any rule
whose first step is `canonicalize()` on the whole path has already made that case unreachable before
it has decided anything else.

**Pure filesystem-resolved containment is unusable for the same reason**, and is strictly weaker
where it does answer: it decides only the paths that happen to exist today, so two runs of the same
command over the same command line can disagree depending on whether the file is still there.

**Pure lexical containment is necessary but not sufficient.** `<root>/link/secret.rs`, where `link`
is a symlink to `/etc`, is lexically inside the root and physically outside it. An implementation
that only compared components would let `peek rm` remove index rows for a file the repository does
not contain. The current code catches this, and the test
`a_symlink_pointing_outside_the_repository_is_outside_it` must keep passing.

**Trusted-ancestor containment is what is used**, with lexical containment running first as a
necessary condition. It is the only one of the four that (a) has an answer for a path that does not
exist, and (b) still resolves every component that does exist, so a symlink escape is still caught.
The refinement it adds over pure lexical reasoning is exactly the refinement needed, and no more.

## The decision, in order

Given a root that has already been canonicalized, and a path the user typed:

1. **Anchor it.** Absolute: unchanged. Relative: joined onto the root. Empty: refused. A Windows
   drive-relative spelling (`C:foo`, or a bare `C:`) is refused: its meaning is relative to
   whatever directory that drive happens to be reading, which is process-global state nobody named
   and which `canonicalize` would consult on Peek's behalf. A root that is not absolute is refused
   too, because the root is a precondition here and not an input to be interpreted.
2. **Normalize the spelling.** Collapse `.`, collapse `//`, and apply `..` against the accumulated
   components, clamping at the filesystem root. A `..` that would climb above the root stays there
   and makes step 3 fail, rather than being silently dropped. Separators are the platform's own:
   `\` is a separator on Windows and an ordinary filename character on Unix, and treating it as a
   separator on Unix would take away a legal path.
3. **Test lexical containment.** Component-wise prefix against the root. Not a string prefix:
   `<parent>/repo-old` is not inside `<parent>/repo`. This step never touches the filesystem, so it
   always produces an answer — including for a path nothing has ever heard of, a volume that is not
   mounted, and a share that is not answering.
4. **Put a refusal to the filesystem, once, where the filesystem can settle it.** A refusal at step 3
   is a statement about spelling, and **a directory can have more than one spelling.** The root is
   canonical and a path typed in full is not, so two spellings of one location is the ordinary state
   of affairs rather than an accident:
   - on Windows, `canonicalize` answers `\\?\C:\repo` where the caller wrote `C:\repo` — a prefix
     difference, read as the bare prefix it stands for by the component comparison — and it *also*
     expands a short name on the way to a directory, so `C:\Users\RUNNER~1\repo` is
     `\\?\C:\Users\runneradmin\repo`;
   - on macOS there is no prefix at all and `/var` is a link to `/private/var`, so every temporary
     directory is two spellings of one place.

   Those are settled by asking, and **not** by deciding that two spellings are equal when they are
   not: a *name* carries a location, so only the filesystem can say that `RUNNER~1` and
   `runneradmin` are one directory. The question is asked only where it can mean anything — the
   spelling climbs nowhere, and the volume is the root's own — so **a path that climbs or sits on
   another volume is still refused here, without any I/O at all**, which is what keeps an unanswering
   share from costing a network round trip. And the answer can only overturn a refusal by naming a
   location: a walk that resolves nothing falls back to the spelling, which is what step 3 already
   tested.
5. **Resolve the deepest existing prefix and test that too.** A location outside the root is refused,
   with the resolved location named. This step catches a symlink that pointed out of the tree, and it
   is the only step that can see one: the link is invisible to a comparison of letters. This step may
   add a refusal, and may turn step 4's refusal back into an answer.
6. **What is left is trusted.** The answer is the *resolved* location, not the spelling, so a
   symlink and the directory it points at are one path and the index holds it under one name. The
   missing components are names under a directory whose real location step 5 verified.

Steps 3 and 5 are both necessary and neither is sufficient alone. That is the whole content of the
design, and it is why the two failures this document was written for were failures: the old
implementation had step 5 where step 3 should have been, so a path that had never existed was
answered by the filesystem instead of by the contract. Step 4 exists because step 3 is a *necessary*
condition only: it is a comparison of letters, and letters are not always a location.

### Why the answer is the resolved location, not the spelling

`<root>/src/link/../a.rs`, where `link` is a symlink to `<root>/real`. To a reader of the spelling,
`link/..` is `src`, so the path names `src/a.rs`. A kernel follows the link first and then climbs,
so it opens `<root>/a.rs`. The repository contains both files, so containment cannot separate the two
candidates and only the filesystem can — and it is the index, not the user, that decides which
spelling a file is stored under.

So the gate reads the spelling and the answer reads the resolution. Normalizing the spelling and
stopping there would remove the wrong file's rows and report success: two files, both inside the
root, and no check with anything to say about either. The tail cannot reintroduce the same
ambiguity, because the walk in step 5 only stops where a component is *missing*, and a missing
component has no symlink to follow — so there is nothing for a `..` in the tail to mean other than
what it is read as.

**The answer is not the same file on every platform, and that is not a defect to be smoothed over.**
A kernel resolves `..` after following the link, so the spelling above names `<root>/a.rs`. Windows is
given a DOS path, and the DOS path parser removes `.` and `..` while it converts the path — before the
filesystem is asked anything — so the link is never followed and the same spelling names
`<root>/src/a.rs`. Peek's contract is the location the operating system would open, and Windows would
open `src\a.rs`, so `src\a.rs` is the answer there and `a.rs` is the answer on a kernel. A test that
wrote one of those down as the expectation for both would be asserting a platform's resolution rather
than the contract, so the test asks the filesystem which file the spelling opens and requires the
answer to be that file. The consequence for a caller is the same one *mixed separators* already
records: the two platforms judge an identically spelled path differently, and that is correct.


## Cases, and what each one means

| case | rule |
|---|---|
| **relative path** | anchored to the root, never to the working directory. CWD-independence is the property. |
| **absolute path** | used as given. The root still has to contain it; the root never participates in how it is read. |
| **repository root** | anchored to the working directory. The one path with no other candidate. Canonicalized in full, because it must exist. |
| **current working directory** | read once, in `resolve_root`, for the root only. A path *within* a named root never consults it — and neither does a drive-relative spelling, which is refused rather than resolved against a per-drive working directory. |
| **lexical normalization** | `.`, `//` and `..` are resolved by the tool, in the tool, against the root. The user does not do it, and the filesystem does not get to reinterpret it. |
| **`..` traversal** | resolved lexically for the gate. A climb that leaves the root is refused at step 3 and is **not** put to the filesystem: the refusal is about what the path asked for, not about where it landed, so nothing is opened. A climb that stays inside is an ordinary path, and when the middle of it does not exist the arithmetic is the only thing available. |
| **symlinked parents** | caught at step 5, because the deepest existing prefix is resolved and re-tested. Adds a refusal. |
| **one directory, two spellings** | the reason step 4 exists. A root that is canonical and a path typed in full that is not are two spellings of one location on Windows (the prefix, and a short name the filesystem expands) and on macOS (`/var` for `/private/var`). Settled by asking the filesystem, never by folding names in a comparison — a name carries a location. |
| **a path through a link that names the repository** | answered as the repository. The link is a second name for the root, the filesystem resolves it there, and the index holds the file under the name the walk found. |
| **nonexistent final component** | normal. Addressable, and the reason `rm` works on a deleted file. |
| **nonexistent intermediate component** | normal, and the case the old code could not reach at all. The walk goes up until something resolves, and the unresolvable tail is names under it. |
| **Windows drive letters** | `C:\foo` is absolute and used as given. `C:foo` and a bare `C:` are drive-relative and **refused**: they name whatever directory that drive is currently reading, which is process-global state the user did not name and which `canonicalize` would consult silently. |
| **UNC paths** | absolute. `\\server\share` is compared as a path, not as a network location. A drive-letter root cannot contain one, and the volume comparison at step 4 says so before the filesystem is touched — so a share that is not answering costs no network round trip. |
| **mixed separators** | the platform decides. On Windows `..\..\x` and `../../x` are the same traversal. On Unix a backslash is a character in a name, and treating it as a separator would refuse a legal file. The consequence is that the two platforms judge an identically-spelled path differently, and that is correct: the string means different things on each. |
| **`..` after a symlink** | the platform decides, for the same reason as mixed separators. A kernel follows the link and then climbs; Windows removes the `..` from the DOS path before the filesystem is asked, so the link is never followed. The answer is the file each platform would open. |
| **case-insensitive filesystems** | the root arrives canonical, so it carries the on-disk case. A user who types `C:\Repo` against a root of `C:\repo` is refused, loudly, rather than being guessed at — and every comparison the tool makes internally is between two on-disk spellings. See *What is not decided here*. |
| **worktrees** | a linked worktree has its own root and therefore its own `RepoId` and its own index, keyed on the git common directory. A path into a sibling worktree is outside the root and is refused by the same rule as any other path outside. |

## What is not decided here

**How far the one-directory-two-spellings rule reaches.** This is the question the prefix opened and
it is not finished, because the prefix is the *smallest* of the differences and the only one a
comparison is allowed to settle on its own.

What **is** decided: a verbatim prefix is not a location, so the component comparison reads
`\\?\C:\repo` as the `C:\repo` it stands for. On Windows that is necessary rather than convenient —
`canonicalize` answers in the extended-length form, so the root `resolve_root` hands on and a path
typed in full differ in their first component and in nothing else, and comparing them as strings
refused **every** path inside the repository with a message blaming a symlink for it, which is a
second thing that is not true.

What **is not** decided, and cannot be decided the same way, is everything *below* the prefix,
because a name carries a location and only the filesystem can say whether two of them are one
directory. `canonicalize` expands a short name on the way to a directory, so
`C:\Users\RUNNER~1\repo` and `\\?\C:\Users\runneradmin\repo` differ in a component that means
something; on macOS there is no prefix at all and `/var` is a link to `/private/var`, so every
temporary directory is two spellings of one place. Those are put to the filesystem at step 4 — **not**
by deciding in a comparison that two names are equal, which would silently widen containment to any
pair of paths that happened to be written alike. There is no decision here about which names to fold
because the answer is not a property of the spelling, and a rule that tried to be one would be a rule
about a machine rather than about containment.

**Case-insensitive containment.** On Windows and macOS, `src/Foo.rs` and `src/foo.rs` are one file,
and a user who typed the wrong case has named a file that is inside the repository. **A path typed
with the wrong case carries no `..` and sits on the root's own volume, so step 4 puts it to the
filesystem and the filesystem answers for it — this is now correct by accident of where the step is,
and that is not a reason to rely on it.** It depends on `canonicalize` handing back the on-disk case,
which it does, rather than on anything this document decides. The comparison itself is still exact, so
a root and a path that disagree about case and are not both resolvable are still refused. **This is a
false refusal, not an escape**, and it is not made worse by anything above.

**A climb that hides a link from the walk.** The walk at step 5 stops at the deepest component that
resolves, and it cannot resolve past a component that does not exist. So for
`<root>/never/../link/x.rs` where `never` is absent, the walk reaches `<root>`, rebuilds the tail and
hands back `<root>/link/x.rs` with `link` **unfollowed** — while the answer is reported as inside the
root. No operating system can open that path at all, so there is no "file it would open" for the
answer to be wrong about, but the index would hold `link/x.rs` and a link pointing out of the tree
would go unexamined. This is the trusted-ancestor assumption below, taken one level further than
*The risk accepted* states, and it is recorded rather than fixed because the fix is not a containment
question: it is a question about whether a path nothing can open should be nameable at all.

**Whether a missing component is a directory.** `is_directory` is false for anything that is not a
directory right now, which includes a directory that has not been created yet. `peek rm src/new`
where `src/new` does not exist removes one path rather than a subtree. That is a scope decision the
caller makes, not a containment decision.

**Time of use.** See below.

## The risk accepted

Resolving `<root>/a/b/c.rs` when only `<root>/a` exists requires trusting that `b` is a directory
and not a symlink, and that `c.rs` is a file. Neither exists, so neither has been checked, and the
containment answer is a statement about the moment the path was resolved rather than about the
moment it is used.

A concurrent process that creates `<root>/b` as a symlink to somewhere outside the tree, after this
check and before the caller acts, defeats the check. Peek accepts this because:

- the alternative is refusing every nonexistent target, which makes `peek rm` useless for the one
  job it has — repairing an index after a deletion;
- the caller of the check is `peek rm`, which **does not touch the filesystem**; it removes rows
  from a SQLite index and unlinks nothing, so the confused-deputy window is a *misattributed row
  removal* and not a write to a file outside the tree;
- the repository is the user's own working tree, so the party who would plant the symlink is the
  party who could simply write the file it points at.

**This argument holds for `peek rm` and would not hold for a command that unlinked or wrote.** If a
future command uses `relative_to` to guard a filesystem write, it must re-resolve at the moment of the
write, and this document says so rather than leaving the reuse to look free.

## What a caller observes

**Before → after, on the refusal path.** A path outside the repository whose intermediate
components do not exist used to produce a filesystem error:

```
cannot resolve <path>: cannot resolve <parent>: No such file or directory (os error 2)
```

and it still exits 2, with `refusal.kind` still `outside_repository`, so no caller that branches on
the kind or the code sees a change. What changes is the *message*: it now names the repository the
command was pointed at and the location the path points to, which is the information the caller
needed and did not get. Two cases that used to be indistinguishable are now distinguished, because
they have different causes and different fixes:

```
<path> is not inside the repository at <root>; this command will not touch anything outside the
tree it was pointed at

<path> is written as if it were inside the repository at <root>, but it reaches <outside>, which
is outside the tree. A symlink inside the repository points out of it, and this command will not
follow one to a file the repository does not contain
```

The first is a spelling that names the wrong tree. The second is a spelling that names the right
tree and a symlink that does not lead there, and it names where the link actually goes, which the
old message never did.

**Before → after, on the allowed path.** A nonexistent path *inside* the root, whose parent
directory also does not exist, was refused with the same filesystem error. It is now addressed, and
`peek rm` on it removes whatever rows the index held at that path — which is a measured zero when
the index held none. **A command that previously failed and now succeeds is the observable change
most likely to surprise somebody**, and it is the intended one: the old answer was a filesystem
error standing in for a decision the contract had not made.

**Two new refusals, both on paths that were previously answered by a filesystem call.** A
drive-relative path on Windows (`C:src/a.rs`) is now refused rather than joined onto the root, and
a root that is not absolute is now refused rather than resolved against the working directory. Both
were answered before; neither is reachable from a command that went through `resolve_root`, so
neither changes what any current caller sees. They exist because `relative_to` is a public
function and a caller that skipped the resolver should be told, not accommodated.

**Unchanged, deliberately.** The exit code for a containment refusal is 2, not 3. `exit.rs` says 2
means "the command line was not understood" and 3 means "the question cannot be answered from this
index", and a path outside the repository is arguably the second. That is a taxonomy question about
`outside_repository` and it is not this change's to make; only the message moved.


## The two failures this answers

Both were the same defect seen twice, and both are symptoms rather than causes.

`rm_refuses_a_path_outside_the_repository_and_names_both_places` and the second case inside
`the_exempt_command_lines_still_mean_what_they_meant` run `peek rm` at a path under the fixture's
*index directory* that has never been created. The old code asked the filesystem to resolve the
whole path, the filesystem said no such file, and a question that was answerable without asking —
*is this string inside the root?* — was answered by the filesystem instead, with an error that
mentions neither the root nor the contract.

The failing `canonicalize` was an **answer**, not a specification. It did not say which notion of
containment was intended; it only said the filesystem could not help. Reading the contract out of
that error is how the current three-notions-in-one-function shape got here.

## A failure this change does not fix, and why

`rm_on_a_subtree_removes_everything_at_or_below_it_and_says_so` in `tests/cli.rs` asks
`peek rm src/ledger` on a fixture where `src/ledger.rs` exists and `src/ledger` does not, and
asserts the scope is `subtree`. The answer is `file`, because nothing at `src/ledger` is a
directory. **This test fails on `origin/main` and failed before any of this work**, it is not
caused by this change, and it is not a path-containment question: the path is inside the repository
and the containment check is doing its job. It is a *scope* question — whether a removal of a stem
with no file of its own should be a subtree or a file — and `paths.rs` is not where that is decided;
`commands/rm.rs` reads `Relative::is_directory` and chooses.

It is recorded here because the `is_directory` flag is this module's, and the rule above is what
this module now guarantees about it: `is_directory` is false for anything that is not a directory
right now, **including a directory that has not been created yet**. If the intended answer is that
`src/ledger` means the subtree `src/ledger.rs`, then the flag is the wrong place to express it and
the fix belongs in the caller or in the flag's meaning. This document does not decide that, and
changing `is_directory` to make the test pass would be a scope decision made by a path check.

