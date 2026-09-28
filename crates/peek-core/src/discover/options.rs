//! What to exclude, independent of what the repository's ignore files say.
//!
//! # Two different mechanisms, on purpose
//!
//! Discovery applies two independent exclusion layers, in this order:
//!
//! 1. **Ignore files** — `.gitignore`, `.ignore`, `.git/info/exclude`, and the user's
//!    `core.excludesFile`. These are the repository's own statement of what is not its source,
//!    and they are read by the [`ignore`] crate, which implements git's actual semantics
//!    (negation, directory-only patterns, anchored patterns, nested files). Peek does not
//!    reimplement any of it. Hand-rolled gitignore matching is a well-known source of silent
//!    wrong answers, and this module is the one place where a silent wrong answer destroys an
//!    entire index.
//! 2. **The [`ExcludeSet`]** — a small set of names that are near-universally not source, applied
//!    even when no ignore file mentions them.
//!
//! The two layers are separate because they answer different questions. An ignore file says
//! *"right now, in this checkout, I do not want these"*. The exclusion set says *"these are
//! never source, and I refuse to walk them even if the checkout happens not to ignore them"*.
//!
//! # Why the exclusion set exists at all
//!
//! Because ignore files are not always present. A tarball extraction, a vendored dependency
//! copied into a build pipeline, a CI artifact directory, or a project that simply never wrote a
//! `.gitignore` — in all of those the only defence against indexing `node_modules` is a built-in
//! list. Cortex had such a list, and its failure was in *how* it was applied, not in whether.
//!
//! # Why the exclusion set is configurable and removable
//!
//! Cortex's list was hardcoded inside the walker and applied case-sensitively and to the root
//! entry, so it both missed `Target/` on NTFS and emptied the index of any repository checked out
//! into a directory named `build`. Two rules follow:
//!
//! * **Every name is removable.** [`ExcludeSet::without`] exists because a real repository has a
//!   `build/` directory full of hand-written CMake, or a `dist/` directory of deployment scripts
//!   that are the most important code in the repo. Peek cannot know which. It can only make the
//!   default visible and correctable.
//! * **The set is never applied to the root's own name.** See [`ExcludeSet::excludes`], which
//!   matches against *relative* paths only.
//!
//! Matching is ASCII case-insensitive, so `Target/`, `TARGET/` and `target/` are one policy
//! decision rather than three. This is not a preference: NTFS and APFS are case-insensitive by
//! default, so a case-sensitive list indexes directories the store then lowercases.

use std::collections::BTreeSet;
use std::path::Path;

use super::case::CaseSensitivity;
use super::symlink::SymlinkPolicy;

/// A set of path components that discovery never descends into.
///
/// Names are matched **case-insensitively** against every component of a *repository-relative*
/// path, at any depth. A set is a flat list of names: there is no globbing, no pattern language,
/// and no way to express "only at the top level". That restriction is deliberate — the matching
/// is a `BTreeSet` lookup, it is testable by inspection, and it cannot develop precedence
/// surprises. Anything needing a pattern belongs in a `.gitignore`, which already has one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExcludeSet {
    names: BTreeSet<String>,
}

impl ExcludeSet {
    /// The set Peek applies when the caller expresses no opinion.
    ///
    /// This is deliberately short. Cortex listed thirteen directories and still indexed
    /// `vendor/`, `.next/`, `coverage/`, `Pods/` and every `*.min.js`, because it read no ignore
    /// file; a longer built-in list would repeat that mistake in a new form, discarding real
    /// source to compensate for trees that should have been ignored in the first place.
    ///
    /// Two groups, and the grouping is the point:
    ///
    /// * [`Self::VCS_INTERNALS`] — the version control database. This is not a heuristic; it is
    ///   the repository's own storage format, and it is large, binary, and never source.
    /// * [`Self::ECOSYSTEM_NOISE`] — directories that are near-universally build output or
    ///   dependencies. Each is a judgement call, and each is removable.
    pub fn default_set() -> Self {
        Self::from_names(
            Self::VCS_INTERNALS
                .iter()
                .chain(Self::ECOSYSTEM_NOISE)
                .copied(),
        )
    }

