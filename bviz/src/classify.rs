//! What each file named to a running window holds, told from its head: its
//! kind, and for a file carrying one, the invocation that wrote it.

use crate::group::{File, Kind};
use bviz_input::Input;
use bviz_invocation::InvocationId;
use bviz_ui_graph_args::{GraphFiles, GraphSource, Input as GraphInput, InputKind};
use std::io;
use std::path::PathBuf;

/// The files named together, by what they were found to hold.
pub(crate) struct Classified {
    /// The query outputs merging into one graph, the first named of each kind.
    pub(crate) graph: GraphFiles,
    /// Each query output of a kind `graph` holds a file of, a graph of its own.
    pub(crate) further_graphs: Vec<GraphSource>,
    /// The files of a [`Kind`], in the order they were named.
    pub(crate) files: Vec<File<Input>>,
    /// The files whose head matches no kind bviz reads.
    pub(crate) unknown: Vec<PathBuf>,
    pub(crate) unreadable: Vec<(PathBuf, io::Error)>,
}

/// Reads the head of each of `paths`, once, to tell what it holds. Blocks on
/// the file system, so it runs off the UI thread.
pub(crate) fn classify(paths: Vec<PathBuf>) -> Classified {
    let mut classified = Classified::with_capacity(paths.len());
    for path in paths {
        if let Some((path, input)) = classified.open(path) {
            let kind = input.head().and_then(bviz_sniff::kind_of);
            classified.sort(path, input, holds(kind));
        }
    }
    classified
}

/// Opens each of `chosen` as the kind chosen for it, whatever its head
/// tells. Blocks on the file system, so it runs off the UI thread.
pub(crate) fn classify_as(chosen: Vec<(PathBuf, bviz_sniff::Kind)>) -> Classified {
    let mut classified = Classified::with_capacity(chosen.len());
    for (path, kind) in chosen {
        if let Some((path, input)) = classified.open(path) {
            classified.sort(path, input, holds(Some(kind)));
        }
    }
    classified
}

/// Each kind a user can choose to open a file as, and what it is called.
pub(crate) const KINDS: [(bviz_sniff::Kind, &str); 6] = {
    use bviz_sniff::Kind as Sniffed;
    match Sniffed::Profile {
        Sniffed::Profile
        | Sniffed::Bep
        | Sniffed::ExecLog
        | Sniffed::Query
        | Sniffed::Cquery
        | Sniffed::Aquery => {}
    }
    [
        (Sniffed::Profile, "Profile"),
        (Sniffed::Bep, "Build events"),
        (Sniffed::ExecLog, "Execution log"),
        (Sniffed::Query, "Query"),
        (Sniffed::Cquery, "Cquery"),
        (Sniffed::Aquery, "Aquery"),
    ]
};

impl Classified {
    fn with_capacity(paths: usize) -> Self {
        Self {
            graph: GraphFiles::default(),
            further_graphs: Vec::new(),
            files: Vec::with_capacity(paths),
            unknown: Vec::new(),
            unreadable: Vec::new(),
        }
    }

    /// `path` opened, or recorded as unreadable where it does not open.
    fn open(&mut self, path: PathBuf) -> Option<(PathBuf, Input)> {
        match Input::open(&path) {
            Ok(input) => Some((path, input)),
            Err(error) => {
                self.unreadable.push((path, error));
                None
            }
        }
    }

    /// Files `input`, read from `path`, by what it `holds`.
    fn sort(&mut self, path: PathBuf, input: Input, holds: Holds) {
        match holds {
            Holds::File(kind, probe) => self.files.push(File {
                kind,
                invocation: probe(&input),
                file: input,
            }),
            Holds::Graph(kind) => match self.graph.file_mut(kind) {
                slot @ None => *slot = Some(path),
                Some(_) => (self.further_graphs).push(GraphInput { kind, path }.into()),
            },
            Holds::Unknown => self.unknown.push(path),
        }
    }
}

/// What a file of the kind its head tells holds, to bviz.
enum Holds {
    /// A file of `Kind`, which the probe reads the invocation id of.
    File(Kind, fn(&Input) -> Option<InvocationId>),
    /// A query output, part of a graph.
    Graph(InputKind),
    Unknown,
}

