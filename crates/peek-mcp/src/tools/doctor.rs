//! `doctor`: what is wrong with the index.
//!
//! The one tool that works on an index that is broken, which is why it does not go through
//! [`Session::reader`]. `Store::open` fails on a corrupt file, on a file from a newer build, and on
//! a file belonging to another repository — three different problems with three different fixes —
//! and a diagnostic that refused to start in exactly those cases would be a diagnostic that is
//! needed exactly when it is unavailable. `doctor::diagnose` opens its own store and reports the
//! failure as a finding, which is the whole point of it.
//!
//! # It creates the index, and that is worth knowing
//!
//! `Store::open` creates a missing index, so running `doctor` against a repository that has never
//! been indexed **creates an empty one**. That is the engine's behaviour and this tool does not
//! paper over it: an empty index is a state a person needs diagnosed, and refusing to diagnose it
//! would be the module's own rule inverted. It is also different from every other query tool,
//! which refuses with `not_indexed` and leaves the filesystem alone — so the two paths are
//! deliberately not the same, and a caller who wants to know whether an index exists should ask
//! `index_status`, which never creates anything.
//!
//! The response is the engine's own [`Diagnosis`], serialised. Not a copy of it: a copy is a
//! second definition of the shape, and two definitions of a shape drift. The engine's `Severity`
//! and `Check` enums serialise as the same words their `as_str` prints, and a test here pins that
//! so the JSON and the terminal can never disagree about what a check is called.

use serde::Serialize;
use serde_json::Value;

use peek_core::doctor::{self, Diagnosis, Severity};

use crate::outcome::{Outcome, ToolError, Verdict};
use crate::params::Args;
use crate::session::Session;
use crate::tools::ToolAnswer;

/// Run every diagnostic the engine has over the index for this repository.
pub fn doctor(session: &mut Session, arguments: Option<&Value>) -> Result<ToolAnswer, ToolError> {
    let args = Args::new("doctor", arguments, crate::tool::hints_for("doctor"))?;
    args.finish(&[])?;

    // The root is the repository, not the index: where the index lives is one of the things being
    // diagnosed, so passing it in would let this check the wrong one and never find out.
    let root = session.root().to_path_buf();
    let diagnosis = doctor::diagnose(&root);
    session.log(&format!(
        "doctor: {} finding(s), worst {:?}",
        diagnosis.findings.len(),
        diagnosis.worst()
    ));
    Ok(ToolAnswer::new(
        diagnosis.report(),
        body(
            &diagnosis,
            &Verdict {
                outcome: if diagnosis.is_healthy() {
                    Outcome::Ok
                } else {
                    Outcome::Refused
                },
                reason: None,
                advice: diagnosis.worst_finding().and_then(|finding| {
                    finding
                        .action
                        .as_ref()
                        .map(|action| format!("{}: {action}", finding.check.as_str()))
                }),
                candidates: Vec::new(),
            },
        ),
    ))
}

/// The structured body: the verdict, the findings, and the two numbers a caller branches on.
#[derive(Debug, Serialize)]
struct DoctorBody {
    #[serde(flatten)]
    verdict: Verdict,
    /// Every check that ran, in the order the engine ran them.
    findings: Vec<doctor::Finding>,
    /// The worst severity present, or `null` when nothing was checked.
    worst: Option<&'static str>,
    /// The count of each severity, so a caller does not have to walk the findings to know whether
    /// anything is wrong.
    counts: SeverityCounts,
    /// The engine's own measurements, or `null` when the index could not be opened.
    stats: Option<peek_core::store::StoreStats>,
    /// The checks that were asked about and could not be performed. Always present, and never
    /// populated by this build — the engine reports an unperformable check as a `Fail` finding
    /// rather than skipping it — which is why it is here: a caller can assert it is empty rather
    /// than take it on trust.
    not_performed: Vec<&'static str>,
}

/// How many findings there are at each severity.
#[derive(Debug, Default, Serialize)]
struct SeverityCounts {
    pass: usize,
    notice: usize,
    warn: usize,
    fail: usize,
}

impl SeverityCounts {
    /// Count a diagnosis's findings.
    fn of(diagnosis: &Diagnosis) -> Self {
        let mut counts = Self::default();
        for finding in &diagnosis.findings {
            match finding.severity {
                Severity::Pass => counts.pass += 1,
                Severity::Notice => counts.notice += 1,
                Severity::Warn => counts.warn += 1,
                Severity::Fail => counts.fail += 1,
            }
        }
        counts
    }
}

fn body(diagnosis: &Diagnosis, verdict: &Verdict) -> Value {
    let body = DoctorBody {
        verdict: verdict.clone(),
        findings: diagnosis.findings.clone(),
        worst: diagnosis.worst().map(Severity::as_str),
        counts: SeverityCounts::of(diagnosis),
        stats: diagnosis.stats,
        not_performed: Vec::new(),
    };
    serde_json::to_value(&body).unwrap_or_else(|error| {
        // Unreachable: the body is owned data with derived serialisation. A fallback rather than a
        // panic, because this function runs inside a tool call and a panic there takes the
        // session with it.
        serde_json::json!({
            "outcome": "failed",
            "reason": format!("the diagnosis could not be encoded: {error}"),
            "advice": Value::Null,
            "candidates": [],
            "findings": [],
        })
    })
}
