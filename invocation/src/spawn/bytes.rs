use std::iter::Sum;
use std::ops::{Add, AddAssign};

use super::LoggedSpawn;

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

/// The content of a spawn's inputs, each counted once however many of the
/// sets that gather them hold it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct InputBytes {
    /// The inputs that are the spawn's tools.
    pub tools: Bytes,
    /// The inputs that are none of the spawn's tools.
    pub others: Bytes,
}

impl InputBytes {
    pub fn all(self) -> Bytes {
        self.tools + self.others
    }
}

impl AddAssign for InputBytes {
    fn add_assign(&mut self, other: InputBytes) {
        self.tools += other.tools;
        self.others += other.others;
    }
}

/// Which content of a spawn's files is measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ByteMeasure {
    Outputs,
    /// Every input, its tools among them.
    Inputs,
    /// The inputs that are the spawn's tools.
    Tools,
}

impl ByteMeasure {
    pub const ALL: [ByteMeasure; 3] = [
        ByteMeasure::Outputs,
        ByteMeasure::Inputs,
        ByteMeasure::Tools,
    ];

    /// Where `self` stands in [`ByteMeasure::ALL`].
    pub const fn index(self) -> usize {
        self as usize
    }
}

/// The content of the files a spawn, or several spawns in sum, wrote and
/// read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SpawnBytes {
    pub outputs: Bytes,
    pub inputs: InputBytes,
}

impl SpawnBytes {
    pub fn of(spawn: &LoggedSpawn<'_>) -> Self {
        Self {
            outputs: spawn.output_bytes(),
            inputs: spawn.input_bytes(),
        }
    }

    pub fn measure(self, measure: ByteMeasure) -> Bytes {
        match measure {
            ByteMeasure::Outputs => self.outputs,
            ByteMeasure::Inputs => self.inputs.all(),
            ByteMeasure::Tools => self.inputs.tools,
        }
    }
}

impl AddAssign for SpawnBytes {
    fn add_assign(&mut self, other: SpawnBytes) {
        self.outputs += other.outputs;
        self.inputs += other.inputs;
    }
}
