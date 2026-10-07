// Copyright © 2026 Jalapeno Labs

//! Reaching into a directory an agent owns without following anything it made.
//!
//! An agent can create any file, directory, or symbolic link under its own
//! workspace, and under its own home, at any moment, including between two
//! system calls the satellite makes. A path that is resolved to an absolute
//! location, checked, and then opened by name is a race the agent can win: swap
//! a directory for a link to `/` in the gap and the satellite, running as root,
//! reads or writes wherever the link points.
//!
//! So nothing here opens a path by name below its root. The root is opened
//! once, and every component below it is opened *relative to the directory
//! handle above it*, with `O_NOFOLLOW`, one at a time. A symbolic link anywhere
//! on the way is refused rather than followed, whatever it points at, and there
//! is no window in which a swapped component changes what a handle already
//! refers to. The root itself is the caller's to choose, and every caller
//! passes one the agent cannot replace: a thread's directory, whose parent is
//! the satellite's, or the agent's home, whose parent is the system's.
//!
//! Every operation that reaches into an agent's tree goes through here: the
//! workspace file routes, the listings, the artifact scan, turn attachments,
//! and harness session export and import. One walk, so a rule about links holds
//! everywhere or nowhere.
//!
//! | Refused | As |
//! |---|---|
//! | empty, absolute, an empty, `.`, or `..` component, or a NUL | [`FileError::InvalidPath`] |
//! | any component, or the file itself, is a symbolic link | [`FileError::InvalidPath`] |
//! | nothing at the path, for a read | [`FileError::NotFound`] |
//! | a directory, FIFO, socket, or device at the path or in its way | [`FileError::NotRegular`] |
//!
//! A read opens the file with `O_NONBLOCK` as well, because an agent can leave a
//! FIFO where a file was expected and a blocking open on one waits forever for a
//! writer that never comes. The type is checked on the open handle afterwards.
//!
//! Anything created is handed to the agent account on the open handle rather
//! than by path, so the agent owns what arrived in its tree and a link swapped
//! in between creating an inode and changing its owner changes nothing.

use std::path::Path;

/// The name a write is staged under beside its destination, before the uuid.
///
/// A dot file, so a listing an agent glances at does not lead with it, and a
/// fixed prefix, so an operator who finds one left by a satellite killed mid
/// transfer knows what it is. Listings leave these out: a staged write is not a
/// file yet.
pub const STAGING_PREFIX: &str = ".arsox-upload-";

/// Why a path in an agent's tree could not be read, written, or listed.
#[derive(Debug, thiserror::Error)]
pub enum FileError {
    /// The reason completes "the path ...".
    #[error("the path {0}")]
    InvalidPath(&'static str),

    #[error("nothing is at that path")]
    NotFound,

    #[error("something other than a regular file is at that path or in its way")]
    NotRegular,

    #[error("the file is larger than one write may carry")]
    TooLarge,

    #[error("the file could not be accessed: {0}")]
    Io(#[from] std::io::Error),
}

/// Splits a relative path into the components it names.
///
/// # Errors
///
/// Returns [`FileError::InvalidPath`] for an empty or absolute path, for any
/// empty, `.`, or `..` component, and for a NUL anywhere, which no filesystem
/// name can hold.
pub fn components(path: &str) -> Result<Vec<&str>, FileError> {
    if path.is_empty() {
        return Err(FileError::InvalidPath("is empty"));
    }

    if path.starts_with('/') {
        return Err(FileError::InvalidPath(
            "is absolute rather than relative to the workspace",
        ));
    }

    if path.contains('\0') {
        return Err(FileError::InvalidPath("contains a NUL byte"));
    }

    let pieces: Vec<&str> = path.split('/').collect();

    if pieces.iter().any(|piece| matches!(*piece, "" | "." | "..")) {
        return Err(FileError::InvalidPath(
            "has an empty, '.', or '..' component",
        ));
    }

    Ok(pieces)
}

/// The same components, owned, which is what crosses into a blocking task.
///
/// # Errors
///
/// The same as [`components`].
pub fn owned_components(path: &str) -> Result<Vec<String>, FileError> {
    Ok(components(path)?.into_iter().map(str::to_owned).collect())
}

/// One regular file a listing found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Components from the listing's root, the listed directory included.
    pub path: Vec<String>,

    pub size_bytes: u64,

    /// When the contents were last written, in nanoseconds since the epoch, as
    /// the file's own modification time says. An agent can set it with `touch`,
    /// so it is reported and never trusted.
    pub modified_nanos: i64,

    /// When the inode last changed, in nanoseconds since the epoch.
    ///
    /// The kernel sets it on every write, rename, and ownership change, and
    /// nothing short of root setting the clock moves it back. Together with the
    /// inode and the size it is what lets a hash be reused rather than
    /// recomputed: an agent that rewrites a file and restores its modification
    /// time still moves this.
    pub changed_nanos: i64,

    pub inode: u64,
}

impl Entry {
    /// The path as the contract spells it, `/`-separated.
    #[must_use]
    pub fn joined(&self) -> String {
        self.path.join("/")
    }
}

/// One page of a listing, in path order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Listing {
    pub entries: Vec<Entry>,

