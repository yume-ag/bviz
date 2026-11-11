use std::io::{self, Read};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// How many of a known total of bytes a read has got through, shared between
/// whatever reads and whatever shows how far it has got.
#[derive(Clone)]
pub struct ReadProgress {
    read: Arc<AtomicU64>,
    total: u64,
}

impl ReadProgress {
    /// A read of `total` bytes, none of them read yet.
    pub fn new(total: u64) -> Self {
        Self::counting(Arc::default(), total)
    }

    pub(crate) fn counting(read: Arc<AtomicU64>, total: u64) -> Self {
        Self { read, total }
    }

    /// The bytes read so far, at most the total.
    pub fn read(&self) -> u64 {
        self.read.load(Ordering::Relaxed).min(self.total)
    }

    pub fn total(&self) -> u64 {
        self.total
    }

    /// Whether every byte of the total is read.
    pub fn is_done(&self) -> bool {
        self.read() == self.total
    }
}

/// A reader adding each byte it reads to a [`ReadProgress`], which several
/// such readers may share to count a read of several inputs.
pub struct Counted<R> {
    reader: R,
    read: Arc<AtomicU64>,
}

impl<R> Counted<R> {
    pub fn new(reader: R, progress: &ReadProgress) -> Self {
        Self {
            reader,
            read: progress.read.clone(),
        }
    }
}

impl<R: Read> Read for Counted<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.reader.read(buf)?;
        self.read.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

/// A reader of an input from its start, raising the input's count of bytes
/// read to the furthest place any of its readers has reached, so a second
/// reader going over the same bytes counts none of them twice.
pub(crate) struct Reaching<R> {
    reader: R,
    at: u64,
    furthest: Arc<AtomicU64>,
}

impl<R> Reaching<R> {
    pub(crate) fn new(reader: R, furthest: Arc<AtomicU64>) -> Self {
        Self {
            reader,
            at: 0,
            furthest,
        }
    }
}

impl<R: Read> Read for Reaching<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.reader.read(buf)?;
        self.at += n as u64;
        self.furthest.fetch_max(self.at, Ordering::Relaxed);
        Ok(n)
    }
}
