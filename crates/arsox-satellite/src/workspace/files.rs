// Copyright © 2026 Jalapeno Labs

//! Moving single files into and out of a thread's workspace.
//!
//! `GET /v1/threads/{id}/files/{path}` streams a file out and
//! `PUT /v1/threads/{id}/files/{path}` streams one in, both behind the same
//! bearer check as every other authenticated route. They exist so a host
//! application the satellite cannot reach can still pull what an agent produced
//! and push in what an agent needs: every byte moves on a request the host
//! application opened.
//!
//! # The workspace is the agent's, so every path is hostile
//!
//! An agent can create any file, directory, or symbolic link under its own
//! workspace, at any moment, including between two system calls the satellite
//! makes. A path that is resolved to an absolute location, checked, and then
//! opened by name is a race the agent can win: swap a directory for a link to
//! `/` in the gap and the satellite, running as root, reads or writes wherever
//! the link points.
//!
//! So nothing here opens a path by name. The thread's directory is opened once,
//! and every component below it is opened *relative to the directory handle
//! above it*, with `O_NOFOLLOW`, one at a time. A symbolic link anywhere on the
//! way is refused rather than followed, whatever it points at, and there is no
//! window in which a swapped component changes what a handle already refers to.
//!
//! | Refused | Answer |
//! |---|---|
//! | empty, absolute, an empty, `.`, or `..` component, or a NUL | `400 WORKSPACE_PATH_INVALID` |
//! | any component, or the file itself, is a symbolic link | `400 WORKSPACE_PATH_INVALID` |
//! | nothing at the path, for a read | `404 WORKSPACE_FILE_NOT_FOUND` |
//! | a directory, FIFO, socket, or device at the path or in its way | `409 WORKSPACE_FILE_NOT_REGULAR` |
//!
//! A read opens the file with `O_NONBLOCK` as well, because an agent can leave a
//! FIFO where a file was expected and a blocking open on one waits forever for a
//! writer that never comes. The type is checked on the open handle afterwards.
//!
//! # A write lands whole or not at all
//!
//! The body is streamed into a new file beside the destination, created with
//! `O_EXCL` under a name the agent cannot predict, and renamed over the
//! destination only once every byte `Content-Length` promised has arrived and
//! been synced. A reader never sees a half-written file, and a transfer that
//! fails leaves the destination as it was. Directories missing on the way are
//! created. Everything created is handed to the agent account, on the open
//! handle rather than by path, so the agent owns what arrived in its workspace.
//!
//! `Content-Length` is required and checked against [`MAX_WRITE_BYTES`] before a
//! byte is written, and the stream is counted as it arrives, so a body cannot
//! write past what it declared.

use crate::{Satellite, contract_error, protobuf};
use arsox_sdk::proto::artifact::v1::WorkspaceFileWritten;
use arsox_sdk::proto::error::v1::ErrorCode;
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures_util::StreamExt as _;
use sha2::{Digest as _, Sha256};
use std::fmt::Write as _;
use std::sync::Arc;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

/// The largest file one write may carry.
///
/// Five GiB, matching the largest object a single request may upload to the
/// object stores a host application moves these files onward to. A file past
/// that has to be split for its next hop anyway, and refusing it here keeps a
/// workspace from filling on a transfer that could never be sent on whole.
pub const MAX_WRITE_BYTES: u64 = 5 * 1024 * 1024 * 1024;

/// How much of a file is read into memory at a time on its way out.
///
/// Large enough that a multi-gigabyte download is not millions of reads, small
/// enough that many concurrent downloads hold a trivial amount of memory.
const READ_CHUNK: usize = 64 * 1024;

/// The name a write is staged under beside its destination, before the uuid.
///
/// A dot file, so a listing an agent glances at does not lead with it, and a
/// fixed prefix, so an operator who finds one left by a satellite killed mid
/// transfer knows what it is.
const STAGING_PREFIX: &str = ".arsox-upload-";

