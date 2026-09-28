//! Repository-relative paths.
//!
//! Paths are the backbone of entity identity, so their handling has to be exact.
//!
//! * **Case is preserved, always.** Cortex lowercased every path before using it as a map key,
//!   so on a case-sensitive filesystem `src/Foo.rs` and `src/foo.rs` collapsed into one entry
//!   and one silently overwrote the other. There is no global case folding here. Where
//!   case-insensitive comparison is genuinely wanted (detecting renames on macOS/Windows), it
//!   is an explicit, local operation — see [`RepoPath::eq_ignore_case`].
//! * **Separators are normalised to `/`** at construction, so a path indexed on Windows and the
//!   same path indexed on Linux produce the same identity. This is the *only* normalisation.
//! * **Paths never escape the repository.** `..` components are rejected at construction.

use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// A validated, repository-relative, case-preserving path with `/` separators.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RepoPath(String);

impl RepoPath {
    /// Normalise and validate a repository-relative path.
    ///
    /// Returns `None` if the path is empty, escapes the repository root, or contains a NUL.
    pub fn new(path: impl AsRef<str>) -> Option<Self> {
        let raw = path.as_ref();
        if raw.is_empty() || raw.contains('\0') {
            return None;
        }

        // Windows accepts `\` as a separator; treat it as one everywhere so that the same
        // logical file yields the same identity on every platform.
        let unified = raw.replace('\\', "/");

        let mut parts: Vec<&str> = Vec::new();
        for segment in unified.split('/') {
            match segment {
                "" | "." => continue, // collapse `//` and `./`
                ".." => {
                    // A path that climbs above the root does not identify a repository file.
                    return None;
                }
                other => parts.push(other),
            }
        }

        if parts.is_empty() {
            return None;
        }
        Some(Self(parts.join("/")))
    }

    /// Build a `RepoPath` from a filesystem path, treating it as repository-relative.
    pub fn from_path(path: &Path) -> Option<Self> {
        Self::new(path.to_str()?)
    }

    /// The normalised path as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The final path component, e.g. `main.rs`.
    pub fn file_name(&self) -> &str {
        self.0.rsplit('/').next().unwrap_or(&self.0)
    }

    /// The containing directory, e.g. `crates/peek-core/src`. `None` for a root-level file.
    pub fn parent(&self) -> Option<RepoPath> {
        let (parent, _) = self.0.rsplit_once('/')?;
        RepoPath::new(parent)
    }

    /// The lowercased extension without the dot, e.g. `rs`. `None` when there is none.
    pub fn extension(&self) -> Option<String> {
        let name = self.file_name();
        // `str::rsplit_once('.')` on a dotfile like `.gitignore` yields an empty stem, which is
        // not an extension.
        let (stem, ext) = name.rsplit_once('.')?;
        if stem.is_empty() || ext.is_empty() {
            return None;
        }
        Some(ext.to_ascii_lowercase())
    }

    /// The path with its extension removed, e.g. `src/main.rs` -> `src/main`.
    pub fn without_extension(&self) -> RepoPath {
        let name = self.file_name();
        match name.rsplit_once('.') {
            Some((stem, _)) if !stem.is_empty() => {
                let stripped = format!("{stem}");
                match self.0.rsplit_once('/') {
                    Some((dir, _)) => RepoPath(format!("{dir}/{stripped}")),
                    None => RepoPath(stripped),
                }
            }
            _ => self.clone(),
        }
    }

    /// The path components.
    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.0.split('/')
    }

    /// Depth below the repository root; a root-level file has depth 1.
    pub fn depth(&self) -> usize {
        self.0.split('/').count()
    }

    /// Case-insensitive equality, for rename detection on case-insensitive filesystems only.
    ///
    /// This is deliberately *not* how paths are compared for identity or stored.
    pub fn eq_ignore_case(&self, other: &RepoPath) -> bool {
        self.0.eq_ignore_ascii_case(&other.0)
    }

    /// Whether `self` is `other` or lives underneath it. Used for directory-level operations
    /// such as removing a deleted subtree.
    pub fn is_within(&self, other: &RepoPath) -> bool {
        if self == other {
            return true;
        }
        let prefix = if other.0.ends_with('/') {
            other.0.clone()
        } else {
            format!("{}/", other.0)
        };
        self.0.starts_with(&prefix)
    }
}

impl fmt::Display for RepoPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for RepoPath {
    type Error = PathError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        RepoPath::new(value).ok_or(PathError)
    }
}

