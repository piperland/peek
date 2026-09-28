//! Generated, vendored, and test code detection.
//!
//! # Why this is a documented rule and not a heuristic
//!
//! [`Classification`] is product-visible. It is what `peek overview` tells an agent to ignore, and
//! it decides whether a file's entities enter the graph at all. A hidden heuristic is therefore
//! unacceptable: an operator whose file is silently misclassified has no way to find out why, and
//! an agent told to ignore a hand-written file is actively misled.
//!
//! So the rules are:
//!
//! 1. **Named, ordered, and finite.** [`Classification::of`] is a pure function of a
//!    [`RepoPath`] and the file's leading bytes. First match wins. There is no scoring, no
//!    tie-breaking, and no ordering dependence on directory iteration.
//! 2. **Exposed as data.** Every pattern list is a public `const`, so `peek doctor` can print the
//!    exact table it applied and a disagreement can be diagnosed without reading this source.
//! 3. **Testable rule by rule.** Each rule has at least one test that asserts a specific value,
//!    and the negative cases are tested too: `vendor_notes.md` must not be vendored, and
//!    `testimony.rs` must not be a test.
//!
//! # The rule table
//!
//! Applied top to bottom; the first row that matches wins.
//!
//! | # | Rule | Example | Verdict |
//! |---|---|---|---|
//! | 1 | A **directory** component is in [`Classification::VENDORED_DIRECTORIES`] | `vendor/lib/x.rs` | `Vendored` |
//! | 2 | A **directory** component is in [`Classification::GENERATED_DIRECTORIES`] | `src/gen/api.rs` | `Generated` |
//! | 3 | The file's first [`Classification::HEADER_SCAN_BYTES`] contain a marker from [`Classification::GENERATED_MARKERS`] | `// @generated` | `Generated` |
//! | 4 | The file name contains a marker from [`Classification::GENERATED_NAME_MARKERS`] | `schema_pb2.py` | `Generated` |
//! | 5 | A **directory** component is in [`Classification::TEST_DIRECTORIES`] | `tests/api_test.py` | `Test` |
//! | 6 | The file name matches a test prefix, suffix, or whole stem from the test tables | `api_test.go` | `Test` |
//! | 7 | Otherwise | `src/main.rs` | `Source` |
//!
//! # Two decisions worth arguing with
//!
//! **The file name is not a directory component.** `vendor.rs` in `src/` is `Source`, not
//! `Vendored`. The rule that matters to an agent is "am I about to read third-party code?", and a
//! hand-written file called `vendor.rs` is not third-party. The same applies to a directory named
//! `tests.rs`.
//!
//! **The `test` rule requires a boundary, not a substring.** `latest.rs` ends in `test` and is not
//! a test; `manifest.py` contains `test` and is not a test. Every test marker in
//! [`Classification::TEST_NAME_SUFFIXES`] therefore starts with a separator, so `contains` behaves
//! as a boundary-anchored match without a glob dependency. The reason this is worth the care:
//! `latest.rs` misclassified as test code is invisible in a large repository, and it is the single
//! most common false positive a naive implementation produces.

use crate::model::RepoPath;
use serde::{Deserialize, Serialize};

/// What kind of code a discovered file holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Classification {
    /// Hand-written or otherwise first-party code. The only kind an agent should read unprompted.
    Source,
    /// Machine-generated from another source of truth. Its entities are noise; its *declaration*
    /// may still be a real API surface.
    Generated,
    /// Third-party code carried inside the repository. Not this project's code, so not its
    /// responsibility, but its public types are real and may be referenced from `Source`.
    Vendored,
    /// Test code. Real code, and part of the project's contract, but not product code.
    Test,
}

