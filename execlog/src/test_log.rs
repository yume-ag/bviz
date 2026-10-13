//! Synthetic execution logs, encoded with the generated spawn.proto types.

use bazel_spawn_proto::duration_proto::google::protobuf::Duration as ProtoDuration;
use bazel_spawn_proto::tools::protos::exec_log_entry::{self, Output, Type, output};
use bazel_spawn_proto::tools::protos::{Digest, ExecLogEntry, SpawnMetrics};
use bviz_invocation::Recorded;
use prost::Message;

use crate::{ExecLog, Phase};

pub(crate) fn entry(id: u32, payload: Type) -> ExecLogEntry {
    ExecLogEntry {
        id,
        r#type: Some(payload),
    }
}

pub(crate) fn invocation(id: &str) -> ExecLogEntry {
    entry(
        0,
        Type::Invocation(exec_log_entry::Invocation {
            id: id.into(),
            ..Default::default()
        }),
    )
}

pub(crate) fn file(id: u32, path: &str) -> ExecLogEntry {
    entry(
        id,
        Type::File(exec_log_entry::File {
            path: path.into(),
            digest: None,
        }),
    )
}

/// A file of `bytes` bytes, at `path` relative to the directory holding it
/// when it is a directory's.
pub(crate) fn sized(path: &str, bytes: i64) -> exec_log_entry::File {
    exec_log_entry::File {
        path: path.into(),
        digest: Some(Digest {
            hash: format!("{bytes:064x}"),
            size_bytes: bytes,
            hash_function_name: String::new(),
        }),
    }
}

pub(crate) fn sized_file(id: u32, path: &str, bytes: i64) -> ExecLogEntry {
    entry(id, Type::File(sized(path, bytes)))
}

pub(crate) fn directory(id: u32, path: &str, files: Vec<exec_log_entry::File>) -> ExecLogEntry {
    entry(
        id,
        Type::Directory(exec_log_entry::Directory {
            path: path.into(),
            files,
        }),
    )
}

pub(crate) fn input_set(id: u32, input_ids: &[u32], transitive_set_ids: &[u32]) -> ExecLogEntry {
    entry(
        id,
        Type::InputSet(exec_log_entry::InputSet {
            input_ids: input_ids.to_vec(),
            transitive_set_ids: transitive_set_ids.to_vec(),
        }),
    )
}

pub(crate) fn unresolved_symlink(id: u32, path: &str) -> ExecLogEntry {
    entry(
        id,
        Type::UnresolvedSymlink(exec_log_entry::UnresolvedSymlink {
            path: path.into(),
            target_path: "elsewhere".into(),
        }),
    )
}

fn millis(millis: u64) -> Option<ProtoDuration> {
    let duration = std::time::Duration::from_millis(millis);
    Some(ProtoDuration {
        seconds: duration.as_secs() as i64,
        nanos: duration.subsec_nanos() as i32,
    })
}

/// Metrics of a spawn taking `total` milliseconds, with `phases` reported.
pub(crate) fn metrics(total: u64, phases: &[(Phase, u64)]) -> SpawnMetrics {
    let mut metrics = SpawnMetrics {
        total_time: millis(total),
        ..Default::default()
    };
    for &(phase, time) in phases {
        let field = match phase {
            Phase::Parse => &mut metrics.parse_time,
            Phase::Upload => &mut metrics.upload_time,
            Phase::Network => &mut metrics.network_time,
            Phase::Queue => &mut metrics.queue_time,
            Phase::Setup => &mut metrics.setup_time,
            Phase::Execute => &mut metrics.execution_wall_time,
            Phase::ProcessOutputs => &mut metrics.process_outputs_time,
            Phase::Fetch => &mut metrics.fetch_time,
            Phase::Retry => &mut metrics.retry_time,
            Phase::Other => panic!("no spawn reports the remainder"),
        };
        *field = millis(time);
    }
    metrics
}

pub(crate) fn spawn(runner: &str, mnemonic: &str, metrics: SpawnMetrics) -> exec_log_entry::Spawn {
    exec_log_entry::Spawn {
        runner: runner.into(),
        mnemonic: mnemonic.into(),
        metrics: Some(metrics),
        ..Default::default()
    }
}

pub(crate) fn output_id(id: u32) -> Output {
    Output {
        r#type: Some(output::Type::OutputId(id)),
    }
}

pub(crate) fn invalid_output(path: &str) -> Output {
    Output {
        r#type: Some(output::Type::InvalidOutputPath(path.into())),
    }
}

/// `entries` as Bazel writes a compact log: length-delimited, then compressed.
pub(crate) fn encode(entries: &[ExecLogEntry]) -> Vec<u8> {
    let mut decoded = Vec::new();
    for entry in entries {
        entry
            .encode_length_delimited(&mut decoded)
            .expect("a Vec grows to fit");
    }
    zstd::encode_all(decoded.as_slice(), 0).expect("compress in memory")
}

pub(crate) fn load(entries: &[ExecLogEntry]) -> ExecLog {
    let log = encode(entries);
    match crate::read::read_compressed(log.as_slice(), log.len() as u64) {
        Ok(Recorded::Complete(log)) => log,
        _ => panic!("the log reads whole"),
    }
}
