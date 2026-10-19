//! Which of the files named on the command line open as one document: the
//! outputs of one Bazel invocation, told by the invocation id each carries.

use bviz_invocation::InvocationId;
use std::iter;

/// The kinds of file that carry an invocation id. An invocation's document
/// holds one of each.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Kind {
    Profile,
    Bep,
    ExecLog,
}

/// A file of one of the [`Kind`]s.
pub(crate) struct File<T> {
    pub(crate) file: T,
    pub(crate) kind: Kind,
    /// `None` where the file carries no id.
    pub(crate) invocation: Option<InvocationId>,
}

/// What `bviz_cli::Sources` names, each file with its kind and the invocation
/// it carries the id of.
pub(crate) enum Given<T> {
    Graph { graph: T, files: Vec<File<T>> },
    Files { first: File<T>, rest: Vec<File<T>> },
}

/// What a window opens as one document.
pub(crate) enum Opening<T> {
    /// A file by itself, of the invocation `invocation` where the file names
    /// the one that wrote it.
    Alone {
        file: T,
        invocation: Option<InvocationId>,
    },
    /// Files of the invocation `id`, in the order they were given.
    Invocation {
        id: InvocationId,
        first: T,
        second: T,
        rest: Vec<T>,
    },
}

/// A document while the files after its first are still being gone through.
enum Gathering<T> {
    Alone {
        file: T,
        invocation: Option<InvocationId>,
    },
    Invocation(Gathered<T>),
}

/// The files gathered for the invocation `id`.
struct Gathered<T> {
    id: InvocationId,
    first: T,
    rest: Vec<T>,
    /// The kinds among those files.
    held: Vec<Kind>,
}

impl<T> Gathering<T> {
    /// What `file` opens as where no invocation gathered so far is its own.
    fn begin(file: File<T>) -> Self {
        match file.invocation {
            Some(id) => Self::Invocation(Gathered {
                id,
                first: file.file,
                rest: Vec::new(),
                held: vec![file.kind],
            }),
            None => Self::Alone {
                file: file.file,
                invocation: None,
            },
        }
    }

    fn invocation(&mut self, id: &InvocationId) -> Option<&mut Gathered<T>> {
        match self {
            Self::Invocation(gathered) if gathered.id == *id => Some(gathered),
            _ => None,
        }
    }

    /// An invocation's document holds two files at least; an invocation only
    /// one file was gathered for opens as that file.
    fn gathered(self) -> Opening<T> {
        match self {
            Self::Alone { file, invocation } => Opening::Alone { file, invocation },
            Self::Invocation(gathered) => {
                let mut rest = gathered.rest.into_iter();
                match rest.next() {
                    Some(second) => Opening::Invocation {
                        id: gathered.id,
                        first: gathered.first,
                        second,
                        rest: rest.collect(),
                    },
                    None => Opening::Alone {
                        file: gathered.first,
                        invocation: Some(gathered.id),
                    },
                }
            }
        }
    }
}

/// The invocation `files` name, where those carrying an id all carry the same
/// one.
fn only_invocation<T>(files: &[File<T>]) -> Option<&InvocationId> {
    let mut named = files.iter().filter_map(|file| file.invocation.as_ref());
    let first = named.next()?;
    named.all(|id| id == first).then_some(first)
}

