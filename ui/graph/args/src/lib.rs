//! The flags naming a joint build graph's inputs, and the documents they
//! resolve to. Nothing here opens a window or draws, so a parse can be
//! exercised without a display.

use clap::Args;
use std::path::PathBuf;

/// The long name of [`GraphArgs::aquery`], for messages that name the flag.
pub const AQUERY_LONG: &str = "aquery";
/// The long name of [`GraphArgs::cquery`], for messages that name the flag.
pub const CQUERY_LONG: &str = "cquery";
/// The long name of [`GraphArgs::query`], for messages that name the flag.
pub const QUERY_LONG: &str = "query";

/// The flags naming the files a joint build graph is read from.
#[derive(Args, Debug, Default)]
#[command(next_help_heading = "Graph (one per command line; each file given at most once)")]
pub struct GraphArgs {
    /// `bazel aquery --output=streamed_proto` file.
    #[arg(long = AQUERY_LONG, value_name = "FILE")]
    pub aquery: Option<PathBuf>,
    /// `bazel cquery --output=streamed_proto` file (run cquery with
    /// `--transitions=lite` for configuration-resolved edges).
    #[arg(long = CQUERY_LONG, value_name = "FILE")]
    pub cquery: Option<PathBuf>,
    /// `bazel query --output=streamed_proto` file.
    #[arg(long = QUERY_LONG, value_name = "FILE")]
    pub query: Option<PathBuf>,
}

impl GraphArgs {
    /// The joint graph these flags name, and `None` where they name none.
    pub fn source(self) -> Option<GraphSource> {
        GraphFiles::from(self).source()
    }
}

/// The files a joint build graph is read from, at most one of each kind.
#[derive(Debug, Default)]
pub struct GraphFiles {
    aquery: Option<PathBuf>,
    cquery: Option<PathBuf>,
    query: Option<PathBuf>,
}

impl From<GraphArgs> for GraphFiles {
    fn from(args: GraphArgs) -> Self {
        let GraphArgs {
            aquery,
            cquery,
            query,
        } = args;
        Self {
            aquery,
            cquery,
            query,
        }
    }
}

impl GraphFiles {
    /// The joint graph these files build, and `None` where none was given.
    pub fn source(self) -> Option<GraphSource> {
        let Self {
            aquery,
            cquery,
            query,
        } = self;
        let inputs: Vec<Input> = [
            (InputKind::Aquery, aquery),
            (InputKind::Cquery, cquery),
            (InputKind::Query, query),
        ]
        .into_iter()
        .filter_map(|(kind, path)| path.map(|path| Input { kind, path }))
        .collect();
        (!inputs.is_empty()).then_some(GraphSource { inputs })
    }

    /// The file of `kind` among these, to be read or given.
    pub fn file_mut(&mut self, kind: InputKind) -> &mut Option<PathBuf> {
        match kind {
            InputKind::Aquery => &mut self.aquery,
            InputKind::Cquery => &mut self.cquery,
            InputKind::Query => &mut self.query,
        }
    }
}

/// Which Bazel query produced a `--output=streamed_proto` file, selecting the
/// reader and builder used to turn it into a graph.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputKind {
    Aquery,
    Cquery,
    Query,
}

impl InputKind {
    /// The long name of the flag a file of this kind is given by.
    pub fn flag(self) -> &'static str {
        match self {
            InputKind::Aquery => AQUERY_LONG,
            InputKind::Cquery => CQUERY_LONG,
            InputKind::Query => QUERY_LONG,
        }
    }

    /// The Bazel command whose `--output=streamed_proto` a file of this kind
    /// holds.
    pub fn command(self) -> &'static str {
        match self {
            InputKind::Aquery => "aquery",
            InputKind::Cquery => "cquery",
            InputKind::Query => "query",
        }
    }
}

/// One file a joint graph is read from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Input {
    pub kind: InputKind,
    pub path: PathBuf,
}

/// One build graph, merged from every input given for it: at least one, and
/// at most one of each kind, held in the order [`GraphFiles::source`] gives
/// them whatever order they were named in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphSource {
    inputs: Vec<Input>,
}

impl From<Input> for GraphSource {
    fn from(input: Input) -> Self {
        Self {
            inputs: vec![input],
        }
    }
}

impl GraphSource {
    pub fn inputs(self) -> Vec<Input> {
        self.inputs
    }

    pub fn first_and_rest(&self) -> (&Input, &[Input]) {
        (&self.inputs[0], &self.inputs[1..])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, Parser};

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        graph: GraphArgs,
    }

    fn source(args: &[&str]) -> Option<GraphSource> {
        Cli::try_parse_from(std::iter::once("graph").chain(args.iter().copied()))
            .expect("the flags parse")
            .graph
            .source()
    }

    #[test]
    fn naming_no_query_file_names_no_graph() {
        assert_eq!(source(&[]), None);
    }

    /// Every query file given feeds the one graph, in a fixed order whatever
    /// order the flags came in.
    #[test]
    fn the_query_files_merge_into_a_single_graph() {
        let inputs = |paths: &[(InputKind, &str)]| {
            paths
                .iter()
                .map(|&(kind, path)| Input {
                    kind,
                    path: path.into(),
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            source(&["--query", "q.pb", "--aquery", "a.pb", "--cquery", "c.pb"])
                .expect("three files name a graph")
                .inputs(),
            inputs(&[
                (InputKind::Aquery, "a.pb"),
                (InputKind::Cquery, "c.pb"),
                (InputKind::Query, "q.pb"),
            ])
        );
    }

    #[test]
    fn a_file_given_by_its_kind_is_read_as_that_kind() {
        let mut files = GraphFiles::default();
        *files.file_mut(InputKind::Query) = Some("q.pb".into());
        *files.file_mut(InputKind::Aquery) = Some("a.pb".into());
        let query = Input {
            kind: InputKind::Query,
            path: "q.pb".into(),
        };
        assert_eq!(files.query, Some(query.path.clone()));
        assert_eq!(
            files.source().expect("two files name a graph").inputs()[1],
            query
        );
        assert_eq!(GraphSource::from(query.clone()).inputs(), [query]);
    }

    /// One graph is drawn per command line, so a second `--aquery` would
    /// replace the first rather than open a second graph. The help says so,
    /// since nothing in the parse can.
    #[test]
    fn the_help_says_a_graph_is_built_once_per_command_line() {
        let help = Cli::command().render_long_help().to_string();
        assert!(
            help.contains("one per command line; each file given at most once"),
            "the help leaves the graph's arity unsaid: {help}"
        );
    }
}