fn holds(kind: Option<bviz_sniff::Kind>) -> Holds {
    use bviz_sniff::Kind as Sniffed;
    match kind {
        Some(Sniffed::Profile) => Holds::File(Kind::Profile, bviz_ui_trace::invocation_id),
        Some(Sniffed::Bep) => Holds::File(Kind::Bep, bviz_ui_console::invocation_id),
        Some(Sniffed::ExecLog) => Holds::File(Kind::ExecLog, bviz_ui_execlog::invocation_id),
        Some(Sniffed::Query) => Holds::Graph(InputKind::Query),
        Some(Sniffed::Cquery) => Holds::Graph(InputKind::Cquery),
        Some(Sniffed::Aquery) => Holds::Graph(InputKind::Aquery),
        None => Holds::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn scratch() -> PathBuf {
        std::env::var_os("TEST_TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
    }

    /// A real `--build_event_binary_file`.
    const SAMPLE_BEP: &[u8] = include_bytes!(env!("SAMPLE_BEP"));

    fn sample_bep() -> PathBuf {
        let path = scratch().join("sample.bep");
        std::fs::write(&path, SAMPLE_BEP).unwrap();
        path
    }

    #[test]
    fn a_build_event_stream_is_told_with_the_invocation_it_names() {
        let classified = classify(vec![sample_bep()]);
        let [file] = &classified.files[..] else {
            panic!("one file of a kind");
        };
        assert!(file.kind == Kind::Bep);
        assert!(file.invocation.is_some());
        assert!(classified.graph.source().is_none() && classified.unknown.is_empty());
    }

    #[test]
    fn a_file_of_no_kind_is_unknown_and_a_missing_one_unreadable() {
        let dir = scratch();
        let unknown = dir.join("notes.txt");
        std::fs::write(&unknown, "not a build output").unwrap();
        let missing = dir.join("missing.bep");
        let classified = classify(vec![unknown.clone(), missing.clone()]);
        assert!(classified.files.is_empty() && classified.graph.source().is_none());
        assert_eq!(classified.unknown, [unknown]);
        let unreadable: Vec<&Path> = classified
            .unreadable
            .iter()
            .map(|(path, _)| path.as_path())
            .collect();
        assert_eq!(unreadable, [missing.as_path()]);
    }

    /// A file chosen as a kind is opened as one, whatever its head tells.
    #[test]
    fn a_file_chosen_as_a_kind_opens_as_that_kind() {
        let unknown = scratch().join("chosen.txt");
        std::fs::write(&unknown, "not a build output").unwrap();
        let classified = classify_as(vec![(unknown, bviz_sniff::Kind::Profile)]);
        let [file] = &classified.files[..] else {
            panic!("one file of a kind");
        };
        assert!(file.kind == Kind::Profile);
        assert!(classified.unknown.is_empty());
    }

    /// A `bazel query --output=streamed_proto` stream of one target: a
    /// length-delimited `blaze_query.Target` of type `RULE` holding a rule
    /// named `//a`.
    const QUERY_STREAM: &[u8] = &[9, 0x08, 1, 0x12, 5, 0x0a, 3, b'/', b'/', b'a'];

    /// Of two query outputs of one kind, the first named feeds the merged graph
    /// and the second is a graph of its own.
    #[test]
    fn a_second_query_output_of_a_kind_is_a_graph_of_its_own() {
        let dir = scratch();
        let (first, second) = (dir.join("first.query.pb"), dir.join("second.query.pb"));
        for path in [&first, &second] {
            std::fs::write(path, QUERY_STREAM).unwrap();
        }
        let classified = classify(vec![first.clone(), second.clone()]);
        let query = |path: &PathBuf| GraphInput {
            kind: InputKind::Query,
            path: path.clone(),
        };
        let graph = classified
            .graph
            .source()
            .expect("a query output names a graph");
        assert_eq!(graph.inputs(), [query(&first)]);
        assert_eq!(
            classified.further_graphs,
            [GraphSource::from(query(&second))]
        );
    }
}
