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
| `symbol_precision` | 96.67% (87/90) | 96.67 |
| `symbol_recall` | 96.67% (87/90) | 96.67 |
| `definitions` | 60.23% (53/88) | 60.23 |
| `calls` | 97.50% (39/40) | 97.50 |
| `references` | 0.00% (0/18) | 0.00 |
| `imports` | 66.67% (10/15) | 66.67 |
| `imports_module_retained` | 0.00% (0/3) | 0.00 |
| `members` | 91.67% (22/24) | 91.67 |
| `inheritance_subject` | 100.00% (5/5) | 100.00 |
| `inheritance_base` | 100.00% (5/5) | 100.00 |
| `negative_references` | 100.00% (14/14) | 100.00 |
| `negative_inheritance` | 100.00% (3/3) | 100.00 |
| `incremental` | 100.00% (1200/1200) | 100.00 |
| `query` | 96.67% (29/30) | 96.67 |
| `context` | 100.00% (7/7) | 100.00 |

A floor is a ratchet, not a target. It says "do not go below this without saying so in
a commit". `references` and `imports_module_retained` sit at zero because that is what
the engine measures, and a floor of zero asserts nothing about them — so the numbers
that matter for those two are the ones in `../LANGUAGE_MATRIX.md`, not the pass.

## What the low numbers are

None of them is a rounding artefact, and the three worth naming:

- **`references` at 0 of 18.** Not "few references found": the `References` class is
  unreachable. `spec::ReferenceRule::excluded_parents` lists every node type the spec
  declares as a symbol, and the walker's `is_excluded_reference` walks *up* to the
  nearest declared ancestor, so an identifier inside a function finds `function_item`
  and is excluded. An identifier at file level is not excluded but still dropped,
  because emission requires an enclosing entity. `tests/reference_reachability.rs`
  pins both halves against the engine directly.
- **`definitions` at 53 of 88.** The walker's `declare` gives a top-level declaration a
  parent only when it declares a module; every other top-level symbol gets
  `parent: None` and so has no incoming structural edge. A `const`, a `static`, a
  function and a type alias at the top of a file are all unreachable from their file.
- **`imports_module_retained` at 0 of 3.** The three imports in `lib.rs` are placed by
  `UniqueName` rather than by their `ImportBinding`, so the module the statement named
  is not in the decided state and cannot be read back out.

The full table with denominators is `../../../LANGUAGE_MATRIX.md`, generated from a
measurement rather than written by hand.

## The manifest

`Cargo.toml` is present so the fixture is crate-shaped. The module layout is named after
the directory above the source root, and a fixture without a manifest would make the gate
measure the fallback naming instead of the real one.