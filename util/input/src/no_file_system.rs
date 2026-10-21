use std::io;
use std::path::Path;

use crate::Input;

impl Input {
    /// Fails with [`io::ErrorKind::Unsupported`], as the standard library's
    /// file system does where there is none: no path names a file here.
    pub fn open(_path: &Path) -> io::Result<Input> {
        Err(io::ErrorKind::Unsupported.into())
    }
}
