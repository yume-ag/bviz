use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::{Access, Input, ReadAt};

#[cfg(unix)]
fn read_file_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(file, buf, offset)
}

#[cfg(windows)]
fn read_file_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(file, buf, offset)
}

impl ReadAt for File {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        read_file_at(self, buf, offset)
    }

    /// Reads through the file's cursor, which fills `buf`'s spare capacity
    /// without zeroing it first.
    fn read_to_end_from(self: Box<Self>, offset: u64, buf: &mut Vec<u8>) -> io::Result<usize> {
        let mut file = *self;
        file.seek(SeekFrom::Start(offset))?;
        file.read_to_end(buf)
    }
}

impl Input {
    /// The file at `path`. A regular file is opened at once and its head read
    /// ahead. A directory fails with [`io::ErrorKind::IsADirectory`]. Any
    /// other, such as a FIFO, is a stream opened on its first read and its
    /// head is left unread: opening or reading it could block the caller until
    /// its writer writes.
    pub fn open(path: &Path) -> io::Result<Input> {
        let metadata = std::fs::metadata(path)?;
        if metadata.is_dir() {
            return Err(io::ErrorKind::IsADirectory.into());
        }
        let name = path.display().to_string();
        if !metadata.is_file() {
            let reader = OpenOnRead {
                path: path.to_owned(),
                file: None,
            };
            return Ok(Input {
                name,
                head: None,
                access: Access::Stream {
                    reader: Box::new(reader),
                    size: None,
                },
            });
        }
        let file = File::open(path)?;
        let size = file.metadata()?.len();
        let access = Access::Seekable {
            source: Box::new(file),
            size,
        };
        Input::new(name, access)
    }
}

/// A file opened on its first read.
struct OpenOnRead {
    path: PathBuf,
    file: Option<File>,
}

impl Read for OpenOnRead {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let file = match &mut self.file {
            Some(file) => file,
            unopened @ None => unopened.insert(File::open(&self.path)?),
        };
        file.read(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HEAD_BYTES;
    use crate::tests::bytes;

    fn test_dir() -> PathBuf {
        std::env::var_os("TEST_TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
    }

    #[test]
    fn a_file_opens_seekable() {
        let path = test_dir().join("bviz_input_open");
        let content = bytes(HEAD_BYTES + 3);
        std::fs::write(&path, &content).unwrap();
        let input = Input::open(&path).expect("the file opens");
        assert_eq!(input.name(), path.display().to_string());
        assert_eq!(input.size(), Some(content.len() as u64));
        assert_eq!(input.head(), Some(&content[..HEAD_BYTES]));
        assert!(input.seekable().is_some());
        assert_eq!(input.read_all().unwrap(), content);
        assert!(Input::open(&path.with_extension("missing")).is_err());
    }

    #[test]
    fn a_directory_fails_to_open() {
        assert_eq!(
            Input::open(&test_dir()).map(|_| ()).map_err(|e| e.kind()),
            Err(io::ErrorKind::IsADirectory)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_fifo_opens_as_a_stream_on_its_first_read() {
        use std::ffi::{CString, c_char, c_int};
        use std::os::unix::ffi::OsStrExt;

        unsafe extern "C" {
            fn mkfifo(path: *const c_char, mode: u32) -> c_int;
        }
        const OWNER_READ_WRITE: u32 = 0o600;

        let path = test_dir().join("bviz_input_fifo");
        let _ = std::fs::remove_file(&path);
        let c_path = CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: `c_path` is a NUL-terminated path that outlives the call.
        let made = unsafe { mkfifo(c_path.as_ptr(), OWNER_READ_WRITE) };
        assert_eq!(made, 0, "mkfifo: {}", io::Error::last_os_error());

        // Opening a FIFO blocks until its writer opens it, which it has not.
        let input = Input::open(&path).expect("opens");
        assert_eq!(input.head(), None);
        assert!(input.seekable().is_none());

        let content = bytes(HEAD_BYTES + 3);
        let written = bviz_parallel::spawn({
            let content = content.clone();
            move || std::fs::write(&path, content)
        });
        assert_eq!(input.read_all().unwrap(), content);
        written.join().unwrap().unwrap();
    }
}