    /// The version control database. Excluding this is not optional in practice: a `.git`
    /// directory is large, mostly binary, and contains files with source extensions.
    pub const VCS_INTERNALS: &'static [&'static str] = &[".git", ".hg", ".svn", ".jj", "CVS"];

    /// Directories that are almost always build output or vendored dependencies.
    ///
    /// `build`, `target` and `dist` are here because the cost of not having them is catastrophic
    /// (thousands of files of machine-written output), and the cost of having them is one
    /// [`ExcludeSet::without`] call. `vendor` and `third_party` are *not* here: unlike build
    /// output, committed vendor trees are frequently the only copy of a dependency's source in a
    /// checkout, and silently dropping them is how Cortex lost real code. They are classified as
    /// [`crate::discover::classify::Classification::Vendored`] instead, which tells an agent they
    /// exist without pretending they are absent.
    pub const ECOSYSTEM_NOISE: &'static [&'static str] = &[
        "node_modules",
        "target",
        "dist",
        "build",
        "__pycache__",
        ".venv",
        "venv",
        ".mypy_cache",
        ".pytest_cache",
        ".ruff_cache",
        ".tox",
        ".next",
        ".nuxt",
        ".svelte-kit",
        ".parcel-cache",
        ".gradle",
        ".terraform",
        "Pods",
        "DerivedData",
        "_build",
        "elm-stuff",
        "zig-cache",
        "zig-out",
        ".cache",
        "coverage",
        ".idea",
        ".vscode",
    ];

    /// An empty set. Nothing is excluded beyond what ignore files exclude.
    pub fn empty() -> Self {
        Self {
            names: BTreeSet::new(),
        }
    }

    /// Build a set from an iterator of names, lowercasing each so matching is uniform.
    pub fn from_names<I, S>(names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        Self {
            names: names
                .into_iter()
                .map(|name| name.as_ref().to_ascii_lowercase())
                .collect(),
        }
    }

    /// Add names. Cannot be used to shrink the set; see [`Self::without`].
    pub fn with<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.names.extend(
            names
                .into_iter()
                .map(|name| name.as_ref().to_ascii_lowercase()),
        );
        self
    }

    /// Remove names. This is the escape hatch for a repository whose `build/` is source.
    pub fn without<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        for name in names {
            self.names.remove(&name.as_ref().to_ascii_lowercase());
        }
        self
    }

    /// The names in this set, sorted and lowercased.
    pub fn names(&self) -> impl ExactSizeIterator<Item = &str> {
        self.names.iter().map(String::as_str)
    }

    /// Whether this set is empty.
    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    /// Whether any component of `relative` is in this set.
    ///
    /// # The root-directory trap
    ///
    /// `relative` must be relative to the repository root, and the caller must never pass the
    /// root itself. This is the whole defence against the defect that made Cortex's empty index
    /// silent: a repository checked out into a directory literally named `build` produced zero
    /// files, `file_count: 0`, no error, and a daemon that rebuilt it forever.
    ///
    /// The defence is structural rather than a special case. A repository-relative path cannot
    /// contain the root's own name, because the root is the prefix that was stripped. There is no
    /// comparison anywhere in the walker that could match `build` against the checkout directory,
    /// so there is no ordering of checks that could get it wrong. The walk additionally skips the
    /// depth-0 entry in its pruning hook, so the two layers cannot disagree.
    pub fn excludes(&self, relative: &Path) -> bool {
        relative
            .components()
            .filter_map(|component| component.as_os_str().to_str())
            .any(|component| self.names.contains(&component.to_ascii_lowercase()))
    }
}

impl Default for ExcludeSet {
    fn default() -> Self {
        Self::default_set()
    }
}

