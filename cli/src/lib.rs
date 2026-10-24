//! The command line of the window that shows every kind of Bazel build
//! document: the viewers' flag fragments joined into one parser, and what
//! a command line names for each viewer to read, or the subcommand that writes
//! the bazelrc a build is captured with. It names no window and draws nothing,
//! so a parse can be exercised without a display.

use bviz_bazel_flags_args::BazelrcArgs;
use bviz_ui_console_args::{ConsoleArgs, ConsoleSource};
use bviz_ui_execlog_args::{ExecLogArgs, ExecLogSource};
use bviz_ui_graph_args::{GraphArgs, GraphSource};
use bviz_ui_trace_args::{TraceArgs, TraceSource};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// Opens Bazel build documents: the dependency graph, the action timeline, the
/// console replay, the spawn time by phase.
///
/// The graph and each profile, build event stream and execution log named on
/// the command line open in the one window, their views laid out by role: the
/// graph canvases over the console, beside the profile timeline over the
/// analysis views. The views of one role from several files share its region
/// as tabs.
///
/// Files written by one Bazel invocation open as one document, ordered among
/// the others, in tabs and numbering, by the first of them given. A profile, a
/// build event stream and an execution log each carry the id of the invocation
/// that wrote them, and one file of each kind carrying the same id are that
/// invocation's document. A further file of a kind the invocation holds is a
/// document of its own, and so is a file carrying no id.
///
/// The query, cquery and aquery files carry no id, so bviz cannot check which
/// build a graph describes. It takes the graph for the invocation's when every
/// id the other files carry is the same one, be that a single file's, and opens
/// the graph as a document of its own when they carry several ids or none.
///
/// With no file named, the window opens showing no document.
#[derive(Parser, Debug)]
#[command(
    name = "bviz",
    bin_name = "bviz",
    version,
    about,
    args_conflicts_with_subcommands = true
)]
pub struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    #[command(flatten)]
    graph: GraphArgs,
    #[command(flatten)]
    trace: TraceArgs,
    #[command(flatten)]
    console: ConsoleArgs,
    #[command(flatten)]
    exec_log: ExecLogArgs,
}

#[derive(Subcommand, Debug)]
enum Command {
    Bazelrc(BazelrcArgs),
}

/// What a command line asks bviz to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    Bazelrc(BazelrcArgs),
    Open(Sources),
    /// The window, showing no document.
    Empty,
}

impl Cli {
    pub fn request(self) -> Request {
        match self.command {
            Some(Command::Bazelrc(args)) => Request::Bazelrc(args),
            None => self.sources().map_or(Request::Empty, Request::Open),
        }
    }

    /// What this command line names to open, `None` where it names nothing.
    fn sources(self) -> Option<Sources> {
        let mut files = Vec::with_capacity(
            self.trace.profile.len() + self.console.bep.len() + self.exec_log.exec_log.len(),
        );
        files.extend(self.trace.sources().map(Source::Trace));
        files.extend(self.console.sources().map(Source::Console));
        files.extend(self.exec_log.sources().map(Source::ExecLog));
        if let Some(graph) = self.graph.source() {
            return Some(Sources::Graph { graph, files });
        }
        let mut files = files.into_iter();
        let first = files.next()?;
        Some(Sources::Files {
            first,
            rest: files.collect(),
        })
    }
}

/// A file a viewer reads on its own: a profile, a build event stream or an
/// execution log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    Trace(TraceSource),
    Console(ConsoleSource),
    ExecLog(ExecLogSource),
}

