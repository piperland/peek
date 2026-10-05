//! The resolver: it turns `Pending` relations into decided ones.
//!
//! The engine Peek replaces had no resolution layer at all. Its indexer held a repo-global
//! `HashMap` from a name to symbol ids, tried three lookups against it, and took
//! `symbols.first()` when there was more than one match (audit B1, B3, B23). Two things followed,
//! and both were invisible in the output:
//!
//! * A call bound by an author-written import and a call bound by alphabetical order across the
//!   whole repository produced **byte-identical edges**, because `Edge.reason` was a constant
//!   string. No field on any type could hold the difference, so `explain()` printed prose it
//!   could not support (audit B10, B11).
//! * Every relation that resolved to nothing hit `let Some(id) = … else { continue }` and left
//!   no row, no counter and no log (audit B9). The gap between "Peek believes this" and "Peek
//!   has no idea" was the size of the third-party dependency tree, reported as zero.
//!
//! This module is the structural answer to both. Every rule that fires is named, every decision
//! records the evidence class that justified it, and every relation the resolver cannot decide is
//! written back as an `Unresolved` or an `Ambiguous` with its candidate list intact.
//!
//! # When resolution runs: a second pass, always
//!
//! **Decision: resolution is a follow-up batch, never an inline step of extraction, and it is
//! scoped to what changed.** `indexer::build_full` commits the extractor's output and then calls
//! [`resolve_all`]; `indexer::refresh` commits the changed files and then calls [`resolve_paths`]
//! over exactly those paths plus the edges that point into them. [`ResolutionReport`] is carried
//! on [`crate::indexer::IndexReport`] and named in its summary, so the choice is observable from
//! `peek status` and not only from this file.
//!
//! Four reasons, in the order they mattered:
//!
//! 1. **A decision needs the committed state, not the in-memory one.** Resolution is a function
//!    of the index as a whole: two files declaring `charge` make each other's edges ambiguous.
//!    Running it over the extractor's output before the commit would let it see a half-written
//!    repository and answer questions about it.
//! 2. **The two stages fail for different reasons and must be separately observable.** An
//!    extractor that mis-parses and a resolver that over-reaches are different bugs with
//!    different fixes. Two commits and two reports are what make them separable, and the
//!    generation advances twice so a reader can see that a resolution pass happened.
//! 3. **`refresh` over the changed paths is not sufficient on its own.** Moving `charge` from
//!    `a.rs` to `b.rs` changes every edge that pointed at it while leaving the sources of those
//!    edges untouched. [`resolve_paths`] therefore re-reads the *incoming* edges of every entity
//!    in the changed paths and decides them again. That is contract G9, and it is why the second
//!    pass is scoped by target as well as by source.
//! 4. **A crash between the two commits is honest rather than corrupt.** The index is a complete,
//!    readable generation whose relations are honestly `Pending` — countable, queryable through
//!    `relation_by_state`, and re-decidable by a later pass. Nothing claims to be resolved that
//!    is not.
//!
//! The cost is one extra commit per build and one extra pass of reads. Folding resolution into
//! the extractor's loop saves that and gives up all four properties.
//!
//! # The evidence order
//!
//! `Evidence::strength()` in `model::relation` is the total order and this module is written
//! against it. It is repeated here because `peek explain` prints it, and a reader of that output
//! has to be able to check it against something.
//!
//! | Strength | Class | What it means | Rung |
//! |---:|---|---|---|
//! | 100 | `containment` | grammatical nesting; the enclosing node *is* the target | not resolved here — structural relations are already `Resolved` at extraction |
//! | 90 | `import_binding` | the author wrote an import in this file naming this symbol | **R1** |
//! | 85 | `receiver_type` | a receiver expression whose owner is identified in this file | **R2** |
//! | 80 | `path_match` | the target was named by an explicit path | not used by this module yet |
//! | 70 | `qualified_name_in_scope` | a multi-segment path naming a module this file can locate | **R3** |
//! | 50 | `same_file` | the target is declared in the same file as the reference | **R4** |
//! | 40 | `unique_name` | exactly one entity in the repository carries the name | **R5** |
//! | 10 | `name_only` | a bare name with nothing else known about it | evidence the extractor records, never an answer |
//! | — | `local_binding` | the name is bound by a binder the index holds no entity for | **not a rung**: a relation
//!   carrying it ends `unresolved` at the head of [`Resolver::decide`], before R1 — see
//!   [`rung_name`] and [`refused_for_local_binding`]
//!
//! The order *is* the rung order. Rungs are tried strongest first and the **first rung that
//! returns any candidate decides**; a rung that returns none falls through to the next. A rung
//! that returns *several* does not fall through — several equally supported candidates are an
//! `Ambiguous`, which is a result and never a pick (D-0004). `symbols.first()` does not appear
//! anywhere in this file.
//!
//! **One relation does not walk that order, and it is the receiver.** A relation carrying a
//! receiver skips R1 and goes to R2, because a receiver and an import of the same name are two
//! claims about one symbol and the receiver is the more specific. R2 then either decides or
//! **declines**, and a decline hands the relation back to R1 — see
//! [`Resolver::via_imported_receiver`]. Every other relation walks the ladder straight through, so
//! "the first rung that answers wins" holds, and the one place it does not is written down rather
//! than left to be found.
//!
//! # A re-decision walks the same ladder as the first decision
//!
//! [`resolve_paths`] does not only decide what is pending: with `reconsider_decided` on, which is
//! the default, it also re-decides every edge *pointing into* a changed file, including edges that
//! already hold an answer. That is contract G9 — a definition that moved must drag its callers with
//! it — and it means the ladder runs a second time over a relation whose `ResolutionState` is no
//! longer `Pending`.
//!
//! So the receiver and the scope are read out of the **evidence class**, in every state that has
//! one, rather than out of a `Pending` pattern. Read from `Pending` alone they would be `None` on
//! the second pass, the relation would walk a shorter ladder than the one that placed it, and a
//! `receiver_type` or `qualified_name_in_scope` proof would quietly become a `same_file` or
//! `unique_name` claim. [`receiver_evidence`] and [`scope_evidence`] are the two places that
//! happens, and they are the reason [`import_evidence`] was written the way it was.
//!
//! One relation cannot be read back that way, and the exception is stated rather than papered over.
//! A receiver resolved *through an import* records `import_binding`, and that class has room for
//! the module and not for the name the receiver was written under — so the receiver is gone from
//! the state. [`placed_import`] and [`Resolver::local_names_of`] put it back by looking the binding
//! up from the module instead, which is the same binding the first pass used.
//!
//! `Ambiguous` and `Unresolved` carry no evidence, so a relation that ended in either is re-decided
//! from its bare name. That is a limit of the state and it is stated here rather than papered over.
//!
//! ## The one limit the resolver chose, and what it cost
//!
//! **A refusal for `local_binding` is the case where that limit has teeth**, and it is written down
//! here rather than discovered on a later pass. [`ResolutionState::Unresolved`] carries no evidence,
//! so the moment the head rule fires the class that justified it is gone from the stored row. A
//! second pass handed the row would find nothing in it and walk the ladder, and R5 would place `out`
//! on `report.rs`'s `render.out` parameter — the confidently-wrong edge the refusal exists to
//! remove, put back by the act of removing it.
//!
//! How often that happens is a separate question and it is measured rather than assumed. **A scoped
//! pass does not re-decide a refused row at all**: [`resolve_paths`] reaches a row either through the
//! outgoing edges of a path it was given or through [`Store::incoming`], which matches on
//! `target_path`, and a refusal has no target. So the stored reason is a guard on what a caller may
//! hand [`resolve_paths`] rather than a repair for a path the engine walks on its own, and the two
//! routes that do re-decide these rows both arrive carrying the class — a re-extracted file re-emits
//! the relation as `Pending` with `local_binding` on it. [`refused_for_local_binding`] states which
//! is which.
//!
//! **The alternative was to give [`ResolutionState::Unresolved`] a payload, and it was rejected for
//! the wire format rather than for the model.** [`UnresolvedReason`] is a unit variant for every
//! variant on purpose: the wire form is a bare string in a field three surfaces read, and every
//! index already on disk holds the older wrapped shape — so a refusal carrying its own evidence
//! would be a second shape for one field, bought with a binder node type on the 41 relations of 192
//! the gate measures. The cost of the cheaper answer is stated as a cost:
//!
//! * **`peek explain` keeps the reason and loses the binder.** The row reads
//!   `unresolved (local_binding)`: the class survives, the `binder` that wrote it does not, and a
//!   reader who wants *which* `let` re-extracts the file. It is not a silent loss — the reason is
//!   its own variant, counted in [`ResolutionReport::unresolved_by_reason`] and printed by
//!   [`ResolutionReport::summary`], where `no_candidate` would have hidden the difference between
//!   "this name is not in the repository" and "this occurrence does not mean an entity".
//!
//! # What each rung does, and what it refuses to do
//!
//! ## R1 — import binding
//!
//! The only rung stronger than a receiver, and for one reason: the author wrote it down. It fires
//! when the relation's `target_name` matches a local name bound by an `Imports` relation **in the
//! same file**, which is what lets a bare `charge()` in a file containing `use payments::charge;`
//! resolve to another file's definition.
//!
//! It is skipped up front when the relation carries `ReceiverType` evidence. A receiver and an
//! import of the same name are two claims about one symbol; when they disagree the receiver is the
//! more specific one, and letting the import win would re-create audit B3 under a new name.
//!
//! It also declines when the occurrence's own lexical scope declares the name, which is the first
//! clause of the scope rule — see [A candidate has to be in scope where the use is
//! written](#a-candidate-has-to-be-in-scope-where-the-use-is-written). The rung's evidence is a
//! fact about a *name* in a file; a declaration in front of the use is a fact about *this
//! occurrence*, and the second is the more specific of the two.
//!
//! It is asked a second time, and only for a receiver, when R2 declines. The receiver's *name* is
//! then the local name an import binds, and R1 is the rung that knows which file it came from.
//! [`Resolver::via_imported_receiver`] is the whole of that hand-off.
//!
//! Turning a module path into a file is [`Resolver::module_files`], and it asks **both** routes —
//! the module table and the path guess — because a controlled comparison across five repositories
//! found the table worth +2.5pp of resolved relations on one and -1.1pp on another. The question it
//! now answers first is *which spelling of the path* names a file, and the guess is what answers
//! when no spelling does; the guess is also the only route that can express `super::`, a climb
//! against the importer's own module. [`module_files`] states the rule and the counters that hold it
//! up; the limits of the guess are documented at [`files_for_module`] because they are real.
//!
//! ## R2 — receiver
//!
//! **A receiver is evidence, not a target.** `self.foo()` and `payments::Service::charge()` are
//! different facts and must not land on the same node.
//!
//! R2 finds the *owner* — the type the receiver names — and then accepts only entities declared
//! inside that owner. `self`, `this` and `Self` take their owner from the source's own qualified
//! name, which is exact. A named receiver is matched against the entities declared in the
//! referring file, because that is where a type the receiver names is declared in every language
//! this engine supports.
//!
//! **R2 declines when it has no owner; it does not refuse.** A type that reached the caller's file
//! through an import is not declared in it, so the rung has no owner to look inside and says so.
//! Saying so is not the same as saying there is none, and the difference was the whole of D-0036:
//! a decline hands the relation to R1, which is the rung that knows where the name went. Returning
//! `no_candidate` there was a claim, and it was false.
//!
//! **R2 refuses when it has an owner and finds nothing inside it.** That is the case a single
//! answer was right about, and it is the one that must not fall through: `service.retry()` where
//! `Service` is declared in this file and carries no `retry` must not bind to whatever free
//! function named `retry` sorts first in the repository, which is the single worst behaviour the
//! engine Peek replaces had (audit B3). So the refusal stands, and it stands for the whole ladder:
//! a receiver never reaches R3, R4 or R5.
//!
//! **What "no owner" excludes, and why.** An owner established *only* by a namespace row is not an
//! owner. `impl Gateway { .. }` is indexed as a `Module` named `Gateway` beside the type it
//! belongs to, because the walker's scope stack needs a row to hang the block's methods from — so
//! a file that writes `use alpha::gateway::Gateway;` and also carries an `impl Gateway` holds a
//! `Gateway` that declares nothing. The rule [`prefer_symbols`] applies to a name lookup applies to
//! an owner too: a namespace is outranked by a symbol, and if only a namespace carries the name
//! then the name is not an owner *here*, which means the type reached the file by import.
//!
//! **The gap that leaves, stated rather than hidden.** Rust lets an inherent `impl` block for a
//! type live in a file that does not declare the type, and this module reads one file at a time,
//! so a method reached through such a block is unplaceable from one direction and not the other.
//! `Gateway.send()` written in a file whose only `Gateway` is that block's scope row declines and
//! reaches R1, and resolves. `self.send()` *inside* the block still refuses, because there the
//! owner is a declaration in this file and R2 is right that this declaration carries no `send`.
//! Closing it means recording an `impl` block's target type as something the index can look up
//! across files, which is an extractor change rather than a resolver one.
//!
//! ## R3 — qualified name in scope
//!
//! A multi-segment path such as `crate::payments::Service::charge` names a module this file can
//! locate, and R3 searches that module's files for the name. A single-segment path never reaches
//! here: the extractor already reports `S::new()` as ambiguous, because `S` may be a type or a
//! module and nothing in the syntax distinguishes them.
//!
//! ## R4 — same file
//!
//! The target is declared in the file the reference appears in. For the reference kinds the
//! extractor emits this is the strong claim it sounds like, and it is the rung that makes a
//! single-file call resolve.
//!
//! **Same file is not the same scope**, and this is where the second clause of the scope rule is
//! spent — see [A candidate has to be in scope where the use is
//! written](#a-candidate-has-to-be-in-scope-where-the-use-is-written).
//!
//! ## R5 — unique name
//!
//! Exactly one entity in the repository carries the name. That is a fact about the index, not an
//! intent, so it produces [`ResolutionState::Inferred`] and never [`ResolutionState::Resolved`].
//! Two or more matches is `Ambiguous` — the case the engine Peek replaces resolved by taking the
//! alphabetically first file in the repository.
//!
//! **The uniqueness is over the candidates a use written here can see.** A binding of an unrelated
//! declaration is not one of them, so the sentence this rung stores is narrower than it looks and the
//! `basis` says so. See [A candidate has to be in scope where the use is
//! written](#a-candidate-has-to-be-in-scope-where-the-use-is-written).
//!
//! R5 is the only rung that reaches outside the source file. It exists because the alternative is
//! a large class of honest `Unresolved`, and it is safe precisely because it can only fire when
//! the name is genuinely unique. It never picks between candidates.
//!
//! # A namespace is never what a bare name means
//!
//! Every rung that looks a name up applies one rule on top of its own evidence: **a module is
//! outranked by a symbol.** See [`prefer_symbols`].
//!
//! The rule exists because the index contains entities that carry a name without declaring
//! anything a bare name can denote, and the sharpest of them is a Rust `impl` block. The walker
//! needs a row for one — its scope stack anchors every method to `scope.last().id`, and a relation
//! whose source row is absent is a dangling edge the foreign key rejects — so `impl Gateway { .. }`
//! is indexed as an `EntityKind::Module` named `Gateway`, next to `struct Gateway` in the file.
//! `use alpha::gateway::Gateway;` then had two equally supported candidates and could not name the
//! struct (R-012). A module table makes the same shape ordinary for every file stem, since every
//! file is a module too.
//!
//! It is a **ranking, not an exclusion**, and the difference is the whole design. `use
//! crate::payments;` names a module and nothing else, so when every candidate is a namespace the
//! namespaces are kept and the import still resolves. Excluding modules outright would trade a
//! false ambiguity for a large class of `no_candidate`.
//!
//! What it does buy is the other half of audit B21, which the file entity made a one-sided rule:
//! a name shared by a namespace and a symbol means the symbol, because a call cannot target a
//! namespace and an expression is not one.
//!
//! # A candidate has to be in scope where the use is written
//!
//! The second rule every name lookup applies, and the one this section exists for. **Not in
//! scope is not "lower priority": it is not a candidate.** A bare name is read against the
//! declarations *enclosing* the occurrence — [`Resolver::declared_around`] walks the `Contains`
//! chain up from the relation's source and asks each link what it declares — and a candidate
//! outside that set is dropped rather than ranked.
//!
//! It has two clauses, and they are one rule because they answer one question.
//!
//! * **A declaration in front of the use beats one the use cannot see.** R1 declines when the
//!   occurrence's own scope declares the name: `use crate::model::{entry, Entry}` beside
//!   `fn format_line_inner(entry: &Entry)` is two declarations of one name, and inside that
//!   function the parameter is the one the source means. This is the ordinary meaning of a
//!   shadowed name and it needs no new rung to say so.
//! * **A binding of an unrelated declaration is not a candidate at all.** A parameter belongs to
//!   the function that declares it ([`is_binding`]). `report.rs` declares a parameter
//!   `format_line.count`, and a use of `count` written in `render` or in `describe` cannot mean
//!   it — which is the sense in which **same file is not the same scope**.
//! * **A call invoked through a binding has no static target** ([`Resolver::invocable_around`]).
//!   A reference *refers to* a binding and a call *invokes* one: `body()` where `body` is a
//!   parameter of type `impl Fn()` invokes the value the parameter holds, and the same name in a
//!   reference position is the parameter itself. So this is the one clause that reads the
//!   relation's class, and it was not in the first version — two call edges answered with their
//!   own parameter are what put it there.
//!
//! ## The naive version of this is wrong, and it was measured before it was written
//!
//! "The source's own scope wins" is one rule, and it is not this one. It is silent on the whole
//! second class: `render`'s own scope declares `entries` and `out` and **not** `count`, so the
//! rule has nothing to say about the `count` read written inside it — and what is worse, the wrong
//! target it would leave in place (`format_line.count`) *is* in the source's own file, so the rule
//! read backwards endorses it rather than refusing it.
//!
//! The two clauses were priced separately, over **every relation the index holds** rather than over
//! the wrong edges, and that is what settled it:
//!
//! | clause | repairs | damage |
//! |---|---:|---:|
//! | own scope wins | 3 edges over 2 labelled sites | **0** |
//! | a binding of an unrelated declaration is not a candidate | 2 edges over 2 labelled sites | **0** |
//!
//! **Damage zero is the admissibility criterion**, and it is arithmetic rather than taste: a clause
//! is only safe to adopt while the edges it would un-place are edges the fixture says are right. Both
//! hold, and that is why the rule is written as two clauses over one question rather than as one
//! clause that would have to be split later.
//!
//! Sixteen labelled relations are covered by the first clause and thirty-one by the second, and
//! **fifteen are covered by the second and not the first** — including all four of the surviving
//! wrong edges and seven more of the identical shape. That difference is the measurement: if the two
//! clause populations were the same set, "the source's own scope wins" would have been the whole
//! answer and the second clause would be redundant. It is not, and the test that says so is
//! `gate::the_two_scope_clauses_are_not_one_rule`, over the whole labelled population and not over
//! the four rows that motivated the question.
//!
//! **What the rule must not become.** Preferring the source's own scope *blindly* would replace
//! `model.rs`'s answer to `label` and `count` — where the field shorthand reads the **parameter** —
//! with the field of the same name, and `format_line`'s answer to `count` with `Entry.count`. Both
//! are decided-and-right today. That is why the first clause says "over a candidate the use cannot
//! see" rather than "over any candidate", and the second says "not a candidate" rather than "a
//! weaker candidate": the difference between the two clauses and a third rule that breaks six edges
//! is entirely in what each one refuses.
//!
//! ## What it costs, and what it cannot do
//!
//! A field is reached through a receiver and not by a bare name, and the extractor records no
//! receiver on a `References` edge, so **no rung here can tell `entry.count` from `count`.** The
//! field stays a candidate everywhere for that reason, and it is the reason a name that only a
//! receiver could disambiguate may still answer with a field of the wrong type. Closing that needs a
//! receiver on a reference edge, which is an extractor change.
//!
//! The scope walk is a handful of indexed seeks per distinct relation source and is cached per
//! source for the pass, on the same assumption as every other cache in this module: the store does
//! not change under a pass.
//!
//! # `Resolved` and `Inferred` are different claims
//!
//! A target bound by an import binding, by an identified receiver, by a located module, or by a
//! definition in the same file is [`ResolutionState::Resolved`]: the target is proven. A target
//! bound by a repository-wide uniqueness check, or by a receiver matched while ignoring letter
//! case, is [`ResolutionState::Inferred`]: the target is a claim, and the relation's `basis` says
//! which claim, in words a consumer can audit.
//!
//! This is also the answer to "where did the case-insensitive fallback go". There is none, and
//! the reason is the interesting part: a case-insensitive lookup needs an index the store does not
//! have, and the only way to get one without adding a query path is to scan every entity in the
//! repository — the flat global name table this module exists to replace. So every name
//! comparison in this file is case-**sensitive**, with one narrow and visible exception: R2 may
//! match a receiver against an in-file type ignoring case, and when it does it says so in the
//! `basis` and records `Inferred` rather than `Resolved`. An unproven match is never reported as
//! a proof.
//!
//! # One transaction per pass
//!
//! A pass builds one [`IndexUpdate`] and hands it to [`Store::apply_update`] exactly once. A
//! failure at any point rolls the batch back and the previous generation stays readable with its
//! previous decisions intact. A pass that decides nothing does not commit at all, so
//! re-resolving an unchanged index costs a read and changes nothing.
//!
//! # Limits are reported, never silent — and a file is not one of them
//!
//! Every candidate lookup through the store is bounded, because an unbounded one is how the
//! predecessor's `resolve_target_id` became the first line of all ten of its queries. When a limit
//! truncates a candidate set the pass records it in [`ResolutionReport::truncated`], because a limit
//! that is not visible is a limit that quietly changes the answer.
//!
//! **A file's entity list is not one of the bounded lookups, and that is a decision rather than an
//! omission.** It used to be. The enumeration was `entities_in_file(path, entities_per_file)` with a
//! default of 512, and both doors into a file — its own outgoing edges, and the edges arriving at the
//! entities it declares — opened through that one list. So a file with more than 512 entities had
//! its tail outside the pass entirely: the relations of those entities were extracted, never placed,
//! and never refused. `Pending` is the one state that answers no question in either direction, so
//! nothing in the output said so.
//!
//! The measurement is specific enough to check. On `BurntSushi/ripgrep`, `crates/core/flags/defs.rs`
//! holds 1,363 entities; a refresh of it decided 2,121 of its 3,599 relations and left **348**
//! pending. `2,121 examined + 348 undecided = 2,469`, which is the whole of what that pass was
//! responsible for — the 3,599 also counts the containment edges the walker settles at extraction
//! and no pass examines ([`is_resolvable`]). So the missing work was exactly the part outside the
//! pass, not a class of relation the ladder declined. The lowest-ranked pending source was **#514**,
//! one row past the bound, which is what discriminates the two explanations: a rung that declined a
//! class of relation would not line up with the position of the bound. A second scoped refresh of
//! that file changed nothing (348 → 348), because the bound cut the same tail off again; only
//! [`resolve_all`] clears them, `build_full` is its only caller, and `watch` calls `refresh` — so
//! the leak accumulated one large file at a time on exactly the operation that runs on every
//! keystroke.
//!
//! The pass now **pages** that read: `all_entities_in_file` asks for one page and asks again until
//! a page comes back short. So `entities_page` is a page size and nothing else — the size of one
//! read, not the size of the answer. A caller passing [`ResolutionOptions::default`] now gets the
//! whole of every file, and a *complete* pass reports `truncated == 0` however large the file was.
//! That is a narrowing of what `truncated` counts, and it is the honest direction: the counter now
//! describes the lookups that can make an answer **narrower** (a name's candidate set, an entity's
//! edges, a candidate list) and not the one that used to make an answer **absent**.
//!
//! ## What is left bounded, and what a caller who hits a bound gets
//!
//! Four lookups still cut, and each is a *search* rather than an enumeration: there is no next page
//! of a candidate set. A name lookup at `entities_by_name`, an outgoing-edge read at
//! `outgoing_per_source`, an incoming-edge read at `incoming_per_entity`, and a stored `Ambiguous`
//! list at `max_candidates`. A cut-short read is counted.
//!
//! Three of the four get a smaller answer and the count. One gets a **refusal**, and the difference
//! is the point. Cutting a candidate *list* cannot make the answer wrong — every candidate found is
//! still a candidate, so "at least these two" survives a dropped third — and cutting the *edges* of
//! an entity leaves relations out of scope rather than mis-deciding the ones inside it. Cutting a
//! **uniqueness check** is different: R5's answer is the sentence "exactly one entity named X is
//! indexed", and one candidate found behind a bound that stopped there is a claim about a window,
//! written into the field whose whole purpose is to be auditable. So `via_unique_name`
//! declines on a cut-short read and the relation comes out `Unresolved` — visibly unestablished
//! rather than confidently wrong, with the count saying why. That is the same rule R2 applies when
//! it names an owner and finds nothing inside it, and the narrower reading of "a smaller answer is
//! not a coherent answer": it holds where the partial view turns an answer into a **false claim**,
//! and not where it only makes an answer less complete.
//!
//! # What a scoped pass cannot see
//!
//! [`resolve_paths`] re-reads the outgoing edges of the files it was given and the incoming
//! edges of the entities they declare. That covers the two cases a refresh creates: a new or
//! changed edge out of a changed file, and an edge that used to point into a changed file.
//!
//! It does **not** cover a third: an `Ambiguous` or `Unresolved` edge elsewhere in the index
//! whose *candidate set* just changed, because a file appeared or an unrelated one was deleted.
//! Such an edge has no `target_path`, so `Store::incoming` cannot match it, and there is no
//! index from a target *name* back to the relations that name it. Closing that would need a new
//! index — a schema change — or a full re-resolve on every edit. Both are worse than the gap, so
//! the gap is documented here and pinned by a test rather than papered over. A full rebuild does
//! correct it: the extractor re-emits the edge as `Pending` and the pass decides it again.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::model::entity::{EntityId, EntityKind};
use crate::model::path::RepoPath;
use crate::model::relation::{
    Evidence, Relation, RelationKey, RelationKind, ResolutionState, UnresolvedReason,
};
use crate::store::{IndexUpdate, Store, StoreError};