impl TryFrom<&str> for RepoPath {
    type Error = PathError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        RepoPath::new(value).ok_or(PathError)
    }
}

impl From<RepoPath> for String {
    fn from(value: RepoPath) -> Self {
        value.0
    }
}

/// Returned when a string cannot be a repository-relative path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PathError;

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("path is empty, escapes the repository root, or contains a NUL byte")
    }
}

impl std::error::Error for PathError {}

#[cfg(test)]
mod tests {
    use super::RepoPath;
    use std::path::Path;

    fn p(s: &str) -> RepoPath {
        RepoPath::new(s).expect("valid path")
    }

    #[test]
    fn preserves_case() {
        assert_eq!(p("src/Main.rs").as_str(), "src/Main.rs");
        assert_eq!(p("src/Main.rs").file_name(), "Main.rs");
    }

    #[test]
    fn paths_differing_only_in_case_stay_distinct() {
        // The defect this type exists to prevent: Cortex keyed its file map on a lowercased
        // path, so these two files collided and one overwrote the other.
        assert_ne!(p("src/Foo.rs"), p("src/foo.rs"));
        assert!(!p("src/Foo.rs").eq_ignore_case(&p("src/foo.rs")) == false);
    }

    #[test]
    fn eq_ignore_case_is_opt_in() {
        assert!(p("src/Foo.rs").eq_ignore_case(&p("SRC/foo.RS")));
        assert!(!p("src/Foo.rs").eq_ignore_case(&p("src/Bar.rs")));
    }

    #[test]
    fn normalises_windows_separators() {
        assert_eq!(p(r"crates\peek-core\src\lib.rs").as_str(), "crates/peek-core/src/lib.rs");
    }

    #[test]
    fn collapses_redundant_segments() {
        assert_eq!(p("./src//lib/./main.rs").as_str(), "src/lib/main.rs");
    }

    #[test]
    fn rejects_paths_escaping_the_root() {
        assert!(RepoPath::new("../secrets.txt").is_none());
        assert!(RepoPath::new("src/../../etc/passwd").is_none());
    }

    #[test]
    fn rejects_empty_and_nul() {
        assert!(RepoPath::new("").is_none());
        assert!(RepoPath::new("/").is_none());
        assert!(RepoPath::new("./").is_none());
        assert!(RepoPath::new("a\0b").is_none());
    }

    #[test]
    fn extension_handles_dotfiles_correctly() {
        assert_eq!(p("src/main.rs").extension().as_deref(), Some("rs"));
        assert_eq!(p("a/b/Makefile.TXT").extension().as_deref(), Some("txt"));
        // A dotfile has no extension, and a trailing dot is not one either.
        assert_eq!(p(".gitignore").extension(), None);
        assert_eq!(p("weird.").extension(), None);
        assert_eq!(p("Makefile").extension(), None);
    }

    #[test]
    fn without_extension() {
        assert_eq!(p("src/main.rs").without_extension().as_str(), "src/main");
        assert_eq!(p("main.rs").without_extension().as_str(), "main");
        assert_eq!(p(".gitignore").without_extension().as_str(), ".gitignore");
    }

    #[test]
    fn parent_and_depth() {
        assert_eq!(p("a/b/c.rs").parent().map(|x| x.to_string()), Some("a/b".into()));
        assert_eq!(p("top.rs").parent(), None);
        assert_eq!(p("a/b/c.rs").depth(), 3);
        assert_eq!(p("top.rs").depth(), 1);
    }

    #[test]
    fn is_within_handles_subtrees_without_prefix_collisions() {
        let src = p("src");
        assert!(p("src/main.rs").is_within(&src));
        assert!(src.is_within(&src));
        // `srcgen` must not count as being inside `src`.
        assert!(!p("srcgen/main.rs").is_within(&src));
    }

    #[test]
    fn serde_round_trip() {
        let original = p("crates/peek-core/src/model/path.rs");
        let json = serde_json::to_string(&original).expect("serialise");
        assert_eq!(json, "\"crates/peek-core/src/model/path.rs\"");
        let back: RepoPath = serde_json::from_str(&json).expect("deserialise");
        assert_eq!(back, original);
    }

    #[test]
    fn serde_rejects_invalid_paths() {
        assert!(serde_json::from_str::<RepoPath>("\"../escape\"").is_err());
    }

    #[test]
    fn from_path_accepts_filesystem_paths() {
        let path = Path::new("src").join("main.rs");
        assert_eq!(
            RepoPath::from_path(&path).map(|x| x.to_string()),
            Some("src/main.rs".into())
        );
    }
}
