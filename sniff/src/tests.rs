use bazel_build_event_proto::build_event_stream::build_event::Payload;
use bazel_build_event_proto::build_event_stream::build_event_id::{BuildStartedId, Id, ProgressId};
use bazel_build_event_proto::build_event_stream::{
    BuildEvent, BuildEventId, BuildStarted, Progress,
};
use bazel_query_proto::analysis::{
    self, ActionGraphContainer, Artifact, ConfiguredTarget, CqueryResult, PathFragment, RuleClass,
};
use bazel_query_proto::blaze_query::target::Discriminator;
use bazel_query_proto::blaze_query::{Rule, SourceFile, Target};
use bazel_spawn_proto::tools::protos::{EnvironmentVariable, SpawnExec};
use flate2::Compression;
use flate2::write::GzEncoder;
use std::io::Write;

use super::*;

/// A real `--build_event_binary_file`.
const SAMPLE_BEP: &[u8] = include_bytes!(env!("SAMPLE_BEP"));

/// Short enough that every stream below cuts its first message off, long
/// enough to hold the fields a kind's first message is told by.
const SHORTEST_TELLING_HEAD: usize = 12;

fn stream<M: prost::Message>(messages: &[M]) -> Vec<u8> {
    messages
        .iter()
        .flat_map(M::encode_length_delimited_to_vec)
        .collect()
}

fn event_id(id: Id) -> Option<BuildEventId> {
    Some(BuildEventId { id: Some(id) })
}

fn bep() -> Vec<u8> {
    let progress = || event_id(Id::Progress(ProgressId { opaque_count: 0 }));
    stream(&[
        BuildEvent {
            id: event_id(Id::Started(BuildStartedId {})),
            children: progress().into_iter().collect(),
            payload: Some(Payload::Started(BuildStarted {
                uuid: "3f1c2a4e-0d5b-4a8e-9c7f-1b2d3e4f5a6b".into(),
                command: "build".into(),
                ..Default::default()
            })),
            ..Default::default()
        },
        BuildEvent {
            id: progress(),
            payload: Some(Payload::Progress(Progress {
                stderr: "Loading: 0 packages loaded\n".into(),
                ..Default::default()
            })),
            ..Default::default()
        },
    ])
}