    /// Whether more files follow the last entry.
    pub more: bool,
}

/// Where a listing starts and how much of it to return.
#[derive(Debug, Clone, Default)]
pub struct Page {
    /// Only files strictly after this path, in listing order. Components from
    /// the listing's root, like [`Entry::path`].
    pub after: Option<Vec<String>>,

    /// At most this many entries. `None` returns everything, which only the
    /// artifact scan asks for.
    pub limit: Option<usize>,
}

/// Opens a regular file for reading, returning it and its size.
///
/// # Errors
///
/// See the module table.
pub fn open_file(root: &Path, pieces: &[String]) -> Result<(std::fs::File, u64), FileError> {
    platform::open_file(root, pieces)
}

/// Creates the directory `pieces` names below `root`, and every one on the way.
///
/// What is created belongs to the agent account. A directory that is already
/// there is left as it is, whoever owns it.
///
/// # Errors
///
/// Returns [`FileError::InvalidPath`] when a link is in the way and
/// [`FileError::NotRegular`] when a file is.
pub fn ensure_directory(root: &Path, pieces: &[String]) -> Result<(), FileError> {
    platform::ensure_directory(root, pieces)
}

/// Lists the regular files under the directory `prefix` names below `root`.
///
/// **Ordered by path, compared one component at a time**, which is the order a
/// depth-first walk visits files in when each directory's names are sorted. That
/// is what lets a page start after a cursor without walking what came before it:
/// a directory that sorts before the cursor, and does not lead to it, is skipped
/// whole.
///
/// Symbolic links are never followed and never listed, and neither is anything
/// but a regular file or a directory. A name that is not UTF-8 cannot be spelled
/// in the contract, so it is skipped with a debug log, and so is a staged write.
///
/// A prefix naming nothing lists nothing, which is the honest answer for an
/// artifacts/ directory no agent has written yet.
///
/// # Errors
///
/// Returns [`FileError::InvalidPath`] when the prefix crosses a link and
/// [`FileError::NotRegular`] when it names something other than a directory.
pub fn list(root: &Path, prefix: &[String], page: &Page) -> Result<Listing, FileError> {
    platform::list(root, prefix, page)
}

/// Creates the staged file a write streams into.
///
/// # Errors
///
/// See the module table.
pub fn stage(root: &Path, pieces: &[String]) -> Result<(Staged, std::fs::File), FileError> {
    platform::stage(root, pieces)
}

#[doc(inline)]
pub use platform::Staged;

/// The descriptor-relative walk, on the platforms that have one.
#[cfg(unix)]
mod platform {
    use super::{Entry, FileError, Listing, Page, STAGING_PREFIX};
    use rustix::fd::{AsFd, OwnedFd};
    use rustix::fs::{AtFlags, FileType, Mode, OFlags};
    use rustix::io::Errno;
    use std::path::Path;

    /// Directories created on the way to a written file.
    const DIRECTORY_MODE: u32 = 0o755;

    /// A written file. The agent owns it, so it can change this as it likes.
    const FILE_MODE: u32 = 0o644;

