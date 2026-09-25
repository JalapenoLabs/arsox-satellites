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
//! `GET /v1/threads/{id}/files` lists what the workspace holds, and
//! `GET /v1/threads/{id}/artifacts` lists its artifacts/ directory with each
//! file's SHA-256, both paged in path order.
//!
//! # The workspace is the agent's, so every path is hostile
//!
//! Every path is walked by [`super::confined`], one component at a time from the
//! thread's directory, never through a symbolic link.
//!
//! | Refused | Answer |
//! |---|---|
//! | empty, absolute, an empty, `.`, or `..` component, or a NUL | `400 WORKSPACE_PATH_INVALID` |
//! | any component, or the file itself, is a symbolic link | `400 WORKSPACE_PATH_INVALID` |
//! | nothing at the path, for a read | `404 WORKSPACE_FILE_NOT_FOUND` |
//! | a directory, FIFO, socket, or device at the path or in its way | `409 WORKSPACE_FILE_NOT_REGULAR` |
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

use super::confined::{self, FileError, components};
use crate::api::Protobuf;
use crate::{Satellite, contract_error, protobuf};
use arsox_sdk::proto::artifact::v1::{
    ListWorkspaceFilesRequest, ListWorkspaceFilesResponse, WorkspaceFile, WorkspaceFileWritten,
};
use arsox_sdk::proto::common::v1::{PageRequest, PageResponse};
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

impl FileError {
    /// The response a caller is given for this failure.
    pub(crate) fn response(&self, path: &str) -> Response {
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

/// Where a listing starts, and how much of it a caller asked for.
///
/// `base` is prepended to the cursor, so a listing whose paths are relative to
/// a directory, such as the artifacts listing, can hand out cursors in its own
/// terms. A cursor is a path, and it is checked like one: a caller that could
/// send `..` there could not escape anything, since it is only ever compared,
/// but a cursor no listing produced is a bug worth saying so about.
///
/// # Errors
///
/// Answers `REQUEST_FIELD_INVALID` for a cursor that is not a relative path,
/// boxed because a response is large and the refusal is the rare path.
pub(crate) fn listing_page(
    request: Option<PageRequest>,
    base: &[String],
) -> Result<confined::Page, Box<Response>> {
    let request = request.unwrap_or_default();

    let limit = match request.limit {
        0 => crate::store::DEFAULT_PAGE,
        asked => asked.min(crate::store::MAX_PAGE),
    };

    let after = if request.cursor.is_empty() {
        None
    } else {
        let pieces = confined::owned_components(&request.cursor).map_err(|error| {
            Box::new(contract_error(
                StatusCode::BAD_REQUEST,
                ErrorCode::RequestFieldInvalid,
                &format!("page.cursor {:?}: {error}", request.cursor),
            ))
        })?;

        Some(base.iter().cloned().chain(pieces).collect())
    };

    Ok(confined::Page {
        after,
        limit: Some(usize::try_from(limit).unwrap_or(usize::MAX)),
    })
}

/// Lists the regular files in a thread's workspace, one page at a time.
async fn list_files(
    State(satellite): State<Arc<Satellite>>,
    Path(thread_id): Path<String>,
    Protobuf(request): Protobuf<ListWorkspaceFilesRequest>,
) -> Response {
    let directory = match thread_workspace(&satellite, &thread_id).await {
        Ok(directory) => directory,
        Err(response) => return response,
    };

    // One trailing separator is forgiven, because `repos/api/` is how a person
    // writes a directory. Everything else about the prefix is checked like a
    // file route's path.
    let prefix_text = request
        .path_prefix
        .strip_suffix('/')
        .unwrap_or(&request.path_prefix)
        .to_owned();
    let prefix = if prefix_text.is_empty() {
        Vec::new()
    } else {
        match confined::owned_components(&prefix_text) {
            Ok(prefix) => prefix,
            Err(error) => return error.response(&prefix_text),
        }
    };

    let page = match listing_page(request.page, &[]) {
        Ok(page) => page,
        Err(response) => return *response,
    };

    let listed =
        tokio::task::spawn_blocking(move || confined::list(&directory, &prefix, &page)).await;

    let listing = match listed {
        Ok(Ok(listing)) => listing,
        Ok(Err(error)) => return error.response(&prefix_text),
        Err(panicked) => {
            return FileError::Io(std::io::Error::other(panicked)).response(&prefix_text);
        }
    };

    let next_cursor = if listing.more {
        listing
            .entries
            .last()
            .map(confined::Entry::joined)
            .unwrap_or_default()
    } else {
        String::new()
    };

    let files = listing
        .entries
        .into_iter()
        .map(|entry| WorkspaceFile {
            // Under the directory rather than the directory itself, which a
            // listing of regular files never reports anyway.
            is_artifact: entry.path.len() > 1 && entry.path[0] == crate::artifacts::DIRECTORY,
            path: entry.joined(),
            size_bytes: entry.size_bytes,
            modified_at: Some(crate::store::from_nanos(entry.modified_nanos)),
        })
        .collect();

    protobuf(&ListWorkspaceFilesResponse {
        files,
        page: Some(PageResponse {
            next_cursor,
            // Counting every file would walk the whole tree on every page.
            // Absent says "not computed" rather than claiming a total.
            total: None,
        }),
    })
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
pub(crate) async fn thread_workspace(
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
    Router::new()
        .route("/v1/threads/{thread_id}/files", get(list_files))
        .route(
            "/v1/threads/{thread_id}/files/{*path}",
            get(read_file).put(write_file),
        )
}
