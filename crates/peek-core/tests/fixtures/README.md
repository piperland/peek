# Test fixtures

Committed trees for `peek_core::discover` tests. These are real directories with real files, real
`.gitignore` semantics and real byte content, not trees assembled inside a test body. A reviewer
should be able to read a rule and its proof side by side, and should be able to run `git status` to
see that a fixture change is a deliberate, reviewable edit.

## `mixed/`

The main fixture. Exercises, in one tree:

| Area | Files | Asserted by |
|---|---|---|
| Ignore-file semantics | `.gitignore`, `.ignore`, `src/nested/.gitignore` | `tests/ignore_files.rs` |
| Default exclusion policy | `target/`, `node_modules/`, `__pycache__/` | `tests/exclusion_policy.rs` |
| Overridability | same tree, `ExcludeSet::without("target")` | `tests/exclusion_policy.rs` |
| Classification | source, generated, vendored, test | `tests/classification.rs` |
| Unsupported extensions | `README.md`, `docs/data.json`, `docs/README.md` | `tests/stats.rs` |
| Hidden files are indexed | `.github/workflows/ci.yml` | `tests/exclusion_policy.rs` |
| Size cap | `src/large.rs` | `tests/limits.rs` |
| Non-UTF-8 skip | `src/corrupt.rs` — a `.rs` file with invalid UTF-8 bytes | `tests/limits.rs` |

`mixed/` is deliberately **not** a git repository and has no `.git` directory. Discovery sets
`require_git(false)`, so ignore files apply outside a checkout, and this tree is the evidence that
it does. The worktree-identity test builds a real repository at runtime instead, because that one
genuinely needs `git`.

## `modules/`

Module and package trees for `peek_core::extract`. `nested/` for `mod.rs` at every level,
`several/` for one crate with several modules and an inline one, `cross/` for a `use` that leaves
one package for another, and `reexport/` for a renamed re-export and a glob. See
`modules/README.md` for the table.

`modules/` has no `.gitignore`, so every file in it is a file a test actually reads and
`git clean -fdx` in the verification sandbox is safe for it by construction.

## Runtime trees

Some things cannot be committed:

* **Symlinks.** Git stores them, but their behaviour differs across platforms and a checkout on a
  filesystem without symlink support would break the fixture. `tests/support.rs` builds them.
* **A real git repository with a linked worktree.** `tests/identity.rs` creates one with the `git`
  CLI, because asserting that two worktrees get different ids requires actually having two.

Everything under `mixed/` that a test asserts on is committed. If a test needs a file that is not
there, the test fails rather than creating it.
