//! The module `nested::outer::middle::inner`: the deepest file in the tree, declared three
//! `mod.rs` levels above the file that declares it.

pub struct Inner;

impl Inner {
    pub fn name() -> &'static str {
        "inner"
    }
}
