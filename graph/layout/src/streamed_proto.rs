//! Reader for Bazel's `--output=streamed_proto`: a sequence of length-delimited
//! messages (a varint byte count followed by that many bytes), repeated to EOF.

use std::fmt;
use std::io::{self, Read};

use bazel_query_proto::analysis::{ActionGraphContainer, CqueryResult};
use bazel_query_proto::blaze_query::Target;
use bviz_input::delimited::{self, Delimited, Next};
use bviz_input::reserved;
use prost::Message;

/// Stream bytes per `Target` of a query's output, rounded down from the densest
/// outputs measured so the targets are reserved at or above their final count.
const STREAM_BYTES_PER_TARGET: u64 = 512;

/// Stream bytes per `CqueryResult` of a cquery's output, rounded down like
/// [`STREAM_BYTES_PER_TARGET`].
const STREAM_BYTES_PER_CQUERY_RESULT: u64 = 1536;

/// Stream bytes per `ActionGraphContainer` of an aquery's output, rounded down
/// like [`STREAM_BYTES_PER_TARGET`].
const STREAM_BYTES_PER_ACTION_GRAPH_CONTAINER: u64 = 160;

/// Why a streamed_proto stream cannot be read.
#[derive(Debug)]
pub enum ReadError {
    Io(io::Error),
    Decode(prost::DecodeError),
    /// The stream ends inside a message or its delimiter.
    CutOff,
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReadError::Io(e) => write!(f, "{e}"),
            ReadError::Decode(e) => write!(f, "{e}"),
            ReadError::CutOff => write!(f, "the stream ends inside a message or its delimiter"),
        }
    }
}

impl std::error::Error for ReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ReadError::Io(e) => Some(e),
            ReadError::Decode(e) => Some(e),
            ReadError::CutOff => None,
        }
    }
}

impl From<delimited::Error> for ReadError {
    fn from(e: delimited::Error) -> Self {
        match e {
            delimited::Error::Io(e) => ReadError::Io(e),
            delimited::Error::Delimiter(e) => ReadError::Decode(e),
        }
    }
}

/// Decodes every length-delimited `M` from `stream`, in order, reserving room
/// for one per `bytes_per_message` of its `size` where that is known and that
/// much memory can be had.
fn read_length_delimited<M: Message + Default>(
    stream: impl Read,
    size: Option<u64>,
    bytes_per_message: u64,
) -> Result<Vec<M>, ReadError> {
    let capacity = size.map_or(0, |size| {
        usize::try_from(size / bytes_per_message).unwrap_or(usize::MAX)
    });
    let mut messages = reserved(capacity);
    let mut stream = Delimited::buffered(stream);
    loop {
        match stream.next(|message: &[u8]| M::decode(message))? {
            Next::Message(message) => messages.push(message.map_err(ReadError::Decode)?),
            Next::End => return Ok(messages),
            Next::CutOff => return Err(ReadError::CutOff),
        }
    }
}

/// Parses every `ActionGraphContainer` from an aquery streamed_proto stream of
/// `size` bytes, where that is known.
pub fn read_action_graph_containers(
    stream: impl Read,
    size: Option<u64>,
) -> Result<Vec<ActionGraphContainer>, ReadError> {
    read_length_delimited(stream, size, STREAM_BYTES_PER_ACTION_GRAPH_CONTAINER)
}

/// Parses every `Target` from a query streamed_proto stream of `size` bytes,
/// where that is known.
pub fn read_query_targets(stream: impl Read, size: Option<u64>) -> Result<Vec<Target>, ReadError> {
    read_length_delimited(stream, size, STREAM_BYTES_PER_TARGET)
}

/// Parses every `CqueryResult` from a cquery streamed_proto stream of `size`
/// bytes, where that is known.
pub fn read_cquery_results(
    stream: impl Read,
    size: Option<u64>,
) -> Result<Vec<CqueryResult>, ReadError> {
    read_length_delimited(stream, size, STREAM_BYTES_PER_CQUERY_RESULT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bazel_query_proto::blaze_query::SourceFile;
    use bazel_query_proto::blaze_query::target::Discriminator;

    fn target(name: &str) -> Target {
        Target {
            r#type: Discriminator::SourceFile as i32,
            source_file: Some(SourceFile {
                name: name.into(),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn reads_every_message_and_fails_a_cut_stream() {
        let targets = [target("//a:a.txt"), target("//b:b.txt")];
        let stream: Vec<u8> = targets
            .iter()
            .flat_map(Message::encode_length_delimited_to_vec)
            .collect();
        let size = Some(stream.len() as u64);
        assert_eq!(
            read_query_targets(stream.as_slice(), size).unwrap(),
            targets
        );
        assert_eq!(read_query_targets(&[][..], Some(0)).unwrap(), []);
        assert!(matches!(
            read_query_targets(&stream[..stream.len() - 1], None),
            Err(ReadError::CutOff)
        ));
    }
}
