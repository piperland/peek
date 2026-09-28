//! File discovery: deciding what in a repository is worth indexing.
//!
//! Discovery is the front door of the whole engine. Extraction, storage, `peek overview` and
//! `peek doctor` all see exactly the file set this module produces, so a defect here is invisible
//! until it is catastrophic.
//!
//! # The defects this module exists to prevent
//!
//! Every rule below exists because Cortex got it wrong, and in every case the failure was
//! **silent** — a wrong answer here was reported as a successful index.
//!
//! | Cortex defect | Peek's rule |
//! |---|---|
//! | Thirteen hardcoded directory names and no ignore file at all. `vendor/`, `dist/`, `coverage/`, `*.min.js`, `*_pb2.py` were indexed; `build`/`target`/`dist` were over-broad and dropped real source. | Ignore files are honoured by the [`ignore`] crate, never hand-rolled. The built-in policy is a separate, documented, **removable** set: [`ExcludeSet`]. |
//! | The skip filter was applied to the **root entry**, so a repository checked out into a directory literally named `build` produced an empty index with no error. | The policy is only ever matched against *repository-relative* paths, which by construction cannot contain the root's own name. |
//! | `walkdir`'s `file_type()` is symlink metadata, so a symlink to a source file was never indexed. | File symlinks resolving **inside** the root are followed and indexed; anything escaping it is refused. See [`SymlinkPolicy`]. |
//! | The skip list was matched case-sensitively while the store key was lowercased, so on NTFS `Target/` and `NODE_MODULES/` were indexed and then folded. | Policy matching is ASCII case-insensitive, and the filesystem's real case sensitivity is detected for duplicate detection. Output paths keep their case. See [`CaseSensitivity`]. |
//! | `fs::read_to_string` on a 200 MB file went straight to the parser. | A configurable [`DiscoveryOptions::max_file_bytes`] cap, enforced before the read, and counted. |
//! | One non-UTF-8 file aborted the entire repository and persisted nothing. | Files that do not decode are skipped and **counted**. No entry can abort the walk. |
//! | Nothing distinguished two worktrees, so one store could be thrashed by both. | [`RepoId`] is a function of the canonical root *and* the git common directory, so worktrees cannot share a store. See [`RepoIdentity`]. |
//! | There was no notion of generated or vendored code, so `doctor` could not tell an agent what to ignore. | Every discovered file carries a [`Classification`] assigned by a documented, ordered rule table. |
//!
//! # Policy decisions and their reasoning
//!
//! **The built-in policy beats ignore files.** Precedence is [`ExcludeSet`] (highest) over
//! `.gitignore` / `.ignore` / `.git/info/exclude` / `core.excludesFile` (lowest). A `!important.rs`
//! in `.gitignore` therefore cannot re-include a file under `target/`. The alternative — letting
//! a file inside the repository silently widen the built-in policy — makes the policy
//! unfalsifiable, because the one thing a user must be able to do is *reliably* re-enable a
//! directory. Re-enabling goes through [`DiscoveryOptions::with_excludes`], where it is visible,
//! testable, and cannot be triggered by accident.
//!
//! **`.gitignore` is honoured outside a git repository.** `WalkBuilder`'s `require_git` defaults
//! to `true`, so git only applies ignore files inside a checkout. Peek passes `require_git(false)`:
//! an agent pointed at a plain directory tree should get the same answer as one pointed at a
//! checkout, and a `.gitignore` is a statement of intent that holds whether or not `.git` happens
//! to be present.
//!
//! **Hidden files are not skipped.** `WalkBuilder`'s default skips them. `.github/workflows`,
//! `.claude/settings.json` and `container/Dockerfile` are precisely what an agent needs, so
//! [`DiscoveryOptions::skip_hidden`] defaults to `false` and the exclusion set carries the caches
//! instead.
//!
//! **Vendored trees are classified, not skipped.** Cortex's list silently dropped `vendor/`. That
//! destroys information an agent needs: *"there are 40 000 lines of third-party code here"* is a
//! more useful and more different fact than *"it is missing"*. A `vendor/` that the project
//! gitignores is never walked, exactly as git intends; a `vendor/` the project **commits** is
//! walked and labelled [`Classification::Vendored`], and the extractor is free to decline it.
//!
//! # The one metric this module refuses to fabricate
//!
//! [`DiscoveryStats::excluded`] counts entries Peek's own policy rejected, which it can count
//! exactly because it prunes them from a walk hook that observes the entry before pruning it.
//! Entries removed by an ignore file are pruned *inside* the `ignore` crate and are never observed.
//! There is therefore deliberately **no** "skipped as ignored" counter, and
//! [`DiscoveryStats::entries_visited`] means "entries the walker produced" rather than "inodes on
//! disk". Both facts are stated in the field documentation instead of being papered over with a
//! number that would look complete and be wrong.
//!
//! # Walk integrity
//!
//! [`FileDiscovery::discover`] is a single pass with no early exit and no `?` on any per-entry
//! operation. An entry that fails is counted in [`DiscoveryStats::unreadable`] and described in
//! [`DiscoveryReport::issues`]; the walk continues. That is the direct answer to one bad file
//! killing a whole repository.

