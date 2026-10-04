# The per-language gate's Rust fixture.

One crate, deliberately small and completely readable. Every declaration in it is
listed in `gate.expect`, and the expectation file is the ground truth the gate
scores the engine against. Nothing here is built; the fixture is parsed.

## What is in it, and why

| Feature | Where | Why it is here |
|---|---|---|
| Same name in two files | `report::format_line`, `service::format_line` | A graph that resolves by bare name picks one. A graph that reports two candidates is measurably better, and the ambiguity surface is exercised. |
| A generic inherent impl | `model::Pair` | The impl block's declared name carries its type arguments, so a method's owner is spelled `Pair<U>` rather than `Pair`. That is a real identity limitation and it is why `Pair.first` is not addressable. |
| A supertrait | `traits::Saveable: Clock` | The one `inherits` edge in the fixture. Cortex declared the kind and never constructed one. |
| Two implementations of one trait | `Ticks`, `Meters` | Forces a real choice, and the two `Receipt` associated types are the same name in the same file under different impl blocks. |
| Grouped, aliased and glob imports | `report.rs`, `service.rs` | The three import shapes whose handling E4 names separately. |
| A relative `use` at the crate root | `lib.rs` | Resolves by repository-wide uniqueness, and loses the module the statement named. |
| A call inside a macro body | `counted!(body())` | The body is a token tree, so the call is not an expression the walker can see. |
| A one-segment path call | `service::describe(..)` | `Type::method()` is byte-identical, so this is reported ambiguous rather than guessed. |
| A nested inline module | `outer::inner::deep` | Exercises a three-deep ownership chain in the qualified name. |
| A call through a variable of known type | `clock.now()` | The receiver is `Evidence::ReceiverType`, the one call in the fixture that resolves through a receiver. |

## The numbers this fixture produces

Measured, not asserted as a target. Each row is the figure the gate reports for the
engine at the time the floor was recorded, and each floor in `gate.expect` is set to
the figure the same run produced.

| Dimension | Measured | Floor |
|---|---|---|
| `symbol_precision` | 96.67% (87/90) | 96.66 |
| `symbol_recall` | 96.67% (87/90) | 96.66 |
| `definitions` | 60.23% (53/88) | 60.22 |
| `calls` | 97.50% (39/40) | 97.50 |
| `references` | 96.15% (25/26) | 96.14 |
| `resolution_correctness` | 75.00% (36/48) | 75.00 |
| `imports` | 66.67% (10/15) | 66.66 |
| `imports_module_retained` | 0.00% (0/3) | 0.00 |
| `members` | 91.67% (22/24) | 91.66 |
| `inheritance_subject` | 100.00% (5/5) | 100.00 |
| `inheritance_base` | 100.00% (5/5) | 100.00 |
| `negative_references` | 100.00% (14/14) | 100.00 |
| `negative_inheritance` | 100.00% (3/3) | 100.00 |
| `incremental` | 100.00% (2288/2288) | 100.00 |
| `query` | 100.00% (30/30) | 100.00 |
| `context` | 100.00% (7/7) | 100.00 |

A floor is a ratchet, not a target. It says "do not go below this without saying so in
a commit". Two of them sit at zero because that is what the engine measures, and a
floor of zero asserts nothing about them — so for those two the number that matters is
the measurement, not the pass. The full table with denominators is
`../../../LANGUAGE_MATRIX.md`, generated from the measurement.

Every floor is **truncated**, so it is always reachable: 87/90 is 96.666…% and its floor
is 96.66, not the 96.67 the rendered figure shows. That is not a quibble — it is what
a lower bound is, and the gate found it by failing on a transcription of its own
output.

## What the low numbers are

None of them is a rounding artefact, and the two worth naming:

- **`definitions` at 53 of 88.** The walker's `declare` gives a top-level declaration a
  parent only when it declares a module; every other top-level symbol gets
  `parent: None` and so has no incoming structural edge. A `const`, a `static`, a
  function and a type alias at the top of a file are all unreachable from their file.
- **`imports_module_retained` at 0 of 3.** The three imports in `lib.rs` are placed by
  `UniqueName` rather than by their `ImportBinding`, so the module the statement named
  is not in the decided state and cannot be read back out.

`references` used to be the third, and it was not a rounding artefact either: the
`References` class was **unreachable**, not rare. `spec::ReferenceRule::excluded_parents`
listed every node type the spec declares as a symbol and the walker's
`is_excluded_reference` walked *up* to the nearest declared ancestor, so an identifier
inside a function found `function_item` in the list and was excluded — the list was meant
to suppress a declaration's own name and it suppressed every name in the body. The rule
now asks whether an occurrence is the name the enclosing declaration introduced, which is
a question about one node rather than about a subtree; `tests/reference_reachability.rs`
pins that against the engine directly, and `tests/reference_value.rs` measures what the
edge is worth once it exists.

The remaining 1 of 26 is `src/traits.rs`'s `Saveable.save | entry`, and the gate reports
it as a miss because that line is believed wrong: `Saveable.save` has no body, so `entry`
occurs only as the parameter's binding. It is left in place rather than corrected — see
the note at the top of `gate.expect` for why a miss is reported and a contradiction is
corrected.

## `resolution_correctness`: six decided edges point at the wrong entity

`references` asks whether an edge **exists**. It does not ask whether it points at the
right thing, and for a class that was unreachable until the previous change nobody had
reason to ask. Making the class reachable is what exposed this: **six reference rows are
decided and land on an entity the source never referred to**, while every other column in
the matrix reads 100.00 or near it.

The six are listed by name in the generated matrix, each with the rung that placed it, and
two shapes account for all of them:

- **A local binding resolved to a parameter of another function.** `summarise | out` and
  `Runner.run | last` are `let` bindings. The unique-name rung found `render.out` and the
  same-file rung found `retry.last` — both real declarations the source never named. No
  entity is the referent, so the right answer was `Unresolved`.
- **A field read resolved to a parameter of the function the read sits in.** `render |
  count` and `describe | count` are `entry.count` on an `Entry`, and both land on
  `format_line.count`: a different function's parameter, same file, same name.
  `describe | entry` and `format_line_inner | entry` are parameters that **shadow an
  import of the same name**, and the import rung won.

The fixture states where each must point, on a `binds` or `binds_nothing` line. The gate
counts decided-and-wrong separately from undecided and prints both, because they are
different failures with different remedies: a gap is fixed by extracting more, a wrong
edge by resolving differently or not at all, and one number for both hides the second.

### Why the denominator is the decided relations

`resolution_correctness` is a fraction over the relations the engine **decided**, so an
engine could raise it by deciding less. `scripts/language-gate-mutation-check.sh`
demonstrates exactly that: dropping `field_identifier` from the reference rule removes the
two wrong `count` edges along with everything else, and the figure goes **up** to 78.26.
That is why the wrong count and the undecided count sit beside the fraction in
`LANGUAGE_MATRIX.md` rather than inside it, and why the mutation check prints the fact
instead of just the pass.

The full table with denominators is `../../../LANGUAGE_MATRIX.md`, generated from a
measurement rather than written by hand.

## The manifest

`Cargo.toml` is present so the fixture is crate-shaped. The module layout is named after
the directory above the source root, and a fixture without a manifest would make the gate
measure the fallback naming instead of the real one.