#[cfg(test)]
mod tests;

/// The local name a glob import carries. `use a::b::*;` binds no single name, so no rule can
/// ever prove a target for it.
const GLOB: &str = "*";

/// How many ancestor directories a module path is anchored at.
///
/// The anchor list is the referring file's own directory and then each directory above it, up to
/// the repository root. A repository deeper than this needs a module table, not a longer scan;
/// the limit is what keeps the anchor list bounded.
const MAX_ANCHOR_DEPTH: usize = 6;

/// The longest module path a rule will act on. A path longer than this is not a module
/// reference; treating one as such would multiply the file seeks without bound.
const MAX_MODULE_SEGMENTS: usize = 8;

/// Every bounded lookup the resolver makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolutionOptions {
    /// Entities read from one file **per read**, before the next page is asked for.
    ///
    /// **This is a page size, not a cut-off.** A file with more entities than this is read to the
    /// end, one page at a time; nothing about a large file is left outside the pass. It was a
    /// cut-off until paging, and the difference was the whole of a real defect: on
    /// `BurntSushi/ripgrep` a refresh of `crates/core/flags/defs.rs` (1,363 entities) decided
    /// 2,121 of the file's 3,599 relations and left **348** `Pending` — extracted, never decided,
    /// never refused. The lowest-ranked pending source was #514, exactly one row past the old
    /// bound. Nothing later revisited them: a second scoped refresh of that file changed nothing
    /// (348 → 348), because only [`resolve_all`] clears them and `build_full` is its only caller,
    /// so on a watched repository the leak accumulated one large file at a time.
    ///
    /// What the bound still buys is the size of one read: memory held per lookup, and rows walked
    /// per query. Since the pass reads each file once ([`Resolver::entities_in_file`]) that is a
    /// smaller effect than it was when the read was per relation — a caller who sets it very small
    /// pays more seeks for the same answer, and a caller who sets it very large pays more memory
    /// per query, and neither changes what the pass decides. A value of zero is treated as one
    /// rather than as "read nothing", because a page size of zero would report every file in the
    /// repository as empty, which is a bound that destroys the answer instead of narrowing it.
    pub entities_page: usize,
    /// Entities read for one name before the read is abandoned.
    ///
    /// Unlike [`Self::entities_page`] this one **does** cut the search short, because a name lookup
    /// is a candidate set rather than an enumeration: there is no "next page" of a name, only
    /// further candidates that the rung would have to weigh. A cut-short name read is recorded in
    /// [`ResolutionReport::truncated`], and R5 refuses to answer from one at all — see
    /// `Resolver::via_unique_name`.
    pub entities_by_name: usize,
    /// Relations read from one source before the read is abandoned.
    pub outgoing_per_source: usize,
    /// Relations read into one target before the read is abandoned.
    pub incoming_per_entity: usize,
    /// Modules read for one qualified-name lookup before the read is abandoned.
    ///
    /// Separate from `entities_by_name` because it is a different index with a different
    /// selectivity: a qualified name names one module, so the answer is normally zero or one, and
    /// a limit sized for a name lookup would be two orders of magnitude too generous here.
    pub modules_per_lookup: usize,
    /// Candidates written into one `Ambiguous` before the rest are dropped.
    pub max_candidates: usize,
    /// Re-decide relations that were already decided because their target is in scope.
    ///
    /// This is the "a definition moved" case, and it is on by default. Without it a move that
    /// makes a previously unique name ambiguous would leave a confidently-wrong `Inferred` edge
    /// in place indefinitely. Honoured by [`resolve_paths`] only; [`resolve_all`] ignores it,
    /// because a full build has re-emitted every relation as `Pending` already.
    pub reconsider_decided: bool,
    /// Ask the module table before guessing at file paths.
    ///
    /// **This is a measurement switch, not a user setting.** It exists because the module table
    /// was added and the only honest way to report what it is worth is a controlled comparison,
    /// which needs both arms from one build — and a comparison across two builds is exactly the
    /// mistake that produced R-010, where a harness defect was read as a trend. It reads one
    /// environment variable and nothing else, and the default is `true` on every path, so a
    /// deployment that sets nothing behaves identically to yesterday.
    ///
    /// The fallback is kept in either position: with the table off, `files_for_module` alone
    /// produces the pre-existing behaviour, which is what makes this a real A/B rather than a
    /// no-op.
    pub use_module_table: bool,
}

/// The environment variable that turns the module table off. Diagnostic only.
const ENV_MODULE_TABLE: &str = "PEEK_MODULE_TABLE";

impl Default for ResolutionOptions {
    fn default() -> Self {
        // Read once, and only treat an explicit falsy value as off. An unset variable, an empty
        // one, and anything unrecognised all leave the table on: a diagnostic switch that has to
        // be spelled exactly right to *disable* something is a switch that will be on when the
        // measurement needs it off, and the measurement will be wrong in a way nobody notices.
        let use_module_table = std::env::var(ENV_MODULE_TABLE)
            .map(|value| value != "0" && !value.eq_ignore_ascii_case("false"))
            .unwrap_or(true);
        Self {
            entities_page: 512,
            entities_by_name: 512,
            outgoing_per_source: 512,
            incoming_per_entity: 512,
            modules_per_lookup: 8,
            max_candidates: 32,
            reconsider_decided: true,
            use_module_table,
        }
    }
}

impl ResolutionOptions {
    /// Set the page size for a per-file entity read. Chaining, so a test does not have to name the
    /// field.
    ///
    /// Named for what it now is. The bound it replaces was a cut-off, and the name it had —
    /// `entities_per_file`, "how many entities a file has" — described that cut-off rather than
    /// anything a caller would want to set, which is part of why the defect behind it survived: a
    /// caller who noticed the truncation had no way to know which field to widen, and the field
    /// whose name matched the symptom was the wrong one.
    #[must_use]
    pub fn with_entities_page(mut self, page: usize) -> Self {
        self.entities_page = page;
        self
    }

    /// Set the per-name entity read limit.
    #[must_use]
    pub fn with_entities_by_name(mut self, limit: usize) -> Self {
        self.entities_by_name = limit;
        self
    }

    /// Set the candidate-list cap.
    #[must_use]
    pub fn with_max_candidates(mut self, limit: usize) -> Self {
        self.max_candidates = limit;
        self
    }

    /// Turn the "a definition moved" re-decision on or off.
    #[must_use]
    pub fn with_reconsider_decided(mut self, reconsider: bool) -> Self {
        self.reconsider_decided = reconsider;
        self
    }
}

/// The name of the rule a decision was made by, as it is stored.
///
/// A `Resolved` relation carries its rule in this class; an `Inferred` relation carries the class
/// *and* spells the claim out in its `basis`. `peek explain` reads this function to answer "why
/// do you think this calls that" without re-parsing the file, which is the capability Cortex's
/// `explain()` advertised and could not honestly deliver (audit B11).
#[must_use]
pub fn rule_name(evidence: &Evidence) -> &'static str {
    evidence.class()
}

