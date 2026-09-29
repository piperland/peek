//! The module `beta::service`, which implements a trait declared in the other package.

use alpha::gateway::Gateway;

pub struct Stripe;

impl Gateway for Stripe {
    fn charge(&self, amount: u32) -> u32 {
        amount
    }
}
