//! Length-delimited messages, each a varint byte count and then that many
//! bytes, as Bazel writes its binary build event files and
//! `--output=streamed_proto`.

use std::fmt;
use std::io::{self, BufRead, BufReader, Read};

/// Read-buffer capacity of [`Delimited::buffered`], several times the typical
/// message so most are read in place.
const READ_BUFFER_BYTES: usize = 64 << 10;

/// The longest encoding of a length delimiter, a 64-bit varint.
const MAX_DELIMITER_BYTES: usize = 10;

/// Set on every byte of a varint but its last.
const VARINT_CONTINUES: u8 = 0x80;

/// The most of a message's declared length reserved ahead of its bytes: past
/// it the spill grows as the bytes arrive, so a delimiter declaring more than
/// the stream holds costs no more than this.
const SPILL_RESERVE_BYTES: usize = 16 << 20;

/// The messages of a stream, read one at a time.
pub struct Delimited<R> {
    stream: R,
    /// A message the stream's buffer did not hold whole, gathered across
    /// refills; reused from one such message to the next.
    spill: Vec<u8>,
}

/// How a stream holds its next message.
#[derive(Debug, PartialEq, Eq)]
pub enum Next<T> {
    Message(T),
    /// The stream ends cleanly, ahead of a delimiter.
    End,
    /// The stream ends inside a delimiter or a message.
    CutOff,
}