/// The ladder rung a given evidence class belongs to.
///
/// Separate from [`rule_name`] because the rung and the evidence class are not the same
/// vocabulary: R2 records `receiver_type` but is the *receiver owner* rule. The mapping is total
/// and a test pins it, because a rung that quietly stopped firing would be a silent change in
/// what the engine believes.
///
/// **`local_binding` is the one class that names a rule rather than belonging to one.** It is a
/// claim that the relation has **no** target, so it cannot rank candidates and the rule it names
/// is a refusal at the head of [`Resolver::decide`] rather than a rung that answers. The name
/// this function returns is therefore the class string rather than a rung's, and a reader who
/// takes it for a rung will look for a place in the order where `local_binding` can fire and find
/// none — which is the correct answer. See [`refused_for_local_binding`].
#[must_use]
pub fn rung_name(evidence: &Evidence) -> &'static str {
    match evidence {
        Evidence::Containment => "containment",
        Evidence::ImportBinding { .. } => "import_binding",
        Evidence::ReceiverType { .. } => "receiver_owner",
        Evidence::PathMatch => "path_match",
        Evidence::QualifiedNameInScope { .. } => "scope_qualified_name",
        Evidence::SameFile => "same_file",
        Evidence::UniqueName => "unique_name",
        Evidence::NameOnly => "name_only",
        Evidence::LocalBinding { .. } => "local_binding",
    }
}

/// The package a file belongs to, when the index says so.
///
/// `Unnamed` is a package whose module row carries no prefix at all, which is the package root of
/// a repository with no directory above its source root. It is **not** the same as [`None`], which
/// means the file has no module row: an unprefixed package can still prefix a qualified name — with
/// nothing — and a file with no row cannot be asked about at all. Collapsing the two would make
/// "this index records no packages" and "this package is called nothing" the same fact, and they
/// answer differently.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Package {
    /// The package name, with the `::` a qualified name needs already appended.
    Prefixed(String),
    /// A package the index records without a prefix.
    Unnamed,
}

/// Whether a module path says "in this crate" rather than naming a package.
///
/// **The rule, and it is read off the path and the index rather than off the language.** A path
/// whose first segment is `crate` or `self`, or is the importing file's own package, names something
/// inside that crate: the author wrote the word for it. Anything else names a package, and `src` is
/// not a segment of a module path, so only the table can say which file that package means.
///
/// `super` is a climb and is left out on purpose: it is a path *out* of the current module, and
/// [`Resolver::module_qualified_names`] declines to spell it at all, so it is the guess's shape and
/// the guess's shape is what answers.
///
/// The erring direction is deliberate. A bare `use foo::Bar` in a crate that also has a module
/// `foo` — Rust 2015 spelling, where such a path is crate-relative — is classified as naming a
/// package, and the table answers it. That is still the right answer, because the table tries the
/// importer's own package prefix before the bare spelling; it just costs four seeks instead of one
/// probe. A classification that guessed the other way would move the edge.
fn is_crate_relative(module: &str, package: Option<&str>) -> bool {
    match module.split("::").next() {
        None => false,
        Some("") | Some("crate") | Some("self") => true,
        Some(head) => package == Some(head),
    }
}

/// Which route answered one module lookup.
///
/// Three cases and no fourth, because [`Resolver::module_files`] asks both routes and returns one
/// of them; a fourth would be a case the rule does not have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModuleFileOutcome {
    /// The module table named a file for one of its spellings.
    Table,
    /// The path guess named a file.
    Guess,
    /// Neither route named a file, so the caller has nothing to search.
    Neither,
}

/// What one module lookup did.
///
/// The inputs [`Resolver::module_files`] has, kept as a record rather than as arguments to six
/// counters, so that the classification is written once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ModuleFileLookup {
    /// How many qualified-name seeks the table was asked for.
    asked: usize,
    /// Which route's file list was returned.
    outcome: ModuleFileOutcome,
    /// Whether the importer's package could be named at all, which is what decides whether the
    /// table had a package-prefixed spelling to offer.
    package_known: bool,
    /// Whether the path the table answered was one that named a package, rather than one the author
    /// wrote as `crate::`/`self::` or with the importer's own package as its head. That is the
    /// route's question, and it is what the conflict rule turned on — not which spelling happened
    /// to match, which is an implementation detail of the order.
    named: bool,
}

/// How the module table and the path guess compared, counted over one pass.
///
/// **This is here because the recorded measurement said what the table was worth and not which of
/// its two answers was the better one.** The three that carry a decision:
///
/// * `table_answered` against `guess_answered` is which route placed the files.
/// * `table_answered_named` is how often the table was asked a *package* path — the shape only it
///   can answer, and the whole of its recorded value on `rust-lang/cargo`.
/// * `package_unknown` is how often the importer's package could not be named, so the table had no
///   package-prefixed spelling to offer at all. **Before the fix, on `BurntSushi/ripgrep`, that was
///   every lookup and 43 of the repository's 110 files; and 349 of the 351 intra-crate proofs the
///   policy cost that repository were `crate::` paths.** A reader who thinks the fix is cosmetic
///   reads that number.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ModuleFileReport {
    /// Module lookups, of every outcome.
    pub lookups: u64,
    /// Qualified-name seeks the table was asked for, which is one indexed lookup each.
    pub asked: u64,
    /// Lookups the module table answered.
    pub table_answered: u64,
    /// Of [`Self::table_answered`], the ones where the path named a package: a cross-package path,
    /// which is the only shape the table can answer and the guess cannot.
    pub table_answered_named: u64,
    /// Of [`Self::table_answered`], the ones where the path did not name a package, so the table
    /// answered only because the guess found nothing.
    pub table_answered_crate_relative: u64,
    /// Lookups where the importer's package could not be named, so no prefixed spelling existed.
    pub package_unknown: u64,
    /// Lookups the path guess answered.
    pub guess_answered: u64,
    /// Lookups neither route answered, and the caller fell through the ladder.
    pub neither: u64,
}

impl ModuleFileReport {
    /// A one-line summary for [`ResolutionReport::summary`], empty when there is nothing to say.
    fn clause(&self) -> String {
        if self.lookups == 0 {
            return String::new();
        }
        format!(
            ", module files: {lookups} lookup(s) and {asked} seek(s), table {table} \
             ({named} naming a package, {relative} crate-relative), guess {guess}, \
             neither {neither}, {unknown} with no package",
            lookups = self.lookups,
            asked = self.asked,
            table = self.table_answered,
            named = self.table_answered_named,
            relative = self.table_answered_crate_relative,
            guess = self.guess_answered,
            neither = self.neither,
            unknown = self.package_unknown,
        )
    }

    /// Record one lookup.
    fn record(&mut self, lookup: ModuleFileLookup) {
        self.lookups += 1;
        self.asked += lookup.asked as u64;
        if !lookup.package_known {
            self.package_unknown += 1;
        }
        match lookup.outcome {
            ModuleFileOutcome::Table => {
                self.table_answered += 1;
                match lookup.named {
                    true => self.table_answered_named += 1,
                    false => self.table_answered_crate_relative += 1,
                }
            }
            ModuleFileOutcome::Guess => self.guess_answered += 1,
            ModuleFileOutcome::Neither => self.neither += 1,
        }
    }
}

/// What one resolution pass did, measured.
///
/// Every number here is counted. A number that cannot be counted is not reported, because a
/// plausible zero is how the predecessor's `explain()` came to describe an empty caller list as a
/// fact.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolutionReport {
    /// Relations the pass looked at.
    pub examined: u64,
    /// Proven: an import binding, an identified receiver, a located module, or a definition in
    /// the same file.
    pub resolved: u64,
    /// A claim rather than a proof, with the claim written into the relation's `basis`.
    pub inferred: u64,
    /// Two or more equally supported candidates. Every one of them was written down.
    pub ambiguous: u64,
    /// No target could be established.
    pub unresolved: u64,
    /// Unresolved relations by reason, so "why" is a number and not a guess.
    pub unresolved_by_reason: BTreeMap<String, u64>,
    /// Relations that were already decided and were decided again, because the entity they
    /// pointed at is inside the pass's scope.
    pub reconsidered: u64,
    /// Edges the caller read *before* its write, whose targets that write removed.
    ///
    /// Reported separately from `reconsidered` because they are a different population: these
    /// arrived from outside the pass's paths and had already been demoted to a null target, so
    /// nothing but the caller's own capture could have found them. A caller repairing a rename
    /// needs this number without inferring it from `examined`.
    pub displaced: u64,
    /// Relation rows the pass actually rewrote.
    pub relations_written: u64,
    /// Lookups this pass abandoned at a bound, and what that count does and does not include.
    ///
    /// **It means "a bound cut a lookup short", and the lookups it covers are the candidate sets
    /// and the adjacency reads.** A name lookup that stopped at [`Self::entities_by_name`], an
    /// outgoing-edge read that stopped at [`Self::outgoing_per_source`], an incoming-edge read that
    /// stopped at [`Self::incoming_per_entity`], and a candidate list that stopped at
    /// [`Self::max_candidates`]. Each of those can remove options from an answer, so a non-zero count
    /// means an answer on this build may be less complete than the index could have supported.
    ///
    /// **It does not include a file's entity list, which is read to the end.** Until the pass
    /// paged that read, this counter was where the size of a file showed up, and a caller who saw
    /// it had been told that work had been skipped but not which work; the skipped work was the
    /// tail of every large file, and it was left `Pending` rather than decided. A complete pass now
    /// reports **zero** here however large the file was, and a non-zero value can only mean one of
    /// the four bounded candidate or adjacency lookups above. That is a narrowing of what the field
    /// counts, stated here rather than left for a caller to infer from a number that happens to
    /// have changed.
    ///
    /// Every one of the four is a *bounded search* rather than an enumeration, which is the
    /// distinction the whole field rests on. There is no "next page" of a candidate set, so the
    /// honest response to hitting the bound is to count it and, where the partial view would make
    /// the answer a claim rather than a finding, to refuse — see `Resolver::via_unique_name`.
    pub truncated: u64,
    /// Whether any `Pending` relation is still in the index after the pass.
    pub pending_remaining: bool,
    /// Whether the pass committed. A pass that decided nothing does not.
    pub committed: bool,
    /// The store generation after the pass.
    pub generation: u64,
    /// How the module table and the path guess compared over the pass.
    ///
    /// Reported because the two routes are asked about the same question on every import and only
    /// one of them answers, and a reader cannot otherwise tell which — nor how often the table had
    /// no package-prefixed spelling to offer, which is what the intra-crate regressions were.
    ///
    /// **It is a pass total and not a per-relation record.** A relation carries the rung that placed
    /// it and not the lookup that located the file: `Evidence::ImportBinding` has room for the
    /// module as written and no field for a file set, and `ResolutionState::Resolved` has no basis
    /// string to put it in. Recording it per relation means a field on `model::Evidence` and a new
    /// key in the stored `resolution_json` of every decided relation, which is a wider contract
    /// change than this one is and is reported rather than made here.
    pub module_files: ModuleFileReport,
}

impl ResolutionReport {
    /// The name of this pass, for a report that has to say which one ran.
    pub const PASS: &'static str = "resolution pass 2";

    /// Record one decision.
    fn record(&mut self, decision: &Decision) {
        match decision {
            Decision::Resolved { .. } => self.resolved += 1,
            Decision::Inferred { .. } => self.inferred += 1,
            Decision::Ambiguous { .. } => self.ambiguous += 1,
            Decision::Unresolved { reason } => {
                self.unresolved += 1;
                *self
                    .unresolved_by_reason
                    .entry(reason.as_str().to_owned())
                    .or_insert(0) += 1;
            }
        }
    }

    /// A one-line summary for `peek status` and the MCP `index_status` primitive.
    pub fn summary(&self) -> String {
        let mut reasons: Vec<String> = self
            .unresolved_by_reason
            .iter()
            .map(|(reason, count)| format!("{reason} {count}"))
            .collect();
        reasons.sort();
        let reason_text = match reasons.is_empty() {
            true => String::new(),
            false => format!(" ({})", reasons.join(", ")),
        };
        format!(
            "{}: examined {}, resolved {}, inferred {}, ambiguous {}, unresolved {}{}, \
             reconsidered {}, displaced {}, {} rows at generation {}{}{}{}",
            Self::PASS,
            self.examined,
            self.resolved,
            self.inferred,
            self.ambiguous,
            self.unresolved,
            reason_text,
            self.reconsidered,
            self.displaced,
            self.relations_written,
            self.generation,
            match self.committed {
                true => "",
                false => ", no commit",
            },
            match self.truncated {
                0 => String::new(),
                other => format!(", {other} lookup(s) truncated at a bound"),
            },
            self.module_files.clause(),
        )
    }
}

/// What the ladder concluded about one relation.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Decision {
    /// Proven. The target exists and the evidence justifies it.
    Resolved { target: EntityId, by: Evidence },
    /// A claim rather than a proof, with the claim written out.
    Inferred {
        target: EntityId,
        by: Evidence,
        basis: String,
    },
    /// More than one equally supported candidate, strongest evidence first. The first is *not* a
    /// recommendation.
    Ambiguous { candidates: Vec<EntityId> },
    /// No target could be established. Written back with this reason rather than dropped, because
    /// a relation that resolved to nothing has to stay countable.
    Unresolved { reason: UnresolvedReason },
}

/// What the receiver rung concluded about one relation.
///
/// The distinction between the two variants is the whole of D-0036's rule, and it is a distinction
/// about **what the rung looked at**, not about the repository. `Declined` says "I cannot answer
/// this"; it never says "there is no answer". Refusing is a claim, and the claim R2 used to make
/// about an imported type was false.
#[derive(Debug)]
enum Receiver {
    /// The rung placed the target, or refused with a reason it can justify from this file.
    Answered(Decision),
    /// The rung could not name the receiver's owner from the caller's own file.
    Declined,
}

/// A candidate target and the evidence that put it on the list.
#[derive(Debug, Clone)]
struct Found {
    id: EntityId,
    by: Evidence,
    /// The candidate was only reached through a guess — a case-folded comparison. A guess is
    /// never allowed to become a `Resolved`.
    guessed: bool,
}

/// A local name bound by an import declaration, as the extractor recorded it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ImportBinding {
    local: String,
    module: String,
    alias: Option<String>,
}

/// The module and alias of an import relation, whichever decided state it is in.
///
/// An import relation is `Pending` until this pass runs, so that is the common case; one already
/// decided by an earlier pass is re-read when its module moves, and its evidence has to come from
/// wherever it now lives.
fn import_evidence(state: &ResolutionState) -> Option<(String, Option<String>)> {
    match state {
        ResolutionState::Pending {
            evidence: Evidence::ImportBinding { module, alias },
            ..
        }
        | ResolutionState::Resolved {
            by: Evidence::ImportBinding { module, alias },
        }
        | ResolutionState::Inferred {
            by: Evidence::ImportBinding { module, alias },
            ..
        } => Some((module.clone(), alias.clone())),
        _ => None,
    }
}

/// The receiver a relation carries, in whichever decided state it is in.
///
/// **The state matters, and reading only `Pending` is a defect.** A relation is `Pending` the first
/// time the ladder sees it and decided afterwards, and [`resolve_paths`] re-decides every edge
/// pointing into a changed file — a second time, against a row that already holds a decision. A
/// receiver read from `Pending` alone is `None` on that second pass, so the relation walks a
/// *different* ladder than the one that placed it, and a `receiver_type` answer can be replaced by a
/// `same_file` or `unique_name` one. The evidence class is the only field that survives a decision
/// carrying the receiver through it, which is what D-0003 put in the schema.
///
/// `Ambiguous` and `Unresolved` carry no evidence at all, so a receiver relation that ended in one
/// of those has nothing to read back and is re-decided from its bare name. That is a limit of the
/// state rather than of this function, and it is why the two are named here rather than worked
/// around.
fn receiver_evidence(state: &ResolutionState) -> Option<String> {
    match state {
        ResolutionState::Pending {
            evidence: Evidence::ReceiverType { receiver },
            ..
        }
        | ResolutionState::Resolved {
            by: Evidence::ReceiverType { receiver },
        }
        | ResolutionState::Inferred {
            by: Evidence::ReceiverType { receiver },
            ..
        } => Some(receiver.clone()),
        _ => None,
    }
}