/// Everything that changes what a walk produces.
///
/// The defaults are the ones Peek uses when a caller says nothing. Each field is settable
/// individually, so a caller who wants a different policy changes one thing rather than
/// reconstructing the whole configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryOptions {
    excludes: ExcludeSet,
    max_file_bytes: u64,
    max_depth: Option<usize>,
    skip_hidden: bool,
    read_ignore_files: bool,
    require_git: bool,
    case_sensitivity: Option<CaseSensitivity>,
    symlinks: SymlinkPolicy,
}

impl Default for DiscoveryOptions {
    fn default() -> Self {
        Self {
            excludes: ExcludeSet::default(),
            // Cortex applied no cap at all and fed a 200 MB file straight to the parser. Two
            // megabytes is far above any hand-written source file in any first-parity language
            // and far below anything that can stall a parse.
            max_file_bytes: 2 * 1024 * 1024,
            max_depth: None,
            // `true` would skip `.github/`, `.claude/` and `container/Dockerfile`, all of which
            // are exactly what an agent needs. Cortex inherited the `ignore` crate's default
            // here, which is why its index had no CI configuration in it.
            skip_hidden: false,
            read_ignore_files: true,
            // `false` because the `ignore` crate otherwise ignores `.gitignore` outside a
            // repository, and Peek is routinely pointed at a plain directory that has never been
            // a checkout. A `.gitignore` is a statement of intent that holds regardless of
            // whether `.git` happens to be present.
            require_git: false,
            case_sensitivity: None,
            symlinks: SymlinkPolicy::default(),
        }
    }
}

impl DiscoveryOptions {
    /// The exclusion set applied on top of ignore files.
    pub fn excludes(&self) -> &ExcludeSet {
        &self.excludes
    }

    /// The largest file, in bytes, that will be read. Larger files are skipped and counted.
    pub fn max_file_bytes(&self) -> u64 {
        self.max_file_bytes
    }

    /// The maximum directory depth to descend, or `None` for no limit.
    pub fn max_depth(&self) -> Option<usize> {
        self.max_depth
    }

    /// Whether dot-prefixed files and directories are skipped.
    pub fn skip_hidden(&self) -> bool {
        self.skip_hidden
    }

    /// Whether `.gitignore` and friends are honoured.
    pub fn read_ignore_files(&self) -> bool {
        self.read_ignore_files
    }

    /// Whether ignore files only apply inside a git repository.
    pub fn require_git(&self) -> bool {
        self.require_git
    }

    /// The case sensitivity to use for duplicate detection, or `None` to detect it.
    pub fn case_sensitivity(&self) -> Option<CaseSensitivity> {
        self.case_sensitivity
    }

    /// How symlinks are treated.
    pub fn symlinks(&self) -> SymlinkPolicy {
        self.symlinks
    }

    /// Replace the exclusion set.
    pub fn with_excludes(mut self, excludes: ExcludeSet) -> Self {
        self.excludes = excludes;
        self
    }

    /// Set the file size cap in bytes.
    pub fn with_max_file_bytes(mut self, max_file_bytes: u64) -> Self {
        self.max_file_bytes = max_file_bytes;
        self
    }

    /// Set the maximum directory depth, or `None` for no limit.
    pub fn with_max_depth(mut self, max_depth: Option<usize>) -> Self {
        self.max_depth = max_depth;
        self
    }

    /// Skip or include dot-prefixed files and directories.
    pub fn with_hidden_skipped(mut self, skip: bool) -> Self {
        self.skip_hidden = skip;
        self
    }

    /// Honour or ignore `.gitignore` and its relatives.
    pub fn with_ignore_files(mut self, read: bool) -> Self {
        self.read_ignore_files = read;
        self
    }

    /// Apply ignore files only inside a git repository.
    pub fn with_require_git(mut self, require: bool) -> Self {
        self.require_git = require;
        self
    }

    /// Fix the case sensitivity instead of detecting it.
    pub fn with_case_sensitivity(mut self, case_sensitivity: CaseSensitivity) -> Self {
        self.case_sensitivity = Some(case_sensitivity);
        self
    }

    /// Set how symlinks are treated.
    pub fn with_symlinks(mut self, symlinks: SymlinkPolicy) -> Self {
        self.symlinks = symlinks;
        self
    }
}
