//! The module `several::b`. Its import is a `crate::` path, which is the one spelling a resolver
//! can anchor on without knowing where the crate root is.

use crate::a::Alpha;

pub struct Beta(pub Alpha);
