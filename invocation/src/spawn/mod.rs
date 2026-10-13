//! The spawns an invocation ran, as its execution log records them.

mod bytes;
mod logged;
mod timing;

pub use bytes::Bytes;
pub use logged::{Builder, Logged, LoggedSpawn, LoggedSpawns, SpawnRunner};
pub use timing::{Phase, PhaseTiming, REPORTED_PHASES};
