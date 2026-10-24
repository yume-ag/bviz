use std::borrow::Cow;
use std::fmt;
use std::io;
use std::path::PathBuf;

use crate::{HEAD_BYTES, Input};

/// A file the user named to bviz: by its path, or, where there is no file
/// system to name it in, by its name and the bytes it holds.
pub enum Named {
    Path(PathBuf),
    Memory {
        /// The browser's `File.name` for it: the file's base name, without
        /// the directory a browser does not tell, so two files of one name
        /// are distinct files that share a label.
        name: String,
        bytes: Vec<u8>,
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
        }
    }

    /// What the user knows the file by: its path as given, or its name.
    pub fn name(&self) -> Cow<'_, str> {
        match self {
            Named::Path(path) => path.to_string_lossy(),
            Named::Memory { name, .. } => Cow::Borrowed(name),
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
        }
    }
}

/// A [`Named`] file opened. It holds the file alone until it is taken as an
/// [`Input`] to read, or given back unread as the file it was opened from.
pub struct Opened(Open);

enum Open {
    Path { path: PathBuf, input: Input },
    Memory { name: String, bytes: Vec<u8> },
}

impl Opened {
    /// As [`Input::head`].
    pub fn head(&self) -> Option<&[u8]> {
        match &self.0 {
            Open::Path { input, .. } => input.head(),
            Open::Memory { bytes, .. } => Some(&bytes[..bytes.len().min(HEAD_BYTES)]),
        }
    }

    pub fn into_input(self) -> Input {
        match self.0 {
            Open::Path { input, .. } => input,
            Open::Memory { name, bytes } => Input::in_memory(name, bytes),
        }
    }

    pub fn into_named(self) -> Named {
        match self.0 {
            Open::Path { path, .. } => Named::Path(path),
            Open::Memory { name, bytes } => Named::Memory { name, bytes },
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
