//! The module `alpha::gateway`, which is what `use alpha::gateway::Gateway;` in the other package
//! names.

pub trait Gateway {
    fn charge(&self, amount: u32) -> u32;
}