/// What a command line names to open: the one graph its graph flags describe,
/// where it gives any, and each repeatable flag's files in the order given.
/// Either way it names at least one thing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Sources {
    Graph {
        graph: GraphSource<PathBuf>,
        files: Vec<Source>,
    },
    Files {
        first: Source,
        rest: Vec<Source>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use bviz_ui_graph_args::InputKind;
    use clap::CommandFactory;
    use clap::error::ErrorKind;

    fn sources(args: &[&str]) -> Option<Sources> {
        Cli::try_parse_from(std::iter::once("bviz").chain(args.iter().copied()))
            .expect("the command line parses")
            .sources()
    }

    fn request(args: &[&str]) -> Result<Request, clap::Error> {
        Cli::try_parse_from(std::iter::once("bviz").chain(args.iter().copied())).map(Cli::request)
    }

    fn bazelrc(config: &str, dir: &str) -> Request {
        Request::Bazelrc(BazelrcArgs {
            config: config.parse().unwrap(),
            dir: dir.parse().unwrap(),
        })
    }

    fn graph(inputs: &[(InputKind, &str)], files: Vec<Source>) -> Sources {
        let mut graph = bviz_ui_graph_args::GraphFiles::default();
        for &(kind, path) in inputs {
            *graph.file_mut(kind) = Some(path.into());
        }
        Sources::Graph {
            graph: graph.source().expect("the inputs name a graph"),
            files,
        }
    }

    fn files(first: Source, rest: Vec<Source>) -> Sources {
        Sources::Files { first, rest }
    }

    fn trace(path: &str) -> Source {
        Source::Trace(TraceSource { path: path.into() })
    }

    fn console(path: &str) -> Source {
        Source::Console(ConsoleSource { path: path.into() })
    }

    fn exec_log(path: &str) -> Source {
        Source::ExecLog(ExecLogSource { path: path.into() })
    }

    #[test]
    fn a_command_line_naming_no_file_asks_for_an_empty_window() {
        assert_eq!(sources(&[]), None);
        assert_eq!(request(&[]).unwrap(), Request::Empty);
    }

    #[test]
    fn each_flag_names_a_source_of_its_own_kind() {
        assert_eq!(
            sources(&["--aquery", "a.pb"]).unwrap(),
            graph(&[(InputKind::Aquery, "a.pb")], Vec::new())
        );
        assert_eq!(
            sources(&["--cquery", "c.pb"]).unwrap(),
            graph(&[(InputKind::Cquery, "c.pb")], Vec::new())
        );
        assert_eq!(
            sources(&["--query", "q.pb"]).unwrap(),
            graph(&[(InputKind::Query, "q.pb")], Vec::new())
        );
        assert_eq!(
            sources(&["--profile", "p.gz"]).unwrap(),
            files(trace("p.gz"), Vec::new())
        );
        assert_eq!(
            sources(&["--bep", "b.bep"]).unwrap(),
            files(console("b.bep"), Vec::new())
        );
        assert_eq!(
            sources(&["--exec_log", "e.zst"]).unwrap(),
            files(exec_log("e.zst"), Vec::new())
        );
        assert_eq!(
            sources(&["--profile", "one.gz", "--profile", "two.gz"]).unwrap(),
            files(trace("one.gz"), vec![trace("two.gz")])
        );
    }

    /// Files of every kind open together, the graph ahead of the rest wherever
    /// its flags stand.
    #[test]
    fn a_graph_and_one_file_of_each_kind_are_four_sources() {
        let named = sources(&[
            "--exec_log",
            "e.zst",
            "--profile",
            "p.gz",
            "--bep",
            "b.bep",
            "--aquery",
            "a.pb",
        ])
        .unwrap();
        assert_eq!(
            named,
            graph(
                &[(InputKind::Aquery, "a.pb")],
                vec![trace("p.gz"), console("b.bep"), exec_log("e.zst")]
            )
        );
    }

    /// The flags are the surface users' scripts are written against, so their
    /// names and the file placeholder they take are pinned here.
    #[test]
    fn the_flags_keep_their_names() {
        let help = Cli::command().render_long_help().to_string();
        for flag in [
            "--aquery",
            "--cquery",
            "--query",
            "--profile",
            "--bep",
            "--exec_log",
        ] {
            assert!(
                help.contains(&format!("{flag} <FILE>")),
                "no {flag}: {help}"
            );
        }
    }

    #[test]
    fn the_bazelrc_subcommand_takes_a_config_name_and_a_directory() {
        assert_eq!(
            request(&["bazelrc"]).unwrap(),
            Request::Bazelrc(BazelrcArgs::default())
        );
        assert_eq!(
            request(&["bazelrc", "--config", "ci", "--dir=out/x"]).unwrap(),
            bazelrc("ci", "out/x")
        );
    }

    /// The words of each bviz command line `rc`'s header shows, joined across
    /// `\` continuations and cut at a `>` redirect.
    fn header_commands(rc: &str) -> Vec<Vec<String>> {
        let mut lines = rc
            .lines()
            .map_while(|line| line.strip_prefix('#'))
            .map(str::trim);
        let mut commands = Vec::new();
        while let Some(line) = lines.next() {
            if !line.starts_with("bviz ") {
                continue;
            }
            let mut command = line.to_owned();
            while let Some(head) = command.strip_suffix('\\') {
                command = format!("{head}{}", lines.next().expect("a continued line"));
            }
            commands.push(
                command
                    .split_whitespace()
                    .take_while(|&word| word != ">")
                    .map(str::to_owned)
                    .collect(),
            );
        }
        commands
    }

    /// Each bviz command line the header shows parses as the command it is
    /// shown as: the one opening every captured file, and the one writing the
    /// same bazelrc again.
    #[test]
    fn the_bazelrc_header_shows_bviz_command_lines_bviz_parses() {
        for args in [
            BazelrcArgs::default(),
            BazelrcArgs {
                config: "ci".parse().unwrap(),
                dir: "out/capture".parse().unwrap(),
            },
        ] {
            let commands = header_commands(&args.bazelrc());
            let requests: Vec<Request> = commands
                .iter()
                .map(|words| {
                    Cli::try_parse_from(words)
                        .map(Cli::request)
                        .unwrap_or_else(|e| panic!("{words:?}: {e}"))
                })
                .collect();
            let [Request::Open(Sources::Graph { graph, files }), regenerate] = &requests[..] else {
                panic!("{commands:?}");
            };
            assert_eq!(graph.first_and_rest().1.len(), 2, "{commands:?}");
            assert_eq!(files.len(), 3, "{commands:?}");
            assert_eq!(regenerate, &Request::Bazelrc(args));
        }
    }

    #[test]
    fn a_config_name_no_bazelrc_section_can_hold_is_a_usage_error() {
        let err = request(&["bazelrc", "--config=build:x"]).expect_err("a colon");
        assert_eq!(err.kind(), ErrorKind::ValueValidation);
    }

    /// The subcommand opens no window, so it takes no file to open.
    #[test]
    fn the_bazelrc_subcommand_takes_no_file_flag() {
        assert!(request(&["--profile", "p.gz", "bazelrc"]).is_err());
        assert!(request(&["bazelrc", "--profile", "p.gz"]).is_err());
    }

    #[test]
    fn a_command_line_without_the_subcommand_opens_what_it_names() {
        assert_eq!(
            request(&["--profile", "p.gz"]).unwrap(),
            Request::Open(files(trace("p.gz"), Vec::new()))
        );
    }
}