/// The qualified scope a relation carries, in whichever decided state it is in.
///
/// The same reason and the same cost as [`receiver_evidence`], one rung along: a path call decided
/// by the scope rung has its scope recorded in the evidence class, and a re-decision that ignored
/// it would fall through to the repository-wide rungs and report a claim where a proof was stored.
fn scope_evidence(state: &ResolutionState) -> Option<String> {
    match state {
        ResolutionState::Pending {
            evidence: Evidence::QualifiedNameInScope { scope },
            ..
        }
        | ResolutionState::Resolved {
            by: Evidence::QualifiedNameInScope { scope },
        }
        | ResolutionState::Inferred {
            by: Evidence::QualifiedNameInScope { scope },
            ..
        } => Some(scope.clone()),
        _ => None,
    }
}

/// The binder node type a relation carries as a local binding, in whichever state it is in.
///
/// **Three states, and the second and third are read for a reason rather than in case.** This engine
/// produces none of them — the head rule refuses before either could be reached — and they are read
/// anyway because the class is the field that carries the claim, and a row a `Resolved`/`Inferred`
/// build, or a future build, left carrying it has to get the same answer as one found `Pending`. The
/// second pass is the case that makes this a habit rather than a precaution: [`resolve_paths`]
/// re-decides rows that already hold an answer, and a `Pending`-only read finds `None` there and
/// walks a different ladder — see [`refused_for_local_binding`].
fn local_binding(state: &ResolutionState) -> Option<String> {
    match state {
        ResolutionState::Pending {
            evidence: Evidence::LocalBinding { binder },
            ..
        }
        | ResolutionState::Resolved {
            by: Evidence::LocalBinding { binder },
        }
        | ResolutionState::Inferred {
            by: Evidence::LocalBinding { binder },
            ..
        } => Some(binder.clone()),
        _ => None,
    }
}

/// Whether a relation is refused because a binder claims the name, in any state it can be in.
///
/// **The fourth arm is the cost, and it is why this is a separate function rather than a fourth
/// case in [`local_binding`].** [`ResolutionState::Unresolved`] carries no evidence, so once the
/// head rule fires the class is gone from the stored row and `peek explain` can name the reason but
/// not the binder. Handed the row, the ladder finds nothing in it and R5 will place `out` on
/// `report.rs`'s `render.out` — the exact confidently-wrong edge the rule exists to remove, restored
/// by the act of removing it.
///
/// **How often a refused row is handed back is a separate question, and it was measured rather than
/// assumed.** [`resolve_paths`] opens two doors: the outgoing edges of the paths it was given, and
/// the incoming edges of the entities those paths declare. A refusal has no target, so
/// `Store::incoming` matches on `target_path` and cannot find it — a scoped pass therefore does not
/// re-decide a refused row at all, and this arm never fires from one. The two routes that do hand a
/// row over are the caller's displaced snapshot (this function's arm, and the reason it is here) and
/// a re-extraction of the row's own file, which re-emits the relation as `Pending` **with the class**
/// and is refused by the arm above. So the arm is a guard on a public function's input rather than a
/// repair for a path the engine walks, and it is written as one.
///
/// What it does not do is make the claim checkable. The binder is gone from the row and this
/// function cannot put it back; that is a real loss and it is why the reason exists as a variant
/// of its own rather than as [`UnresolvedReason::NoCandidate`], which would have hidden the
/// distinction between "the name is not in the repository" and "the name is not what this
/// occurrence means".
fn refused_for_local_binding(state: &ResolutionState) -> bool {
    match state {
        ResolutionState::Unresolved {
            reason: UnresolvedReason::LocalBinding,
        } => true,
        other => local_binding(other).is_some(),
    }
}

/// The module an import placed a relation through, in whichever decided state it is in.
///
/// **The module is the whole of what survives, and that is enough to place it again.** A relation
/// an import rung placed records the module and not the receiver: `Gateway.send()` names the method
/// in `target_name` and the type in the receiver, and the evidence class has room for the first and
/// not the second. A re-decision therefore cannot read the receiver back, and without this the
/// relation would walk a shorter ladder and come out with a different — weaker, or wrong — answer.
///
/// The module is a sufficient key, because it identifies the import: the binding is found in the
/// caller's own file by the module it names, and the binding names the type the method belongs to.
/// Two bindings from one module (`use a::X; use a::Y;`) are both tried and the candidates are
/// decided together, so the pair is an `Ambiguous` rather than a pick.
///
/// Only reachable for a relation that is **not** an import, which is what distinguishes "an import
/// placed this reference" from "this relation *is* an import". [`Resolver::decide`] checks the
/// kind before asking.
fn placed_import(state: &ResolutionState) -> Option<String> {
    match state {
        ResolutionState::Pending {
            evidence: Evidence::ImportBinding { module, .. },
            ..
        }
        | ResolutionState::Resolved {
            by: Evidence::ImportBinding { module, .. },
        }
        | ResolutionState::Inferred {
            by: Evidence::ImportBinding { module, .. },
            ..
        } => Some(module.clone()),
        _ => None,
    }
}

/// The resolver's per-pass state: the options, the read caches, and the truncation counter.
struct Resolver<'s> {
    store: &'s Store,
    options: ResolutionOptions,
    /// The import bindings of each file this pass has already read. A file is read once.
    imports: BTreeMap<RepoPath, Vec<ImportBinding>>,
    /// Every entity of each file this pass has already read. A file is read once, and **not only
    /// because the ladder wants it that way** — see [`Resolver::entities_in_file`].
    files: BTreeMap<RepoPath, Arc<Vec<crate::model::Entity>>>,
    /// The lexical scope of each relation source this pass has already walked.
    ///
    /// **Cached per source rather than per file**, because the scope is a chain of
    /// declarations and not a list: a relation written in a parameter has a longer
    /// chain than one written in a function, and a file holds both. The chain is
    /// four links deep in every shape the extractors emit and the walk is bounded by
    /// a `seen` set rather than by a depth counter, so the cost is a handful of
    /// indexed seeks per distinct source.
    scope: BTreeMap<EntityId, Vec<EntityId>>,
    /// Lookups abandoned at a limit, carried into the report rather than hidden.
    truncated: u64,
    /// How the table and the guess compared, read out into the report at the end of the pass for the
    /// same reason `truncated` is: a counter owned by the resolver and read out of the scope that
    /// owns it cannot be forgotten at the point of construction and silently report zero.
    module_files: ModuleFileReport,
}

impl<'s> Resolver<'s> {
    fn new(store: &'s Store, options: ResolutionOptions) -> Self {
        Self {
            store,
            options,
            imports: BTreeMap::new(),
            files: BTreeMap::new(),
            scope: BTreeMap::new(),
            truncated: 0,
            module_files: ModuleFileReport::default(),
        }
    }

    /// Every entity one file declares, read a page at a time and then kept for the rest of the pass.
    ///
    /// **Cached, and the reason is arithmetic rather than taste.** R4 — the same-file rung — is
    /// reached by nearly every relation the ladder cannot place earlier, and each of those asks for
    /// the entities of the file the relation is written in. So without a cache a file is read once
    /// per relation that names it, and reading all of it rather than a prefix of it multiplies
    /// that by however much bigger the file is than the old cut-off.
    ///
    /// Both halves of that were measured on `BurntSushi/ripgrep`, full build, release, on the
    /// verification sandbox, and the middle row is the one that matters:
    ///
    /// | | full build | refresh of `defs.rs` |
    /// |---|---:|---:|
    /// | bounded read (before paging) | 9.7s | 5.4s |
    /// | paged, not cached | 13.5s | 5.6s |
    /// | paged and cached | 3.3s | 1.0s |
    ///
    /// Paging on its own costs 38% of a build, because `defs.rs` — 1,363 entities, 2,469 relations
    /// the pass must decide — was being re-read 2,469 times at three seeks each. Caching it turns
    /// those into three reads. The middle row is also the same cost the indexer's widening of the
    /// per-file bound was paying, for the same reason: a wider bound made an already-repeated read
    /// read more. So the two changes are not independent, and either one alone leaves the file
    /// being read once per relation.
    ///
    /// Correct because the store does not change under a pass: every decision goes into one
    /// [`IndexUpdate`] applied after the last relation has been decided, so the entity rows this
    /// read returns are the rows every later read would return. That is the same assumption
    /// [`Resolver::imports`] already rests on, and the reason both caches live here rather than in
    /// the store.
    ///
    /// **The cache is smaller than what the pass already holds.** [`resolve_all`] materialises every
    /// relation it is going to decide, in one `Vec`, before deciding any of them — 145,650 rows for
    /// `rust-lang/cargo` against 27,440 entities, and a `Relation` is no smaller than an `Entity`,
    /// since it carries an identity of its own plus the evidence the decision was made by. So the
    /// entity rows are a fraction of a structure this pass already builds, not a new memory class.
    /// `Arc` because the callers iterate the list while calling back into the resolver, so handing
    /// out a borrow of the cache would make the resolver immutable for the whole lookup; a shared
    /// handle costs one atomic bump.
    ///
    /// Note what is *not* here: no `truncated`. A read that had to ask for a second page was not cut
    /// short — it was completed. Counting a page boundary as a truncation would put the size of an
    /// ordinary file into the counter and leave a caller unable to tell "this build answered from a
    /// narrower view" from "this build read a big file", which are opposite situations.
    fn entities_in_file(
        &mut self,
        path: &RepoPath,
    ) -> Result<Arc<Vec<crate::model::Entity>>, StoreError> {
        if let Some(known) = self.files.get(path) {
            return Ok(Arc::clone(known));
        }
        let read = Arc::new(all_entities_in_file(
            self.store,
            path,
            self.options.entities_page,
        )?);
        self.files.insert(path.clone(), Arc::clone(&read));
        Ok(read)
    }

    /// Read the entities carrying one name, and whether the bound cut the read short.
    ///
    /// The tuple rather than a bare `Vec` because the caller has to be able to tell a complete
    /// candidate set from a prefix of one, and the only place that difference exists is here.
    /// Reporting it by counting is not enough: the counter is a pass-wide total, so by the time a
    /// rung asked the question the count would no longer say which read was responsible.
    fn entities_named(
        &mut self,
        name: &str,
    ) -> Result<(Vec<crate::model::Entity>, bool), StoreError> {
        let limit = self.options.entities_by_name;
        let found = self.store.entities_named(name, limit)?;
        let cut_short = found.len() >= limit;
        if cut_short {
            self.truncated += 1;
        }
        Ok((found, cut_short))
    }

    // -----------------------------------------------------------------------
    // The ladder
    // -----------------------------------------------------------------------

    /// Decide one relation. The rungs are tried strongest first.
    fn decide(&mut self, relation: &Relation) -> Result<Decision, StoreError> {
        // Read from the state rather than from a `Pending` pattern, because a scoped pass decides
        // relations that already hold a decision — see `receiver_evidence`.
        let receiver = receiver_evidence(&relation.resolution);
        let scope = scope_evidence(&relation.resolution);

        // **Not a rung.** `local_binding` is a claim that there is no target, so there is nothing
        // for a rung to rank: R1-R5 would each answer a question about which entity the name
        // denotes, and the answer is that this occurrence does not denote one. So the rule goes
        // ahead of every rung, and ahead of the glob-import check below, because both of those are
        // claims about a name and neither is a claim about *this* occurrence.
        //
        // The figure below is **the gate fixture's measurement, not a property of the rule**: on
        // `tests/fixtures/gate/rust` it removed 7 decided-and-wrong edges and 0 decided-and-right
        // ones, taking `resolution_correctness` from 75.00 (36/48) to 87.80 (36/41) over a
        // denominator that shrank by the 7 wrong rows and by nothing else. One fixture, and the
        // only one with a `binds_nothing` label at all.
        if refused_for_local_binding(&relation.resolution) {
            return Ok(Decision::Unresolved {
                reason: UnresolvedReason::LocalBinding,
            });
        }

        // A glob import binds no single name, so no rule can ever prove a target for it. Saying
        // so beats falling through to a name lookup on the character `*`, which is roughly what
        // the predecessor's `looks_like_module` heuristic amounted to.
        if relation.kind == RelationKind::Imports && relation.target_name == GLOB {
            return Ok(Decision::Unresolved {
                reason: UnresolvedReason::Unsupported,
            });
        }

        // R1. Skipped for a receiver: a receiver and an import of one name are two claims about
        // the same symbol, and the receiver is the more specific of the two. R2 asks again below,
        // and only if it declines.
        if receiver.is_none()
            && let Some(decision) = self.via_import_binding(relation)?
        {
            return Ok(decision);
        }

        // R2. Decides when it can name the receiver's owner in the caller's own file, refuses when it
        // can name one and finds nothing inside it, and declines when it cannot name one at all.
        if let Some(receiver) = receiver {
            match self.via_receiver(relation, &receiver)? {
                Receiver::Answered(decision) => return Ok(decision),
                Receiver::Declined => {
                    let owners = [receiver];
                    if let Some(decision) = self.via_imported_receiver(relation, &owners)? {
                        return Ok(decision);
                    }
                    // Neither the caller's own file nor an import into it names an owner. That is a
                    // real absence rather than a rung that could not look: `service.retry()` has no
                    // owner anywhere this pass can see, and the alternatives are a free function
                    // of the same name somewhere in the repository, which is audit B3.
                    return Ok(Decision::Unresolved {
                        reason: UnresolvedReason::NoCandidate,
                    });
                }
            }
        }

        // R1 again, for a relation an import placed and this pass is seeing a second time. Its
        // evidence names the module it was placed through, the module names the binding, and the
        // binding names the type — so the receiver is found without being stored. See
        // `placed_import` for why it is not in the evidence in the first place.
        if relation.kind != RelationKind::Imports
            && let Some(module) = placed_import(&relation.resolution)
        {
            let owners = self.local_names_of(&module, relation.source.path())?;
            if let Some(decision) = self.via_imported_receiver(relation, &owners)? {
                return Ok(decision);
            }
        }

        // R3.
        if let Some(scope) = scope
            && let Some(decision) = self.via_scope(relation, &scope)?
        {
            return Ok(decision);
        }

        // R4.
        if let Some(decision) = self.via_same_file(relation)? {
            return Ok(decision);
        }

        // R5.
        if let Some(decision) = self.via_unique_name(relation)? {
            return Ok(decision);
        }

        Ok(Decision::Unresolved {
            reason: reason_for_nothing_found(&relation.target_name),
        })
    }

    /// Every declaration `source` is lexically written inside, nearest first.
    ///
    /// **Containment, not the qualified name.** `A.b.c` is declared inside `A.b`, but a
    /// qualified name does not say that: the layout module a file is given and the
    /// `mod x { .. }` block beside it both qualify the things under them differently, and
    /// only the `Contains` edge says what encloses what. The chain is walked upwards with
    /// [`Store::incoming`] and terminates on a `seen` set rather than a depth counter, so
    /// it is total even for a graph whose containment is not a tree.
    fn scope_of(&mut self, source: &EntityId) -> Result<Vec<EntityId>, StoreError> {
        if let Some(known) = self.scope.get(source) {
            return Ok(known.clone());
        }
        let limit = self.options.outgoing_per_source;
        let mut chain: Vec<EntityId> = vec![source.clone()];
        let mut seen: BTreeSet<EntityId> = BTreeSet::new();
        let mut at = 0usize;
        while at < chain.len() {
            let current = chain[at].clone();
            at += 1;
            if !seen.insert(current.clone()) {
                continue;
            }
            for relation in self
                .store
                .incoming(&current, Some(RelationKind::Contains), limit)?
            {
                if chain.contains(&relation.source) {
                    continue;
                }
                chain.push(relation.source);
            }
        }
        self.scope.insert(source.clone(), chain.clone());
        Ok(chain)
    }

    /// Every declaration the use's own scope makes under `name`, nearest first.
    ///
    /// **This is the whole of what "in scope" means in this module, and it is the rule
    /// every name lookup applies on top of its own evidence** — the same place
    /// [`prefer_symbols`] applies one. A bare name is read against the declarations
    /// enclosing the occurrence, not against the file and not against the repository:
    /// `format_line_inner`'s own parameter `entry` is in scope inside `format_line_inner`
    /// and means nothing anywhere else, which is what makes it the answer there even
    /// though the file also imports a function called `entry`, and what makes it
    /// *not* the answer inside `render`.
    ///
    /// **Direct children only.** A parameter of a sibling function is not in scope inside
    /// another function, and the distinction is the whole of the second half of the rule:
    /// `report.rs` declares a parameter `format_line.count`, and a use of `count` written
    /// in `render` cannot mean it.
    fn declared_around(
        &mut self,
        relation: &Relation,
        name: &str,
    ) -> Result<Vec<EntityId>, StoreError> {
        let limit = self.options.outgoing_per_source;
        let scope = self.scope_of(&relation.source)?;
        let mut found: Vec<EntityId> = Vec::new();
        for owner in scope {
            let enclosed = self
                .store
                .outgoing(&owner, Some(RelationKind::Contains), limit)?;
            if enclosed.len() >= limit {
                self.truncated += 1;
            }
            for relation in enclosed {
                let Some(target) = relation.target else {
                    continue;
                };
                if target.name() == name
                    && is_declaration(target.kind())
                    && !found.contains(&target)
                {
                    found.push(target);
                }
            }
        }
        Ok(found)
    }