pub mod case;
pub mod classify;
pub mod error;
pub mod git;
pub mod options;
pub mod stats;
pub mod symlink;

#[cfg(test)]
mod tests;

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::model::{Language, RepoPath};
use symlink::Resolution;

// Re-exported so a caller writes `discover::Classification` rather than
// `discover::classify::Classification`. The modules stay public because `doctor` prints the tables
// the classification and exclusion rules are built from: the rules have to be inspectable, not just
// documented.
pub use case::CaseSensitivity;
pub use classify::Classification;
pub use error::DiscoveryError;
pub use git::{RepoId, RepoIdSource, RepoIdentity};
pub use options::{DiscoveryOptions, ExcludeSet};
pub use stats::{DiscoveryStats, WalkIssue, WalkIssueReason};
pub use symlink::SymlinkPolicy;

/// A file discovery selected for indexing, with everything known about it before it is read.
///
/// No absolute path appears here on purpose. An index that stores machine-specific paths cannot be
/// moved between machines; [`DiscoveryReport::absolute`] derives one on demand instead.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DiscoveredFile {
    /// Repository-relative, `/`-separated, case preserved. Never lowercased.
    pub path: RepoPath,
    /// The language implied by the extension. An unrecognised extension is a skip, not a file
    /// with an unknown language, so this is never optional.
    pub language: Language,
    /// Whether this is source, generated, vendored, or test code.
    pub classification: Classification,
    /// Size on disk in bytes, measured during the walk.
    pub size_bytes: u64,
}

/// Why a walk found nothing.
///
/// This is a first-class outcome, never a successful count of zero. Cortex's empty-index trap was
/// a repository checked out into a directory named `build`: the walker skipped the root, produced
/// zero files, reported success with `file_count: 0`, and the daemon read that as "never indexed"
/// and rebuilt forever. Distinguishing *no files* from *nothing indexable* from *nothing usable*
/// is what makes that failure mode diagnosable instead of merely absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmptyReason {
    /// No regular file reached the extension check. Either the tree is genuinely empty, or every
    /// file in it was pruned by an ignore file or by the exclusion policy. The statistics
    /// separate these where the counters allow it; where they cannot, this reason is the honest
    /// "nothing to look at".
    NoFiles,
    /// Every examined file was skipped because its extension maps to no [`Language`].
    NoIndexableExtensions,
    /// Indexable files existed, and every one of them was then skipped — too large, not valid
    /// UTF-8, unreadable, or a case-insensitive duplicate of a path already yielded.
    AllUnusable,
}

impl EmptyReason {
    /// A stable lowercase identifier for logs and `doctor` output.
    pub const fn as_str(self) -> &'static str {
        match self {
            EmptyReason::NoFiles => "no_files",
            EmptyReason::NoIndexableExtensions => "no_indexable_extensions",
            EmptyReason::AllUnusable => "all_unusable",
        }
    }
}

impl std::fmt::Display for EmptyReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Everything one discovery pass produced. Present for both outcomes, so an empty result carries
/// the same evidence as a full one.
#[derive(Debug, Clone)]
pub struct DiscoveryReport {
    repo: RepoIdentity,
    root: PathBuf,
    files: Vec<DiscoveredFile>,
    stats: DiscoveryStats,
    issues: Vec<WalkIssue>,
}

