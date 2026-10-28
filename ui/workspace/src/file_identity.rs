//! What tells a file named to bviz apart from another, as far as the platform
//! lets bviz tell.

use bviz_input::Named;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use std::{fs, io};

/// A file as bviz can tell it apart: two names of one unchanged file are
/// equal.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum FileIdentity {
    /// A file in the file system, by where it is, its symbolic links resolved,
    /// and by its size and time of last change, which tell it apart from the
    /// file it was before it was written again.
    Path {
        path: PathBuf,
        size: u64,
        modified: Option<SystemTime>,
    },
    /// A file a browser hands over, by what the browser tells of it: its base
    /// name alone, so two distinct files of one name, size, and time of change
    /// are the same file to bviz.
    Browser {
        name: String,
        size: u64,
        /// The browser's `File.lastModified`, in milliseconds since the epoch.
        last_modified: u64,
    },
}

impl FileIdentity {
    /// The identity of the file at `path`. Blocks on the file system.
    pub fn of_path(path: &Path) -> io::Result<Self> {
        let metadata = fs::metadata(path)?;
        Ok(Self::Path {
            path: fs::canonicalize(path)?,
            size: metadata.len(),
            modified: metadata.modified().ok(),
        })
    }

    /// The identity of a file a browser tells the `name`, `size`, and
    /// `File.lastModified` of.
    pub fn of_browser_file(name: String, size: u64, last_modified: f64) -> Self {
        Self::Browser {
            name,
            size,
            last_modified: last_modified as u64,
        }
    }
}

/// `file` with its identity, `None` where the platform tells none, as for a
/// file that cannot be looked at.
pub struct Identified<T> {
    pub file: T,
    pub identity: Option<FileIdentity>,
}

impl<T> Identified<T> {
    /// The identities of those of `files` that have one.
    pub fn identities(files: &[Self]) -> Vec<FileIdentity> {
        files
            .iter()
            .filter_map(|file| file.identity.clone())
            .collect()
    }

    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Identified<U> {
        Identified {
            file: f(self.file),
            identity: self.identity,
        }
    }
}

impl Identified<Named> {
    /// `named` with the identity its path gives, which a file held in memory
    /// has none of. Blocks on the file system.
    pub fn of_named(named: Named) -> Self {
        let identity = match &named {
            Named::Path(path) => FileIdentity::of_path(path).ok(),
            Named::Memory { .. } => None,
        };
        Self {
            file: named,
            identity,
        }
    }
}

/// What a file named to a window repeats.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Repeat {
    /// A file an open document was read from.
    Open,
    /// A file of a batch still being opened.
    BeingOpened,
    /// A file named before it in its own batch.
    NamedTwice,
}

impl Repeat {
    /// How a list of the files repeating others says what each repeats.
    pub(crate) fn reads(self) -> &'static str {
        match self {
            Self::Open => "Open already",
            Self::BeingOpened => "Being opened",
            Self::NamedTwice => "Named twice",
        }
    }
}

/// For each file of `batch`, in order, the file it repeats: one of `open`,
/// one of `pending`, or one before it in `batch`, in that precedence; `None`
/// where it repeats none, as a file of no identity never does.
pub(crate) fn repeats<'a>(
    batch: impl ExactSizeIterator<Item = Option<&'a FileIdentity>>,
    open: &HashSet<&FileIdentity>,
    pending: &HashSet<&FileIdentity>,
) -> Vec<Option<Repeat>> {
    let mut named = HashSet::with_capacity(batch.len());
    let mut repeats = Vec::with_capacity(batch.len());
    for identity in batch {
        repeats.push(identity.and_then(|identity| {
            let twice = !named.insert(identity);
            if open.contains(identity) {
                Some(Repeat::Open)
            } else if pending.contains(identity) {
                Some(Repeat::BeingOpened)
            } else {
                twice.then_some(Repeat::NamedTwice)
            }
        }));
    }
    repeats
}

/// The files of `batch` to open: each that `repeats` says repeats none, and
/// each that does where the answer of its place among those in `again` is to
/// open it again.
pub(crate) fn kept<T>(
    batch: Vec<T>,
    repeats: &[Option<Repeat>],
    again: impl IntoIterator<Item = bool>,
) -> Vec<T> {
    let mut again = again.into_iter();
    let mut kept = Vec::with_capacity(batch.len());
    for (file, repeat) in batch.into_iter().zip(repeats) {
        if repeat.is_none() || again.next() == Some(true) {
            kept.push(file);
        }
    }
    kept
}

/// Names a batch of files [`Batches`] holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BatchId(u64);

/// The files of each batch named to a window from when it is named until it
/// settles, as the batches being opened.
#[derive(Default)]
pub(crate) struct Batches {
    next: u64,
    held: Vec<(BatchId, Vec<FileIdentity>)>,
}

impl Batches {
    pub(crate) fn hold(&mut self, files: Vec<FileIdentity>) -> BatchId {
        let batch = BatchId(self.next);
        self.next += 1;
        self.held.push((batch, files));
        batch
    }