    /// The in-scope declarations this relation may actually be answered with.
    ///
    /// **The third clause of the scope rule, and the only one that reads the relation's
    /// class.** A reference *refers to* a binding and a call *invokes* one, and those
    /// are different questions: `last = body()` where `body` is a parameter of type
    /// `impl Fn()` invokes the **value** the parameter holds, which is not a declaration
    /// the index can name, while the same name in `body.count` refers to the parameter
    /// itself and the two classes answer differently from one occurrence. So an in-scope
    /// binding is a candidate for a reference and not for a call, and a call that has
    /// only a binding to point at is left unestablished rather than given a target that
    /// says "this function invokes its own parameter".
    ///
    /// **Bounded to the in-scope set on purpose.** Outside the scope a binding is not a
    /// candidate at all, so the distinction is only ever asked about the one binding
    /// the occurrence can actually see; the out-of-scope case is already settled by
    /// [`is_binding`].
    fn invocable_around(&self, relation: &Relation, in_scope: Vec<EntityId>) -> Vec<EntityId> {
        match relation.kind {
            RelationKind::Calls => in_scope
                .into_iter()
                .filter(|id| !is_binding(id.kind()))
                .collect(),
            _ => in_scope,
        }
    }

    /// R1: the name is bound by an import in the referring file.
    ///
    /// **Declines when the use's own scope already declares the name**, and that is the
    /// first of the two places the scope rule reads. The rung's evidence is "the author
    /// wrote an import in this file naming this symbol", which is a fact about a *name* in
    /// a file; it is not a fact about *this occurrence*. `use crate::model::{entry, Entry}`
    /// beside `fn format_line_inner(entry: &Entry)` is two declarations of one name, and
    /// the parameter shadows the import inside the function that declares it — in every
    /// language this engine reads, and in Rust because the parameter list is inside the
    /// body of the function.
    ///
    /// The decline is not a refusal: the relation falls through to R4, which finds the
    /// parameter the scope makes. A file that imports `charge` and has an unrelated
    /// function with a parameter called `charge` is untouched, because that function's own
    /// scope is the one that declares it.
    fn via_import_binding(&mut self, relation: &Relation) -> Result<Option<Decision>, StoreError> {
        let source_path = relation.source.path().clone();
        let bindings = self.bindings_for(&relation.target_name, &source_path)?;
        if bindings.is_empty() {
            return Ok(None);
        }
        if !self
            .declared_around(relation, &relation.target_name)?
            .is_empty()
        {
            return Ok(None);
        }

        let mut found: Vec<Found> = Vec::new();
        for binding in &bindings {
            let evidence = Evidence::ImportBinding {
                module: binding.module.clone(),
                alias: binding.alias.clone(),
            };
            for id in self.targets_of(binding, &source_path)? {
                if !found.iter().any(|already| already.id == id) {
                    found.push(Found {
                        id,
                        by: evidence.clone(),
                        guessed: false,
                    });
                }
            }
        }
        Ok(match found.is_empty() {
            true => None,
            false => Some(
                self.decide_candidates(found, "resolved through an import binding in this file"),
            ),
        })
    }

    /// The import bindings of one file whose local name is `name`.
    fn bindings_for(
        &mut self,
        name: &str,
        path: &RepoPath,
    ) -> Result<Vec<ImportBinding>, StoreError> {
        Ok(self
            .bindings_in(path)?
            .into_iter()
            .filter(|binding| binding.local == name)
            .collect())
    }

    /// Every import binding one file declares, read once and cached.
    ///
    /// Owned rather than borrowed: the bindings live in this resolver's cache, and holding a borrow
    /// of that cache across the `self.targets_of` calls that consume them would make the resolver
    /// immutable for the whole lookup. Split from [`Resolver::bindings_for`] so the two questions
    /// the ladder asks of a file — *which name does this import bind* and *which import binds this
    /// module* — read the cache instead of duplicating it.
    fn bindings_in(&mut self, path: &RepoPath) -> Result<Vec<ImportBinding>, StoreError> {
        if !self.imports.contains_key(path) {
            let limit = self.options.outgoing_per_source;
            let mut bindings = Vec::new();
            let entities = self.entities_in_file(path)?;
            for entity in entities.iter() {
                let relations =
                    self.store
                        .outgoing(&entity.id, Some(RelationKind::Imports), limit)?;
                if relations.len() >= limit {
                    self.truncated += 1;
                }
                for relation in relations {
                    if let Some((module, alias)) = import_evidence(&relation.resolution) {
                        bindings.push(ImportBinding {
                            local: relation.target_name.clone(),
                            module,
                            alias,
                        });
                    }
                }
            }
            self.imports.insert(path.clone(), bindings);
        }
        Ok(self.imports.get(path).cloned().unwrap_or_default())
    }

    /// The local names the imports of `path` bind out of `module`.
    ///
    /// The second way of finding a receiver, and the one a re-decision has to use: a relation placed
    /// by an import records the module it was placed through and not the name it was reached by, so
    /// the module is what the binding is looked up from. Every matching name is returned rather than
    /// the first, because a file may import two things from one module and the answer is the one the
    /// method belongs to — which is [`Resolver::via_imported_receiver`]'s to give, not this one's.
    fn local_names_of(&mut self, module: &str, path: &RepoPath) -> Result<Vec<String>, StoreError> {
        let mut names: Vec<String> = Vec::new();
        for binding in self.bindings_in(path)? {
            if binding.module == module && !names.contains(&binding.local) {
                names.push(binding.local);
            }
        }
        Ok(names)
    }

    /// The entities an import binding names.
    ///
    /// An aliased binding (`use a::b as c`) names a module, so the target is that module's file
    /// and the whole path is the module. An unaliased binding names an item inside a module, so
    /// the target is an entity carrying that name in the module's file, and both readings of the
    /// path are tried — because `use a::b::c` may mean the item `c` in module `a::b` or the
    /// module `a::b::c`, and nothing in the syntax says which. If both exist the caller gets an
    /// `Ambiguous`, which is the honest answer; reporting one would be a pick.
    fn targets_of(
        &mut self,
        binding: &ImportBinding,
        importer: &RepoPath,
    ) -> Result<Vec<EntityId>, StoreError> {
        if binding.local == GLOB {
            return Ok(Vec::new());
        }
        let aliased = binding.alias.is_some();
        let mut found: Vec<EntityId> = Vec::new();
        for file in self.module_files(&binding.module, importer, !aliased)? {
            for entity in self.entities_in_file(&file)?.iter() {
                let wanted = match &binding.alias {
                    Some(_) => entity.kind() == EntityKind::File,
                    None => is_declaration(entity.kind()) && entity.name == binding.local,
                };
                if wanted {
                    found.push(entity.id.clone());
                }
            }
        }
        Ok(found)
    }

    /// The files a module path names: the guess's, if the guess can name one, and the
    /// table's first answer otherwise.
    ///
    /// The extractor writes a `Module` entity for every file it reads, with a qualified name that
    /// is the path from the package's source root. That turns "which file does `alpha::gateway`
    /// name" from a question answered by guessing at paths — a handful of primary-key seeks per
    /// anchor, per reading, with no way to cross a package boundary — into **one indexed seek** on
    /// `entities_with_qualified_name`.
    ///
    /// ## What was wrong, and it was two things and neither was "the table is inexact"
    ///
    /// This used to collect the files of *every* qualified name it tried, return the union, and
    /// fall back to the path guess only when that union was empty. The controlled comparison in
    /// `.agent/EVIDENCE/MODULE-TABLE.md` measured that policy across five repositories and found it
    /// worth **+2.5pp** of resolved relations on `rust-lang/cargo` and **-1.1pp** on
    /// `serde-rs/serde`, monotone in the count of cross-package references. Both regressions were
    /// diagnosed here, from the stored rows rather than from the counts:
    ///
    /// 1. **The package prefix was usually missing, so the lookup could only offer a spelling meant
    ///    for another package.** [`Resolver::package_of`] read the first
    ///    [`ResolutionOptions::modules_per_lookup`] entities of the importing file, which is
    ///    `ORDER BY kind` — and `Module` and `Package` sort *after* `function`, `constant` and
    ///    `method` — so on a real source file it found no package at all: **43 of
    ///    `BurntSushi/ripgrep`'s 110 indexed files**. With no package, `crate::decompress::X` was
    ///    looked up as `decompress`, and that *is* a row this index holds, because
    ///    `crates/cli/src/lib.rs` declares `mod decompress;`. The answer was the file that
    ///    **declares** the module rather than the file that **is** it. Fixed in `package_of`.
    /// 2. **The unprefixed spelling collides with a declaration row, and the guess can tell.** A
    ///    repository whose crates keep their modules directly under the crate directory — ripgrep's
    ///    `crates/core/flags/defs.rs` — gets every file named for the directory above it, so
    ///    `extract::modules::locate` records the package as `flags` and the table still cannot
    ///    spell `crate::flags::Flag`; it reaches `flags`, which is the `mod flags;` row in
    ///    `crates/core/main.rs`. **That is a defect in the extractor's package naming and not
    ///    something the resolver can repair** — the package is not in the index under any name the
    ///    import uses. What the resolver *can* do is notice that the file it was handed is not a file
    ///    the referring file's own import could have meant, and that the guess found one it could.
    ///
    /// ## The rule
    ///
    /// **A path says which crate it means, and that decides which route is asked.** See
    /// [`is_crate_relative`] for the reading and for the direction it errs in.
    ///
    /// 1. **A crate-relative path is the guess's** — `crate::`, `self::`, or a head that is the
    ///    importing file's own package — because a file inside the referring file's own directory
    ///    chain is the only thing such a path can mean, and [`files_for_module`] is anchored there
    ///    and [`Resolver::module_qualified_names`] is anchored at a package name.
    /// 2. **A path that names a package is the table's**, because `src` is not a segment of a module
    ///    path and no anchor list can produce `crates/globset/src/lib.rs` from
    ///    `crates/beta/src/`. That is the whole of the table's recorded value — 4,593 more
    ///    `import_binding` proofs on `rust-lang/cargo` — and first-hit-wins is what stops the
    ///    cross-package *fallback* spelling from answering a path the package-prefixed spelling
    ///    could answer.
    /// 3. **Whichever route was asked second gets its turn** if the first found nothing, because a
    ///    path one of them can place and the other cannot is the one case where the other's silence
    ///    is not an answer.
    /// 4. **Otherwise there is no file**, which is a decline, and the caller falls through the rest
    ///    of the ladder.
    ///
    /// **Asking only one route first is what keeps this cheap**, and the ordering is not
    /// symmetrical: on `rust-lang/cargo` the table answered 12,234 lookups and every one of them
    /// named a package, so none of them spent a single candidate probe. Reversing the two arms —
    /// always guess first — measured 30.0s against 20.5s for the guess-only baseline on that
    /// repository, and asking the table *after* the guess on every lookup is the same cost.
    ///
    /// **The union of both routes was tried first and rejected by measurement**, which is worth
    /// recording because a union is what "try both" most plainly means. On `BurntSushi/ripgrep`,
    /// full build, release: 2,256 lookups, and searching the union of the table's and the guess's
    /// file sets added a file the table had not named in **6** of them, recovering 7 of the 336
    /// proofs the policy was costing. The two routes do not disagree about intra-crate paths —
    /// whenever the table answers, its set already contains the guess's — so widening it cannot
    /// help, and the rule has to be a precedence rather than a union.
    ///
    /// **What the losing candidate was good for, so this is not read as "the guess wins".** The
    /// table reaches across a package boundary and the guess cannot, at any anchor depth; that is
    /// the whole of its value and step 2 is where it is spent. The guess reaches what the table
    /// cannot spell, because it is anchored to a *file* rather than to a package name; that is the
    /// whole of its, and step 1 is where it is spent. Neither is a generalisation of the other,
    /// which is the honest reason this is a precedence and not a preference.
    ///
    /// [`ResolutionOptions::use_module_table`] turns the whole of this off, which is how the two
    /// arms of the measurement come out of one build. **The off arm is [`files_for_module`] alone,
    /// exactly as it was** — not the same rule with the table part removed — because a baseline
    /// that moves is not a baseline, and the recorded per-rung figures were read against it.
    fn module_files(
        &mut self,
        module: &str,
        importer: &RepoPath,
        strip_last: bool,
    ) -> Result<Vec<RepoPath>, StoreError> {
        if !self.options.use_module_table {
            return Ok(files_for_module(module, importer, strip_last));
        }

        // The importer's own package, so `crate::a::b` becomes `package::a::b`. Taken from the
        // module row the importing file *is*, which is the only place the package name is
        // recorded — a directory name would be a guess, and a guess here is what produced
        // `no_candidate` for 343 of 344 edges on a real cross-crate repository. Read before
        // anything else because it decides which route is asked first, and therefore what the pass
        // spends its seeks on.
        let package = self.package_of(importer)?;
        let package_name = match &package {
            None => None,
            Some(Package::Unnamed) => None,
            Some(Package::Prefixed(prefix)) => Some(prefix.as_str()),
        };

        // Which route to ask first, and this is the whole conflict rule.
        //
        // **A path whose head is `crate`, `self` or the importer's own package names something in
        // the referring file's own crate, and only a file inside the referring file's own
        // directory chain can be it.** The guess is anchored there; the table is anchored at a
        // package name, which for `crate::flags::Flag` in a crate with no `src` directory is the
        // string `flags`, and the row that string names is the `mod flags;` declaration in
        // `core/main.rs` rather than the module's own file. So on those paths the guess answers
        // and the table's seeks are not spent.
        //
        // **Anything else is a path that names a package**, and there the table is the only route
        // that can answer it at all — `src` is not a segment of a module path, so no anchor list
        // produces `crates/globset/src/lib.rs` from `crates/cli/src/` — so it answers first and
        // the guess's seeks are not spent. That ordering is also what keeps the cost of this rule
        // near zero: on `rust-lang/cargo` it is the difference between 20,366 and 15,679 candidate
        // probes per pass.
        let crate_relative = is_crate_relative(module, package_name);
        let mut guessed: Option<Vec<RepoPath>> = None;
        if crate_relative {
            guessed = Some(self.guessed_files(module, importer, strip_last)?);
            if let Some(files) = guessed.as_ref().filter(|files| !files.is_empty()) {
                self.module_files.record(ModuleFileLookup {
                    asked: 0,
                    outcome: ModuleFileOutcome::Guess,
                    package_known: package.is_some(),
                    named: false,
                });
                return Ok(files.clone());
            }
        }

        // The table, at the first spelling that names a file. First-hit-wins is what stops the
        // cross-package *fallback* spelling from answering a path the package-prefixed spelling
        // could answer; taking the union instead is what let a declaration site's `mod x;` row
        // answer a path about the module's own file.
        let mut asked = 0usize;
        for qualified in
            self.module_qualified_names(module, package_name, strip_last, crate_relative)
        {
            asked += 1;
            let mut found: Vec<RepoPath> = Vec::new();
            for entity in self
                .store
                .entities_with_qualified_name(&qualified, self.options.modules_per_lookup)?
            {
                if entity.kind() == EntityKind::Module {
                    push_new(&mut found, entity.path().clone());
                }
            }
            if !found.is_empty() {
                self.module_files.record(ModuleFileLookup {
                    asked,
                    outcome: ModuleFileOutcome::Table,
                    package_known: package.is_some(),
                    named: !crate_relative,
                });
                return Ok(found);
            }
        }

        // The table named nothing. The guess gets its turn, because a path it can place and the
        // table cannot is the one case where the table's silence is not an answer.
        let guessed = match guessed {
            Some(files) => files,
            None => self.guessed_files(module, importer, strip_last)?,
        };
        self.module_files.record(ModuleFileLookup {
            asked,
            outcome: match guessed.is_empty() {
                true => ModuleFileOutcome::Neither,
                false => ModuleFileOutcome::Guess,
            },
            package_known: package.is_some(),
            named: false,
        });
        Ok(guessed)
    }

