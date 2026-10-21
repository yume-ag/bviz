//! Writes a synthetic execution log of a large build for the load benchmark:
//! input sets nested many levels deep, every spawn reading through all of
//! them, so the measure of what spawns read walks spawns times files.
//!
//! `bench_synth <out.zst> <spawns> <levels> <files per level>`: each level's
//! set holds files of its own and the level below; each spawn reads the top
//! level, the bottom level as its tools, and a file of its own, and writes
//! one file.

use std::fs::File;
use std::io::{BufWriter, Write};

use bazel_spawn_proto::tools::protos::exec_log_entry::{self, Type};
use bazel_spawn_proto::tools::protos::{Digest, ExecLogEntry};
use prost::Message;

/// Bytes of each file the log names.
const FILE_BYTES: i64 = 4 << 10;

/// The level of zstd compression, Bazel's default for the compact log.
const ZSTD_LEVEL: i32 = 3;

/// Writes entries to a log, numbering them as Bazel does, from one.
struct Log<W: Write> {
    out: W,
    next_id: u32,
    buffer: Vec<u8>,
}

impl<W: Write> Log<W> {
    fn add(&mut self, payload: Type) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        self.write(id, payload);
        id
    }

    fn write(&mut self, id: u32, payload: Type) {
        self.buffer.clear();
        let entry = ExecLogEntry {
            id,
            r#type: Some(payload),
        };
        entry
            .encode_length_delimited(&mut self.buffer)
            .expect("a Vec grows to fit");
        self.out.write_all(&self.buffer).expect("write the log");
    }

    /// A file at `path` whose content is the `content`th distinct one, its
    /// hash leading with what tells it apart, as the reader keys contents by
    /// the head of the hash.
    fn file(&mut self, path: String, content: u64) -> u32 {
        self.add(Type::File(exec_log_entry::File {
            path,
            digest: Some(Digest {
                hash: format!("{content:016x}{:048x}", 0),
                size_bytes: FILE_BYTES,
                hash_function_name: String::new(),
            }),
        }))
    }

    fn set(&mut self, input_ids: Vec<u32>, transitive_set_ids: Vec<u32>) -> u32 {
        self.add(Type::InputSet(exec_log_entry::InputSet {
            input_ids,
            transitive_set_ids,
        }))
    }
}

fn main() {
    let usage = "usage: bench_synth <out.zst> <spawns> <levels> <files per level>";
    let mut args = std::env::args().skip(1);
    let out = args.next().expect(usage);
    let mut count = || -> u64 { args.next().expect(usage).parse().expect(usage) };
    let (spawns, levels, per_level) = (count(), count(), count());
    let file = BufWriter::new(File::create(&out).expect("create the log"));
    let encoder = zstd::stream::write::Encoder::new(file, ZSTD_LEVEL).expect("start zstd");
    let mut log = Log {
        out: encoder,
        next_id: 1,
        buffer: Vec::new(),
    };
    let mut content = 0;
    let mut below = Vec::new();
    let mut bottom = None;
    for level in 0..levels {
        let files = (0..per_level)
            .map(|index| {
                content += 1;
                log.file(format!("level{level}/file{index}"), content)
            })
            .collect();
        let set = log.set(files, below);
        bottom.get_or_insert(set);
        below = vec![set];
    }
    for spawn in 0..spawns {
        content += 1;
        let own = log.file(format!("spawn{spawn}/input"), content);
        let inputs = log.set(vec![own], below.clone());
        content += 1;
        let output = log.file(format!("bazel-out/spawn{spawn}/output"), content);
        log.write(
            0,
            Type::Spawn(exec_log_entry::Spawn {
                runner: "remote".into(),
                mnemonic: "Synthetic".into(),
                target_label: format!("//synthetic:spawn{spawn}"),
                input_set_id: inputs,
                tool_set_id: bottom.unwrap_or(0),
                outputs: vec![exec_log_entry::Output {
                    r#type: Some(exec_log_entry::output::Type::OutputId(output)),
                }],
                ..Default::default()
            }),
        );
    }
    let file = log.out.finish().expect("end zstd");
    file.into_inner().expect("flush the log");
    eprintln!(
        "[synth] {out}: {spawns} spawns over {levels} levels of {per_level} files, {} entries",
        log.next_id - 1
    );
}