fn rule_target(name: &str) -> Target {
    Target {
        r#type: Discriminator::Rule as i32,
        rule: Some(Rule {
            name: name.into(),
            rule_class: "cc_library".into(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn source_target(name: &str) -> Target {
    Target {
        r#type: Discriminator::SourceFile as i32,
        source_file: Some(SourceFile {
            name: name.into(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn query() -> Vec<u8> {
    stream(&[
        rule_target("//app:app"),
        source_target("//app:main.cc"),
        rule_target("//lib:lib"),
    ])
}

fn cquery() -> Vec<u8> {
    let result = |target| CqueryResult {
        results: vec![ConfiguredTarget {
            target: Some(target),
            configuration_id: 1,
            ..Default::default()
        }],
        ..Default::default()
    };
    stream(&[
        result(rule_target("//app:app")),
        result(source_target("//app:main.cc")),
        CqueryResult {
            configurations: vec![analysis::Configuration {
                id: 1,
                mnemonic: "k8-fastbuild".into(),
                ..Default::default()
            }],
            ..Default::default()
        },
    ])
}

fn aquery() -> Vec<u8> {
    stream(&[
        ActionGraphContainer {
            rule_classes: vec![RuleClass {
                id: 1,
                name: "cc_library".into(),
            }],
            ..Default::default()
        },
        ActionGraphContainer {
            targets: vec![analysis::Target {
                id: 1,
                label: "//lib:lib".into(),
                rule_class_id: 1,
            }],
            ..Default::default()
        },
        ActionGraphContainer {
            path_fragments: vec![PathFragment {
                id: 1,
                label: "lib.o".into(),
                parent_id: 0,
            }],
            ..Default::default()
        },
        ActionGraphContainer {
            artifacts: vec![Artifact {
                id: 1,
                path_fragment_id: 1,
                is_tree_artifact: false,
            }],
            ..Default::default()
        },
    ])
}

fn streams() -> [(Kind, Vec<u8>); 4] {
    [
        (Kind::Bep, bep()),
        (Kind::Query, query()),
        (Kind::Cquery, cquery()),
        (Kind::Aquery, aquery()),
    ]
}

#[test]
fn tells_each_streamed_kind() {
    for (kind, stream) in streams() {
        assert_eq!(kind_of(&stream), Some(kind));
    }
    assert_eq!(kind_of(SAMPLE_BEP), Some(Kind::Bep));
}

#[test]
fn tells_a_streamed_kind_from_a_head_that_cuts_its_first_message() {
    for (kind, stream) in streams() {
        for cut in SHORTEST_TELLING_HEAD..stream.len() {
            assert_eq!(kind_of(&stream[..cut]), Some(kind), "{kind:?} cut at {cut}");
        }
    }
}

fn gzipped(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(bytes)
        .expect("an in-memory stream compresses");
    encoder.finish().expect("an in-memory stream compresses")
}

/// The opening of a profile as Bazel writes it.
const PROFILE: &[u8] = b" \n{\"otherData\":{\"build_id\":\"3f1c\"},\"traceEvents\":[";

#[test]
fn tells_a_profile_and_an_exec_log_by_their_framing() {
    assert_eq!(
        kind_of(&[0x28, 0xb5, 0x2f, 0xfd, 0x24]),
        Some(Kind::ExecLog)
    );
    assert_eq!(kind_of(PROFILE), Some(Kind::Profile));
    assert_eq!(kind_of(br#"{"traceEvents":[{"name":"#), Some(Kind::Profile));
}

#[test]
fn tells_a_gzipped_profile_by_what_it_inflates_to() {
    let events = r#"{"name":"action","ph":"X","ts":1,"dur":2},"#.repeat(10_000);
    let profile = gzipped(&[PROFILE, events.as_bytes()].concat());
    assert_eq!(kind_of(&profile), Some(Kind::Profile));
    let cut = profile.len() / 2;
    assert_eq!(
        kind_of(&profile[..cut]),
        Some(Kind::Profile),
        "cut at {cut}"
    );
}

#[test]
fn leaves_a_gzipped_file_other_than_a_profile_unknown() {
    let not_profiles: [&[u8]; 3] = [
        br#"{"name": "bviz", "version": "1.0.0"}"#,
        b"\"traceEvents\" names the events of a trace profile.\n",
        &bep(),
    ];
    for bytes in not_profiles {
        let head = gzipped(bytes);
        assert_eq!(kind_of(&head), None, "{:?}", String::from_utf8_lossy(bytes));
    }
    assert_eq!(kind_of(&[0x1f, 0x8b, 0x08, 0x00]), None);
}

#[test]
fn leaves_other_files_unknown() {
    let bazelrc = b"# Local settings\nbuild --config=remote\ncommon --disk_cache=~/.cache/bazel\n";
    let json = br#"{"name": "bviz", "version": "1.0.0"}"#;
    let stray_key = b"\"traceEvents\" names the events of a trace profile.\n";
    for head in [&b""[..], bazelrc, json, stray_key] {
        assert_eq!(kind_of(head), None, "{:?}", String::from_utf8_lossy(head));
    }
}

#[test]
fn leaves_random_bytes_unknown() {
    const HEADS: usize = 10_000;
    const HEAD_BYTES: usize = 256;
    let mut rng = fastrand::Rng::with_seed(0);
    for _ in 0..HEADS {
        let head: Vec<u8> = (0..HEAD_BYTES).map(|_| rng.u8(..)).collect();
        if head.starts_with(ZSTD_MAGIC) {
            continue;
        }
        assert_eq!(kind_of(&head), None, "{head:02x?}");
    }
}

#[test]
fn leaves_an_uncompressed_execution_log_unknown() {
    let spawn = |command: &[&str]| SpawnExec {
        command_args: command.iter().map(|arg| arg.to_string()).collect(),
        environment_variables: vec![EnvironmentVariable {
            name: "PATH".into(),
            value: "/bin:/usr/bin".into(),
        }],
        mnemonic: "CppCompile".into(),
        runner: "linux-sandbox".into(),
        ..Default::default()
    };
    // A first argument whose opening bytes read as a submessage declaring
    // exactly the bytes after them.
    let filling = format!("ba{}", "z".repeat(usize::from(b'a')));
    for tool in [
        "external/toolchain/bin/clang",
        "/usr/bin/gcc",
        "bazel-out/k8-opt-exec/bin/tool",
        &filling,
    ] {
        for command in [&[tool][..], &[tool, "-c", "lib/lib.cc"]] {
            let log = stream(&[spawn(command), spawn(command)]);
            assert_eq!(kind_of(&log), None, "{command:?}");
        }
    }
}

#[test]
fn target_types_number_the_fields_describing_their_targets() {
    use query_fields::target;
    let fields = [
        (Discriminator::Rule, target::RULE),
        (Discriminator::SourceFile, target::SOURCE_FILE),
        (Discriminator::GeneratedFile, target::GENERATED_FILE),
        (Discriminator::PackageGroup, target::PACKAGE_GROUP),
        (Discriminator::EnvironmentGroup, target::ENVIRONMENT_GROUP),
    ];
    let types: Vec<u64> = fields.iter().map(|&(kind, _)| kind as u64).collect();
    assert_eq!(types, TARGET_TYPES.collect::<Vec<_>>());
    for (kind, field) in fields {
        assert_eq!(u64::from(field), kind as u64 + 1, "{kind:?}");
    }
    let past = i32::try_from(TARGET_TYPES.end() + 1).expect("a small enum value");
    assert!(Discriminator::try_from(past).is_err());
}

/// Varint encodings of `length`.
fn varint(length: u64) -> Vec<u8> {
    let mut encoded = Vec::new();
    prost::encoding::encode_varint(length, &mut encoded);
    encoded
}

/// Lengths a delimiter or field may declare that overflow a sum of offsets.
const HUGE_LENGTHS: [u64; 6] = [
    u64::MAX,
    u64::MAX - 1,
    u64::MAX - 16,
    u32::MAX as u64,
    u32::MAX as u64 - 1,
    u32::MAX as u64 + 1,
];

/// The wire key of a length-delimited field numbered `number`.
fn len_key(number: u8) -> u8 {
    number << 3 | 2
}

/// `bytes` as a submessage field numbered `number`.
fn len_field(number: u8, bytes: &[u8]) -> Vec<u8> {
    [&[len_key(number)][..], &varint(bytes.len() as u64), bytes].concat()
}

/// `message` as a length-delimited stream's only message.
fn delimited(message: &[u8]) -> Vec<u8> {
    [varint(message.len() as u64), message.to_vec()].concat()
}

#[test]
fn a_field_overflowing_its_offset_leaves_a_head_unknown() {
    let heads: [&[u8]; 2] = [
        &[
            0x0b, 0x0a, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01,
        ],
        &[
            0x15, 0x0a, 0x11, 0x1a, 0x0f, 0x0a, 0xf5, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0x01, 0x00, 0x00, 0x00, 0x00, 0x12, 0x00,
        ],
    ];
    for head in heads {
        assert_eq!(kind_of(head), None, "{head:02x?}");
    }
}

#[test]
fn a_huge_declared_length_leaves_a_head_unknown() {
    for length in HUGE_LENGTHS {
        let huge_field = [&[len_key(1)][..], &varint(length), b"\x08\x01"].concat();
        let heads = [
            // A delimiter.
            [&varint(length)[..], b"\x0a\x00"].concat(),
            // A message's field.
            delimited(&huge_field),
            // A field of a submessage.
            delimited(&len_field(1, &huge_field)),
            // A field of a build event id's kind, ahead of a payload.
            delimited(&[len_field(1, &len_field(3, &huge_field)), len_field(3, b"")].concat()),
        ];
        for head in heads {
            assert_eq!(kind_of(&head), None, "{length}: {head:02x?}");
        }
    }
}
