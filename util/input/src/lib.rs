//! The files bviz reads, each opened once: its head is read ahead for whatever
//! asks what the file holds, then replayed ahead of the rest to whatever reads
//! the file.

use std::io::{self, Read};
use std::ops::Deref;
use std::sync::Arc;

#[cfg(any(unix, windows))]
mod file;

pub mod delimited;

/// Bytes of an input read ahead as its [`head`](Input::head): every look a
/// reader takes at a file's start, to tell what it holds or which invocation
/// wrote it, reads within these.
pub const HEAD_BYTES: usize = 256 << 10;

/// A source read at any offset, by several readers at once.
pub trait ReadAt: Send + Sync {
    /// Reads into `buf` from `offset`, returning how many bytes it read: fewer
    /// than `buf` holds only at the end of the source, or where one read
    /// returns short as [`Read::read`] may.
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize>;

    /// Fills `buf` from `offset`, failing with [`io::ErrorKind::UnexpectedEof`]
    /// where the source ends first.
    fn read_exact_at(&self, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
        while !buf.is_empty() {
            match self.read_at(buf, offset) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => {
                    buf = &mut buf[n..];
                    offset += n as u64;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Appends the source from `offset` to its end to `buf`, returning how
    /// many bytes it appended. The source is the caller's alone, so this read
    /// may move state that readers at an offset share, such as a file's
    /// cursor.
    fn read_to_end_from(self: Box<Self>, offset: u64, buf: &mut Vec<u8>) -> io::Result<usize> {
        At::new(self, offset).read_to_end(buf)
    }
}

impl<R: ReadAt + ?Sized> ReadAt for Arc<R> {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        (**self).read_at(buf, offset)
    }
}

/// How an input's bytes are reached.
pub enum Access {
    /// Bytes read at any offset, as often as asked.
    Seekable { source: Box<dyn ReadAt>, size: u64 },
    /// Bytes read once, in order.
    Stream {
        reader: Box<dyn Read + Send>,
        size: Option<u64>,
    },
}

impl Access {
    fn size(&self) -> Option<u64> {
        match self {
            Access::Seekable { size, .. } => Some(*size),
            Access::Stream { size, .. } => *size,
        }
    }
}

/// One file bviz reads.
pub struct Input {
    name: String,
    head: Option<Vec<u8>>,
    access: Access,
}

impl Input {
    /// The input `name` names, with its head read from `access`.
    pub fn new(name: String, mut access: Access) -> io::Result<Input> {
        let head_limit = HEAD_BYTES as u64;
        let expected = access
            .size()
            .map_or(head_limit, |size| size.min(head_limit));
        let mut head = Vec::with_capacity(expected as usize);
        match &mut access {
            Access::Seekable { source, .. } => At::new(&**source, 0)
                .take(head_limit)
                .read_to_end(&mut head)?,
            Access::Stream { reader, .. } => reader.take(head_limit).read_to_end(&mut head)?,
        };
        Ok(Input {
            name,
            head: Some(head),
            access,
        })
    }

    /// What the user knows the input by: on the desktop, the path as given.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The input's size in bytes, where it is known ahead of reading it.
    pub fn size(&self) -> Option<u64> {
        self.access.size()
    }

    /// The first [`HEAD_BYTES`] of the input, or all of a shorter one; `None`
    /// where they were not read ahead.
    pub fn head(&self) -> Option<&[u8]> {
        self.head.as_deref()
    }

    /// Where the bytes that follow the head start.
    fn read_ahead(&self) -> u64 {
        self.head().map_or(0, |head| head.len() as u64)
    }

    /// The input read at any offset, for formats that read ahead of where
    /// they stream from.
    pub fn seekable(&self) -> Option<&dyn ReadAt> {
        match &self.access {
            Access::Seekable { source, .. } => Some(&**source),
            Access::Stream { .. } => None,
        }
    }

    /// The input from its start, read as often as asked; `None` for a stream,
    /// which yields its bytes once.
    pub fn reader(&self) -> Option<impl Read + Send + '_> {
        let rest = At::new(self.seekable()?, self.read_ahead());
        Some(self.head().unwrap_or_default().chain(rest))
    }

    /// The input from its start.
    pub fn into_reader(self) -> impl Read + Send {
        let read_ahead = self.read_ahead();
        let rest = match self.access {
            Access::Seekable { source, .. } => Rest::Seekable(At::new(source, read_ahead)),
            Access::Stream { reader, .. } => Rest::Stream(reader),
        };
        io::Cursor::new(self.head.unwrap_or_default()).chain(rest)
    }

    /// The input, and what opens it afresh as often as asked, where it is
    /// read at any offset; a stream, which yields its bytes once, comes back
    /// as `Err`.
    pub fn reopenable(self) -> Result<(Input, Reopener), Input> {
        let Access::Seekable { source, size } = self.access else {
            return Err(self);
        };
        let source: Arc<dyn ReadAt> = Arc::from(source);
        let reopener = Reopener {
            name: self.name.clone(),
            source: source.clone(),
            size,
        };
        let input = Input {
            name: self.name,
            head: self.head,
            access: Access::Seekable {
                source: Box::new(source),
                size,
            },
        };
        Ok((input, reopener))
    }

    /// The whole input, in one buffer.
    pub fn read_all(self) -> io::Result<Vec<u8>> {
        let read_ahead = self.read_ahead();
        let mut all = self.head.unwrap_or_default();
        if let Some(size) = self.access.size() {
            reserve_exact(&mut all, size.saturating_sub(read_ahead))?;
        }
        match self.access {
            Access::Seekable { source, .. } => source.read_to_end_from(read_ahead, &mut all)?,
            Access::Stream { mut reader, .. } => reader.read_to_end(&mut all)?,
        };
        Ok(all)
    }
}

/// Opens an input read at any offset afresh, holding its source but not its
/// head.
#[derive(Clone)]
pub struct Reopener {
    name: String,
    source: Arc<dyn ReadAt>,
    size: u64,
}

impl Reopener {
    /// The input from its start, its head read ahead again.
    pub fn open(&self) -> io::Result<Input> {
        let access = Access::Seekable {
            source: Box::new(self.source.clone()),
            size: self.size,
        };
        Input::new(self.name.clone(), access)
    }
}

/// An empty vector with room for `capacity` items where that much memory can
/// be had, and growing as it fills where it cannot.
pub fn reserved<T>(capacity: usize) -> Vec<T> {
    let mut items = Vec::new();
    let _ = items.try_reserve_exact(capacity);
    items
}

/// An empty string with room for `capacity` bytes, as [`reserved`] reserves.
pub fn reserved_text(capacity: usize) -> String {
    let mut text = String::new();
    let _ = text.try_reserve_exact(capacity);
    text
}

/// Reserves room in `buf` for `additional` more bytes, failing with
/// [`io::ErrorKind::OutOfMemory`] where that much memory cannot be had.
fn reserve_exact(buf: &mut Vec<u8>, additional: u64) -> io::Result<()> {
    let additional = usize::try_from(additional).map_err(|_| io::ErrorKind::OutOfMemory)?;
    buf.try_reserve_exact(additional)
        .map_err(|e| io::Error::new(io::ErrorKind::OutOfMemory, e))
}

/// A [`ReadAt`] source read in order from an offset.
struct At<S> {
    source: S,
    offset: u64,
}

impl<S> At<S> {
    fn new(source: S, offset: u64) -> Self {
        At { source, offset }
    }
}

impl<S: Deref<Target: ReadAt>> Read for At<S> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.source.read_at(buf, self.offset)?;
        self.offset += n as u64;
        Ok(n)
    }
}