/// Why a workspace path could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum FileError {
    /// The reason completes "the path ...".
    #[error("the path {0}")]
    InvalidPath(&'static str),

    #[error("nothing is at that path")]
    NotFound,

    #[error("something other than a regular file is at that path or in its way")]
    NotRegular,

    #[error("the file is larger than the {MAX_WRITE_BYTES} bytes one write may carry")]
    TooLarge,

    #[error("the file could not be accessed: {0}")]
    Io(#[from] std::io::Error),
}

impl FileError {
    /// The response a caller is given for this failure.
    fn response(&self, path: &str) -> Response {
        let (status, code) = match self {
            Self::InvalidPath(_) => (StatusCode::BAD_REQUEST, ErrorCode::WorkspacePathInvalid),
            Self::NotFound => (StatusCode::NOT_FOUND, ErrorCode::WorkspaceFileNotFound),
            Self::NotRegular => (StatusCode::CONFLICT, ErrorCode::WorkspaceFileNotRegular),
            Self::TooLarge => (
                StatusCode::PAYLOAD_TOO_LARGE,
                ErrorCode::WorkspaceFileTooLarge,
            ),
            Self::Io(error) => {
                tracing::error!(
                    event.name = "workspace.file.failed",
                    file.path = path,
                    "a workspace file operation failed: {error}",
                );
                (StatusCode::INTERNAL_SERVER_ERROR, ErrorCode::Internal)
            }
        };

        contract_error(status, code, &format!("{path:?}: {self}"))
    }
}

/// Splits a workspace-relative path into the components it names.
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

/// Streams a file out of a thread's workspace.
async fn read_file(
    State(satellite): State<Arc<Satellite>>,
    Path((thread_id, path)): Path<(String, String)>,
) -> Response {
    let directory = match thread_workspace(&satellite, &thread_id).await {
        Ok(directory) => directory,
        Err(response) => return response,
    };

    let pieces: Vec<String> = match components(&path) {
        Ok(pieces) => pieces.into_iter().map(str::to_owned).collect(),
        Err(error) => return error.response(&path),
    };

    // Every open below is a blocking system call, and a walk down a deep path
    // is several of them.
    let opened =
        tokio::task::spawn_blocking(move || confined::open_file(&directory, &pieces)).await;

    let (file, size) = match opened {
        Ok(Ok(opened)) => opened,
        Ok(Err(error)) => return error.response(&path),
        Err(panicked) => return FileError::Io(std::io::Error::other(panicked)).response(&path),
    };

    let content_type = mime_guess::from_path(&path)
        .first_or_octet_stream()
        .essence_str()
        .to_owned();

    // Bounded by the size measured on the open handle, so a file the agent
    // appends to mid-download sends exactly the length this response declared.
    // One that shrinks ends the body early, which the client sees as a failed
    // transfer rather than a short file.
    let reader = tokio::fs::File::from_std(file).take(size);
    let body = futures_util::stream::try_unfold(reader, |mut reader| async move {
        let mut buffer = vec![0; READ_CHUNK];
        let read = reader.read(&mut buffer).await?;

        if read == 0 {
            return Ok::<_, std::io::Error>(None);
        }

        buffer.truncate(read);
        Ok(Some((Bytes::from(buffer), reader)))
    });

    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CONTENT_LENGTH, size.to_string()),
        ],
        Body::from_stream(body),
    )
        .into_response()
}

/// Streams a file into a thread's workspace, replacing whatever file was there.
async fn write_file(
    State(satellite): State<Arc<Satellite>>,
    Path((thread_id, path)): Path<(String, String)>,
    request: Request,
) -> Response {
    let declared = match declared_length(request.headers()) {
        Ok(declared) => declared,
        Err((status, code, message)) => return contract_error(status, code, message),
    };

    if declared > MAX_WRITE_BYTES {
        return FileError::TooLarge.response(&path);
    }

    let directory = match thread_workspace(&satellite, &thread_id).await {
        Ok(directory) => directory,
        Err(response) => return response,
    };

    let pieces: Vec<String> = match components(&path) {
        Ok(pieces) => pieces.into_iter().map(str::to_owned).collect(),
        Err(error) => return error.response(&path),
    };

    let staged = tokio::task::spawn_blocking(move || confined::stage(&directory, &pieces)).await;

    let (staged, file) = match staged {
        Ok(Ok(staged)) => staged,
        Ok(Err(error)) => return error.response(&path),
        Err(panicked) => return FileError::Io(std::io::Error::other(panicked)).response(&path),
    };

    let received = match receive(request.into_body(), file, declared).await {
        Ok(received) => received,
        Err(response) => return response,
    };

    // Dropping `staged` on any return above removes the staged file, so only a
    // body that arrived whole reaches the rename.
    let committed = tokio::task::spawn_blocking(move || staged.commit()).await;

    let created = match committed {
        Ok(Ok(created)) => created,
        Ok(Err(error)) => return error.response(&path),
        Err(panicked) => return FileError::Io(std::io::Error::other(panicked)).response(&path),
    };

    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };

    let mut response = protobuf(&WorkspaceFileWritten {
        path,
        size_bytes: declared,
        sha256: received,
        created,
    });
    *response.status_mut() = status;

    response
}

/// Streams a request body into a staged file, returning its SHA-256 in hex.
///
/// The count is checked against `declared` as the bytes arrive, and the file is
/// synced before it is returned, because a rename over an unsynced file can
/// leave an empty destination after a crash.
async fn receive(body: Body, file: std::fs::File, declared: u64) -> Result<String, Response> {
    let mut file = tokio::fs::File::from_std(file);
    let mut hasher = Sha256::new();
    let mut written: u64 = 0;
    let mut chunks = body.into_data_stream();

    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.map_err(|error| {
            contract_error(
                StatusCode::BAD_REQUEST,
                ErrorCode::RequestBodyMalformed,
                &format!("the body failed before it finished arriving: {error}"),
            )
        })?;

        written = written.saturating_add(chunk.len() as u64);
        if written > declared {
            return Err(contract_error(
                StatusCode::BAD_REQUEST,
                ErrorCode::RequestBodyMalformed,
                "the body carried more bytes than its Content-Length declared",
            ));
        }

        hasher.update(&chunk);
        file.write_all(&chunk)
            .await
            .map_err(|error| FileError::Io(error).response("the staged file"))?;
    }

    if written != declared {
        return Err(contract_error(
            StatusCode::BAD_REQUEST,
            ErrorCode::RequestBodyMalformed,
            &format!("the body carried {written} bytes and its Content-Length declared {declared}"),
        ));
    }

    file.sync_data()
        .await
        .map_err(|error| FileError::Io(error).response("the staged file"))?;

    let digest = hasher.finalize();
    let hex = digest
        .iter()
        .fold(String::with_capacity(digest.len() * 2), |mut hex, byte| {
            // Writing into a String cannot fail.
            let _infallible = write!(hex, "{byte:02x}");
            hex
        });

    Ok(hex)
}