    /// The subset of [`files_for_module`]'s paths that the index holds.
    ///
    /// A path the index does not hold can never contribute a candidate — a candidate is an entity,
    /// and there are none in a file this build did not extract — so dropping one cannot change an
    /// answer, only the work. And it is the overwhelming majority of what the guess produces: a
    /// three-segment path from a file four directories down is a dozen anchors of which at most one
    /// exists.
    ///
    /// Read through [`Resolver::entities_in_file`], which pages and then caches, because the
    /// files that *are* held are searched by the caller immediately afterwards and the read is
    /// shared. **A one-row read was tried here instead and measured worse**: on `rust-lang/cargo`
    /// it cost 30.0s against 27.7s for the cached full read, because a held file is then read
    /// twice and the uncached rows are not what the time was going into.
    fn guessed_files(
        &mut self,
        module: &str,
        importer: &RepoPath,
        strip_last: bool,
    ) -> Result<Vec<RepoPath>, StoreError> {
        let mut guessed: Vec<RepoPath> = Vec::new();
        for path in files_for_module(module, importer, strip_last) {
            if guessed.contains(&path) {
                continue;
            }
            if !self.entities_in_file(&path)?.is_empty() {
                guessed.push(path);
            }
        }
        Ok(guessed)
    }

    /// The qualified names a module path could have, in the order they should be tried.
    ///
    /// A path is tried as written and, when the last segment may be an item rather than a module,
    /// with the last segment dropped. `crate` and `self` are stripped because the module table is
    /// named from the package root, so `crate::a::b` is `package::a::b` and the leading `crate`
    /// has no counterpart. `super` is a climb, which only the path guess can express, so a path
    /// that contains one is left to the guess entirely rather than being half-answered.
    ///
    /// `package` is passed rather than read here so that one lookup answers it for both routes,
    /// and `&str` rather than `&RepoPath` because the guess's inputs and the table's inputs are
    /// different things and this one needs only the first segment of a qualified name.
    fn module_qualified_names(
        &self,
        module: &str,
        package: Option<&str>,
        strip_last: bool,
        crate_relative: bool,
    ) -> Vec<String> {
        if module.split("::").any(|part| part == "super") {
            return Vec::new();
        }
        let segments: Vec<&str> = module
            .split("::")
            .filter(|part| !matches!(*part, "" | "crate" | "self"))
            .collect();
        if segments.is_empty() || segments.len() > MAX_MODULE_SEGMENTS {
            return Vec::new();
        }

        let mut readings: Vec<Vec<&str>> = vec![segments.clone()];
        if strip_last && segments.len() > 1 {
            readings.push(segments[..segments.len() - 1].to_vec());
        }
        let mut qualified = Vec::new();
        for reading in readings {
            // **Both spellings of every reading, and which comes first is what the path says.**
            // `crate::a::b` means this crate's `a::b`, so `package::a::b` is tried first and the
            // bare `a::b`, which in a workspace is a perfectly valid qualified name belonging to
            // some *other* package, is the fallback. `alpha::a::b` means the package `alpha`, so
            // the bare spelling goes first and `package::alpha::a::b`, which is a coincidence of
            // the importing crate happening to have a module of that name, is the fallback.
            // Swapping the two orderings costs a wrong answer rather than a lookup, because only
            // the first spelling that names a file is used.
            let mut with_package: Vec<&str> = Vec::with_capacity(reading.len() + 1);
            with_package.extend(package);
            with_package.extend(reading.iter().copied());
            let prefixed = with_package.join("::");
            let bare = reading.join("::");
            let spellings: [&str; 2] = match crate_relative {
                true => [&prefixed, &bare],
                false => [&bare, &prefixed],
            };
            for candidate in spellings {
                if !qualified.contains(&candidate.to_owned()) {
                    qualified.push(candidate.to_owned());
                }
            }
        }
        qualified
    }

    /// The package a file belongs to, read from the module row the file *is*.
    ///
    /// **This is the whole of the intra-crate regression, and it was a lookup that could not
    /// answer.** It used to read [`Store::entities_in_file`] with
    /// [`ResolutionOptions::modules_per_lookup`], which is `ORDER BY kind, qualified_name,
    /// entity_ordinal LIMIT n` — and `Module` and `Package` sort *after* `function`, `constant` and
    /// `method`. So on a real source file the eight rows it read were the file entity and whatever
    /// declarations came first, the namespace row was not among them, and the answer was `None`.
    /// Measured on `BurntSushi/ripgrep`: **43 of the repository's 110 indexed files yielded a
    /// package from those eight rows.**
    ///
    /// With no package, [`Resolver::module_qualified_names`] can only offer the *unprefixed*
    /// spellings — and the unprefixed spelling of `crate::decompress::DecompressionMatcher` is
    /// `decompress`, which is a row this index holds: the `mod decompress;` **declaration** in
    /// `crates/cli/src/lib.rs`. The lookup answered with the file that declares the module rather
    /// than the file that is it, found no `DecompressionMatcher` in it, declined, and the edge fell
    /// to the rung that can only claim. **349 of the 351 intra-crate proofs the module table was
    /// costing that repository were `crate::` paths, and this is why.**
    ///
    /// **The fix is a row the extractor already wrote.** `extract::modules::for_file` emits exactly
    /// one `Contains` edge from the file entity to the module row naming the file's own place in
    /// the module tree, so one indexed seek on `relation_by_target` names it exactly — where "the
    /// first namespace row in a kind-ordered read" was a guess about which of several rows was the
    /// file's own, and on `crates/cli/src/decompress.rs` it picks `DecompressionMatcher` over
    /// `cli::decompress`.
    ///
    /// `None` when the file has no such edge, which is the ordinary case for a language with no
    /// module layout: `for_file` emits nothing for such a language, so *no* file carries one and
    /// `None` means "this index does not record packages". The caller then tries the unprefixed
    /// spelling, which is right rather than a degraded mode.
    /// `max` if the referring file's own crate root, `None` if it is not in the index.
    ///
    /// The package row is read through the file entity's own `Contains` edges, so this is the
    /// package of the file *itself* rather than the first namespace row in a kind-ordered read —
    /// which is a guess about which of several rows was the file's own, and on
    /// `crates/cli/src/decompress.rs` picks `DecompressionMatcher` over `cli::decompress`.
    fn package_of(&self, importer: &RepoPath) -> Result<Option<Package>, StoreError> {
        let file = EntityId::new(
            importer.clone(),
            EntityKind::File,
            importer.file_name().to_owned(),
            0,
        );
        // **Outgoing, not incoming.** `extract::modules::for_file` writes the edge *from* the file
        // entity *to* the module row — "the file is what the module is written in" — so asking for
        // the edges *arriving* at the file returns nothing at all and this function answers `None`
        // for every file, which is the same defect above in a second shape.
        //
        // `outgoing_per_source` rather than `modules_per_lookup`, and deliberately larger: this
        // edge is the only `Contains` a file entity has, so the answer must not depend on how many
        // rows happen to precede it.
        for relation in self.store.outgoing(
            &file,
            Some(RelationKind::Contains),
            self.options.outgoing_per_source,
        )? {
            let Some(target) = relation.target.as_ref() else {
                continue;
            };
            if target.kind() != EntityKind::Module {
                continue;
            }
            // `cli::decompress` is the module; `cli` is the package. Read from the row rather than
            // from the path, so a crate whose name differs from its directory is still spelled the
            // way a `use` statement spells it. No prefix means the file has no package and the
            // qualified name is tried without one — which is exactly how a non-workspace
            // repository spells it, so that is a fallback that is right rather than a degraded mode.
            let qualified = target.qualified_name();
            let package = match qualified.split_once("::") {
                Some((package, _)) => package,
                None => return Ok(Some(Package::Unnamed)),
            };
            let mut prefix = String::with_capacity(package.len() + 2);
            prefix.push_str(package);
            prefix.push_str("::");
            return Ok(Some(Package::Prefixed(prefix)));
        }
        Ok(None)
    }

    /// R2: the receiver names an owner, and the target is declared inside that owner.
    ///
    /// Returns [`Receiver::Declined`] rather than an answer when no owner can be named from the
    /// caller's own file. A type that reached this file through an import has no owner here to
    /// find, and reporting "there is no owner" about that is a claim the index does not support.
    /// The ladder hands it to [`Resolver::via_imported_receiver`] instead, which is the rung that
    /// knows where the name went.
    ///
    /// Returns [`Receiver::Answered`] carrying an `Unresolved` when it *did* name an owner and
    /// found nothing inside it. That is a refusal rather than a decline, and it is the
    /// load-bearing difference: falling through there would bind `service.retry()` to whatever
    /// free function named `retry` sorts first in the repository, which is audit B3 exactly.
    fn via_receiver(
        &mut self,
        relation: &Relation,
        receiver: &str,
    ) -> Result<Receiver, StoreError> {
        let name = relation.target_name.as_str();
        let in_file = self.entities_in_file(relation.source.path())?;

        // `self`, `this` and `Self` name the enclosing declaration, and the source's qualified
        // name already carries it exactly. No guess is involved, and no entity lookup either, so
        // the namespace rule below does not apply to it: the owner here is a prefix of a
        // qualified name, not a row somebody chose.
        let owners: Vec<(String, bool)> = if matches!(receiver, "self" | "this" | "Self") {
            match enclosing_owner(relation) {
                Some(owner) => vec![(owner, false)],
                None => Vec::new(),
            }
        } else {
            let exact: Vec<String> = in_file
                .iter()
                .filter(|entity| entity.name == receiver)
                .map(|entity| entity.name.clone())
                .collect();
            let named: Vec<(String, bool)> = match exact.is_empty() {
                true => {
                    // The documented case-fold, and the only one in this file. A local variable
                    // called `service` and the type `Service` are the same thing in every language
                    // here, but the match is a guess, so it is recorded as one: the decision
                    // becomes `Inferred` and its basis says that letter case was ignored.
                    in_file
                        .iter()
                        .filter(|entity| entity.name.eq_ignore_ascii_case(receiver))
                        .map(|entity| (entity.name.clone(), true))
                        .collect()
                }
                false => exact.into_iter().map(|owner| (owner, false)).collect(),
            };
            // A name this file carries only through a namespace row is not an owner. `impl
            // Gateway { .. }` needs a row of its own for the block's methods to hang from, and that
            // row is a `Module` named `Gateway`; a file holding one beside `use a::Gateway;`
            // holds a `Gateway` that declares nothing. Same rule as `prefer_symbols`, applied to
            // the owner rather than to a candidate: a namespace is outranked by a symbol, and a
            // name only a namespace carries here came in through an import.
            named
                .into_iter()
                .filter(|(owner, _)| names_a_symbol(in_file.as_slice(), owner))
                .collect()
        };

        if owners.is_empty() {
            return Ok(Receiver::Declined);
        }

        let mut found: Vec<Found> = Vec::new();
        for (owner, guessed) in &owners {
            let prefix = format!("{owner}.");
            for entity in in_file.iter() {
                if entity.name == name && entity.id.qualified_name().starts_with(&prefix) {
                    found.push(Found {
                        id: entity.id.clone(),
                        by: Evidence::ReceiverType {
                            receiver: receiver.to_owned(),
                        },
                        guessed: *guessed,
                    });
                }
            }
        }
        Ok(Receiver::Answered(match found.is_empty() {
            // Terminal on purpose, and the whole of the rung: falling through from here would bind
            // `service.retry()` to an unrelated free function named `retry`.
            true => Decision::Unresolved {
                reason: UnresolvedReason::NoCandidate,
            },
            false => self.decide_candidates(
                found,
                &format!(
                    "the receiver `{receiver}` was matched to a type declared in this file while \
                     ignoring letter case, which is an inference and not a proof"
                ),
            ),
        }))
    }

    /// R1 asked about a receiver: the receiver's name reached this file through an import, and the
    /// import says which file it came from.
    ///
    /// **This is the hand-off, and the only fall-through in the ladder.** It reports
    /// [`Evidence::ImportBinding`] rather than [`Evidence::ReceiverType`] because the honest
    /// description of how the target was found is the author's own `use`: the receiver's name was
    /// not read off a type declared next to the call. `peek explain` therefore reports
    /// `import_binding` for `Gateway.send()` on an imported `Gateway` and `receiver_type` for one
    /// declared in the caller's file, and a caller can tell which rung answered without reading
    /// this file.
    ///
    /// The method is searched for **inside the file that declares the type**, so the target is the
    /// method on the right type, in the package the import named. The receiver's name is only a
    /// handle onto that type: `Gateway` in the caller's file is a local spelling, and a method of
    /// some other `send` anywhere else is not what the call means.
    ///
    /// `owners` is every local name the receiver may have been written under — one, on the first
    /// pass, and however many the module matched on a re-decision. More than one is an `Ambiguous`
    /// rather than a pick, which is the same rule every other rung follows.
    ///
    /// The binding lookup is exact, with no case folding, so `Service` imported and `service`
    /// written is not placed here. That is the deliberate limit: a fold is a guess, and R2's
    /// case-folded receiver stays a guess whether it resolves locally or not.
    ///
    /// An *aliased* binding names a module rather than an item, which is how
    /// [`Resolver::targets_of`] reads one, so its owner is a file entity and nothing inside it can
    /// carry a `Type.` prefix. `use alpha::gateway::Gateway as G; G.send();` therefore stays
    /// unplaced here, exactly as it was before this rung existed.
    fn via_imported_receiver(
        &mut self,
        relation: &Relation,
        owners: &[String],
    ) -> Result<Option<Decision>, StoreError> {
        let name = relation.target_name.as_str();
        let importer = relation.source.path().clone();
        let mut found: Vec<Found> = Vec::new();
        for owner_name in owners {
            for binding in self.bindings_for(owner_name, &importer)? {
                if binding.local == GLOB {
                    continue;
                }
                let evidence = Evidence::ImportBinding {
                    module: binding.module.clone(),
                    alias: binding.alias.clone(),
                };
                let mut owners_of_binding: Vec<Found> = Vec::new();
                for id in self.targets_of(&binding, &importer)? {
                    let already = owners_of_binding.iter().any(|seen| seen.id == id);
                    if !already {
                        owners_of_binding.push(Found {
                            id,
                            by: evidence.clone(),
                            guessed: false,
                        });
                    }
                }
                // Ranked before the search rather than after it, and that ordering is the point: the
                // collision `prefer_symbols` documents is between a type and the `impl` block that
                // holds its methods, so leaving both owners in would turn one type's methods into an
                // ambiguity between two spellings of that type.
                for owner in prefer_symbols(owners_of_binding) {
                    let declared = owner.id.name();
                    let prefix = format!("{declared}.");
                    let file = owner.id.path().clone();
                    for entity in self.entities_in_file(&file)?.iter() {
                        let inside = entity.name == name;
                        if inside && entity.id.qualified_name().starts_with(&prefix) {
                            found.push(Found {
                                id: entity.id.clone(),
                                by: evidence.clone(),
                                guessed: false,
                            });
                        }
                    }
                }
            }
        }
        Ok(match found.is_empty() {
            true => None,
            false => Some(self.decide_candidates(
                found,
                "the receiver's type was reached through an import binding in this file",
            )),
        })
    }

    /// R3: a multi-segment path naming a module this file can locate.
    ///
    /// Asks the module table, through [`Resolver::module_files`], the same way R1 does and for
    /// the same reason: a fully-qualified path is the one shape the path guess cannot reach
    /// across a package boundary, because `src` is not a segment of a module path and nothing
    /// in the name mentions it. This rung used to call [`files_for_module`] itself, so the table
    /// was never asked about the paths the extractor had already written down — the table was
    /// reachable only from `use` statements, and a `other_crate::a::b::f()` call fell through
    /// to the repository-wide rungs with the answer sitting in the index.
    ///
    /// **`strip_last` is `true`, and `true` is the reason this rung fires at all.** The
    /// extractor's `Callee::path` is everything *before* the final name, so the last segment of
    /// a scope is not always a module: in `crate::payments::Service::charge()` — the worked
    /// example this file's own documentation opens the rung with — that segment is a type, the
    /// table holds no row for `payments::Service` and never will, and the reading that hits is
    /// the one with the segment dropped, which is the module `payments`: the file that declares
    /// the type and its `impl` together. `false` would confine the file set to paths every
    /// segment of which is a module, turning that whole class into `no_candidate` rather than
    /// into a wrong answer — and because [`Resolver::module_files`] hands the same flag to the
    /// fallback, it would be a regression in the behaviour that predates the table, not merely a
    /// different way of asking it.
    ///
    /// The flag governs how far along the path the search for a **file** goes. It says nothing
    /// about the name being looked up, which is `relation.target_name` and never a path
    /// segment: that name is matched with [`is_declaration`] and ranked by [`prefer_symbols`],
    /// so a submodule sharing a callee's name cannot outrank a function of that name declared
    /// in the same file. Reading it the other way round is what would resolve a call to a
    /// namespace, and `strip_last` is not where that is decided.
    fn via_scope(
        &mut self,
        relation: &Relation,
        scope: &str,
    ) -> Result<Option<Decision>, StoreError> {
        let name = relation.target_name.as_str();
        let evidence = Evidence::QualifiedNameInScope {
            scope: scope.to_owned(),
        };
        let mut found: Vec<Found> = Vec::new();
        for file in self.module_files(scope, relation.source.path(), true)? {
            for entity in self.entities_in_file(&file)?.iter() {
                if entity.name == name && is_declaration(entity.kind()) {
                    found.push(Found {
                        id: entity.id.clone(),
                        by: evidence.clone(),
                        guessed: false,
                    });
                }
            }
        }
        Ok(match found.is_empty() {
            true => None,
            false => Some(self.decide_candidates(
                found,
                "resolved through a qualified path this file can locate",
            )),
        })
    }