/// What follows an input's head.
enum Rest {
    Seekable(At<Box<dyn ReadAt>>),
    Stream(Box<dyn Read + Send>),
}

impl Read for Rest {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Rest::Seekable(at) => at.read(buf),
            Rest::Stream(reader) => reader.read(buf),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    impl ReadAt for Vec<u8> {
        fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
            let start = usize::try_from(offset).map_or(self.len(), |at| at.min(self.len()));
            let n = buf.len().min(self.len() - start);
            buf[..n].copy_from_slice(&self[start..start + n]);
            Ok(n)
        }
    }

    /// Bytes that differ at every offset a test reads from.
    pub(crate) fn bytes(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 7 + i / 251) as u8).collect()
    }

    fn seekable(content: Vec<u8>) -> Input {
        let size = content.len() as u64;
        let access = Access::Seekable {
            source: Box::new(content),
            size,
        };
        Input::new("seekable".into(), access).expect("an in-memory input reads")
    }

    fn stream(content: Vec<u8>) -> Input {
        let access = Access::Stream {
            reader: Box::new(io::Cursor::new(content)),
            size: None,
        };
        Input::new("stream".into(), access).expect("an in-memory input reads")
    }

    fn read(mut reader: impl Read) -> Vec<u8> {
        let mut out = Vec::new();
        reader
            .read_to_end(&mut out)
            .expect("an in-memory input reads");
        out
    }

    /// Lengths short of, at, and past the head.
    const LENGTHS: [usize; 5] = [0, 1, HEAD_BYTES - 1, HEAD_BYTES, 3 * HEAD_BYTES + 5];

    #[test]
    fn the_head_is_the_start_of_the_input() {
        for len in LENGTHS {
            let content = bytes(len);
            let head = Some(&content[..len.min(HEAD_BYTES)]);
            assert_eq!(seekable(content.clone()).head(), head, "{len}");
            assert_eq!(stream(content.clone()).head(), head, "{len}");
        }
    }

    #[test]
    fn a_short_input_reserves_no_more_head_than_it_holds() {
        let input = seekable(bytes(3));
        assert_eq!(input.head.expect("read ahead").capacity(), 3);
    }

    #[test]
    fn every_reader_replays_the_head_then_the_rest() {
        for len in LENGTHS {
            let content = bytes(len);
            assert_eq!(read(seekable(content.clone()).into_reader()), content);
            assert_eq!(read(stream(content.clone()).into_reader()), content);
            assert_eq!(seekable(content.clone()).read_all().unwrap(), content);
            assert_eq!(stream(content.clone()).read_all().unwrap(), content);
        }
    }

    #[test]
    fn a_seekable_input_reads_from_its_start_as_often_as_asked() {
        let content = bytes(2 * HEAD_BYTES + 1);
        let input = seekable(content.clone());
        assert_eq!(read(input.reader().expect("seekable")), content);
        assert_eq!(read(input.reader().expect("seekable")), content);
        assert_eq!(input.size(), Some(content.len() as u64));
        let mut tail = [0u8; 2];
        let at = content.len() - tail.len();
        let source = input.seekable().expect("seekable");
        source.read_exact_at(&mut tail, at as u64).unwrap();
        assert_eq!(tail, content[at..]);
        let past_end = source.read_exact_at(&mut tail, at as u64 + 1);
        assert_eq!(
            past_end.map_err(|e| e.kind()),
            Err(io::ErrorKind::UnexpectedEof)
        );
    }

    #[test]
    fn a_stream_has_no_reader_to_restart() {
        let input = stream(bytes(1));
        assert!(input.reader().is_none());
        assert!(input.seekable().is_none());
        assert_eq!(input.size(), None);
        assert!(input.reopenable().is_err());
    }

    #[test]
    fn a_reopened_input_reads_as_the_first_opening_did() {
        for len in LENGTHS {
            let content = bytes(len);
            let Ok((input, reopener)) = seekable(content.clone()).reopenable() else {
                panic!("a seekable input reopens");
            };
            assert_eq!(read(input.into_reader()), content, "{len}");
            for _ in 0..2 {
                let again = reopener.open().expect("an in-memory input reads");
                assert_eq!(again.name(), "seekable");
                assert_eq!(again.size(), Some(len as u64));
                assert_eq!(again.head(), Some(&content[..len.min(HEAD_BYTES)]));
                assert_eq!(read(again.into_reader()), content, "{len}");
            }
        }
    }

    #[test]
    fn a_size_past_what_memory_can_hold_fails_the_read_whole() {
        let access = Access::Stream {
            reader: Box::new(io::Cursor::new(bytes(1))),
            size: Some(u64::MAX),
        };
        let input = Input::new("huge".into(), access).expect("an in-memory input reads");
        assert_eq!(
            input.read_all().map_err(|e| e.kind()),
            Err(io::ErrorKind::OutOfMemory)
        );
    }
}
