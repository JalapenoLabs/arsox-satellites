// Copyright © 2026 Jalapeno Labs

//! Moving files into and out of a thread's workspace.
//!
//! Both directions are opened by the caller, so a host application the
//! satellite cannot reach can still pull what an agent produced and push in
//! what an agent needs. The bytes stream both ways: nothing here holds a whole
//! file in memory, which is what lets a multi-gigabyte artifact pass through a
//! process with a modest heap.
//!
//! Paths are relative to the thread's workspace root, `/`-separated, and the
//! satellite is the authority on them. It refuses an absolute path, a `.` or
//! `..` component, and any path that crosses a symbolic link, because the
//! workspace belongs to the agent and a link there points wherever the agent
//! chose. See [`ThreadHandle::read_file`](super::ThreadHandle::read_file) and
//! [`ThreadHandle::write_file`](super::ThreadHandle::write_file).

use super::{Error, Result};
use crate::proto::error::v1::{Error as ContractError, ErrorCode};
use bytes::Bytes;
use futures_util::Stream;
use std::fmt::{Debug, Formatter};
use std::pin::Pin;

/// The bytes of a file, as they arrive.
///
/// Boxed and pinned for the reason [`EventStream`](super::EventStream) is: a
/// caller polls it directly rather than pinning it first.
pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes>> + Send>>;

/// A file on its way out of a thread's workspace.
///
/// The length is known before the first byte, so the bytes can be streamed
/// straight into a destination that needs it up front, such as an object store
/// that refuses a chunked upload.
pub struct FileDownload {
    content_length: u64,
    content_type: Option<String>,
    body: ByteStream,
}

impl FileDownload {
    pub(super) fn new(content_length: u64, content_type: Option<String>, body: ByteStream) -> Self {
        Self {
            content_length,
            content_type,
            body,
        }
    }

    /// The file's size in bytes, as the satellite measured it when it opened it.
    #[must_use]
    pub fn content_length(&self) -> u64 {
        self.content_length
    }

    /// The satellite's best guess at the file's media type, when it made one.
    #[must_use]
    pub fn content_type(&self) -> Option<&str> {
        self.content_type.as_deref()
    }

    /// The file's bytes, in order.
    ///
    /// Yields an error when the transfer fails part way. A stream that ends
    /// without one delivered exactly [`Self::content_length`] bytes.
    #[must_use]
    pub fn into_body(self) -> ByteStream {
        self.body
    }
}

/// Written by hand because the body is a stream, which has no rendering.
impl Debug for FileDownload {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileDownload")
            .field("content_length", &self.content_length)
            .field("content_type", &self.content_type)
            .finish_non_exhaustive()
    }
}

/// The address of one workspace path on a satellite.
///
/// Each `/`-separated piece becomes one URL segment, percent-encoded, so a name
/// holding a space, a `?`, or a `#` reaches the satellite as the name it is.
///
/// # Errors
///
/// Returns `WORKSPACE_PATH_INVALID` for a path the satellite would refuse
/// anyway: empty, absolute, or holding an empty, `.`, or `..` piece. Refused
/// here rather than sent, because a URL cannot carry those faithfully: the URL
/// standard resolves a dot segment, even a percent-encoded one, before the
/// request leaves, so `a/../secret` would quietly arrive as `secret`.
///
/// Also returns an error when the satellite's base URL does not parse.
pub(super) fn file_url(base: &str, thread_id: &str, path: &str) -> Result<reqwest::Url> {
    let refused = path
        .split('/')
        .any(|piece| matches!(piece, "" | "." | ".."));
    if refused {
        return Err(Error::contract(ContractError {
            code: ErrorCode::WorkspacePathInvalid.into(),
            message: format!(
                "{path:?} is not a relative workspace path: it is empty, absolute, or has an \
                 empty, '.', or '..' component"
            ),
            retryable: false,
            details: None,
            trace_id: None,
        }));
    }

    let mut url = reqwest::Url::parse(base)
        .map_err(|error| Error::transport(format!("the satellite URL does not parse: {error}")))?;

    url.path_segments_mut()
        .map_err(|_cannot_be_a_base| Error::transport("the satellite URL cannot carry a path"))?
        .pop_if_empty()
        .extend(["v1", "threads", thread_id, "files"])
        .extend(path.split('/'));

    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    const THREAD: &str = "019fd32f-a25f-7611-a4fe-c93cc2a6d782";

    #[test]
    fn a_path_becomes_one_segment_per_piece() {
        let url =
            file_url("http://satellite:8080", THREAD, "repos/api/out put#1.txt").expect("builds");

        assert_eq!(
            url.as_str(),
            format!("http://satellite:8080/v1/threads/{THREAD}/files/repos/api/out%20put%231.txt")
        );
    }

    #[test]
    fn a_path_a_url_would_resolve_is_refused_rather_than_sent() {
        // The URL crate drops a `..` segment outright, so `a/../secret` would
        // otherwise address `secret` and the satellite would never see what was
        // asked for.
        for path in ["a/../secret", "..", "./a", "", "/etc/passwd", "a//b", "a/"] {
            let error =
                file_url("http://satellite:8080/", THREAD, path).expect_err("should be refused");
            assert_eq!(
                error.code(),
                Some(ErrorCode::WorkspacePathInvalid),
                "{path:?}"
            );
        }

        // Three dots is an ordinary name.
        file_url("http://satellite:8080/", THREAD, "a/.../b").expect("an ordinary name");
    }
}
