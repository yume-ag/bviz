//! Lays out a Bazel build graph as a compound graph using elk-rs.
//!
//! Reads `--output=streamed_proto` bytes from `aquery`, `cquery`, or `query`
//! into a common package-grouped compound graph and produces absolute-
//! coordinate box and connector geometry. aquery yields a target ->
//! configured-target -> action/artifact hierarchy and cquery a target ->
//! configured-target hierarchy; query yields one node per target. Edges run
//! from each dependency to the target that consumes it.
pub mod aquery;
pub mod cquery;
pub mod filter;
pub mod graph;
pub mod intern;
pub mod joint;
mod label;
pub mod layout;
pub mod lod;
pub mod map;
pub mod query;
pub mod quotient;
pub mod reduce;
pub mod streamed_proto;

#[cfg(test)]
mod test_support;

pub use graph::{EdgeKind, Graph, Kind};
pub use intern::{Id, Names};
pub use joint::build_joint;
pub use layout::{Anchor, Box, Connector, LaidOut, layout, layout_pinned};
pub use map::{MapLayout, map_layout};
