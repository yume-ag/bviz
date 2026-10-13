use std::iter::Sum;
use std::ops::{Add, AddAssign};

/// An amount of file content.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Bytes(u64);

impl Bytes {
    pub const ZERO: Bytes = Bytes(0);

    pub const fn new(bytes: u64) -> Bytes {
        Bytes(bytes)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Saturates: a sum past `u64::MAX` bytes is no amount a build writes.
impl Add for Bytes {
    type Output = Bytes;

    fn add(self, other: Bytes) -> Bytes {
        Bytes(self.0.saturating_add(other.0))
    }
}

impl AddAssign for Bytes {
    fn add_assign(&mut self, other: Bytes) {
        *self = *self + other;
    }
}

impl Sum for Bytes {
    fn sum<I: Iterator<Item = Bytes>>(iter: I) -> Bytes {
        iter.fold(Bytes::ZERO, Add::add)
    }
}