    /// Holds `files` alone of `batch`, of whose files the rest are not to be
    /// opened.
    pub(crate) fn keep(&mut self, batch: BatchId, files: Vec<FileIdentity>) {
        if let Some((_, held)) = self.held.iter_mut().find(|(id, _)| *id == batch) {
            *held = files;
        }
    }

    pub(crate) fn settle(&mut self, batch: BatchId) {
        self.held.retain(|(id, _)| *id != batch);
    }

    pub(crate) fn files(&self) -> impl Iterator<Item = &FileIdentity> {
        self.held.iter().flat_map(|(_, files)| files)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn browser(name: &str) -> FileIdentity {
        FileIdentity::of_browser_file(name.to_owned(), 1, 0.0)
    }

    fn repeats_of(
        batch: &[Option<&FileIdentity>],
        open: &[&FileIdentity],
        pending: &[&FileIdentity],
    ) -> Vec<Option<Repeat>> {
        let open = open.iter().copied().collect();
        let pending = pending.iter().copied().collect();
        repeats(batch.iter().copied(), &open, &pending)
    }

    #[test]
    fn a_file_repeats_one_open_one_being_opened_or_one_named_before_it() {
        let [a, b, c, d] = ["a.bep", "b.pb", "c.profile.gz", "d.log"].map(browser);
        let batch = [Some(&a), Some(&b), Some(&c), Some(&d), Some(&c)];
        assert_eq!(
            repeats_of(&batch, &[&a], &[&b]),
            [
                Some(Repeat::Open),
                Some(Repeat::BeingOpened),
                None,
                None,
                Some(Repeat::NamedTwice),
            ]
        );
    }

    #[test]
    fn a_file_open_and_being_opened_repeats_the_open_one() {
        let a = browser("a.bep");
        assert_eq!(
            repeats_of(&[Some(&a), Some(&a)], &[&a], &[&a]),
            [Some(Repeat::Open), Some(Repeat::Open)]
        );
    }

    #[test]
    fn a_file_of_no_identity_repeats_none() {
        let a = browser("a.bep");
        assert_eq!(repeats_of(&[None, None], &[&a], &[&a]), [None, None]);
    }

    #[test]
    fn of_repeating_files_only_those_to_open_again_are_kept() {
        let repeated = Some(Repeat::Open);
        let repeats = [None, repeated, None, repeated, repeated];
        let kept = kept(vec!['a', 'b', 'c', 'd', 'e'], &repeats, [true, false, true]);
        assert_eq!(kept, ['a', 'b', 'c', 'e']);
    }

    #[test]
    fn a_settled_batch_holds_its_files_no_more() {
        let [a, b, c] = ["a.bep", "b.pb", "c.log"].map(browser);
        let mut batches = Batches::default();
        let first = batches.hold(vec![a.clone(), b.clone()]);
        let second = batches.hold(vec![c.clone()]);
        batches.keep(first, vec![b.clone()]);
        assert_eq!(batches.files().collect::<Vec<_>>(), [&b, &c]);
        batches.settle(second);
        assert_eq!(batches.files().collect::<Vec<_>>(), [&b]);
        batches.settle(first);
        assert_eq!(batches.files().count(), 0);
    }

    fn scratch() -> PathBuf {
        std::env::var_os("TEST_TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join("file_identity")
    }

    #[test]
    fn two_paths_to_one_file_name_one_identity() {
        let dir = scratch();
        fs::create_dir_all(dir.join("sub")).unwrap();
        let file = dir.join("a.bep");
        fs::write(&file, b"build events").unwrap();
        let roundabout = dir.join("sub").join("..").join("a.bep");
        assert_eq!(
            FileIdentity::of_path(&file).unwrap(),
            FileIdentity::of_path(&roundabout).unwrap()
        );
    }

    #[test]
    fn a_file_written_again_to_another_size_is_another_file() {
        let dir = scratch();
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("b.bep");
        fs::write(&file, b"short").unwrap();
        let before = FileIdentity::of_path(&file).unwrap();
        fs::write(&file, b"rather longer").unwrap();
        assert_ne!(before, FileIdentity::of_path(&file).unwrap());
    }

    #[test]
    fn a_path_naming_no_file_has_no_identity() {
        let missing = Identified::of_named(Named::Path(scratch().join("missing.bep")));
        assert!(missing.identity.is_none());
    }

    #[test]
    fn browser_files_are_told_apart_by_name_size_and_time_of_change() {
        let file = |name: &str, size, modified| {
            FileIdentity::of_browser_file(name.to_owned(), size, modified)
        };
        assert_eq!(file("a.pb", 3, 1.0e12), file("a.pb", 3, 1.0e12));
        assert_ne!(file("a.pb", 3, 1.0e12), file("b.pb", 3, 1.0e12));
        assert_ne!(file("a.pb", 3, 1.0e12), file("a.pb", 4, 1.0e12));
        assert_ne!(file("a.pb", 3, 1.0e12), file("a.pb", 3, 2.0e12));
    }
}