impl DiscoveryReport {
    /// The identity of the checkout this walk covered. Two worktrees of one repository get
    /// different ids and therefore cannot share a store.
    pub fn repo(&self) -> &RepoIdentity {
        &self.repo
    }

    /// The canonical repository root that was walked.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The absolute path of a repository-relative path, for reading the file.
    pub fn absolute(&self, path: &RepoPath) -> PathBuf {
        self.root.join(path.as_str())
    }

    /// The discovered files, sorted by path with case preserved.
    pub fn files(&self) -> &[DiscoveredFile] {
        &self.files
    }

    /// The counts behind this walk.
    pub fn stats(&self) -> &DiscoveryStats {
        &self.stats
    }

    /// Per-entry problems that did not stop the walk, in the order they were seen.
    pub fn issues(&self) -> &[WalkIssue] {
        &self.issues
    }

    fn empty_reason(&self) -> Option<EmptyReason> {
        if !self.files.is_empty() {
            return None;
        }
        if self.stats.files_examined == 0 {
            return Some(EmptyReason::NoFiles);
        }
        if self.stats.unsupported_extension == self.stats.files_examined {
            return Some(EmptyReason::NoIndexableExtensions);
        }
        Some(EmptyReason::AllUnusable)
    }
}

/// The outcome of a discovery pass.
///
/// The split is the point: [`Discovery::Empty`] cannot be mistaken for a successful walk that
/// happened to find nothing, because it is a different variant a caller must handle.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Discovery {
    /// At least one file is worth indexing.
    Found(DiscoveryReport),
    /// Nothing is worth indexing. The report is still returned, and still explains why.
    Empty {
        /// Why the walk yielded nothing.
        reason: EmptyReason,
        /// The full report, including counts, so the emptiness is diagnosable.
        report: DiscoveryReport,
    },
}

impl Discovery {
    /// The report, whichever variant this is.
    pub fn report(&self) -> &DiscoveryReport {
        match self {
            Discovery::Found(report) => report,
            Discovery::Empty { report, .. } => report,
        }
    }

    /// Consume this outcome and take the report.
    pub fn into_report(self) -> DiscoveryReport {
        match self {
            Discovery::Found(report) | Discovery::Empty { report, .. } => report,
        }
    }

    /// The discovered files; empty when the outcome is [`Discovery::Empty`].
    pub fn files(&self) -> &[DiscoveredFile] {
        self.report().files()
    }

    /// The counts behind this walk.
    pub fn stats(&self) -> &DiscoveryStats {
        self.report().stats()
    }

    /// The repository identity this walk covered.
    pub fn repo(&self) -> &RepoIdentity {
        self.report().repo()
    }

    /// Per-entry problems that did not stop the walk.
    ///
    /// Present on both variants: a walk that found nothing can still have refused a file, and
    /// "found nothing because everything was skipped" is a different diagnosis from "found
    /// nothing because the tree was empty". `peek doctor` needs the difference.
    pub fn issues(&self) -> &[WalkIssue] {
        self.report().issues()
    }

    /// `true` when nothing was found.
    pub fn is_empty(&self) -> bool {
        matches!(self, Discovery::Empty { .. })
    }

    /// Why the walk yielded nothing, or `None` when it yielded something.
    pub fn empty_reason(&self) -> Option<EmptyReason> {
        match self {
            Discovery::Found(_) => None,
            Discovery::Empty { reason, .. } => Some(*reason),
        }
    }
}

/// Walks a repository and yields the files worth indexing.
///
/// One instance covers one root. [`FileDiscovery::discover`] may be called more than once; each
/// call is an independent full walk, and for an unchanged tree the two produce identical output.
#[derive(Debug, Clone)]
pub struct FileDiscovery {
    root: PathBuf,
    options: DiscoveryOptions,
}

