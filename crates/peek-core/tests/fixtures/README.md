# Test fixtures

Committed trees for `peek_core::discover` tests. These are real directories with real files, real
`.gitignore` semantics and real byte content, not trees assembled inside a test body. A reviewer
should be able to read a rule and its proof side by side, and should be able to run `git status` to
see that a fixture change is a deliberate, reviewable edit.

## `mixed/`

The main fixture. Exercises, in one tree:

The tests live in the library, under `src/discover/tests/`, not in `tests/` at the crate root — they
reach `pub(crate)` discovery internals. The table names the module and a test in it, so a claim can
be checked by reading rather than by searching for a file that is not there.

| Area | Files | Asserted by |
|---|---|---|
| Ignore-file semantics | `.gitignore`, `.ignore`, `src/nested/.gitignore` | `gitignore_rules.rs` - `nested_ignore_file_is_scoped_to_its_own_directory`, `dot_ignore_file_is_honoured_alongside_gitignore` |
| Ignore files outside a checkout | same tree, no `.git` | `gitignore_rules.rs` - `ignore_files_apply_outside_a_git_repository` |
| Default exclusion policy | `target/`, `node_modules/`, `__pycache__/` | `exclusion_policy.rs` |
| Overridability | same tree, `ExcludeSet::without("target")` | `exclusion_policy.rs` |
| Classification | source, generated, vendored, test | `classification.rs` - `a_generated_directory_makes_the_file_generated`, `vendored_wins_over_generated`, `a_generated_file_name_makes_the_file_generated_without_reading_it` |
| Unsupported extensions | `docs/data.json` (the tree's other non-source files are ignored or classified) | `limits.rs` - `a_tree_of_unsupported_files_says_so` |
| Hidden files are indexed | `.github/scripts/setup.rs` | `exclusion_policy.rs` - `hidden_paths_are_indexed_by_default`, `hidden_paths_can_be_skipped` |
| Size cap | *no file here* - the cap is exercised on trees built at runtime | `limits.rs` - `a_file_above_the_cap_is_skipped_and_counted` |
| Non-UTF-8 skip | `src/corrupt.rs` - a `.rs` file with invalid UTF-8 bytes | `limits.rs` - `one_non_utf8_file_does_not_abort_the_repository` |

Two files in `mixed/` are named by nothing and asserted on by nothing: `src/large.rs` and
`.github/workflows/ci.yml`. They are **realistic noise**, and that is a reason to keep them - a
fixture where every file has a test is a fixture that has been fitted to its tests. But nobody should
assume they are load-bearing: the only whole-tree assertion is `the_by_classification_breakdown_adds_up`,
which checks that the breakdown *sums to* the yielded count, so it holds at any file count. And
nobody should delete them as unused without deciding that noise is worth nothing - the size cap is
tested on a runtime tree precisely because a committed file cannot pin a default cap of 2 MiB.

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
