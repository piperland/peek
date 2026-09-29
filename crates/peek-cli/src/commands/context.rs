//! `peek context`: the product.
//!
//! # The budget is the contract, and this command does not soften it
//!
//! Four things are printed on every run, whatever the outcome:
//!
//! 1. **The budget it was given.** Not the budget it settled for.
//! 2. **What it spent**, including the report, because the report is part of the answer.
//! 3. **What it dropped**, with the reason and the cost each omission would have taken.
//! 4. **The counting rule**, `ceil(utf8 bytes / 3)`, so the arithmetic can be reproduced rather
//!    than believed.
//!
//! And two refusals rather than a silent overrun:
//!
//! * **No `--budget` is a refusal.** The engine will not invent one either: it requires a budget
//!   and refuses below the cost of its own report. A default here would be a number nobody wrote
//!   down, used to size an answer, which is the one thing this project has decided not to do. The
//!   refusal carries the minimum, so one round trip is enough to fix it.
//! * **A budget too small for the report is a refusal**, with `minimum_tokens` set, and it exits 3.
//! * **A budget too small for the target is a refusal too**, even though the engine answers it
//!   with an empty pack and `Ok`. The engine is right that the target resolved and the graph was
//!   read; the *caller* asked for context and got none, and exiting 0 there would convert "you get
//!   nothing" into "it worked". The answer is still full: the pack, the omissions, the cost of the
//!   target, and a floor for a budget that could have held it.
//!
//! # What is under a budget that fills completely
//!
//! `BudgetStatus::Complete` means *the neighbourhood was exhausted*, not *the repository is fully
//! described*: it is depth 1 by construction. And if `candidates_unexamined` is non-zero then some
//! enumeration stopped at a bound, so even `complete` is complete only with respect to what was
//! examined. Both are stated in the `could not:` block, because a reader who does not know the
//! difference will read a short answer as a complete one.

use peek_core::query::{BudgetStatus, ContextPack, Query};

use crate::answer::{Answer, ContextAnswer, floor_for_target};
use crate::commands::Outcome;
use crate::commands::query::{open_for_query, refusal_from_query};
use crate::exit::{Failure, Refusal, Status, kind};

/// Compile a token-budgeted slice of the repository.
pub fn run(
    location: Option<&crate::paths::Location>,
    command: &'static str,
    text: &str,
    budget: Option<u64>,
) -> Result<Outcome, Failure> {
    let store = open_for_query(location, command)?;
    let query = Query::new(&store);
    let minimum = query.minimum_budget();

    let Some(requested) = budget else {
        return Err(Failure::refused(
            command,
            Refusal::new(
                kind::NO_BUDGET,
                "no --budget was given. This build will not invent one: a budget is a promise about \
                 what the answer costs, and a number nobody wrote down is not one. Pass --budget N",
            )
            .with_minimum(minimum),
        ));
    };
    if requested < minimum {
        return Err(Failure::refused(
            command,
            Refusal::new(
                kind::BUDGET_TOO_SMALL,
                format!(
                    "a budget of {requested} token(s) cannot hold the {minimum}-token report that \
                     would state what was left out. The report is charged against the budget \
                     because a reader who does not know what was dropped has been misled"
                ),
            )
            .with_minimum(minimum),
        ));
    }

    let pack: ContextPack = match query.peek(text, requested) {
        Ok(pack) => pack,
        Err(error) => return Err(refusal_from_query(command, error)),
    };

    let status = pack.budget.status;
    let floor = floor_for_target(&pack);
    let mut notes: Vec<String> = Vec::new();
    let mut did_not: Vec<String> = vec![
        "a unit is the index's own rendering of a declaration — kind, name, path, line, signature \
         and doc comment — and not its body. Nothing here shows what is inside a function"
            .to_owned(),
        "the pack is one hop from the target by construction. A larger budget does not reach \
         further; it buys more of the same neighbourhood"
            .to_owned(),
        format!(
            "the counting rule is {}, an estimate and not a tokenizer. The byte count beside every \
             cost is what a real tokenizer would be applied to",
            pack.budget.counter.rule()
        ),
        "the target's own file is not a unit unless the target is that file; ask for the file to \
         get its declarations"
            .to_owned(),
    ];

    match status {
        BudgetStatus::Complete => notes.push(format!(
            "the answer is complete: the neighbourhood was exhausted with {} token(s) of the \
             budget unspent, so the budget was not what limited it",
            pack.budget.remaining_tokens
        )),
        BudgetStatus::Reduced => notes.push(
            "the answer is reduced, not truncated: a declaration is included whole or not at all, \
             and the fill stopped at the first that did not fit rather than skipping to a smaller \
             one. This is therefore a prefix of the full ranking"
                .to_owned(),
        ),
        BudgetStatus::Insufficient => notes.push(
            "nothing was included. The target resolved and its neighbourhood was read; what was \
             missing is room, and saying so is more useful than an error string"
                .to_owned(),
        ),
    }
    if pack.budget.candidates_unexamined > 0 {
        did_not.push(format!(
            "the neighbourhood was not fully examined: {} enumeration(s) stopped at a bound, and \
             each one hides at least one candidate. A pack reporting `complete` is complete only \
             with respect to what was examined",
            pack.budget.candidates_unexamined
        ));
    }
    if pack.budget.remaining_tokens == 0 && status != BudgetStatus::Insufficient {
        did_not.push(
            "the budget was spent to the token. A pack that fills exactly is a coincidence of the \
             arithmetic, not evidence that the rule is tight"
                .to_owned(),
        );
    }

    // One status, decided once, from the pack's own value rather than from the branch above, so
    // the exit code and the flag on the answer cannot disagree.
    let status = status_of(&pack);
    let refused = status == Status::Refused;
    let answer = ContextAnswer {
        pack,
        minimum_budget: minimum,
        minimum_for_target: floor,
        budget_was_chosen: false,
        refused,
        notes,
        did_not,
    };

    if refused {
        let refusal = Refusal::new(
            kind::BUDGET_INSUFFICIENT,
            match floor {
                Some(_) => format!(
                    "the target itself does not fit a budget of {minimum_min} token(s) alongside \
                     the {report}-token report",
                    minimum_min = answer.pack.budget.requested_tokens,
                    report = minimum
                ),
                None => format!(
                    "the target itself does not fit a budget of {} token(s)",
                    answer.pack.budget.requested_tokens
                ),
            },
        );
        let refusal = match floor {
            Some(floor) => refusal.with_minimum(floor),
            None => refusal,
        };
        return Ok(Outcome::limited(
            Status::Refused,
            refusal,
            Answer::Context(answer),
        ));
    }

    Ok(Outcome::ok(Answer::Context(answer)))
}

/// The status a pack implies for the process, as a function of the value rather than of a
/// re-derivation of it, so a test can assert one against the other.
///
/// **`Insufficient` is a refusal, not a success.** The engine returns it with an empty pack and
/// `Ok`, and is right to: the target resolved and the graph around it was read, so nothing failed.
/// But the caller asked for context and received none, and exiting 0 there would convert "you get
/// nothing" into "it worked". D-0009 is a refusal rather than a truncated answer, and that
/// includes the exit code.
#[must_use]
pub fn status_of(pack: &ContextPack) -> Status {
    match pack.budget.status {
        BudgetStatus::Insufficient => Status::Refused,
        BudgetStatus::Complete | BudgetStatus::Reduced => Status::Ok,
    }
}
