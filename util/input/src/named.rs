use std::borrow::Cow;
use std::fmt;
use std::io;
use std::path::PathBuf;

use crate::{Access, HEAD_BYTES, Input, ReadAt};

/// A file the user named to bviz: by its path, or, where there is no file
/// system to name it in, by its name and the bytes it holds or the source
/// they are read from.
///
/// A browser's `File.name` names the file in the latter two: the file's base
/// name, without the directory a browser does not tell, so two files of one
/// name are distinct files that share a label.
pub enum Named {
    Path(PathBuf),
    Memory {
        name: String,
        bytes: Vec<u8>,
    },
    /// A file read at any offset through `source`, of `size` bytes.
    Seekable {
        name: String,
        source: Box<dyn ReadAt>,
        size: u64,
    },
}

impl Named {
    /// The file opened, or, where it does not open, given back with why.
    pub fn open(self) -> Result<Opened, (Named, io::Error)> {
        match self {
            Named::Path(path) => match Input::open(&path) {
                Ok(input) => Ok(Opened(Open::Path { path, input })),
                Err(error) => Err((Named::Path(path), error)),
            },
            Named::Memory { name, bytes } => Ok(Opened(Open::Memory { name, bytes })),
            Named::Seekable { name, source, size } => match Input::read_head(&*source, size) {
                Ok(head) => Ok(Opened(Open::Seekable {
                    name,
                    source,
                    size,
                    head,
                })),
                Err(error) => Err((Named::Seekable { name, source, size }, error)),
            },
        }
    }

    /// The file's size in bytes, where it is known without opening it.
    pub fn size(&self) -> Option<u64> {
        match self {
            Named::Path(path) => (std::fs::metadata(path).ok())
                .filter(|metadata| metadata.is_file())
                .map(|metadata| metadata.len()),
            Named::Memory { bytes, .. } => Some(bytes.len() as u64),
            Named::Seekable { size, .. } => Some(*size),
        }
    }

    /// What the user knows the file by: its path as given, or its name.
    pub fn name(&self) -> Cow<'_, str> {
        match self {
            Named::Path(path) => path.to_string_lossy(),
            Named::Memory { name, .. } | Named::Seekable { name, .. } => Cow::Borrowed(name),
        }
    }
}

impl From<PathBuf> for Named {
    fn from(path: PathBuf) -> Self {
        Named::Path(path)
    }
}

impl fmt::Debug for Named {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Named::Path(path) => f.debug_tuple("Path").field(path).finish(),
            Named::Memory { name, bytes } => f
                .debug_struct("Memory")
                .field("name", name)
                .field("size", &bytes.len())
                .finish(),
            Named::Seekable { name, size, .. } => f
                .debug_struct("Seekable")
                .field("name", name)
                .field("size", size)
                .finish(),
        }
    }
}

/// A [`Named`] file opened. It holds the file alone until it is taken as an
/// [`Input`] to read, or given back unread as the file it was opened from.
pub struct Opened(Open);

enum Open {
    Path {
        path: PathBuf,
        input: Input,
    },
    Memory {
        name: String,
        bytes: Vec<u8>,
    },
    Seekable {
        name: String,
        source: Box<dyn ReadAt>,
        size: u64,
        head: Vec<u8>,
    },
}

impl Opened {
    /// As [`Input::head`].
    pub fn head(&self) -> Option<&[u8]> {
        match &self.0 {
            Open::Path { input, .. } => input.head(),
            Open::Memory { bytes, .. } => Some(&bytes[..bytes.len().min(HEAD_BYTES)]),
            Open::Seekable { head, .. } => Some(head),
        }
    }

    pub fn into_input(self) -> Input {
        match self.0 {
            Open::Path { input, .. } => input,
            Open::Memory { name, bytes } => Input::in_memory(name, bytes),
            Open::Seekable {
                name,
                source,
                size,
                head,
            } => Input {
                name,
                head: Some(head),
                access: Access::Seekable { source, size },
                reached: Default::default(),
            },
        }
    }