/// A refusal of a write's declared length: its status, code, and message.
type LengthRefusal = (StatusCode, ErrorCode, &'static str);

/// The length a write declared, which is required.
fn declared_length(headers: &HeaderMap) -> Result<u64, LengthRefusal> {
    let Some(value) = headers.get(header::CONTENT_LENGTH) else {
        return Err((
            StatusCode::LENGTH_REQUIRED,
            ErrorCode::WorkspaceFileLengthRequired,
            "a write needs a Content-Length, so its size can be checked before it is written",
        ));
    };

    value
        .to_str()
        .ok()
        .and_then(|text| text.parse::<u64>().ok())
        .ok_or((
            StatusCode::BAD_REQUEST,
            ErrorCode::RequestBodyMalformed,
            "the Content-Length is not a number of bytes",
        ))
}

/// The workspace directory of a thread that exists.
async fn thread_workspace(
    satellite: &Satellite,
    thread_id: &str,
) -> Result<std::path::PathBuf, Response> {
    if let Err(error) = satellite.store.thread(thread_id).await {
        return Err(crate::api::store_failure(&error));
    }

    crate::workspace::thread_directory(&satellite.workspace_root, thread_id).map_err(|error| {
        contract_error(
            StatusCode::BAD_REQUEST,
            ErrorCode::RequestFieldInvalid,
            &error.to_string(),
        )
    })
}

/// The file routes, to be mounted behind authentication.
pub(crate) fn routes() -> Router<Arc<Satellite>> {
    Router::new().route(
        "/v1/threads/{thread_id}/files/{*path}",
        get(read_file).put(write_file),
    )
}

/// The descriptor-relative walk, on the platforms that have one.
#[cfg(unix)]
mod confined {
    use super::{FileError, STAGING_PREFIX};
    use rustix::fd::{AsFd, OwnedFd};
    use rustix::fs::{AtFlags, FileType, Mode, OFlags};
    use rustix::io::Errno;
    use std::path::Path;

    /// Directories created on the way to a written file.
    const DIRECTORY_MODE: u32 = 0o755;

    /// A written file. The agent owns it, so it can change this as it likes.
    const FILE_MODE: u32 = 0o644;

    /// Flags every directory in a walk is opened with.
    fn directory_flags() -> OFlags {
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
    }

    /// Opens the directory `pieces` names below `root`, creating what is
    /// missing when `create` is set.
    fn walk(root: &Path, pieces: &[String], create: bool) -> Result<OwnedFd, FileError> {
        // The thread directory is the satellite's own: its parent is the
        // workspace root, which the agent cannot write. It is still opened
        // without following a link, which costs nothing.
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

    /// Opens a regular file for reading, returning it and its size.
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

    /// A write staged beside its destination, removed unless it is committed.
    #[derive(Debug)]
    pub(super) struct Staged {
        directory: OwnedFd,
        staging: String,
        leaf: String,
        created: bool,
        committed: bool,
    }

    /// Creates the staged file a write streams into.
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
        pub(super) fn commit(mut self) -> Result<bool, FileError> {
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
                // Something that is not a file took the destination while the
                // body was arriving.
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
mod confined {
    use super::FileError;
    use std::path::Path;

    fn unsupported() -> FileError {
        FileError::Io(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "workspace files are served only on Unix, where a path can be walked without \
             following links",
        ))
    }

    pub(super) fn open_file(
        _root: &Path,
        _pieces: &[String],
    ) -> Result<(std::fs::File, u64), FileError> {
        Err(unsupported())
    }

    #[derive(Debug)]
    pub(super) struct Staged;

    impl Staged {
        pub(super) fn commit(self) -> Result<bool, FileError> {
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
    use super::confined::{open_file, stage};
    use super::*;
    use std::io::{Read as _, Write as _};
    use std::os::unix::fs::symlink;

    /// A scratch workspace, removed when the test ends.
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
        components(path)
            .expect("a valid path")
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    fn read(root: &std::path::Path, path: &str) -> Result<String, FileError> {
        let (mut file, size) = open_file(root, &owned(path))?;
        let mut contents = String::new();
        file.read_to_string(&mut contents).expect("readable");
        assert_eq!(size, contents.len() as u64);
        Ok(contents)
    }

    fn write(root: &std::path::Path, path: &str, contents: &str) -> Result<bool, FileError> {
        let (staged, mut file) = stage(root, &owned(path))?;
        file.write_all(contents.as_bytes()).expect("writable");
        staged.commit()
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
}
