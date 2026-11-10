//! Lays out a Bazel build graph as a compound graph using elk-rs.
//!
//! Reads `--output=streamed_proto` bytes from `aquery`, `cquery`, or `query`
//! into a common package-grouped compound graph and produces absolute-
//! coordinate box and connector geometry. aquery yields a target ->
//! configured-target -> action/artifact hierarchy and cquery a target ->
//! configured-target hierarchy; query yields one node per target. Edges run
//! from each dependency to the target that consumes it.
pub mod actions;
pub mod aquery;
pub mod capability;
pub mod cquery;
pub mod dominators;
pub mod exec_path;
pub mod facets;
pub mod filter;
pub mod graph;
pub mod intern;
pub mod joint;
mod label;
pub mod layout;
pub mod lod;
pub mod map;
pub mod metrics;
pub mod paths;
pub mod provenance;
pub mod query;
pub mod query_lang;
pub mod quotient;
mod reach;
pub mod reduce;
pub use bviz_label::repo;
pub mod select;
pub use bviz_label::shown;
mod sketch;
pub mod streamed_proto;
pub mod target_pattern;
pub mod target_set;
pub mod transition;

#[cfg(test)]
mod test_support;

pub use actions::{ActionDependencies, action_dependencies};
pub use capability::{Axis, Capabilities, Support};
pub use facets::Facets;
pub use graph::{EdgeKind, Graph, Kind};
pub use intern::{Id, Names};
pub use joint::{Joint, build_joint};
pub use label::Resolver;
pub use layout::{Anchor, Box, Connector, LaidOut, layout, layout_pinned};
pub use map::{MapLayout, map_layout};
pub use provenance::{Input, InputSet, Provenance};

/// The switch that has each stage of loading and laying out a graph report
/// how long it took.
pub const TIMING_SWITCH: &str = "BVIZ_GRAPH_TIMING";

/// Runs the graph stage `label`, reporting how long it took when
/// [`TIMING_SWITCH`] is set.
pub fn timed<T>(label: &str, stage: impl FnOnce() -> T) -> T {
    bviz_switches::timed(TIMING_SWITCH, label, stage)
}