    pub fn into_named(self) -> Named {
        match self.0 {
            Open::Path { path, .. } => Named::Path(path),
            Open::Memory { name, bytes } => Named::Memory { name, bytes },
            Open::Seekable {
                name, source, size, ..
            } => Named::Seekable { name, source, size },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::bytes;
    use std::io::Read;

    fn memory(name: &str, content: Vec<u8>) -> Named {
        Named::Memory {
            name: name.into(),
            bytes: content,
        }
    }

    #[test]
    fn a_file_in_memory_opens_seekable_under_its_name() {
        let content = bytes(HEAD_BYTES + 3);
        let opened = memory("a.profile.gz", content.clone()).open().unwrap();
        assert_eq!(opened.head(), Some(&content[..HEAD_BYTES]));
        let input = opened.into_input();
        assert_eq!(input.name(), "a.profile.gz");
        assert_eq!(input.size(), Some(content.len() as u64));
        assert_eq!(input.head(), Some(&content[..HEAD_BYTES]));
        let mut read = Vec::new();
        input
            .reader()
            .expect("seekable")
            .read_to_end(&mut read)
            .unwrap();
        assert_eq!(read, content);
        assert_eq!(input.read_all().unwrap(), content);
    }

    #[test]
    fn reading_a_file_in_memory_whole_hands_over_its_very_buffer() {
        let content = bytes(HEAD_BYTES + 3);
        let at = content.as_ptr();
        let input = memory("taken", content).open().unwrap().into_input();
        let read = input.read_all().unwrap();
        assert_eq!(read.as_ptr(), at);
    }

    #[test]
    fn a_file_in_memory_given_back_unread_keeps_its_buffer() {
        let content = bytes(3);
        let at = content.as_ptr();
        let Named::Memory { name, bytes } = memory("kept", content).open().unwrap().into_named()
        else {
            panic!("a file in memory is given back in memory");
        };
        assert_eq!((name.as_str(), bytes.as_ptr()), ("kept", at));
    }

    fn seekable(name: &str, content: Vec<u8>) -> Named {
        let size = content.len() as u64;
        Named::Seekable {
            name: name.into(),
            source: Box::new(content),
            size,
        }
    }

    #[test]
    fn a_seekable_file_opens_on_its_source() {
        let content = bytes(HEAD_BYTES + 3);
        let opened = seekable("a.pb", content.clone()).open().unwrap();
        assert_eq!(opened.head(), Some(&content[..HEAD_BYTES]));
        let input = opened.into_input();
        assert_eq!(input.name(), "a.pb");
        assert_eq!(input.size(), Some(content.len() as u64));
        assert!(input.seekable().is_some());
        assert_eq!(input.read_all().unwrap(), content);
    }

    #[test]
    fn a_seekable_file_given_back_unread_reads_from_its_start() {
        let content = bytes(HEAD_BYTES + 3);
        let named = seekable("again.pb", content.clone())
            .open()
            .unwrap()
            .into_named();
        assert_eq!(
            format!("{named:?}"),
            format!(
                r#"Seekable {{ name: "again.pb", size: {} }}"#,
                content.len()
            )
        );
        let input = named.open().unwrap().into_input();
        assert_eq!(input.read_all().unwrap(), content);
    }

    /// A source every read of which fails.
    struct Unreadable;

    impl ReadAt for Unreadable {
        fn read_at(&self, _: &mut [u8], _: u64) -> io::Result<usize> {
            Err(io::ErrorKind::NotFound.into())
        }
    }

    #[test]
    fn a_seekable_file_whose_head_does_not_read_is_given_back() {
        let named = Named::Seekable {
            name: "gone.bep".into(),
            source: Box::new(Unreadable),
            size: 1,
        };
        let Err((named, error)) = named.open() else {
            panic!("an unreadable source does not open");
        };
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert_eq!(named.name(), "gone.bep");
    }

    #[test]
    fn a_path_that_does_not_open_is_given_back() {
        let missing = PathBuf::from("missing").join("file.bep");
        let Err((named, _)) = Named::from(missing.clone()).open() else {
            panic!("no file is at {}", missing.display());
        };
        assert_eq!(named.name(), missing.to_string_lossy());
    }

    #[test]
    fn a_file_in_memory_reopens_on_its_bytes() {
        let content = bytes(HEAD_BYTES + 3);
        let opened = memory("again", content.clone()).open().unwrap();
        let Ok((input, reopener)) = opened.into_input().reopenable() else {
            panic!("an in-memory file reopens");
        };
        drop(input);
        assert_eq!(reopener.open().unwrap().read_all().unwrap(), content);
    }

    #[test]
    fn a_file_is_named_by_what_the_user_knows_it_by() {
        assert_eq!(Named::from(PathBuf::from("dir/a.bep")).name(), "dir/a.bep");
        let held = memory("b.bep", vec![1]);
        assert_eq!(held.name(), "b.bep");
        assert_eq!(format!("{held:?}"), r#"Memory { name: "b.bep", size: 1 }"#);
    }
}