impl Classification {
    /// Directory names whose contents are third-party code. Matched as an entire path
    /// component, case-insensitively.
    pub const VENDORED_DIRECTORIES: &'static [&'static str] = &[
        "vendor",
        "vendors",
        "third_party",
        "thirdparty",
        "third-party",
        "extern",
        "external",
        "gopath",
        "pods",
        "packages",
    ];

    /// Directory names whose contents are machine-generated.
    pub const GENERATED_DIRECTORIES: &'static [&'static str] =
        &["generated", "gen", "__generated__", "autogen", "proto_gen", "protos_gen"];

    /// Directory names whose contents are test code.
    pub const TEST_DIRECTORIES: &'static [&'static str] = &[
        "test",
        "tests",
        "testing",
        "spec",
        "specs",
        "__tests__",
        "testdata",
        "e2e",
        "integration_tests",
        "fixtures",
    ];

    /// Case-insensitive substrings that mark a file header as generated. Matched only within
    /// [`Self::HEADER_SCAN_BYTES`], so a licence header at the top of a file does not mask a marker
    /// below it, and a data file that happens to contain the phrase in its body does not get
    /// classified by it.
    pub const GENERATED_MARKERS: &'static [&'static str] = &[
        "code generated by",
        "code generated",
        "automatically generated",
        "auto-generated",
        "autogenerated",
        "do not edit",
        "do not modify",
        "generated by",
        "generated file",
        "this file is generated",
        "generated from",
        "@generated",
    ];

    /// Case-insensitive substrings that mark a *file name* as generated, independent of its
    /// contents. These exist for the languages whose generated files carry no usable header:
    /// protocol buffer output, minified bundles, and TypeScript declaration shims.
    pub const GENERATED_NAME_MARKERS: &'static [&'static str] = &[
        // Protocol buffers and gRPC.
        "_pb2.py",
        "_pb2_grpc.py",
        "_pb.js",
        "_pb.d.ts",
        ".pb.go",
        ".pb.cc",
        ".pb.h",
        ".pb.hpp",
        // Minified bundles.
        ".min.js",
        ".min.mjs",
        ".min.css",
        // Declaration shims and code generators.
        ".d.ts",
        ".g.dart",
        ".freezed.dart",
        ".mocks.dart",
        ".designer.cs",
        ".g.cs",
        ".g.iots",
        ".generated.",
        ".gen.",
    ];

    /// Prefix patterns for test file names, matched case-sensitively on the file name.
    pub const TEST_NAME_PREFIXES: &'static [&'static str] = &["test_", "test-"];

    /// Suffix patterns for test file names, including the extension. Matched
    /// case-insensitively.
    pub const TEST_NAME_SUFFIXES: &'static [&'static str] = &[
        "_test.",
        "_tests.",
        "_spec.",
        "_specs.",
        ".test.",
        ".tests.",
        ".spec.",
        ".specs.",
    ];

    /// Whole file names that are test code regardless of directory, compared case-insensitively.
    pub const TEST_FILE_NAMES: &'static [&'static str] = &["conftest.py", "conftest.js", "setup.py"];

    /// How many leading bytes of a file are scanned for a generated-code marker.
    ///
    /// Two kilobytes covers a short licence header plus a marker. It deliberately does not scan
    /// the whole file: a 200 MB data file mentioning "generated by" in a string literal must not
    /// be classified as generated code.
    pub const HEADER_SCAN_BYTES: usize = 2048;

    /// Classify one file.
    ///
    /// `source` is the decoded file content. Only the first [`Self::HEADER_SCAN_BYTES`] are
    /// examined, and the caller has already guaranteed the content is valid UTF-8, so this
    /// function cannot fail.
    pub fn of(path: &RepoPath, source: &str) -> Self {
        if has_component(path, Self::VENDORED_DIRECTORIES) {
            return Classification::Vendored;
        }
        if has_component(path, Self::GENERATED_DIRECTORIES) {
            return Classification::Generated;
        }
        if contains_marker(&scan_window(source), Self::GENERATED_MARKERS) {
            return Classification::Generated;
        }
        let name = path.file_name();
        if contains_marker(&name.to_ascii_lowercase(), Self::GENERATED_NAME_MARKERS) {
            return Classification::Generated;
        }
        if has_component(path, Self::TEST_DIRECTORIES) {
            return Classification::Test;
        }
        if Self::is_test_name(name) {
            return Classification::Test;
        }
        Classification::Source
    }

    /// The stable lowercase identifier used in output and in [`Self`]'s serde encoding.
    pub const fn as_str(self) -> &'static str {
        match self {
            Classification::Source => "source",
            Classification::Generated => "generated",
            Classification::Vendored => "vendored",
            Classification::Test => "test",
        }
    }

    /// Whether this kind of file should be treated as the project's own code.
    ///
    /// `doctor` and `overview` use this to decide what to tell an agent to ignore. Only
    /// [`Classification::Source`] is the project's own code; test code *is* the project's code,
    /// but it is not the code the agent is usually asking about, so it is deliberately not
    /// included. The distinction is a judgement, so it is named rather than implied by a
    /// comparison at each call site.
    pub const fn is_project_code(self) -> bool {
        matches!(self, Classification::Source)
    }

    /// Whether a file name marks a test file, requiring a word boundary around the marker.
    fn is_test_name(name: &str) -> bool {
        if Self::TEST_FILE_NAMES
            .iter()
            .any(|known| name.eq_ignore_ascii_case(known))
        {
            return true;
        }
        let lowered = name.to_ascii_lowercase();
        if Self::TEST_NAME_PREFIXES
            .iter()
            .any(|prefix| lowered.starts_with(prefix))
        {
            return true;
        }
        Self::TEST_NAME_SUFFIXES
            .iter()
            .any(|suffix| lowered.contains(suffix))
    }
}

impl std::fmt::Display for Classification {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Whether any **directory** component of `path` is one of `names`, compared case-insensitively.
///
/// The file name is excluded on purpose. `src/vendor.rs` is the project's own code, and the rule
/// that matters to an agent is "am I about to read third-party code?" — telling it to ignore a
/// hand-written file because of its name would be a real error, and one invisible in a large
/// repository.
fn has_component(path: &RepoPath, names: &[&str]) -> bool {
    let full = path.as_str();
    let directories = match full.rfind('/') {
        Some(index) => &full[..index],
        None => "",
    };
    directories
        .split('/')
        .filter(|component| !component.is_empty())
        .any(|component| names.iter().any(|name| component.eq_ignore_ascii_case(name)))
}

/// Whether `haystack`, already lowercased where relevant, contains any of `markers`.
fn contains_marker(haystack: &str, markers: &[&str]) -> bool {
    markers.iter().any(|marker| haystack.contains(marker))
}

/// The first [`Classification::HEADER_SCAN_BYTES`] of `source`, lowercased for matching.
///
/// Truncation is by character boundary, not byte, so a multi-byte character straddling the limit
/// cannot panic on slicing. A `char_indices().take_while` walk is the cheapest correct way to do
/// that; `floor_char_boundary` is not available on the minimum supported Rust version.
fn scan_window(source: &str) -> String {
    let mut window = String::with_capacity(Classification::HEADER_SCAN_BYTES.min(source.len()));
    for (index, character) in source.char_indices() {
        if index >= Classification::HEADER_SCAN_BYTES {
            break;
        }
        for lowered in character.to_lowercase() {
            window.push(lowered);
        }
    }
    window
}
