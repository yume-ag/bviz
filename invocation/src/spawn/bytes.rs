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
    /// The content of the inputs no other spawn of the log read, each
    /// distinct content counted once; summed over spawns, the content only
    /// one of them read.
    pub read_alone: Bytes,
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
        self.read_alone += other.read_alone;
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
    /// The content of the inputs only one spawn read.
    ReadAlone,
}

impl ByteMeasure {
    pub const ALL: [ByteMeasure; 4] = [
        ByteMeasure::Outputs,
        ByteMeasure::Inputs,
        ByteMeasure::Tools,
        ByteMeasure::ReadAlone,
    ];

    /// Where `self` stands in [`ByteMeasure::ALL`].
    pub const fn index(self) -> usize {
        self as usize
    }

    /// Whether the measure is of a spawn's inputs, which are measured after
    /// the rest of its log is read.
    pub const fn of_inputs(self) -> bool {
        match self {
            ByteMeasure::Outputs => false,
            ByteMeasure::Inputs | ByteMeasure::Tools | ByteMeasure::ReadAlone => true,
        }
    }
}

/// The content of the files a spawn, or several spawns in sum, wrote and
/// read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpawnBytes {
    pub outputs: Bytes,
    /// `None` while the inputs of a spawn summed are being measured.
    pub inputs: Option<InputBytes>,
}

/// No spawns, which wrote and read nothing.
impl Default for SpawnBytes {
    fn default() -> Self {
        Self {
            outputs: Bytes::ZERO,
            inputs: Some(InputBytes::default()),
        }
    }
}

impl SpawnBytes {
    pub fn of(spawn: &LoggedSpawn<'_>) -> Self {
        Self {
            outputs: spawn.output_bytes(),
            inputs: spawn.input_bytes(),
        }
    }

    /// `None` for a measure of inputs still being measured.
    pub fn measure(self, measure: ByteMeasure) -> Option<Bytes> {
        match measure {
            ByteMeasure::Outputs => Some(self.outputs),
            ByteMeasure::Inputs => Some(self.inputs?.all()),
            ByteMeasure::Tools => Some(self.inputs?.tools),
            ByteMeasure::ReadAlone => Some(self.inputs?.read_alone),
        }
    }
}

impl AddAssign for SpawnBytes {
    fn add_assign(&mut self, other: SpawnBytes) {
        self.outputs += other.outputs;
        self.inputs = self.inputs.zip(other.inputs).map(|(mut sum, other)| {
            sum += other;
            sum
        });
    }
}
