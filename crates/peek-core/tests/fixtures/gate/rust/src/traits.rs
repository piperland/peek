//! Two traits, one with a supertrait, and one trait implemented twice.

use crate::model::Entry;

/// Something that reports the current time.
pub trait Clock {
    /// The current time, as whole seconds.
    fn now(&self) -> u64;
}

/// Something that can be written down.
///
/// A supertrait: `Saveable` inherits `Clock`, which is the `inherits` edge the
/// gate scores and the one Cortex never emitted at all.
pub trait Saveable: Clock {
    /// Writes the entry down.
    fn save(&mut self, entry: &Entry) -> bool;

    /// The associated type this trait hands back.
    type Receipt;

    /// The receipt for the last save.
    fn receipt(&self) -> Self::Receipt;
}

/// The store `report` writes through.
pub struct Store {
    /// How many entries have been written.
    pub written: usize,
}

/// A clock that counts seconds since it was made.
pub struct Ticks {
    /// The starting point.
    pub origin: u64,
}

impl Clock for Ticks {
    fn now(&self) -> u64 {
        self.origin
    }
}

impl Saveable for Ticks {
    fn save(&mut self, entry: &Entry) -> bool {
        self.origin += entry.count as u64;
        true
    }

    type Receipt = usize;

    fn receipt(&self) -> usize {
        self.origin as usize
    }
}

/// A second implementation of the same two traits, for the case where a name
/// has two plausible targets and the engine has to say so rather than pick.
pub struct Meters {
    /// How many ticks have elapsed.
    pub elapsed: u64,
}

impl Clock for Meters {
    fn now(&self) -> u64 {
        self.elapsed
    }
}

impl Saveable for Meters {
    fn save(&mut self, entry: &Entry) -> bool {
        self.elapsed += entry.count as u64;
        true
    }

    type Receipt = u64;

    fn receipt(&self) -> u64 {
        self.elapsed
    }
}