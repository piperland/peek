//! The module `reexport::inner::helpers`, the target of the glob re-export.
//!
//! A glob is a binding of `*`, not of `assist`. Nothing in this file is named by the `pub use
//! crate::inner::helpers::*;` above it, which is exactly why the star must not be mistaken for a
//! name that a resolver could look up.

pub fn assist() -> u32 {
    1
}