    /// R4: the target is declared in the same file as the reference.
    ///
    /// **Same file is not the same scope, and the difference is decided here.**
    /// [`Resolver::declared_around`] answers what the occurrence's own lexical scope
    /// declares, and the rung answers from that alone when it declares anything: a
    /// declaration in front of the use beats one in the same file it cannot see, which is
    /// the ordinary meaning of a shadowed name and needs no new rung to say so.
    ///
    /// When the scope says nothing, the answer is every declaration in the file **except a
    /// binding of an unrelated one**. A parameter belongs to the function that declares it
    /// and means nothing anywhere else, so `report.rs`'s `format_line.count` is not a
    /// candidate for the `count` read in `render` or in `describe` — and dropping it rather
    /// than ranking it lower is deliberate: with nothing else in the file carrying the
    /// name, ranking it lower would leave the rung answering with the only thing it has.
    ///
    /// A field and a method are **not** bindings and stay candidates everywhere. A field is
    /// reached through a receiver and not by a bare name in the source, and the extractor
    /// records no receiver on a `References` edge, so the rung cannot tell `entry.count`
    /// from `count` and must offer the field rather than refuse. That is a limit of what the
    /// index holds, not a claim that a field is in scope; it is stated here rather than left
    /// to be found by a reader who assumes otherwise.
    fn via_same_file(&mut self, relation: &Relation) -> Result<Option<Decision>, StoreError> {
        let name = relation.target_name.as_str();
        let visible = self.invocable_around(relation, self.declared_around(relation, name)?);
        let mut found: Vec<Found> = Vec::new();
        if visible.is_empty() {
            for entity in self.entities_in_file(relation.source.path())?.iter() {
                if entity.name == name
                    && is_declaration(entity.kind())
                    && !is_binding(entity.kind())
                {
                    found.push(Found {
                        id: entity.id.clone(),
                        by: Evidence::SameFile,
                        guessed: false,
                    });
                }
            }
        } else {
            for id in visible {
                found.push(Found {
                    id,
                    by: Evidence::SameFile,
                    guessed: false,
                });
            }
        }
        Ok(match found.is_empty() {
            true => None,
            false => Some(self.decide_candidates(found, "resolved to a definition in this file")),
        })
    }

    /// R5: exactly one entity in the repository carries the name.
    ///
    /// The only rung that leaves the source file, and the only one whose answer is never a
    /// proof: uniqueness is a fact about the index, not a statement about intent. Two or more
    /// matches is an `Ambiguous` — the case the predecessor answered with the alphabetically
    /// first file in the whole repository.
    ///
    /// # Why this rung refuses to answer from a cut-short read
    ///
    /// **A uniqueness claim is the one answer in this file that a partial view turns from right
    /// into wrong, so this is where a partial view is refused rather than counted.** The sentence
    /// this rung stores says "exactly one entity named X is indexed", and it is only true if the
    /// read saw every entity with that name. [`entities_named`] can stop at
    /// [`entities_by_name`] having seen one, at which point the rung has a single candidate and a
    /// claim about a window — and the window is precisely where the other candidates would be. The
    /// stored `basis` would name the truncated read as its own evidence: "exactly one entity named
    /// `charge` is indexed, at src/a.rs" would be a false statement about the repository, written
    /// down in the field whose whole purpose is to be auditable.
    ///
    /// So a cut-short read returns `None`. The relation falls through to the tail of
    /// [`Resolver::decide`] and comes out `Unresolved`, which is the honest state: *no target was
    /// established*, and it is visibly unestablished rather than confidently wrong. The count in
    /// [`ResolutionReport::truncated`] says why.
    ///
    /// Two things this refusal is **not**, and both matter:
    ///
    /// * It is not a refusal to answer. A *complete* read of a genuinely unique name still answers,
    ///   as `Inferred`, exactly as before. Only the cut-short case declines, so the rung cannot be
    ///   satisfied by deleting it — the test that pins this reads the same fixture twice, once with
    ///   a bound that cuts and once with a bound that does not, and requires different answers.
    /// * It is not a refusal on the *ambiguity* path. Two or more candidates found is an
    ///   `Ambiguous` whether or not the read was cut short, because "at least these two" stays true
    ///   however many more there are. Refusing there would discard a correct answer over a
    ///   possibility, which is the opposite of what a refusal is for.
    ///
    /// This is the same rule R2 applies when it names an owner and finds nothing inside it — see
    /// [`Receiver::Answered`]: a rung that cannot see enough to support its claim says so rather
    /// than answering from what it happened to see. It is also the narrow reading of "a smaller
    /// answer is not a coherent answer". A truncated *candidate list* is a smaller answer that stays
    /// coherent — every candidate found is still a candidate — so [`Resolver::cap`] truncates and
    /// counts. A truncated *uniqueness check* is not: it inverts into a positive claim about the
    /// whole repository.
    fn via_unique_name(&mut self, relation: &Relation) -> Result<Option<Decision>, StoreError> {
        let name = relation.target_name.as_str();
        // The second place the scope rule reads, and the one that decides a field read.
        //
        // **A binding outside the scope is not a weaker candidate; it is not a candidate.**
        // `Entry.count` is a field and `entry.count` is a parameter of another file's
        // function, and only the first is something a use written here can mean. Ranking
        // rather than dropping would leave two candidates and an `Ambiguous` where one
        // entity is the answer, which is the difference between a gap and an edge.
        let visible = self.invocable_around(relation, self.declared_around(relation, name)?);
        let mut found: Vec<Found> = Vec::new();
        let (entities, cut_short) = self.entities_named(name)?;
        for entity in entities {
            if !is_declaration(entity.kind()) {
                continue;
            }
            if is_binding(entity.kind()) && !visible.contains(&entity.id) {
                continue;
            }
            found.push(Found {
                id: entity.id.clone(),
                by: Evidence::UniqueName,
                guessed: false,
            });
        }
        let mut found = prefer_symbols(found);
        if found.is_empty() {
            return Ok(None);
        }
        // No sort needed: `Store::entities_named` orders by `path, kind, qualified_name,
        // entity_ordinal`, which is exactly `EntityId`'s own ordering, so the candidates arrive
        // in the order they will be written in.
        if found.len() > 1 {
            return Ok(Some(Decision::Ambiguous {
                candidates: self.cap(found.into_iter().map(|f| f.id).collect()),
            }));
        }
        // One candidate and a read that stopped at the bound is not uniqueness. See the module
        // documentation above for why this is a refusal rather than an answer with a caveat.
        if cut_short {
            return Ok(None);
        }
        let only = found.remove(0);
        // The basis names where the single candidate is, so `peek explain` can be checked against
        // the file rather than taken on trust. Built before the move, for the obvious reason.
        let basis = format!(
            "exactly one entity a use of `{name}` written here can see is indexed, at {}; a name \
             that happens to be unique among those is a claim about the index, not about the code",
            only.id
        );
        Ok(Some(Decision::Inferred {
            target: only.id,
            by: Evidence::UniqueName,
            basis,
        }))
    }

    /// Turn one rung's candidate set into a decision.
    ///
    /// Candidates are ordered by evidence strength and then by identity, so the stored list is
    /// deterministic and reproducible across runs. Within one rung the strength is equal by
    /// construction, so in practice the identity tiebreak is what decides the order — and that is
    /// exactly why the order is documented as a *presentation* order. Nothing in this file ever
    /// takes the first candidate, and the first candidate in a stored `Ambiguous` is not a
    /// recommendation; it is only the one that sorts first.
    ///
    /// `guess_note` is the basis written when the candidate was only reached through a
    /// case-folded match. A `Resolved` decision writes no basis, because `ResolutionState` has no
    /// basis field for it; the rule is readable from the evidence class alone, which is what
    /// [`rule_name`] returns.
    fn decide_candidates(&mut self, found: Vec<Found>, guess_note: &str) -> Decision {
        let mut found = prefer_symbols(found);
        // Sorted by hand rather than with `sort_by_key`, because the key is a tuple containing a
        // `Reverse` and a `String`-bearing identity, and the point of the comparison is to be
        // readable: strongest evidence first, then a stable identity order.
        found.sort_by(|a, b| {
            b.by.strength()
                .cmp(&a.by.strength())
                .then_with(|| a.id.cmp(&b.id))
        });

        // Drop duplicate identities before counting, keeping the first occurrence — which, after
        // the sort, is the one with the strongest evidence. Without this, a case-folded match
        // reports the *same* entity twice and calls it ambiguity: the edge comes back
        // `Ambiguous { [X, X] }`, which is not uncertainty but a bug that reads exactly like it.
        // An agent shown two identical candidates cannot tell a real ambiguity from a
        // double-count, so the two are indistinguishable at the point where it matters most.
        found.dedup_by(|a, b| a.id == b.id);
        if found.len() > 1 {
            return Decision::Ambiguous {
                candidates: self.cap(found.into_iter().map(|f| f.id).collect()),
            };
        }
        // `dedup_by` can empty the list, so this is not a "there is one" assumption.
        let Some(only) = found.pop() else {
            return Decision::Unresolved {
                reason: UnresolvedReason::NoCandidate,
            };
        };
        match only.guessed {
            false => Decision::Resolved {
                target: only.id,
                by: only.by,
            },
            true => Decision::Inferred {
                target: only.id,
                by: only.by,
                basis: guess_note.to_owned(),
            },
        }
    }

    /// Apply the candidate cap, recording that it was applied.
    fn cap(&mut self, mut candidates: Vec<EntityId>) -> Vec<EntityId> {
        if candidates.len() > self.options.max_candidates {
            self.truncated += 1;
            candidates.truncate(self.options.max_candidates);
        }
        candidates
    }
}

/// Whether the ladder should decide this relation at all.
///
/// Structural relations are excluded, and the reason is that re-deciding one can only lose
/// information. `Defines`, `Contains` and `Owns` are settled by grammar — the enclosing node *is*
/// the target — and the walker already wrote them as `Resolved { by: Containment }`. That is not
/// "pending, awaiting a decision"; it is a decision, and the strongest one the model has. Running
/// the ladder over one would replace `containment` with `same_file`, which is a downgrade the
/// report would then present as an improvement.
fn is_resolvable(relation: &Relation) -> bool {
    !relation.kind.is_structural()
}

/// Whether an entity kind can be the target of a name reference.
///
/// `File` is excluded because audit B21 is exactly what happens when it is not: a file whose
/// name matches a symbol wins a name lookup and produces a well-formed empty answer instead of a
/// reported ambiguity. A module is still allowed through here, because `use crate::payments;`
/// names a module and that import has to be able to find it; [`prefer_symbols`] is what stops it
/// from competing with a symbol that shares the name.
fn is_declaration(kind: EntityKind) -> bool {
    kind != EntityKind::File
}

/// Whether an entity kind is a **binding**: a name introduced by a declaration and
/// meaningless outside it.
///
/// **Three kinds, and the list is the rule.** A parameter, a local variable and a type
/// parameter are the declarations whose name is lexically local; everything else — a
/// field, a method, a constant, a free function — is written in a scope wide enough that
/// the same-file and repository-wide rungs can still offer it. See
/// [`Resolver::declared_around`] for what the distinction decides.
///
/// A property is deliberately **not** here. A property belongs to an object literal, so
/// the name is as local as a parameter's, but no fixture measures a language that
/// indexes one and adding it on reasoning alone would be a rule fitted to nothing.
fn is_binding(kind: EntityKind) -> bool {
    matches!(
        kind,
        EntityKind::Parameter | EntityKind::Variable | EntityKind::TypeParameter
    )
}

/// Whether an entity kind is a namespace rather than something a name can denote.
///
/// A namespace can be named — `use crate::payments;`, `crate::payments::Service::charge()` — but it
/// cannot be called, instantiated or used as a value, so no *bare* name denotes one. `Package` is
/// here for the same reason as `Module`: it is a boundary, and a boundary is not a declaration any
/// expression can name.
fn is_namespace(kind: EntityKind) -> bool {
    matches!(kind, EntityKind::Module | EntityKind::Package)
}

/// Drop namespace candidates from a rung's candidate set when a symbol is also a candidate.
///
/// **The rule: a namespace never wins a name lookup against a real symbol.** One function, called
/// from the two places a decision is built, because a rule stated once is a rule that cannot be
/// forgotten in one of them — R5 carries its own ambiguity rule and does not go through
/// `decide_candidates`, so it has to say so itself.
///
/// Why it is needed: the index holds entities that carry a name without declaring anything a name
/// can denote. The sharpest is a Rust `impl` block — the walker's scope stack anchors every method
/// to its enclosing scope's id, and a relation whose source row is absent is a dangling edge, so
/// `impl Gateway { .. }` has to be a row, and the row is an `EntityKind::Module` named `Gateway`
/// sitting beside `struct Gateway`. With both counted as candidates, `use alpha::gateway::Gateway;`
/// was `Ambiguous` and could not name the struct (R-012). A module table made the same shape
/// ordinary for every file stem, since every file is a module too.
///
/// Why it is a ranking and not an exclusion: a name that only a namespace carries is still a name
/// the author wrote, and dropping it would trade one false answer for a larger class of honest
/// `no_candidate`. When every candidate is a namespace, the namespaces stand and the import
/// resolves; when a symbol is also a candidate, the symbol is what the name means.
fn prefer_symbols(found: Vec<Found>) -> Vec<Found> {
    let any_symbol = found
        .iter()
        .any(|candidate| !is_namespace(candidate.id.kind()));
    if !any_symbol {
        return found;
    }
    found
        .into_iter()
        .filter(|candidate| !is_namespace(candidate.id.kind()))
        .collect()
}

/// Whether any entity in `file` that is not a namespace carries `name`.
///
/// The owner half of the rule [`prefer_symbols`] states for candidates. A namespace can be named
/// but declares nothing a bare name denotes, and the sharpest row of that kind in a Rust index is
/// the one an `impl` block needs so its methods have a scope to hang from: `impl Gateway { .. }`
/// is a `Module` named `Gateway`, sitting beside the `struct Gateway` it belongs to.
///
/// So a caller file holding `use alpha::gateway::Gateway;` *and* an `impl Gateway { .. }` has two
/// `Gateway` rows and declares the type in neither. Reading that as an owner would make the
/// receiver rung refuse a call it never looked at, which is the false negative D-0036 removed on
/// the other side of the same case.
fn names_a_symbol(file: &[crate::model::Entity], name: &str) -> bool {
    file.iter()
        .any(|entity| entity.name == name && !is_namespace(entity.kind()))
}

/// The declaration enclosing `relation.source`, if it has one.
///
/// `A.b.c` is declared inside `A.b`, so the owner of `c` is the whole prefix. A file entity has
/// no enclosing declaration, and neither does a top-level function.
fn enclosing_owner(relation: &Relation) -> Option<String> {
    if !relation.source.kind().is_member() {
        return None;
    }
    let (head, _) = relation.source.qualified_name().rsplit_once('.')?;
    match head.is_empty() {
        true => None,
        false => Some(head.to_owned()),
    }
}

/// Why a name could not be bound, given that no rung found anything for it.
///
/// A bare name matching nothing in the repository is one fact. A *qualified* name matching
/// nothing — `std::fmt::Debug`, `java.util.List`, `../payments/service` — is a different fact:
/// the thing named lives in a standard library, a third-party dependency, or a file the indexer
/// refused. Reporting both as `NoCandidate` would make the unresolved bucket a number nobody can
/// act on, and the distinction is the one `UnresolvedReason::External` exists for.
fn reason_for_nothing_found(target_name: &str) -> UnresolvedReason {
    let qualified = target_name.contains("::")
        || target_name.contains('/')
        || (target_name.contains('.') && !target_name.starts_with('.'));
    match qualified {
        true => UnresolvedReason::External,
        false => UnresolvedReason::NoCandidate,
    }
}