impl FileDiscovery {
    /// Configure a walk of `root`.
    ///
    /// This does not touch the filesystem. The root is validated and canonicalised by
    /// [`FileDiscovery::discover`], so a [`FileDiscovery`] can be built long before the walk runs
    /// and a failure is reported where it happens rather than where it was configured.
    pub fn new(root: impl AsRef<Path>, options: DiscoveryOptions) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
            options,
        }
    }

    /// The configured options.
    pub fn options(&self) -> &DiscoveryOptions {
        &self.options
    }

    /// The root as given, before canonicalisation.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Walk the tree.
    ///
    /// Fails only for problems that make the walk meaningless — an unreadable root, or a root
    /// that is not a directory. Every per-entry problem is recorded in the report instead.
    pub fn discover(&self) -> Result<Discovery, DiscoveryError> {
        let root = self.canonical_root()?;
        let repo = RepoIdentity::identify(&root);
        let case_sensitivity = self
            .options
            .case_sensitivity()
            .unwrap_or_else(|| CaseSensitivity::detect(&root));

        let mut builder = ignore::WalkBuilder::new(&root);
        builder
            .max_depth(self.options.max_depth())
            // Peek honours `.gitignore` outside a repository too; see the module docs.
            .require_git(false)
            .git_ignore(self.options.read_ignore_files())
            .git_exclude(self.options.read_ignore_files())
            .git_global(self.options.read_ignore_files())
            .ignore(self.options.read_ignore_files())
            .parents(self.options.read_ignore_files())
            .hidden(self.options.skip_hidden())
            // Symlinks are followed by Peek, deliberately and visibly, rather than by the
            // walker. See `SymlinkPolicy`.
            .follow_links(false);

        // Pruning excluded directories has to happen inside the walk, or a `node_modules` costs a
        // full traversal even though every file in it is discarded.
        let policy = self.options.excludes().clone();
        let prune_root = root.clone();
        let excluded = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&excluded);
        builder.filter_entry(move |entry: &ignore::DirEntry| -> bool {
            // The `ignore` crate currently short-circuits its own skip logic for the walk root
            // before consulting this filter, so the guard below is not load-bearing *today*. It
            // stays because the alternative is depending on that crate's internal ordering: if it
            // ever changed, a checkout in a directory named `build` would silently produce an empty
            // index again, and nothing in the type system or the tests here would say why.
            //
            // The real, structural defence is the next line: `policy.excludes` only ever receives a
            // repository-relative path, and a relative path cannot contain the root's own name
            // because the root is the prefix that was stripped.
            if entry.depth() == 0 {
                return true;
            }
            let is_excluded = match entry.path().strip_prefix(prune_root.as_path()) {
                Ok(relative) => policy.excludes(relative),
                Err(_) => false,
            };
            if is_excluded {
                counter.fetch_add(1, Ordering::Relaxed);
            }
            !is_excluded
        });

        let mut walk = WalkState::new(&root, &self.options, case_sensitivity);
        for entry in builder.build() {
            match entry {
                Ok(entry) => walk.visit(&entry),
                Err(error) => walk.record_walk_error(error),
            }
        }

        Ok(walk.finish(repo, excluded.load(Ordering::Relaxed)))
    }

    fn canonical_root(&self) -> Result<PathBuf, DiscoveryError> {
        let metadata =
            fs::metadata(&self.root).map_err(|source| DiscoveryError::RootUnreadable {
                path: self.root.clone(),
                source,
            })?;
        if !metadata.is_dir() {
            return Err(DiscoveryError::RootNotADirectory {
                path: self.root.clone(),
            });
        }
        // Canonicalising once, here, is what makes the repository identity, the symlink
        // containment check and the relative-path computation agree on one spelling of the root.
        // Without it, `starts_with` on Windows is a case-sensitive comparison against a
        // `\\?\`-prefixed path and the containment check is unsound.
        fs::canonicalize(&self.root).map_err(|source| DiscoveryError::RootUnreadable {
            path: self.root.clone(),
            source,
        })
    }
}

/// Mutable state for a single walk. Private: a caller can never observe a half-finished walk.
struct WalkState<'a> {
    root: PathBuf,
    options: &'a DiscoveryOptions,
    case_sensitivity: CaseSensitivity,
    candidates: Vec<DiscoveredFile>,
    stats: DiscoveryStats,
    issues: Vec<WalkIssue>,
}