/// Why a stream's next message cannot be read.
#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    /// A delimiter runs past the longest varint, its tenth byte sets bits past
    /// the 64th, or the length it declares overflows `usize`.
    Delimiter(prost::DecodeError),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::Delimiter(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            Error::Delimiter(e) => Some(e),
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

impl<R: Read> Delimited<BufReader<R>> {
    /// The messages of an unbuffered `stream`.
    pub fn buffered(stream: R) -> Self {
        Delimited::new(BufReader::with_capacity(READ_BUFFER_BYTES, stream))
    }
}

impl<R: BufRead> Delimited<R> {
    /// The messages of `stream`, read through its own buffer.
    pub fn new(stream: R) -> Self {
        Delimited {
            stream,
            spill: Vec::new(),
        }
    }

    /// The next message, as `read` makes of its bytes.
    pub fn next<T>(&mut self, read: impl FnOnce(&[u8]) -> T) -> Result<Next<T>, Error> {
        let length = match self.delimiter()? {
            Next::Message(length) => length,
            Next::End => return Ok(Next::End),
            Next::CutOff => return Ok(Next::CutOff),
        };
        let available = fill_buf(&mut self.stream)?;
        if available.len() >= length {
            let message = read(&available[..length]);
            self.stream.consume(length);
            return Ok(Next::Message(message));
        }
        self.spill.clear();
        self.spill.reserve(length.min(SPILL_RESERVE_BYTES));
        while self.spill.len() < length {
            let available = fill_buf(&mut self.stream)?;
            if available.is_empty() {
                return Ok(Next::CutOff);
            }
            let taken = available.len().min(length - self.spill.len());
            self.spill.extend_from_slice(&available[..taken]);
            self.stream.consume(taken);
        }
        Ok(Next::Message(read(&self.spill)))
    }

    /// The length the next delimiter declares, consuming the delimiter.
    fn delimiter(&mut self) -> Result<Next<usize>, Error> {
        let mut delimiter = [0u8; MAX_DELIMITER_BYTES];
        for end in 0..MAX_DELIMITER_BYTES {
            let Some(&byte) = fill_buf(&mut self.stream)?.first() else {
                return Ok(match end {
                    0 => Next::End,
                    _ => Next::CutOff,
                });
            };
            self.stream.consume(1);
            delimiter[end] = byte;
            if byte & VARINT_CONTINUES == 0 {
                return decode_length(&delimiter[..=end]);
            }
        }
        decode_length(&delimiter)
    }
}

/// The bytes `stream` buffers, refilled where it has run out, retrying a read
/// that was interrupted.
fn fill_buf<R: BufRead>(stream: &mut R) -> io::Result<&[u8]> {
    loop {
        match stream.fill_buf() {
            Ok([]) => return Ok(&[]),
            Ok(_) => break,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    // The borrow checker rejects returning the slice from within the loop; a
    // buffer holding bytes refills with no read.
    stream.fill_buf()
}

fn decode_length(delimiter: &[u8]) -> Result<Next<usize>, Error> {
    prost::decode_length_delimiter(delimiter)
        .map(Next::Message)
        .map_err(Error::Delimiter)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delimited(messages: &[&[u8]]) -> Vec<u8> {
        let mut stream = Vec::new();
        for message in messages {
            prost::encode_length_delimiter(message.len(), &mut stream).expect("a delimiter");
            stream.extend_from_slice(message);
        }
        stream
    }

    /// Every message of `stream`, read through a buffer of `capacity` bytes,
    /// and how the stream ends.
    fn read_all(stream: impl Read, capacity: usize) -> (Vec<Vec<u8>>, Next<()>) {
        let mut messages = Delimited::new(BufReader::with_capacity(capacity, stream));
        let mut read = Vec::new();
        loop {
            match messages.next(<[u8]>::to_vec).expect("delimiters decode") {
                Next::Message(message) => read.push(message),
                Next::End => return (read, Next::End),
                Next::CutOff => return (read, Next::CutOff),
            }
        }
    }

    /// Long enough that a small buffer holds it only in parts.
    const LONG: &[u8] = &[7; 300];

    #[test]
    fn reads_each_message_whole_however_it_is_buffered() {
        let messages: [&[u8]; 4] = [b"first", b"", LONG, b"last"];
        let stream = delimited(&messages);
        for capacity in [1, 3, 64, READ_BUFFER_BYTES] {
            let (read, end) = read_all(stream.as_slice(), capacity);
            assert_eq!(read, messages, "capacity {capacity}");
            assert_eq!(end, Next::End, "capacity {capacity}");
        }
    }

    /// Fails every other read as interrupted.
    struct Interrupting<R> {
        stream: R,
        interrupted: bool,
    }

    impl<R: Read> Read for Interrupting<R> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.interrupted = !self.interrupted;
            if self.interrupted {
                return Err(io::ErrorKind::Interrupted.into());
            }
            self.stream.read(buf)
        }
    }

    #[test]
    fn an_interrupted_read_is_retried() {
        let messages: [&[u8]; 3] = [b"first", LONG, b"last"];
        let stream = delimited(&messages);
        for capacity in [1, 3, READ_BUFFER_BYTES] {
            let interrupting = Interrupting {
                stream: stream.as_slice(),
                interrupted: false,
            };
            let (read, end) = read_all(interrupting, capacity);
            assert_eq!(read, messages, "capacity {capacity}");
            assert_eq!(end, Next::End, "capacity {capacity}");
        }
    }

    #[test]
    fn a_declared_length_reserves_no_more_than_its_bound() {
        let mut stream = Vec::new();
        prost::encode_length_delimiter(usize::MAX >> 1, &mut stream).expect("a delimiter");
        stream.extend_from_slice(LONG);
        let mut messages = Delimited::new(BufReader::with_capacity(3, stream.as_slice()));
        assert_eq!(
            messages.next(|_| ()).expect("the delimiter decodes"),
            Next::CutOff
        );
        assert!(messages.spill.capacity() <= SPILL_RESERVE_BYTES);
    }

    #[test]
    fn a_stream_cut_inside_a_message_or_delimiter_is_cut_off() {
        let stream = delimited(&[b"first", LONG]);
        let second_at = 1 + b"first".len();
        for cut in [second_at + 1, second_at + 2, stream.len() - 1] {
            for capacity in [3, READ_BUFFER_BYTES] {
                let (read, end) = read_all(&stream[..cut], capacity);
                assert_eq!(read, [b"first"], "cut at {cut}");
                assert_eq!(end, Next::CutOff, "cut at {cut}");
            }
        }
        assert_eq!(read_all(&[][..], 3), (Vec::new(), Next::End));
    }

    #[test]
    fn a_delimiter_past_the_longest_varint_fails() {
        let stream = [VARINT_CONTINUES; MAX_DELIMITER_BYTES + 1];
        let mut messages = Delimited::buffered(&stream[..]);
        assert!(matches!(messages.next(|_| ()), Err(Error::Delimiter(_))));
    }

    #[test]
    fn a_delimiter_past_64_bits_fails() {
        let mut stream = [0xff; MAX_DELIMITER_BYTES];
        stream[MAX_DELIMITER_BYTES - 1] = 0x02;
        let mut messages = Delimited::buffered(&stream[..]);
        assert!(matches!(messages.next(|_| ()), Err(Error::Delimiter(_))));
    }
}
