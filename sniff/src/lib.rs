//! Which of Bazel's outputs a file holds, told from its first bytes.

mod analysis_fields;
mod build_event_fields;
mod query_fields;
mod wire;

use std::io::Read;
use std::ops::RangeInclusive;

use flate2::read::GzDecoder;

use analysis_fields::{
    action, action_graph_container as container, artifact, aspect_descriptor, configuration,
    configured_target, cquery_result, dep_set_of_files, path_fragment, rule_class,
};
use build_event_fields::build_event;
use query_fields::target as query_target;
use wire::{Field, Message, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A JSON trace profile, `--profile`, gzipped or plain.
    Profile,
    /// A compact execution log, `--execution_log_compact_file`.
    ExecLog,
    /// A binary build event stream, `--build_event_binary_file`.
    Bep,
    /// `bazel query --output=streamed_proto`.
    Query,
    /// `bazel cquery --output=streamed_proto`.
    Cquery,
    /// `bazel aquery --output=streamed_proto`.
    Aquery,
}

const GZIP_MAGIC: &[u8] = &[0x1f, 0x8b];
const ZSTD_MAGIC: &[u8] = &[0x28, 0xb5, 0x2f, 0xfd];

/// Top-level keys of a JSON trace profile, one of which Bazel writes first.
const PROFILE_KEYS: [&[u8]; 2] = [b"\"traceEvents\"", b"\"otherData\""];

/// Bytes of a gzipped head inflated to tell a profile, many times the length
/// of the key a profile opens with.
const INFLATED_PROFILE_BYTES: u64 = 4 << 10;

/// How many messages after the first must share its shape, for the kinds
/// whose every message has the same shape.
const CONFIRMING_MESSAGES: usize = 2;

/// `blaze_query.Target.Discriminator`'s values, `RULE` through
/// `ENVIRONMENT_GROUP`.
const TARGET_TYPES: RangeInclusive<u64> = 1..=5;

/// `analysis.ActionGraphContainer`'s fields, each a list of one kind of
/// entry, with the id field that kind of entry opens with; an action's is its
/// target's.
const GRAPH_ENTRIES: [(u32, u32); 8] = [
    (container::ARTIFACTS, artifact::ID),
    (container::ACTIONS, action::TARGET_ID),
    (container::TARGETS, analysis_fields::target::ID),
    (container::DEP_SET_OF_FILES, dep_set_of_files::ID),
    (container::CONFIGURATION, configuration::ID),
    (container::ASPECT_DESCRIPTORS, aspect_descriptor::ID),
    (container::RULE_CLASSES, rule_class::ID),
    (container::PATH_FRAGMENTS, path_fragment::ID),
];

/// The kind of a file that begins with `head`; `None` where `head` is empty
/// or matches no kind.
pub fn kind_of(head: &[u8]) -> Option<Kind> {
    if head.starts_with(GZIP_MAGIC) {
        return is_json_profile(&inflated_prefix(head)).then_some(Kind::Profile);
    }
    if is_json_profile(head) {
        return Some(Kind::Profile);
    }
    if head.starts_with(ZSTD_MAGIC) {
        return Some(Kind::ExecLog);
    }
    streamed_kind(head)
}

/// Up to [`INFLATED_PROFILE_BYTES`] of what the gzip stream `head` begins
/// inflates to.
fn inflated_prefix(head: &[u8]) -> Vec<u8> {
    let mut inflated = Vec::with_capacity(INFLATED_PROFILE_BYTES as usize);
    // A head that cuts the stream off fails the read once its bytes are
    // inflated, leaving them in `inflated`.
    let _ = GzDecoder::new(head)
        .take(INFLATED_PROFILE_BYTES)
        .read_to_end(&mut inflated);
    inflated
}

fn is_json_profile(head: &[u8]) -> bool {
    head.trim_ascii_start().starts_with(b"{")
        && PROFILE_KEYS
            .iter()
            .any(|key| head.windows(key.len()).any(|window| window == *key))
}