/// What `given` names as the documents a window opens, in the order their first
/// files were given.
///
/// Files carrying one invocation id form one document, which takes one file of
/// each [`Kind`]; a further file of a kind the document holds opens alone, as
/// does a file carrying no id. A graph carries none either: nothing tells which
/// build it describes, so it is taken for the one invocation the files with it
/// name, and opens alone where they name several or none.
pub(crate) fn by_invocation<T>(given: Given<T>) -> (Opening<T>, Vec<Opening<T>>) {
    let (mut leading, files) = match given {
        Given::Graph { graph, files } => {
            let leading = match only_invocation(&files) {
                Some(id) => Gathering::Invocation(Gathered {
                    id: id.clone(),
                    first: graph,
                    rest: Vec::new(),
                    held: Vec::new(),
                }),
                None => Gathering::Alone {
                    file: graph,
                    invocation: None,
                },
            };
            (leading, files)
        }
        Given::Files { first, rest } => (Gathering::begin(first), rest),
    };
    let mut others = Vec::with_capacity(files.len());
    for file in files {
        let gathered = file.invocation.as_ref().and_then(|id| {
            iter::once(&mut leading)
                .chain(&mut others)
                .find_map(|gathering| gathering.invocation(id))
        });
        let apart = match gathered {
            Some(gathered) if !gathered.held.contains(&file.kind) => {
                gathered.held.push(file.kind);
                gathered.rest.push(file.file);
                continue;
            }
            Some(_) => Gathering::Alone {
                file: file.file,
                invocation: file.invocation,
            },
            None => Gathering::begin(file),
        };
        others.push(apart);
    }
    (
        leading.gathered(),
        others.into_iter().map(Gathering::gathered).collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use bviz_ui_workspace::invocation_label;

    const A: &str = "a5f7f3e6-4fda-4675-bc16-8ede5420b210";
    const B: &str = "0c3d9a77-14be-4f0e-9d0a-55817e2200e2";

    fn id(id: &str) -> InvocationId {
        InvocationId::new(id).expect("a non-empty id")
    }

    fn profile(name: &'static str, of: Option<&str>) -> File<&'static str> {
        File {
            file: name,
            kind: Kind::Profile,
            invocation: of.map(id),
        }
    }

    fn bep(name: &'static str, of: Option<&str>) -> File<&'static str> {
        File {
            file: name,
            kind: Kind::Bep,
            invocation: of.map(id),
        }
    }

    fn exec_log(name: &'static str, of: Option<&str>) -> File<&'static str> {
        File {
            file: name,
            kind: Kind::ExecLog,
            invocation: of.map(id),
        }
    }

    /// The documents `given` opens as, each written as its files' names: a
    /// invocation's as the label it opens under, then its files.
    fn grouped(given: Given<&'static str>) -> Vec<Vec<String>> {
        let (leading, others) = by_invocation(given);
        iter::once(leading)
            .chain(others)
            .map(|opening| match opening {
                Opening::Alone { file, .. } => vec![file.to_string()],
                Opening::Invocation {
                    id,
                    first,
                    second,
                    rest,
                } => iter::once(invocation_label(&id).to_string())
                    .chain([first, second].into_iter().chain(rest).map(String::from))
                    .collect(),
            })
            .collect()
    }

    fn files(files: Vec<File<&'static str>>) -> Vec<Vec<String>> {
        let mut files = files.into_iter();
        let first = files.next().expect("a window opens at least one file");
        grouped(Given::Files {
            first,
            rest: files.collect(),
        })
    }

    fn with_graph(files: Vec<File<&'static str>>) -> Vec<Vec<String>> {
        grouped(Given::Graph { graph: "g", files })
    }

    #[test]
    fn files_naming_one_invocation_open_as_one_document() {
        assert_eq!(
            files(vec![profile("p", Some(A)), bep("b", Some(A))]),
            [["Invocation a5f7f3e6", "p", "b"]]
        );
    }

    #[test]
    fn an_execution_log_joins_its_invocation() {
        assert_eq!(
            files(vec![
                exec_log("e", Some(A)),
                profile("p", Some(A)),
                bep("b", Some(A)),
                exec_log("e2", Some(A)),
            ]),
            [vec!["Invocation a5f7f3e6", "e", "p", "b"], vec!["e2"]]
        );
    }

    #[test]
    fn files_naming_different_invocations_open_apart() {
        assert_eq!(
            files(vec![profile("p", Some(A)), bep("b", Some(B))]),
            [["p"], ["b"]]
        );
    }

    #[test]
    fn a_file_naming_no_invocation_opens_alone() {
        assert_eq!(
            files(vec![
                profile("p", Some(A)),
                profile("unnamed", None),
                bep("b", Some(A)),
                bep("silent", None),
            ]),
            [
                vec!["Invocation a5f7f3e6", "p", "b"],
                vec!["unnamed"],
                vec!["silent"],
            ]
        );
        assert_eq!(
            files(vec![profile("p", None), bep("b", None)]),
            [["p"], ["b"]]
        );
    }

    /// An invocation's document shows one timeline and one console, so a second
    /// profile or stream of the same invocation is a document of its own.
    #[test]
    fn an_invocation_takes_one_file_of_each_kind() {
        assert_eq!(
            files(vec![
                profile("p1", Some(A)),
                profile("p2", Some(A)),
                bep("b1", Some(A)),
                bep("b2", Some(A)),
                profile("p3", Some(A)),
            ]),
            [
                vec!["Invocation a5f7f3e6", "p1", "b1"],
                vec!["p2"],
                vec!["b2"],
                vec!["p3"],
            ]
        );
    }

    #[test]
    fn a_lone_file_opens_as_itself() {
        assert_eq!(files(vec![profile("p", Some(A))]), [["p"]]);
        assert_eq!(with_graph(Vec::new()), [["g"]]);
        assert_eq!(
            files(vec![profile("p1", Some(A)), profile("p2", Some(A))]),
            [["p1"], ["p2"]]
        );
    }

    /// A graph names no invocation, so it is taken for the one the files given
    /// with it agree on, be that a single file. A file naming none has no say.
    #[test]
    fn a_graph_joins_the_one_invocation_the_other_files_name() {
        assert_eq!(
            with_graph(vec![profile("p", Some(A)), bep("b", Some(A))]),
            [["Invocation a5f7f3e6", "g", "p", "b"]]
        );
        assert_eq!(
            with_graph(vec![bep("b", Some(A))]),
            [["Invocation a5f7f3e6", "g", "b"]]
        );
        assert_eq!(
            with_graph(vec![profile("unnamed", None), bep("b", Some(A))]),
            [vec!["Invocation a5f7f3e6", "g", "b"], vec!["unnamed"]]
        );
        assert_eq!(
            with_graph(vec![profile("p1", Some(A)), profile("p2", Some(A))]),
            [vec!["Invocation a5f7f3e6", "g", "p1"], vec!["p2"]]
        );
    }

    #[test]
    fn a_graph_opens_alone_where_the_files_name_several_invocations_or_none() {
        assert_eq!(
            with_graph(vec![profile("p", Some(A)), bep("b", Some(B))]),
            [["g"], ["p"], ["b"]]
        );
        assert_eq!(with_graph(vec![profile("p", None)]), [["g"], ["p"]]);
    }

    /// Two builds given together are two documents, in the order their first
    /// files were given.
    #[test]
    fn documents_keep_the_order_of_their_first_file() {
        assert_eq!(
            files(vec![
                profile("pa", Some(A)),
                profile("lone", None),
                profile("pb", Some(B)),
                bep("bb", Some(B)),
                bep("ba", Some(A)),
            ]),
            [
                vec!["Invocation a5f7f3e6", "pa", "ba"],
                vec!["lone"],
                vec!["Invocation 0c3d9a77", "pb", "bb"],
            ]
        );
    }

    /// The invocation each document opening by itself is of, by its id.
    fn lone_invocations(given: Given<&'static str>) -> Vec<(&'static str, Option<String>)> {
        let (leading, others) = by_invocation(given);
        iter::once(leading)
            .chain(others)
            .filter_map(|opening| match opening {
                Opening::Alone { file, invocation } => {
                    Some((file, invocation.map(|id| id.as_str().to_string())))
                }
                Opening::Invocation { .. } => None,
            })
            .collect()
    }

    /// A file opening by itself keeps the invocation it names, whether it is
    /// the only file of it or one of a kind its invocation's document holds; a
    /// graph names none.
    #[test]
    fn a_file_opening_by_itself_keeps_the_invocation_it_names() {
        let given = Given::Files {
            first: profile("pa", Some(A)),
            rest: vec![profile("pb", Some(A)), bep("b", None)],
        };
        assert_eq!(
            lone_invocations(given),
            [
                ("pa", Some(A.to_string())),
                ("pb", Some(A.to_string())),
                ("b", None),
            ]
        );
        let graph = Given::Graph {
            graph: "g",
            files: vec![profile("pa", Some(A)), profile("pb", Some(B))],
        };
        assert_eq!(
            lone_invocations(graph),
            [
                ("g", None),
                ("pa", Some(A.to_string())),
                ("pb", Some(B.to_string())),
            ]
        );
    }
}