    /// Nanoseconds in one second, for turning a `stat` time into one integer.
    const NANOS_PER_SECOND: i64 = 1_000_000_000;

    /// Flags every directory in a walk is opened with.
    fn directory_flags() -> OFlags {
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
    }

    /// Opens the directory `pieces` names below `root`, creating what is
    /// missing when `create` is set.
    fn walk(root: &Path, pieces: &[String], create: bool) -> Result<OwnedFd, FileError> {
        // The root is the caller's, and every caller passes one whose parent the
        // agent cannot write. It is still opened without following a link on
        // its last component, which costs nothing.
        let mut directory = match rustix::fs::open(root, directory_flags(), Mode::empty()) {
            Ok(directory) => directory,
            Err(Errno::NOENT) => return Err(FileError::NotFound),
            Err(errno) => return Err(FileError::Io(errno.into())),
        };

        for piece in pieces {
            directory = descend(&directory, piece, create)?;
        }

        Ok(directory)
    }

    /// Opens one directory below another, never through a link.
    fn descend(directory: &OwnedFd, piece: &str, create: bool) -> Result<OwnedFd, FileError> {
        match rustix::fs::openat(directory, piece, directory_flags(), Mode::empty()) {
            Ok(child) => return Ok(child),
            Err(Errno::NOENT) if create => {}
            Err(errno) => return Err(classify(directory, piece, errno)),
        }

        match rustix::fs::mkdirat(directory, piece, Mode::from_raw_mode(DIRECTORY_MODE)) {
            // Somebody else created it between the two calls, which the open
            // below settles either way.
            Ok(()) | Err(Errno::EXIST) => {}
            Err(errno) => return Err(classify(directory, piece, errno)),
        }

        let child = rustix::fs::openat(directory, piece, directory_flags(), Mode::empty())
            .map_err(|errno| classify(directory, piece, errno))?;

        give_to_agent(&child)?;

        Ok(child)
    }

    /// Names what an open that failed ran into.
    ///
    /// `O_NOFOLLOW` reports a link as `ELOOP`, and `O_DIRECTORY` reports a link
    /// or a file as `ENOTDIR` depending on the kernel, so the entry itself is
    /// looked at, without following it, to say which.
    fn classify(directory: &OwnedFd, piece: &str, errno: Errno) -> FileError {
        if errno == Errno::NOENT {
            return FileError::NotFound;
        }

        match rustix::fs::statat(directory, piece, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => match FileType::from_raw_mode(stat.st_mode) {
                FileType::Symlink => {
                    FileError::InvalidPath("crosses a symbolic link, which is never followed")
                }
                // A directory that still would not open is not something the
                // agent arranged, so it is reported as the failure it is.
                FileType::Directory => FileError::Io(errno.into()),
                // A file, FIFO, socket, or device where a directory or a
                // file was needed.
                _other => FileError::NotRegular,
            },
            Err(Errno::NOENT) => FileError::NotFound,
            Err(_unreadable) => FileError::Io(errno.into()),
        }
    }

    pub(super) fn open_file(
        root: &Path,
        pieces: &[String],
    ) -> Result<(std::fs::File, u64), FileError> {
        let Some((leaf, parents)) = pieces.split_last() else {
            return Err(FileError::InvalidPath("is empty"));
        };

        let directory = walk(root, parents, false)?;

        // `O_NONBLOCK` so a FIFO left where a file was expected answers at once
        // instead of waiting for a writer. It changes nothing for a regular file.
        let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
        let file = rustix::fs::openat(&directory, leaf.as_str(), flags, Mode::empty())
            .map_err(|errno| classify(&directory, leaf, errno))?;

        let stat = rustix::fs::fstat(&file).map_err(std::io::Error::from)?;
        if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
            return Err(FileError::NotRegular);
        }

        let size = u64::try_from(stat.st_size).unwrap_or_default();

