//! A crate whose modules nest three levels deep, every level in its own `mod.rs`.
//!
//! `mod outer;` names a module whose body lives in `src/outer/mod.rs`. That is the shape the
//! qualified-name rule has to read correctly at every level: `outer` is the module
//! `nested::outer`, `middle` is declared *inside* `outer/mod.rs`, and `inner.rs` is the module
//! `nested::outer::middle::inner`.

mod outer;

pub fn crate_root() -> &'static str {
    "nested"
}
