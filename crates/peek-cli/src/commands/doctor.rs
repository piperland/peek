//! `peek doctor`: format the diagnosis.
//!
//! # What this does not do
//!
//! It does not reimplement a check. Every finding comes from
//! [`peek_core::doctor::diagnose`], which is a function from a store to a list of findings and
//! which the engine's own tests cover one way at a time. This module flattens that list into a
//! serialisable value and prints it; a formatter that grew a rule of its own would be a second
//! source of truth about whether an index is healthy, which is precisely what audit D found the
//! predecessor doing with its `health()` function.
//!
//! # The one judgement this command does make
//!
//! **`Store::open` creates a store when the file is absent**, so diagnosing a repository that was
//! never indexed describes an index this command just made. Left alone, `doctor` on a fresh
//! checkout would print a report whose worst finding is `notice` and exit zero — a green answer
//! about an install that does not exist, which is the failure mode the whole `doctor` module
//! documents. So the existence check happens first and the status becomes `refused`, while the
//! full report is still printed: the user learns the state of the empty index *and* that there was
//! no index. The finding severities are the engine's; only the process exit code is added here,
//! and the answer carries `index_existed` so a JSON consumer can see the difference.

use crate::answer::{Answer, Counts, DoctorAnswer, DoctorFinding, severity_rank};
use crate::commands::{Outcome, need};
use crate::exit::{Failure, Refusal, Status, kind};
use crate::paths::Location;
use peek_core::doctor::{Diagnosis, Severity};

/// Diagnose the index, and fail loudly when the diagnosis does.
pub fn run(location: Option<&Location>, command: &'static str) -> Result<Outcome, Failure> {
    let location = need(location, command)?;
    let existed = location.index_existed;
    let diagnosis = peek_core::doctor::diagnose(&location.root);

    let mut findings: Vec<DoctorFinding> =
        diagnosis.findings.iter().map(DoctorFinding::from).collect();
    // Worst first, stable within a severity, so two runs over an unchanged index print the same
    // report. `sort_by_key` is stable; the input order is the order the checks ran, which is the
    // order the engine's own report preserves within a severity. The ranking table is the one the
    // renderer uses, so the order printed and the order serialised cannot come from two of them.
    findings.sort_by_key(|finding| std::cmp::Reverse(severity_rank(&finding.severity)));

    let mut did_not = vec![
        "this compared nothing against the working tree: it reports what the store holds and what \
         a filesystem walk can read, not which files have changed since the last commit"
            .to_owned(),
        "this reported files the walk refused as counts per reason and named none of them; \
         `peek index` lists every skipped path with its reason"
            .to_owned(),
        "this did not run the resolver, so a relation still marked pending is a fact about this \
         index and not about the code"
            .to_owned(),
    ];
    if !existed {
        did_not.push(format!(
            "there was no index at {} before this command ran, and opening one creates it: this \
             report describes an index this command made, so the only check that could say \
             anything about a missing one is `index_openable`, which passed by creating the file",
            location.index_text()
        ));
    }

    let answer = DoctorAnswer {
        index_existed: existed,
        healthy: diagnosis.is_healthy(),
        worst: diagnosis.worst().map(Severity::as_str).map(str::to_owned),
        findings,
        counts: diagnosis.stats.as_ref().map(Counts::from_stats),
        did_not,
    };

    let answer = Answer::Doctor(answer);
    match diagnosis.worst() {
        // The contract's requirement, and the reason this command has its own exit code: an agent
        // has to be able to tell "the index is broken" from "the diagnosis itself failed".
        Some(Severity::Fail) => Ok(Outcome::limited(
            Status::Unhealthy,
            Refusal::new(kind::UNHEALTHY, describe_worst(&diagnosis)),
            answer,
        )),
        _ if !existed => Ok(Outcome::limited(
            Status::Refused,
            Refusal::new(
                kind::NO_INDEX,
                format!(
                    "there is no index for this repository: {} does not exist. The report below \
                     describes the empty index this command created. `peek index` builds a real one",
                    location.index_text()
                ),
            ),
            answer,
        )),
        _ => Ok(Outcome::ok(answer)),
    }
}

/// The one line a reader takes away: the worst finding, in its own words.
fn describe_worst(diagnosis: &Diagnosis) -> String {
    match diagnosis.worst_finding() {
        Some(finding) => format!(
            "peek doctor found a failing check: {} - {}",
            finding.summary, finding.detail
        ),
        None => "peek doctor found a failing check but could not name it".to_owned(),
    }
}