impl<'a> WalkState<'a> {
    /// The lifetime of `options` is the struct's own. Writing `impl WalkState<'_>` and letting
    /// `new` infer it makes the borrow checker unable to tie the two together, which fails to
    /// compile because the returned `Self` would outlive the options it points at.
    fn new(root: &Path, options: &'a DiscoveryOptions, case_sensitivity: CaseSensitivity) -> Self {
        Self {
            root: root.to_path_buf(),
            options,
            case_sensitivity,
            candidates: Vec::new(),
            stats: DiscoveryStats::default(),
            issues: Vec::new(),
        }
    }

    fn visit(&mut self, entry: &ignore::DirEntry) {
        self.stats.entries_visited += 1;
        let Some(kind) = entry.file_type() else {
            self.stats.irregular_skipped += 1;
            return;
        };
        if kind.is_dir() {
            self.stats.directories_visited += 1;
            return;
        }
        let absolute = entry.path().to_path_buf();
        let Some(path) = self.relative(&absolute) else {
            return;
        };
        if kind.is_symlink() {
            self.visit_symlink(&absolute, path);
        } else if kind.is_file() {
            self.consider(&absolute, path);
        } else {
            // Sockets, fifos and devices are not files. Counting them keeps the arithmetic in
            // `DiscoveryStats` closing over every entry the walker produced.
            self.stats.irregular_skipped += 1;
        }
    }

    /// The repository-relative path for an entry, or `None` with a recorded issue when the path
    /// cannot be represented.
    fn relative(&mut self, absolute: &Path) -> Option<RepoPath> {
        let Ok(relative) = absolute.strip_prefix(&self.root) else {
            self.issue(
                &absolute.to_string_lossy(),
                WalkIssueReason::OutsideRepository,
            );
            return None;
        };
        if relative.as_os_str().is_empty() {
            // The walk root itself. It is counted as a directory and carries no path.
            return None;
        }
        let Some(text) = relative.to_str() else {
            // On Unix a path can be bytes that are not UTF-8. `RepoPath` cannot hold it, so the
            // entry is counted rather than quietly dropped.
            self.issue(&relative.to_string_lossy(), WalkIssueReason::NonUtf8Path);
            return None;
        };
        match RepoPath::new(text) {
            Some(path) => Some(path),
            None => {
                // Unreachable for a walked path: a component can be neither empty nor `..`, and
                // neither can contain a NUL. Recorded rather than returned silently, so that if the
                // invariant ever breaks the cause is visible instead of being a missing file.
                self.issue(text, WalkIssueReason::OutsideRepository);
                None
            }
        }
    }

    fn visit_symlink(&mut self, link: &Path, path: RepoPath) {
        match self.options.symlinks().resolve(&self.root, link) {
            Resolution::File { absolute } => {
                self.stats.symlinks_followed += 1;
                self.consider(&absolute, path);
            }
            Resolution::Directory => {
                // Not descending is what makes a directory cycle impossible by construction,
                // rather than merely unlikely. See the `SymlinkPolicy` docs.
                self.stats.symlink_directories_skipped += 1;
            }
            Resolution::Skipped => self.stats.symlink_skipped += 1,
            Resolution::Escapes { target } => {
                self.stats.symlink_escapes += 1;
                self.issue(
                    path.as_str(),
                    WalkIssueReason::SymlinkEscapes {
                        target: target.to_string_lossy().into_owned(),
                    },
                );
            }
            Resolution::Unresolvable { detail } => {
                self.stats.unreadable += 1;
                self.issue(
                    path.as_str(),
                    WalkIssueReason::UnresolvableSymlink { detail },
                );
            }
        }
    }

