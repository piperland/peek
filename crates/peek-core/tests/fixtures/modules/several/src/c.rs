//! The module `several::c`, which declares an inline module as well as a function.
//!
//! The inline module is the case where a name prefix would be wrong to apply: it is declared
//! inside this file, so its qualified name is `nested_inline` and the function inside it is
//! `nested_inline.gamma`, not `several::c::nested_inline.gamma`.

pub mod nested_inline {
    pub struct Gamma;
}

pub fn gamma() -> nested_inline::Gamma {
    nested_inline::Gamma
}