/// The kind of a stream of length-delimited messages, by the wire structure
/// of the fields each kind's messages open with. Only those fields are read,
/// so a later Bazel may add fields within a query target's rule or file, to a
/// configured target after its target, to an action graph entry after its id
/// and to a build event after its id and the submessage that follows it, and
/// may add kinds of build event id, without hiding the kind. A field added to
/// `Target`, `CqueryResult` or `ActionGraphContainer` itself, or a target type
/// added to `Target.Discriminator`, hides it. The kinds' shapes of the first
/// message exclude one another.
fn streamed_kind(head: &[u8]) -> Option<Kind> {
    let mut messages = wire::delimited(head);
    let first = messages.next()?;
    let (kind, shape): (Kind, fn(Message) -> bool) = if is_query_target(first) {
        (Kind::Query, is_query_target)
    } else if is_cquery_result(first) {
        (Kind::Cquery, |_| true)
    } else if graph_entry_field(first)
        // An action has the shape of the `CqueryResult.configurations` entry a
        // cquery with no results opens with.
        .is_some_and(|field| field != container::ACTIONS)
    {
        (Kind::Aquery, is_graph_entry)
    } else if is_build_event(first) {
        (Kind::Bep, |_| true)
    } else {
        return None;
    };
    messages
        .take(CONFIRMING_MESSAGES)
        .filter(|message| message.is_whole())
        .all(shape)
        .then_some(kind)
}

/// A `blaze_query.Target`: its type, then the one field describing a target
/// of that type, numbered one past the type.
fn is_query_target(target: Message) -> bool {
    let mut fields = target.fields();
    let Some(Field {
        number: query_target::TYPE,
        value: Value::Varint(kind),
    }) = fields.next()
    else {
        return false;
    };
    TARGET_TYPES.contains(&kind)
        && matches!(
            fields.next(),
            Some(Field { number, value: Value::Len(_) })
                if kind.checked_add(1) == Some(u64::from(number))
        )
        && fields.at_end()
}

/// An `analysis.CqueryResult` holding one `ConfiguredTarget`, whose target
/// comes first.
fn is_cquery_result(result: Message) -> bool {
    let Some((cquery_result::RESULTS, configured)) = sole_message(result) else {
        return false;
    };
    matches!(
        configured.fields().next(),
        Some(Field {
            number: configured_target::TARGET,
            value: Value::Len(target),
        }) if is_query_target(target)
    )
}

fn is_graph_entry(container: Message) -> bool {
    graph_entry_field(container).is_some()
}

/// The field of an `analysis.ActionGraphContainer` holding one entry.
fn graph_entry_field(container: Message) -> Option<u32> {
    let (number, entry) = sole_message(container)?;
    let &(_, id) = GRAPH_ENTRIES.iter().find(|&&(field, _)| field == number)?;
    let opens_with_id = matches!(
        entry.fields().next(),
        Some(Field { number, value: Value::Varint(_) }) if number == id
    );
    opens_with_id.then_some(number)
}

/// A `build_event_stream.BuildEvent`: its id, a `BuildEventId` holding one
/// kind of id, followed by a submessage other than an id.
fn is_build_event(event: Message) -> bool {
    let mut fields = event.fields();
    let Some(Field {
        number: build_event::ID,
        value: Value::Len(id),
    }) = fields.next()
    else {
        return false;
    };
    sole_message(id).is_some_and(|(_, kind)| kind.is_well_formed())
        && matches!(
            fields.next(),
            Some(Field { number, value: Value::Len(_) }) if number != build_event::ID
        )
}

/// The number and submessage of `message`'s only field, where that field is
/// a submessage.
fn sole_message(message: Message) -> Option<(u32, Message)> {
    let mut fields = message.fields();
    let Some(Field {
        number,
        value: Value::Len(inner),
    }) = fields.next()
    else {
        return None;
    };
    fields.at_end().then_some((number, inner))
}

#[cfg(test)]
mod tests;