    /// Decide whether one concrete file is worth indexing, and why not if it is not.
    ///
    /// Order matters and is deliberate: cheapest and most selective checks first, so the
    /// expensive read happens only for a file that is already known to be a candidate.
    fn consider(&mut self, absolute: &Path, path: RepoPath) {
        self.stats.files_examined += 1;

        let Some(extension) = path.extension() else {
            self.stats.unsupported_extension += 1;
            self.issue(
                path.as_str(),
                WalkIssueReason::UnsupportedExtension { extension: None },
            );
            return;
        };
        let Some(language) = Language::from_extension(&extension) else {
            self.stats.unsupported_extension += 1;
            self.issue(
                path.as_str(),
                WalkIssueReason::UnsupportedExtension {
                    extension: Some(extension.to_owned()),
                },
            );
            return;
        };

        let metadata = match fs::metadata(absolute) {
            Ok(metadata) => metadata,
            Err(error) => {
                self.stats.unreadable += 1;
                self.issue(
                    path.as_str(),
                    WalkIssueReason::Unreadable {
                        detail: error.to_string(),
                    },
                );
                return;
            }
        };
        if metadata.len() > self.options.max_file_bytes() {
            self.stats.too_large += 1;
            self.issue(
                path.as_str(),
                WalkIssueReason::TooLarge {
                    bytes: metadata.len(),
                    cap: self.options.max_file_bytes(),
                },
            );
            return;
        }

        // One read serves both the decode check and the classification header. The extractor will
        // read the file again; that redundancy is the price of guaranteeing that nothing which
        // fails to decode is ever handed downstream. The size cap bounds it.
        let contents = match fs::read(absolute) {
            Ok(contents) => contents,
            Err(error) => {
                self.stats.unreadable += 1;
                self.issue(
                    path.as_str(),
                    WalkIssueReason::Unreadable {
                        detail: error.to_string(),
                    },
                );
                return;
            }
        };
        let source = match String::from_utf8(contents) {
            Ok(source) => source,
            Err(error) => {
                self.stats.not_utf8 += 1;
                self.issue(
                    path.as_str(),
                    WalkIssueReason::NotUtf8 {
                        offset: error.utf8_error().valid_up_to(),
                    },
                );
                return;
            }
        };

        // The read length, not the earlier `stat`. A file edited between the two would otherwise
        // be reported at a size that was never true, and `stats.bytes` would not sum to anything
        // a reader could reconcile against the file list.
        let size_bytes = u64::try_from(source.len()).unwrap_or(u64::MAX);
        self.candidates.push(DiscoveredFile {
            classification: Classification::of(&path, &source),
            path,
            language,
            size_bytes,
        });
    }

    fn record_walk_error(&mut self, error: ignore::Error) {
        self.issue(
            "<walk>",
            WalkIssueReason::Unreadable {
                detail: error.to_string(),
            },
        );
    }

    /// Sort, drop duplicates, fill the derived statistics, and decide the outcome.
    fn finish(mut self, repo: RepoIdentity, excluded: usize) -> Discovery {
        // Sorting before duplicate detection is what makes the result reproducible: filesystem
        // read order is not stable, so "first one wins" would otherwise depend on the walk.
        self.candidates
            .sort_by(|left, right| left.path.as_str().cmp(right.path.as_str()));

        let mut seen: HashSet<String> = HashSet::with_capacity(self.candidates.len());
        let mut files = Vec::with_capacity(self.candidates.len());
        for candidate in std::mem::take(&mut self.candidates) {
            // `CaseSensitivity::key` is the one place case is folded anywhere in Peek, and it
            // folds into a transient set, never into a stored path.
            if !seen.insert(self.case_sensitivity.key(candidate.path.as_str())) {
                self.stats.duplicates += 1;
                self.issue(candidate.path.as_str(), WalkIssueReason::Duplicate);
                continue;
            }
            files.push(candidate);
        }

        self.stats.excluded = excluded;
        self.stats.files_yielded = files.len();
        for file in &files {
            self.stats.bytes += file.size_bytes;
            self.stats.largest_file = self.stats.largest_file.max(file.size_bytes);
            *self.stats.by_language.entry(file.language).or_default() += 1;
            *self
                .stats
                .by_classification
                .entry(file.classification)
                .or_default() += 1;
        }
        self.stats.walk_issues = self.issues.len();

        let report = DiscoveryReport {
            repo,
            root: self.root,
            files,
            stats: self.stats,
            issues: self.issues,
        };
        match report.empty_reason() {
            None => Discovery::Found(report),
            Some(reason) => Discovery::Empty { reason, report },
        }
    }

    fn issue(&mut self, path: &str, reason: WalkIssueReason) {
        self.issues.push(WalkIssue {
            path: path.to_owned(),
            reason,
        });
    }
}