        Ok((std::fs::File::from(file), size))
    }

    pub(super) fn ensure_directory(root: &Path, pieces: &[String]) -> Result<(), FileError> {
        walk(root, pieces, true).map(drop)
    }

    pub(super) fn list(root: &Path, prefix: &[String], page: &Page) -> Result<Listing, FileError> {
        let directory = match walk(root, prefix, false) {
            Ok(directory) => directory,
            // Nothing there yet is an empty directory as far as a caller asking
            // "what is under here" is concerned.
            Err(FileError::NotFound) => return Ok(Listing::default()),
            Err(error) => return Err(error),
        };

        let mut listing = Listing::default();
        let mut path = prefix.to_vec();

        collect(
            &directory,
            &mut path,
            page.after.as_deref(),
            page,
            &mut listing,
        )?;

        Ok(listing)
    }

    /// Walks one directory in name order, depth first, filling `listing`.
    ///
    /// Returns once the page is full, with `listing.more` saying whether that
    /// is because something is left. `after` is dropped for a subtree that
    /// sorts wholly past it, which is every subtree once the walk has passed
    /// the cursor.
    fn collect(
        directory: &OwnedFd,
        path: &mut Vec<String>,
        after: Option<&[String]>,
        page: &Page,
        listing: &mut Listing,
    ) -> Result<(), FileError> {
        for name in sorted_names(directory)? {
            if listing.more {
                return Ok(());
            }

            path.push(name);

            // Component by component, which `Vec<String>`'s own ordering is.
            // A path that leads to the cursor is walked for what lies past it;
            // one that sorts before it, or is it, is skipped whole.
            let leads_to_cursor =
                after.is_some_and(|cursor| cursor.len() > path.len() && cursor.starts_with(path));
            let skipped = !leads_to_cursor && after.is_some_and(|cursor| path.as_slice() <= cursor);

            if !skipped {
                let within = if leads_to_cursor { after } else { None };
                visit(directory, path, within, page, listing)?;
            }

            path.pop();
        }

        Ok(())
    }

    /// Lists one entry: a file is taken, a directory is walked, and anything
    /// else, links included, is left alone.
    fn visit(
        parent: &OwnedFd,
        path: &mut Vec<String>,
        after: Option<&[String]>,
        page: &Page,
        listing: &mut Listing,
    ) -> Result<(), FileError> {
        let Some(name) = path.last() else {
            return Ok(());
        };

        let stat = match rustix::fs::statat(parent, name.as_str(), AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            // Removed since the directory was read, which an agent working in
            // it does constantly.
            Err(Errno::NOENT) => return Ok(()),
            Err(errno) => return Err(FileError::Io(errno.into())),
        };

        match FileType::from_raw_mode(stat.st_mode) {
            // A file only leads to the cursor when the cursor names something
            // below a file, which no listing produced. It sorts before it.
            FileType::RegularFile if after.is_some() => {}
            FileType::RegularFile => {
                if page
                    .limit
                    .is_some_and(|limit| listing.entries.len() >= limit)
                {
                    listing.more = true;
                    return Ok(());
                }

                listing.entries.push(Entry {
                    path: path.clone(),
                    size_bytes: u64::try_from(stat.st_size).unwrap_or_default(),
                    modified_nanos: stat
                        .st_mtime
                        .saturating_mul(NANOS_PER_SECOND)
                        .saturating_add(i64::try_from(stat.st_mtime_nsec).unwrap_or_default()),
                    changed_nanos: stat
                        .st_ctime
                        .saturating_mul(NANOS_PER_SECOND)
                        .saturating_add(i64::try_from(stat.st_ctime_nsec).unwrap_or_default()),
                    inode: stat.st_ino,
                });
            }
            FileType::Directory => {
                match rustix::fs::openat(parent, name.as_str(), directory_flags(), Mode::empty()) {
                    Ok(child) => collect(&child, path, after, page, listing)?,
                    // Swapped for a link or removed since it was looked at. The
                    // open refused to follow it, which is the whole point.
                    Err(Errno::NOENT | Errno::LOOP | Errno::NOTDIR) => {}
                    Err(errno) => return Err(FileError::Io(errno.into())),
                }
            }
            _link_or_special => {}
        }

        Ok(())
    }

    /// The names in a directory, sorted, without `.`, `..`, or staged writes.
    fn sorted_names(directory: &OwnedFd) -> Result<Vec<String>, FileError> {
        let mut reader = rustix::fs::Dir::read_from(directory).map_err(std::io::Error::from)?;
        let mut names = Vec::new();

        while let Some(entry) = reader.read() {
            let entry = entry.map_err(std::io::Error::from)?;
            let bytes = entry.file_name().to_bytes();

            if bytes == b"." || bytes == b".." {
                continue;
            }

            let Ok(name) = std::str::from_utf8(bytes) else {
                tracing::debug!(
                    event.name = "workspace.listing.unnamed",
                    file.name = %String::from_utf8_lossy(bytes),
                    "skipping a file whose name is not UTF-8, which the contract cannot spell",
                );
                continue;
            };

            if name.starts_with(STAGING_PREFIX) {
                continue;
            }

            names.push(name.to_owned());
        }

        // Bytewise, which is what the cursor is compared with.
        names.sort_unstable();

        Ok(names)
    }

    /// A write staged beside its destination, removed unless it is committed.
    #[derive(Debug)]
    pub struct Staged {
        directory: OwnedFd,
        staging: String,
        leaf: String,
        created: bool,
        committed: bool,
    }

    pub(super) fn stage(
        root: &Path,
        pieces: &[String],
    ) -> Result<(Staged, std::fs::File), FileError> {
        let Some((leaf, parents)) = pieces.split_last() else {
            return Err(FileError::InvalidPath("is empty"));
        };

        let directory = walk(root, parents, true)?;

        // Checked now so a write aimed at a directory or a link is refused
        // before a byte is sent, rather than after the whole body arrived. The
        // rename checks again, because the agent can change this meanwhile.
        let created = match rustix::fs::statat(&directory, leaf.as_str(), AtFlags::SYMLINK_NOFOLLOW)
        {
            Ok(stat) => match FileType::from_raw_mode(stat.st_mode) {
                FileType::RegularFile => false,
                FileType::Symlink => {
                    return Err(FileError::InvalidPath(
                        "names a symbolic link, which is never written through",
                    ));
                }
                _other => return Err(FileError::NotRegular),
            },
            Err(Errno::NOENT) => true,
            Err(errno) => return Err(FileError::Io(errno.into())),
        };

        let staging = format!("{STAGING_PREFIX}{}", uuid::Uuid::now_v7());

        let flags =
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let file = rustix::fs::openat(
            &directory,
            staging.as_str(),
            flags,
            Mode::from_raw_mode(FILE_MODE),
        )
        .map_err(std::io::Error::from)?;

        let staged = Staged {
            directory,
            staging,
            leaf: leaf.clone(),
            created,
            committed: false,
        };

        // After `staged` exists, so a failure here still removes the file.
        give_to_agent(&file)?;

        Ok((staged, std::fs::File::from(file)))
    }

    impl Staged {
        /// Renames the staged file over its destination.
        ///
        /// Returns whether the write created the file rather than replacing one.
        ///
        /// # Errors
        ///
        /// Returns [`FileError::NotRegular`] when something that is not a file
        /// took the destination while the bytes were arriving.
        pub fn commit(mut self) -> Result<bool, FileError> {
            match rustix::fs::renameat(
                &self.directory,
                self.staging.as_str(),
                &self.directory,
                self.leaf.as_str(),
            ) {
                Ok(()) => {
                    self.committed = true;
                    Ok(self.created)
                }
                Err(Errno::ISDIR | Errno::NOTDIR | Errno::NOTEMPTY) => Err(FileError::NotRegular),
                Err(errno) => Err(FileError::Io(errno.into())),
            }
        }
    }

    /// Removes a staged file that never reached its destination.
    ///
    /// A single unlink on a handle the satellite already holds, so running it
    /// synchronously on whichever thread drops this costs less than handing it
    /// to a blocking pool.
    impl Drop for Staged {
        fn drop(&mut self) {
            if self.committed {
                return;
            }

            if let Err(errno) =
                rustix::fs::unlinkat(&self.directory, self.staging.as_str(), AtFlags::empty())
            {
                tracing::warn!(
                    event.name = "workspace.file.staging_left",
                    file.name = self.staging,
                    "could not remove a staged write that did not complete: {errno}",
                );
            }
        }
    }

    /// Hands something the satellite created to the agent account, by handle.
    fn give_to_agent(handle: impl AsFd) -> Result<(), FileError> {
        crate::privilege::give_handle_to_agent(handle).map_err(FileError::Io)
    }
}

