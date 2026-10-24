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
    pub fn source(self) -> Option<GraphSource<PathBuf>> {
        GraphFiles::from(self).source()
    }
}

/// The files a joint build graph is read from, at most one of each kind.
#[derive(Debug)]
pub struct GraphFiles<F> {
    aquery: Option<F>,
    cquery: Option<F>,
    query: Option<F>,
}

impl<F> Default for GraphFiles<F> {
    fn default() -> Self {
        Self {
            aquery: None,
            cquery: None,
            query: None,
        }
    }
}

impl From<GraphArgs> for GraphFiles<PathBuf> {
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

impl<F> GraphFiles<F> {
    /// The joint graph these files build, and `None` where none was given.
    pub fn source(self) -> Option<GraphSource<F>> {
        let Self {
            aquery,
            cquery,
            query,
        } = self;
        let inputs: Vec<Input<F>> = [
            (InputKind::Aquery, aquery),
            (InputKind::Cquery, cquery),
            (InputKind::Query, query),
        ]
        .into_iter()
        .filter_map(|(kind, file)| file.map(|file| Input { kind, file }))
        .collect();
        (!inputs.is_empty()).then_some(GraphSource { inputs })
    }

    /// The file of `kind` among these, to be read or given.
    pub fn file_mut(&mut self, kind: InputKind) -> &mut Option<F> {
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
pub struct Input<F> {
    pub kind: InputKind,
    pub file: F,
}

/// One build graph, merged from every input given for it: at least one, and
/// at most one of each kind, held in the order [`GraphFiles::source`] gives
/// them whatever order they were named in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphSource<F> {
    inputs: Vec<Input<F>>,
}

impl<F> From<Input<F>> for GraphSource<F> {
    fn from(input: Input<F>) -> Self {
        Self {
            inputs: vec![input],
        }
    }
}

impl<F> GraphSource<F> {
    pub fn inputs(self) -> Vec<Input<F>> {
        self.inputs
    }

    pub fn first_and_rest(&self) -> (&Input<F>, &[Input<F>]) {
        (&self.inputs[0], &self.inputs[1..])
    }

    /// The same graph, each of its files named as `name` names it.
    pub fn map<G>(self, mut name: impl FnMut(F) -> G) -> GraphSource<G> {
        let inputs = (self.inputs.into_iter())
            .map(|Input { kind, file }| Input {
                kind,
                file: name(file),
            })
            .collect();
        GraphSource { inputs }
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

    fn source(args: &[&str]) -> Option<GraphSource<PathBuf>> {
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
                    file: path.into(),
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
        let mut files = GraphFiles::<PathBuf>::default();
        *files.file_mut(InputKind::Query) = Some("q.pb".into());
        *files.file_mut(InputKind::Aquery) = Some("a.pb".into());
        let query: Input<PathBuf> = Input {
            kind: InputKind::Query,
            file: "q.pb".into(),
        };
        assert_eq!(files.query, Some(query.file.clone()));
        assert_eq!(
            files.source().expect("two files name a graph").inputs()[1],
            query
        );
        assert_eq!(GraphSource::from(query.clone()).inputs(), [query]);
    }

    #[test]
    fn a_graph_named_anew_keeps_its_kinds_and_order() {
        let renamed = source(&["--query", "q.pb", "--aquery", "a.pb"])
            .expect("two files name a graph")
            .map(|path| path.display().to_string());
        let input = |kind, file: &str| Input {
            kind,
            file: file.to_owned(),
        };
        assert_eq!(
            renamed.inputs(),
            [
                input(InputKind::Aquery, "a.pb"),
                input(InputKind::Query, "q.pb")
            ]
        );
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
