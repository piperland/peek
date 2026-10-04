//! Types, and the functions that construct them.

/// One recorded item.
pub struct Entry {
    /// The recorded label.
    pub label: String,
    /// How many times it occurred.
    pub count: u32,
}

/// How severe an entry is.
pub enum Severity {
    /// Not worth reporting.
    Quiet,
    /// Worth reporting.
    Loud,
}

/// A pair of values of the same type, with an inherent `first`.
pub union Pair<T: Copy> {
    /// The leading value.
    pub left: T,
    /// The trailing value.
    pub right: T,
}

/// Something that can be named twice, once as itself and once behind an alias.
pub type Alias = Entry;

/// Builds an `Entry` from its two fields.
pub fn entry(label: &str, count: u32) -> Entry {
    Entry {
        label: label.to_owned(),
        count,
    }
}

/// A method on `Pair`, so the gate can score member resolution against a real
/// inherent impl rather than only against trait impls.
///
/// The type parameter is `U`, not `T`, so the two type parameters in this file
/// have different qualified names. Reusing `T` would produce two rows with the
/// same qualified name and different ordinals, and a reviewer could not tell
/// from the expectations file which row a symbol line referred to.
impl<U: Copy> Pair<U> {
    /// Returns the leading value.
    pub fn first(&self) -> U {
        unsafe { self.left }
    }
}

/// A free function whose name is also the name of a method, so a resolver that
/// ignores the receiver picks one of the two.
pub fn first() -> u8 {
    0
}