/// The same operations, refused, where there is no descriptor-relative walk.
#[cfg(not(unix))]
mod platform {
    use super::{FileError, Listing, Page};
    use std::path::Path;

    fn unsupported() -> FileError {
        FileError::Io(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "an agent's files are reached only on Unix, where a path can be walked without \
             following links",
        ))
    }

    pub(super) fn open_file(
        _root: &Path,
        _pieces: &[String],
    ) -> Result<(std::fs::File, u64), FileError> {
        Err(unsupported())
    }

    pub(super) fn ensure_directory(_root: &Path, _pieces: &[String]) -> Result<(), FileError> {
        Err(unsupported())
    }

    pub(super) fn list(
        _root: &Path,
        _prefix: &[String],
        _page: &Page,
    ) -> Result<Listing, FileError> {
        Err(unsupported())
    }

    /// A write that can never be staged here.
    #[derive(Debug)]
    pub struct Staged;

    impl Staged {
        /// Refused, like everything else on this platform.
        ///
        /// # Errors
        ///
        /// Always.
        pub fn commit(self) -> Result<bool, FileError> {
            Err(unsupported())
        }
    }

    pub(super) fn stage(
        _root: &Path,
        _pieces: &[String],
    ) -> Result<(Staged, std::fs::File), FileError> {
        Err(unsupported())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::{Read as _, Write as _};
    use std::os::unix::fs::symlink;

    /// A scratch directory, removed when the test ends.
    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("arsox-files-{}", uuid::Uuid::now_v7()));
            std::fs::create_dir_all(&root).expect("should create the scratch workspace");
            Self(root)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            drop(std::fs::remove_dir_all(&self.0));
        }
    }

    fn owned(path: &str) -> Vec<String> {
        owned_components(path).expect("a valid path")
    }

    fn read(root: &Path, path: &str) -> Result<String, FileError> {
        let (mut file, size) = open_file(root, &owned(path))?;
        let mut contents = String::new();
        file.read_to_string(&mut contents).expect("readable");
        assert_eq!(size, contents.len() as u64);
        Ok(contents)
    }

    fn write(root: &Path, path: &str, contents: &str) -> Result<bool, FileError> {
        let (staged, mut file) = stage(root, &owned(path))?;
        file.write_all(contents.as_bytes()).expect("writable");
        staged.commit()
    }

    fn listed(root: &Path, prefix: &str, page: &Page) -> (Vec<String>, bool) {
        let prefix = if prefix.is_empty() {
            Vec::new()
        } else {
            owned(prefix)
        };
        let listing = list(root, &prefix, page).expect("listable");

        (
            listing.entries.iter().map(Entry::joined).collect(),
            listing.more,
        )
    }

    #[test]
    fn a_path_that_could_leave_the_workspace_is_refused_before_anything_opens() {
        for hostile in [
            "",
            "/etc/passwd",
            "..",
            "../escape",
            "repos/../../escape",
            "./file",
            "repos//file",
            "repos/",
            "nul\0byte",
        ] {
            assert!(
                matches!(components(hostile), Err(FileError::InvalidPath(_))),
                "{hostile:?} should be refused"
            );
        }

        assert_eq!(
            components("repos/api/.../out put.txt").expect("valid"),
            ["repos", "api", "...", "out put.txt"]
        );
    }

    #[test]
    fn a_file_is_written_whole_and_read_back() {
        let scratch = Scratch::new();

        assert!(write(&scratch.0, "inbox/deep/hello.txt", "hello").expect("written"));
        assert_eq!(
            read(&scratch.0, "inbox/deep/hello.txt").expect("read"),
            "hello"
        );

        // Replacing reports that nothing was created.
        assert!(!write(&scratch.0, "inbox/deep/hello.txt", "again").expect("written"));
        assert_eq!(
            read(&scratch.0, "inbox/deep/hello.txt").expect("read"),
            "again"
        );

        // No staged file is left beside it.
        let names: Vec<String> = std::fs::read_dir(scratch.0.join("inbox/deep"))
            .expect("listable")
            .map(|entry| {
                entry
                    .expect("an entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(names, ["hello.txt"]);
    }

    #[test]
    fn a_write_that_is_never_committed_leaves_nothing_behind() {
        let scratch = Scratch::new();

        let (staged, mut file) = stage(&scratch.0, &owned("half.txt")).expect("staged");
        file.write_all(b"half").expect("writable");
        drop(staged);

        assert_eq!(
            std::fs::read_dir(&scratch.0).expect("listable").count(),
            0,
            "the staged file should be removed"
        );
    }

    #[test]
    fn a_symbolic_link_to_a_file_outside_is_neither_read_nor_written_through() {
        let scratch = Scratch::new();
        let outside = Scratch::new();
        std::fs::write(outside.0.join("secret"), "outside").expect("written");

        symlink(outside.0.join("secret"), scratch.0.join("link")).expect("linked");

        assert!(matches!(
            read(&scratch.0, "link"),
            Err(FileError::InvalidPath(_))
        ));
        assert!(matches!(
            write(&scratch.0, "link", "overwritten"),
            Err(FileError::InvalidPath(_))
        ));
        assert_eq!(
            std::fs::read_to_string(outside.0.join("secret")).expect("readable"),
            "outside"
        );
    }

    #[test]
    fn a_symbolic_link_to_a_directory_in_the_middle_is_never_crossed() {
        let scratch = Scratch::new();
        let outside = Scratch::new();
        std::fs::create_dir_all(outside.0.join("etc")).expect("created");
        std::fs::write(outside.0.join("etc/passwd"), "outside").expect("written");

        std::fs::create_dir_all(scratch.0.join("repos")).expect("created");
        symlink(&outside.0, scratch.0.join("repos/escape")).expect("linked");

        assert!(matches!(
            read(&scratch.0, "repos/escape/etc/passwd"),
            Err(FileError::InvalidPath(_))
        ));
        assert!(matches!(
            write(&scratch.0, "repos/escape/etc/planted", "planted"),
            Err(FileError::InvalidPath(_))
        ));
        assert!(!outside.0.join("etc/planted").exists());
    }

    #[test]
    fn a_directory_or_a_fifo_is_not_a_regular_file() {
        let scratch = Scratch::new();
        std::fs::create_dir_all(scratch.0.join("folder")).expect("created");

        assert!(matches!(
            read(&scratch.0, "folder"),
            Err(FileError::NotRegular)
        ));
        assert!(matches!(
            write(&scratch.0, "folder", "contents"),
            Err(FileError::NotRegular)
        ));

        // A FIFO answers at once rather than waiting forever for a writer.
        rustix::fs::mkfifoat(
            rustix::fs::CWD,
            scratch.0.join("pipe"),
            rustix::fs::Mode::from_raw_mode(0o600),
        )
        .expect("a fifo");
        assert!(matches!(
            read(&scratch.0, "pipe"),
            Err(FileError::NotRegular)
        ));

        // A file where a directory is needed.
        std::fs::write(scratch.0.join("plain"), "file").expect("written");
        assert!(matches!(
            read(&scratch.0, "plain/below"),
            Err(FileError::NotRegular)
        ));
    }

    #[test]
    fn nothing_at_the_path_is_not_found() {
        let scratch = Scratch::new();

        assert!(matches!(
            read(&scratch.0, "missing"),
            Err(FileError::NotFound)
        ));
        assert!(matches!(
            read(&scratch.0, "missing/deeper"),
            Err(FileError::NotFound)
        ));
    }

    #[test]
    fn a_directory_is_created_once_and_a_link_in_its_place_is_refused() {
        let scratch = Scratch::new();

        ensure_directory(&scratch.0, &owned("artifacts/renders")).expect("created");
        ensure_directory(&scratch.0, &owned("artifacts/renders")).expect("already there");
        assert!(scratch.0.join("artifacts/renders").is_dir());

        let outside = Scratch::new();
        symlink(&outside.0, scratch.0.join("elsewhere")).expect("linked");

        assert!(matches!(
            ensure_directory(&scratch.0, &owned("elsewhere/planted")),
            Err(FileError::InvalidPath(_))
        ));
        assert!(!outside.0.join("planted").exists());
    }

    /// A tree whose component order and plain string order disagree: `a-c`
    /// sorts before `a/b` as a string and after it as components.
    fn ordered_tree() -> Scratch {
        let scratch = Scratch::new();

        for path in ["a/b", "a/z/deep", "a-c", "b", "c/d/e", "c/f"] {
            let full = scratch.0.join(path);
            std::fs::create_dir_all(full.parent().expect("a parent")).expect("created");
            std::fs::write(full, path).expect("written");
        }

        scratch
    }

    #[test]
    fn a_listing_is_ordered_one_component_at_a_time() {
        let scratch = ordered_tree();

        let (paths, more) = listed(&scratch.0, "", &Page::default());

        assert_eq!(paths, ["a/b", "a/z/deep", "a-c", "b", "c/d/e", "c/f"]);
        assert!(!more);
    }

    #[test]
    fn pages_walk_the_whole_tree_exactly_once() {
        let scratch = ordered_tree();
        let mut seen = Vec::new();
        let mut after = None;

        loop {
            let page = Page {
                after: after.clone(),
                limit: Some(2),
            };
            let listing = list(&scratch.0, &[], &page).expect("listable");

            seen.extend(listing.entries.iter().map(Entry::joined));
            after = listing.entries.last().map(|entry| entry.path.clone());

            if !listing.more {
                break;
            }
        }

        assert_eq!(seen, ["a/b", "a/z/deep", "a-c", "b", "c/d/e", "c/f"]);
    }

    #[test]
    fn a_listing_under_a_prefix_names_paths_from_the_root() {
        let scratch = ordered_tree();

        let (paths, _more) = listed(&scratch.0, "c", &Page::default());
        assert_eq!(paths, ["c/d/e", "c/f"]);

        let (nothing, _more) = listed(&scratch.0, "absent", &Page::default());
        assert!(nothing.is_empty(), "a missing directory lists nothing");

        assert!(matches!(
            list(&scratch.0, &owned("b"), &Page::default()),
            Err(FileError::NotRegular)
        ));
    }

    #[test]
    fn a_listing_never_follows_or_reports_a_link_or_a_staged_write() {
        let scratch = Scratch::new();
        let outside = Scratch::new();
        std::fs::write(outside.0.join("secret"), "outside").expect("written");

        std::fs::write(scratch.0.join("kept"), "kept").expect("written");
        symlink(&outside.0, scratch.0.join("into-outside")).expect("linked");
        symlink(outside.0.join("secret"), scratch.0.join("to-secret")).expect("linked");
        std::fs::write(
            scratch.0.join(format!("{STAGING_PREFIX}half-written")),
            "half",
        )
        .expect("written");

        let (paths, _more) = listed(&scratch.0, "", &Page::default());

        assert_eq!(paths, ["kept"]);
    }

    #[test]
    fn rewriting_a_file_moves_its_change_time_even_when_its_modification_time_is_restored() {
        let scratch = Scratch::new();
        let path = scratch.0.join("render.png");
        std::fs::write(&path, "first").expect("written");

        let before = list(&scratch.0, &[], &Page::default()).expect("listable");

        // A change time only moves forward by a clock tick, which on some
        // filesystems is coarser than the time these two writes take.
        std::thread::sleep(std::time::Duration::from_millis(20));

        let modified = std::fs::metadata(&path)
            .expect("readable")
            .modified()
            .expect("a modification time");
        std::fs::write(&path, "other").expect("rewritten");
        std::fs::File::options()
            .write(true)
            .open(&path)
            .expect("openable")
            .set_modified(modified)
            .expect("restored");

        let after = list(&scratch.0, &[], &Page::default()).expect("listable");

        assert_eq!(
            before.entries[0].modified_nanos,
            after.entries[0].modified_nanos
        );
        assert_ne!(
            before.entries[0].changed_nanos,
            after.entries[0].changed_nanos
        );
    }
}
