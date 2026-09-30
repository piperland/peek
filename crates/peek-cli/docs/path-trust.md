# What Peek is allowed to trust about a path

This is the answer to one question:

> When a command is asked whether a user-supplied path belongs to the selected repository, and
> that path may not exist, what is Peek allowed to treat as evidence?

It governs `crates/peek-cli/src/paths.rs`, and the one function there that implements it is
`contain`. Nothing else in the crate decides containment.

## The invariant, stated

> **Peek trusts the repository root to be where `canonicalize` said it was. For the path the user
> supplied, Peek trusts only the base it was anchored to and the letters in it. Peek asks the
> filesystem about that path for exactly one reason — to follow a symlink out of the tree — and a
> filesystem that cannot answer is never treated as a refusal and never treated as a permission.**

Broken into the four claims that are actually testable:

1. **The base is the root, not the working directory.** Once `--root` has named a tree, a relative
   path means a path in that tree. The same command line therefore means the same thing whatever
   directory the user is standing in. The root is the single exception, because it is the only path
   with no other candidate to be relative to.
2. **Containment is decidable without the filesystem.** A path that does not exist is still
   classifiable. Being unable to look something up is a fact about *existence*, and must never be
   promoted into a fact about *location*.
3. **A resolution may only make the answer stricter.** Lexical containment is necessary but not
   sufficient: a symlink inside the tree that points out of it is lexically inside and physically
   outside. The filesystem is consulted for exactly that, and its answer can only add refusals.
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

1. **Anchor it.** Absolute: unchanged. Relative: joined onto the root. Empty: refused.
2. **Normalize it lexically.** Collapse `.`, collapse `//`, and apply `..` against the accumulated
   components, clamping at the filesystem root. A `..` that would climb above the root stays there
   and makes step 3 fail, rather than being silently dropped. Separators are unified to `/` first,
   because on Windows `\` is a separator and `..\..` is a traversal spelled the other way.
3. **Test lexical containment.** Component-wise prefix against the root. Not a string prefix:
   `<parent>/repo-old` is not inside `<parent>/repo`. This step never touches the filesystem and
   always produces an answer. **This is the step that produces the refusal for every path that is
   outside the repository, including paths nothing has ever heard of.**
4. **If anything on the path exists, resolve the deepest existing prefix** and test that too. A
   canonical location outside the root means a symlink pointed out of the tree: refused. The
   resolution result is *only ever allowed to refuse*.
5. **What is left is trusted.** The canonical location of the existing prefix is inside the root,
   and the remaining components are names. The repository-relative path is the missing tail. A
   missing component that is not a directory on disk means `is_directory` is false, and a path that
   does not exist is addressable.

Steps 3 and 4 are both necessary and neither is sufficient alone. That is the whole content of the
design, and it is why the two failures this document was written for were failures: the old
implementation had step 4 where step 3 should have been, so a path that had never existed was
answered by the filesystem instead of by the contract.

## Cases, and what each one means

| case | rule |
|---|---|
| **relative path** | anchored to the root, never to the working directory. CWD-independence is the property. |
| **absolute path** | used as given. The root still has to contain it; the root never participates in how it is read. |
| **repository root** | anchored to the working directory. The one path with no other candidate. Canonicalized in full, because it must exist. |
| **current working directory** | read once, in `resolve_root`, for the root only. A path *within* a named root never consults it. |
| **lexical normalization** | `.`, `//` and `..` are resolved by the tool, in the tool, against the root. The user does not do it and the filesystem does not get to reinterpret it. |
| **`..` traversal** | resolved lexically first. Filesystem `..` follows symlinks and can move *further* than `..` asks for; lexical `..` moves exactly one component. A climb that leaves the root is refused at step 3. |
| **symlinked parents** | caught at step 4, because the deepest existing prefix is canonicalized and re-tested. Only ever adds a refusal. |
| **nonexistent final component** | normal. Addressable, and the reason `rm` works on a deleted file. |
| **nonexistent intermediate component** | normal, and the case the old code could not reach. The walk goes up until something resolves, and the unresolvable tail is names under it. |
| **Windows drive letters** | a drive-relative path (`C:foo`, no separator after the colon) is not absolute and is not relative to anything Peek can name, so it is refused rather than guessed. `C:\foo` is absolute. |
| **UNC paths** | absolute. `\\server\share` is compared as a path, not as a network location; a UNC path cannot be inside a drive-letter root, and the comparison says so. |
| **mixed separators** | unified before anything else, so `..\..\x` and `../../x` are the same traversal and neither is a filename. |
| **case-insensitive filesystems** | the root arrives canonical, so it carries the on-disk case. A user who types `C:\Repo` against a root of `C:\repo` is refused, loudly, rather than being guessed at — and the root's own canonicalization means every comparison the tool makes internally is between two on-disk spellings. See *What is not decided here*. |
| **worktrees** | a linked worktree has its own root and therefore its own `RepoId` and its own index, keyed on the git common directory. A path into a sibling worktree is outside the root and is refused by the same rule as any other path outside. |

## What is not decided here

**Case-insensitive containment.** On Windows and macOS, `src/Foo.rs` and `src/foo.rs` are one file,
and a user who typed the wrong case has named a file that is inside the repository. The current code
refuses that, because the root is canonical and the argument is not, and the comparison is exact.
Making it accept the wrong case needs a per-filesystem answer to "does this filesystem fold case",
which is a different question from containment and is not answered here. **This is a false refusal,
not an escape, and it is unchanged by this document.** It is recorded so that a future change to it
knows what it is changing.

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
future command uses `contain` to guard a filesystem write, it must re-resolve at the moment of the
write, and this document says so rather than leaving the reuse to look free.

## What a caller observes

**Before → after, on the refusal path.** A path outside the repository whose intermediate
components do not exist used to produce a filesystem error:

```
cannot resolve <path>: cannot resolve <parent>: No such file or directory (os error 2)
```

and it still exits 2, with `refusal.kind` still `outside_repository`, so no caller that branches on
the kind or the code sees a change. What changes is the *message*: it now names the repository the
command was pointed at and the location it refused to touch, which is the information the caller
needed and did not get.

**Before → after, on the allowed path.** A nonexistent path *inside* the root, whose parent
directory also does not exist, was refused with the same filesystem error. It is now addressed, and
`peek rm` on it removes whatever rows the index held at that path — which is a measured zero when
the index held none. **A command that previously failed and now succeeds is the observable change
most likely to surprise somebody**, and it is the intended one: the old answer was a filesystem
error standing in for a decision the contract had not made.

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