/// The files a module path could name, as seen from `importer`.
///
/// Three details of this are compromises and are stated rather than hidden:
///
/// * **There is no module table.** Peek has `EntityKind::Module` and no way to populate it, so a
///   module path is turned into file paths and each is looked up through the primary key. This
///   function is the reason that gap matters, and it is also the argument for closing it: a
///   `Module` entity per module would replace a handful of seeks with one, and it belongs in the
///   extractor rather than here — a resolver that invented module rows to speed up its own
///   lookups would be a second source of truth (contract H5).
/// * **The anchor list is the referring file's directory and its ancestors.** `crate::` and a
///   bare path use the same list, because the crate root is not recorded anywhere; `super::`
///   skips one anchor per occurrence. The list stops at [`MAX_ANCHOR_DEPTH`] so a deep
///   repository cannot make one import cost an unbounded number of seeks.
/// * **Two readings.** With `strip_last`, the whole path may be a module, or the last segment
///   may be an item inside the module named by the rest. Both are real, both are tried, and if
///   both files exist the caller sees an `Ambiguous` rather than a pick.
///
/// Directory-style module files (`mod.rs`) are recognised only for Rust, where the resolver can
/// see the importer's extension. Every other language's layout is a follow-up: it needs a
/// per-language module convention, which is data and belongs in a `LanguageSpec` rather than in
/// a path test here.
fn files_for_module(module: &str, importer: &RepoPath, strip_last: bool) -> Vec<RepoPath> {
    let mut segments: Vec<String> = Vec::new();
    let mut climb = 0usize;
    for part in module.split("::") {
        match part {
            "" | "crate" | "self" => continue,
            "super" => climb += 1,
            other => segments.push(other.to_owned()),
        }
    }
    if segments.is_empty() || segments.len() > MAX_MODULE_SEGMENTS {
        return Vec::new();
    }

    let extension = importer.extension();
    let mut readings: Vec<Vec<String>> = vec![segments.clone()];
    if strip_last && segments.len() > 1 {
        readings.push(segments[..segments.len() - 1].to_vec());
    }

    let anchors = anchor_directories(importer);
    let mut files: Vec<RepoPath> = Vec::new();
    for reading in &readings {
        let joined = reading.join("/");
        for (depth, anchor) in anchors.iter().enumerate() {
            if depth < climb {
                continue;
            }
            let stem = match anchor.is_empty() {
                true => joined.clone(),
                false => format!("{anchor}/{joined}"),
            };
            if let Some(extension) = &extension
                && let Some(path) = RepoPath::new(format!("{stem}.{extension}"))
            {
                push_new(&mut files, path);
            }
            if extension.as_deref() == Some("rs")
                && let Some(path) = RepoPath::new(format!("{stem}/mod.rs"))
            {
                push_new(&mut files, path);
            }
        }
    }
    files
}

/// Add a candidate file if it has not been produced already.
fn push_new(files: &mut Vec<RepoPath>, path: RepoPath) {
    if !files.contains(&path) {
        files.push(path);
    }
}

/// The directories a module path is anchored at: the importer's own directory, then each
/// directory above it, up to the repository root.
fn anchor_directories(importer: &RepoPath) -> Vec<String> {
    let mut anchors = Vec::new();
    let mut current = importer
        .parent()
        .map_or_else(String::new, |parent| parent.as_str().to_owned());
    for _ in 0..=MAX_ANCHOR_DEPTH {
        anchors.push(current.clone());
        if current.is_empty() {
            break;
        }
        current = match current.rsplit_once('/') {
            Some((head, _)) => head.to_owned(),
            None => String::new(),
        };
    }
    anchors
}

/// A de-duplicating set of relations, in the order the store returned them.
///
/// The same relation is reachable as an outgoing edge of a changed file, as an incoming edge of a
/// changed entity, and as an edge a refresh displaced, so a pass over a scope collects the union
/// and has to de-duplicate it. Deciding the same relation twice would write it twice and count it
/// twice in the report.
///
/// De-duplication is by [`Relation::natural_key`], which is the store's own `UNIQUE` constraint
/// rather than a second opinion about what identity means.
#[derive(Debug, Default)]
struct RelationSet {
    seen: BTreeSet<RelationKey>,
    relations: Vec<Relation>,
}

impl RelationSet {
    fn new() -> Self {
        Self::default()
    }

    /// Add a relation unless this pass has already collected the same one.
    fn insert(&mut self, relation: Relation) {
        if self.seen.insert(relation.natural_key()) {
            self.relations.push(relation);
        }
    }

    fn into_vec(self) -> Vec<Relation> {
        self.relations
    }
}

/// Write one decision back over the relation the store already holds.
///
/// Returns `None` when the decision is identical to what is already stored, which is what makes
/// a second pass over an unchanged index a genuine no-op: no row is rewritten, the batch stays
/// empty, no commit happens, and the generation does not move.
/// Turn a decision into the relation that should replace `relation`, or `None` if nothing changed.
///
/// `force` exists for the one case where "nothing changed" is the wrong conclusion. A displaced
/// edge is re-decided from a snapshot taken *before* the refresh rewrote its row, so comparing the
/// new decision against that snapshot compares against a state the store no longer holds. The
/// decision can be correct and the row still be wrong, and skipping the write on "no change" then
/// leaves a permanently unresolved edge. For those, the write is unconditional: an extra upsert is
/// cheap, a silently broken edge is not.
fn apply_decision(relation: &Relation, decision: Decision, force: bool) -> Option<Relation> {
    let (target, resolution) = match decision {
        Decision::Resolved { target, by } => (Some(target), ResolutionState::Resolved { by }),
        Decision::Inferred { target, by, basis } => {
            (Some(target), ResolutionState::Inferred { by, basis })
        }
        Decision::Ambiguous { candidates } => (None, ResolutionState::Ambiguous { candidates }),
        Decision::Unresolved { reason } => (None, ResolutionState::Unresolved { reason }),
    };
    if !force && target.as_ref() == relation.target.as_ref() && resolution == relation.resolution {
        return None;
    }
    Some(Relation {
        kind: relation.kind,
        source: relation.source.clone(),
        target_name: relation.target_name.clone(),
        target,
        span: relation.span,
        resolution,
    })
}

/// Decide every `Pending` relation in the index, in one pass and one commit.
///
/// This is the second half of a full build: the extractor has committed, and this turns what it
/// wrote into decisions. The whole pending set is read in one query because the store offers no
/// cursor to page it — and because the indexer already materialises the entire relation set in
/// memory in order to write it, so this is the same order of footprint rather than a new one.
///
/// `reconsider_decided` is **ignored** here, and that is deliberate rather than an oversight. A
/// full build has just re-extracted every file, so every relation in the index was re-emitted as
/// `Pending` and nothing is left to reconsider; a refresh is the case that needs it, and that is
/// [`resolve_paths`]. Re-deciding decided edges on a full build would double the work for no
/// gain and would re-write every `Contains` edge the extractor had already settled.
pub fn resolve_all(
    store: &mut Store,
    options: ResolutionOptions,
) -> Result<ResolutionReport, StoreError> {
    let mut in_scope = RelationSet::new();
    for relation in store.relations_in_state(&pending_state(), usize::MAX)? {
        if is_resolvable(&relation) {
            in_scope.insert(relation);
        }
    }
    // A full build has just re-extracted every file, so no relation here was displaced by a
    // partial write — every row in the store matches its snapshot, and there is nothing to force.
    decide_and_commit(store, in_scope.into_vec(), options, &BTreeSet::new())
}

/// Decide the relations belonging to `paths`, the relations that point into them, and the
/// relations the caller displaced.
///
/// Scoped, not global, because a refresh knows which files changed and nothing else has.
///
/// `displaced` is the awkward part and it is not optional. A refresh *removes* the changed
/// files' rows before re-inserting them, and the store's demotion step turns every edge that
/// pointed into a removed entity into an `Unresolved` with a null target. By the time the
/// resolver runs, those edges are invisible to [`Store::incoming`], which matches on
/// `target_path`. So the caller has to read them **before** the write and hand them over here,
/// or a moved definition silently orphans every one of its callers.
///
/// The ordering — read the edges that are about to be broken, write, then re-decide — is the
/// whole of contract G9, and it is why this function takes three arguments rather than one.
///
/// # Nothing in the paths it is given is left out
///
/// A file is read through two doors: its own outgoing edges, and the edges arriving at the
/// entities it declares. Both doors open through the file's **entity list**, so a bounded read of
/// that list closed both of them at once. On `BurntSushi/ripgrep`, refreshing
/// `crates/core/flags/defs.rs` — 1,363 entities — decided 2,121 of the file's 3,599 relations and
/// left 348 `Pending`: extracted, never decided, never refused. The lowest-ranked pending source
/// was #514, one row past the old bound of 512. A second scoped refresh of the same file changed
/// nothing, because the bound cut the same tail off again and nothing else in the engine revisits
/// it.
///
/// So the entity list is **paged, not bounded**: `all_entities_in_file` asks for a page at a
/// time until one comes back short, and the number of entities a file has no longer decides how
/// much of it this pass sees. `entities_page` chooses the page size and nothing else. A caller
/// passing [`ResolutionOptions::default`] gets the whole of every file, which is what a default
/// that is not mentioned in a diagnostic is supposed to mean.
///
/// The two *edge* reads are still bounded, by `outgoing_per_source` and `incoming_per_entity`, and
/// a cut-short read there is now counted in [`ResolutionReport::truncated`] rather than passed over
/// in silence — the same defect with a smaller population, and the count is what makes it
/// checkable from outside. Neither is paged: both are ordered on a nullable column, so a keyset
/// cursor over them is not sound, and they are left as the honest cut-offs they are.
pub fn resolve_paths(
    store: &mut Store,
    paths: &[RepoPath],
    displaced: &[Relation],
    options: ResolutionOptions,
) -> Result<ResolutionReport, StoreError> {
    let mut in_scope = RelationSet::new();
    let mut displaced_keys: BTreeSet<RelationKey> = BTreeSet::new();
    for relation in displaced {
        if is_resolvable(relation) {
            in_scope.insert(relation.clone());
            displaced_keys.insert(relation.natural_key());
        }
    }

    // Lookups the scope enumeration abandons, counted here and added to the report below. They
    // cannot be counted by the resolver, because it does not exist yet: it is built inside
    // `decide_and_commit`, after this loop has run. So this is a second counter rather than one
    // threaded through two phases — a lookup this pass abandoned and did not count is the exact
    // shape of the defect this function exists to prevent, and that is not worth saving a field.
    let mut cut_short: u64 = 0;
    for path in paths {
        // A file is read through two doors: its own outgoing edges, and the edges arriving at the
        // entities it declares. The second is the "a definition moved" half.
        let entities = all_entities_in_file(store, path, options.entities_page)?;
        for entity in &entities {
            let outgoing_limit = options.outgoing_per_source;
            let outgoing = store.outgoing(&entity.id, None, outgoing_limit)?;
            if outgoing.len() >= outgoing_limit {
                cut_short += 1;
            }
            for relation in outgoing {
                if relation.resolution.is_pending() {
                    in_scope.insert(relation);
                }
            }
            if options.reconsider_decided {
                let incoming_limit = options.incoming_per_entity;
                let incoming = store.incoming(&entity.id, None, incoming_limit)?;
                if incoming.len() >= incoming_limit {
                    cut_short += 1;
                }
                for relation in incoming {
                    if is_resolvable(&relation) {
                        in_scope.insert(relation);
                    }
                }
            }
        }
    }

    let mut report = decide_and_commit(store, in_scope.into_vec(), options, &displaced_keys)?;
    report.truncated += cut_short;
    // The displaced edges are counted separately because they are a different population: they
    // are the ones a refresh broke, and a caller repairing a rename needs to know how many it
    // repaired without inferring it from `examined`.
    report.displaced = u64::try_from(displaced_keys.len()).unwrap_or(u64::MAX);
    Ok(report)
}

/// Every entity one file declares, read a page at a time until a page comes back short.
///
/// A free function rather than a method because the two callers that need it are on either side of
/// a resolver: [`resolve_paths`] enumerates a file's entities before one exists, and
/// [`Resolver::entities_in_file`] does it during a decision. One implementation, so the loop that
/// makes "the whole file" true cannot be correct in one place and bounded in the other.
fn all_entities_in_file(
    store: &Store,
    path: &RepoPath,
    page: usize,
) -> Result<Vec<crate::model::Entity>, StoreError> {
    // Zero would ask for a page that holds nothing, which is short, which reads as "that was the
    // last page", which reports every file in the repository as empty. One is the smallest page
    // that cannot lie that way.
    let page = page.max(1);
    let mut found: Vec<crate::model::Entity> = Vec::new();
    let mut cursor: Option<EntityId> = None;
    loop {
        let batch = store.entities_in_file_after(path, cursor.as_ref(), page)?;
        let read = batch.len();
        // The cursor comes from the rows just read, so it is an entity of `path` by construction —
        // which is what `entities_in_file_after` requires — and the walk is strictly forward: the
        // next bound excludes the row the cursor names, so no row repeats and no page loops.
        cursor = batch.last().map(|entity| entity.id.clone());
        found.extend(batch);
        if read < page {
            return Ok(found);
        }
    }
}

/// The state used to ask the store for everything still awaiting a decision.
///
/// Only the tag is read by the query, so the payload is a placeholder. Building it as a real
/// `Pending` rather than a bare string means this call site cannot drift from the model's own
/// spelling of the state.
fn pending_state() -> ResolutionState {
    ResolutionState::Pending {
        evidence: Evidence::NameOnly,
        basis: String::new(),
    }
}

/// Decide `relations` and write every decision in one transaction.
fn decide_and_commit(
    store: &mut Store,
    relations: Vec<Relation>,
    options: ResolutionOptions,
    force_write: &BTreeSet<RelationKey>,
) -> Result<ResolutionReport, StoreError> {
    let mut report = ResolutionReport {
        examined: relations.len() as u64,
        generation: store.generation(),
        ..ResolutionReport::default()
    };
    let mut update = IndexUpdate::empty();

    {
        let mut resolver = Resolver::new(store, options);
        for relation in &relations {
            if !relation.resolution.is_pending() {
                report.reconsidered += 1;
            }
            // A displaced edge is one this pass's own caller has *just rewritten* in the store, by
            // removing and re-inserting the file its target lived in. Its snapshot in `relations`
            // is therefore stale by construction, and comparing a fresh decision against a stale
            // baseline is how a repair silently does nothing: the decision comes out identical to
            // the snapshot, `apply_decision` reports "no change", and the row stays demoted. So for
            // a displaced edge the write is unconditional. Writing an unchanged row costs one
            // upsert; not writing it costs a permanently unresolved edge.
            let forced = force_write.contains(&relation.natural_key());
            let decision = resolver.decide(relation)?;
            report.record(&decision);
            if let Some(mut decided) = apply_decision(relation, decision, forced) {
                // A forced write re-uses the snapshot's target, and the snapshot was taken before
                // the refresh removed the file that target lived in. Writing it back is a foreign
                // key violation — and the failure mode is the whole transaction aborting, so one
                // edge pointing at a deleted file would undo the deletion that produced it.
                //
                // So a forced decision's target is *checked* rather than trusted. A target that is
                // no longer in the index is not a resolution, it is a reference to something that
                // has left, and that is `Unresolved` with a reason.
                if forced {
                    // Both ends. The *source* is the one that bites: a file that declared a
                    // symbol usually also made calls, and those relations were read into the
                    // snapshot before the deletion removed them. Re-inserting one is a foreign
                    // key violation on `source_path`, and because the write is one transaction the
                    // violation aborts the deletion that caused it — a file cannot be removed
                    // because an edge out of it was repaired.
                    if !store.entity(&decided.source)?.is_some() {
                        continue;
                    }
                    if let Some(target) = decided.target.clone()
                        && !store.entity(&target)?.is_some()
                    {
                        decided.target = None;
                        decided.resolution = ResolutionState::Unresolved {
                            reason: UnresolvedReason::NoCandidate,
                        };
                    }
                }
                update = update.with_relation(decided);
            }
        }
        // Read out of the scope that owns the resolver, so the counter cannot be forgotten at
        // the point of construction and silently report zero.
        report.truncated = resolver.truncated;
        report.module_files = resolver.module_files;
    }

    if !update.is_empty() {
        let stats = store.apply_update(update)?;
        report.committed = true;
        report.relations_written = stats.relations_upserted;
        report.generation = stats.generation;
    }
    // Measured, not assumed. The pass decided everything it looked at, so the only question
    // left is whether anything was still pending when it looked.
    report.pending_remaining = !store.relations_in_state(&pending_state(), 1)?.is_empty();
    Ok(report)
}
