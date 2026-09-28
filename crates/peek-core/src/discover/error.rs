//! Failures that make a discovery walk meaningless.
//!
//! Everything a walk can survive — an unreadable directory, a broken symlink, a file that does
//! not decode — is counted in [`crate::discover::DiscoveryStats`] and described in
//! [`crate::discover::WalkIssue`]. It is not an error. An error here means the walk could not
//! start, or could not know where it was, and returning a partial result in that situation would
//! be indistinguishable from a repository that genuinely has no indexable files — which is
//! precisely the trap this module exists to close.

use std::fmt;
use std::io;
use std::path::PathBuf;

/// A discovery walk could not be performed.
#[derive(Debug)]
#[non_exhaustive]
pub enum DiscoveryError {
    /// The root could not be inspected: it does not exist, or it is not readable.
    RootUnreadable {
        /// The root as the caller gave it, not canonicalised.
        path: PathBuf,
        /// The underlying I/O failure.
        source: io::Error,
    },
    /// The root exists but is a file.
    RootNotADirectory {
        /// The root as the caller gave it.
        path: PathBuf,
    },
}

impl fmt::Display for DiscoveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DiscoveryError::RootUnreadable { path, source } => {
                write!(f, "repository root {} could not be read: {source}", path.display())
            }
            DiscoveryError::RootNotADirectory { path } => {
                write!(f, "repository root {} is not a directory", path.display())
            }
        }
    }
}

impl std::error::Error for DiscoveryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            DiscoveryError::RootUnreadable { source, .. } => Some(source),
            DiscoveryError::RootNotADirectory { .. } => None,
        }
    }
}
