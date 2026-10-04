//! The per-language conformance gate: numbers, and the matrix generated from them.
//!
//! # `expect` and `panic` are allowed here, and why
//!
//! A failure inside this file means the measurement is *wrong* — a ground-truth
//! line that does not parse, a fixture that is not there, a floor that is missing —
//! and printing a wrong measurement is worse than stopping. Swallowing the error
//! and continuing would produce a table of numbers that reads as a finding about
//! the engine and is a finding about the gate. `real_repository.rs` makes the same
//! argument for the same exemption, and the crate-level `cfg_attr(test, allow(..))`
//! that covers the unit tests does not reach an integration test crate.
//!
//! # What a run asserts
//!
//! - every registered language meets each dimension's recorded floor, and the
//!   denominator of every figure is a population somebody wrote down;
//! - every language the enum advertises has a row in the matrix, and every row
//!   without measurements says why;
//! - the committed `LANGUAGE_MATRIX.md` and `docs/language-matrix.json` are the
//!   current measurement.
//!
//! # What a run reports
//!
//! Everything else. Which relations ended ambiguous, which imports carry no module
//! because their resolution state holds no evidence, which labelled symbols the
//! engine did not produce. Those are the findings; the pass/fail is the floor.

#![allow(clippy::expect_used, clippy::panic)]

#[path = "gate/mod.rs"]
mod gate;
