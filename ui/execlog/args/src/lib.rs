//! The flags naming the compact execution logs spawn time and bytes are read
//! from, and the documents they resolve to. Nothing here opens a window or
//! draws, so a parse can be exercised without a display.

use clap::Args;
use std::path::PathBuf;

/// The long name of [`ExecLogArgs::exec_log`], for messages that name the flag.
pub const EXEC_LOG_LONG: &str = "exec_log";

/// The compact execution logs spawn time and bytes are read from.
#[derive(Args, Debug)]
#[command(next_help_heading = "Spawn time and bytes (one document per file)")]
pub struct ExecLogArgs {
    /// `bazel build --execution_log_compact_file=FILE` output.
    #[arg(long = EXEC_LOG_LONG, value_name = "FILE")]
    pub exec_log: Vec<PathBuf>,
}

impl ExecLogArgs {
    pub fn sources(self) -> impl Iterator<Item = ExecLogSource> {
        self.exec_log.into_iter().map(|path| ExecLogSource { path })
    }
}

/// One execution log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecLogSource {
    pub path: PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        exec_log: ExecLogArgs,
    }

    fn sources(args: &[&str]) -> Vec<ExecLogSource> {
        Cli::try_parse_from(std::iter::once("execlog").chain(args.iter().copied()))
            .expect("the flags parse")
            .exec_log
            .sources()
            .collect()
    }

    #[test]
    fn a_repeated_exec_log_names_a_document_each() {
        assert_eq!(
            sources(&["--exec_log", "one.binpb.zst", "--exec_log", "two.binpb.zst"]),
            [
                ExecLogSource {
                    path: "one.binpb.zst".into()
                },
                ExecLogSource {
                    path: "two.binpb.zst".into()
                },
            ]
        );
    }

    #[test]
    fn naming_no_log_names_no_document() {
        assert_eq!(sources(&[]), []);
    }
}
