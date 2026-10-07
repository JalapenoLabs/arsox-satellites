// Copyright © 2026 Jalapeno Labs

//! Carrying a thread's harness session out of one thread and into another.
//!
//! A satellite owns no data, so a conversation outlives its thread only if the
//! host keeps it. [`ThreadHandle::export_session`](super::ThreadHandle::export_session)
//! streams the session out as one tar archive, and
//! [`ThreadHandle::import_session`](super::ThreadHandle::import_session) hands the
//! same bytes to a fresh thread, on any satellite, whose first turn then
//! resumes the conversation.
//!
//! The archive describes itself, so a host stores one blob. The harness and the
//! session id also arrive beside it on [`SessionExport`], read from the
//! response's headers, because a host deciding where the archive can go next
//! should not have to open it to find out.

use super::ByteStream;
use crate::proto::harness::v1::Harness;
use std::fmt::{Debug, Formatter};

/// The response header naming the harness that wrote an exported session.
pub(super) const HARNESS_HEADER: &str = "arsox-harness";

/// The response header carrying an exported session's id.
pub(super) const SESSION_ID_HEADER: &str = "arsox-harness-session-id";

/// A thread's harness session on its way out of the satellite.
///
/// The archive is only ever imported into a thread running the same harness:
/// a Claude transcript cannot be resumed by Codex, or the other way round.
pub struct SessionExport {
    /// The harness that wrote the session. Never `Unspecified`.
    pub harness: Harness,

    /// The harness's own id for the session, which the importing thread's
    /// first turn resumes.
    pub harness_session_id: String,

    content_length: u64,
    body: ByteStream,
}

impl SessionExport {
    pub(super) fn new(
        harness: Harness,
        harness_session_id: String,
        content_length: u64,
        body: ByteStream,
    ) -> Self {
        Self {
            harness,
            harness_session_id,
            content_length,
            body,
        }
    }

    /// The archive's size in bytes, known before the first byte arrives.
    #[must_use]
    pub fn content_length(&self) -> u64 {
        self.content_length
    }

    /// The archive's bytes, in order.
    ///
    /// Yields an error when the transfer fails part way. A stream that ends
    /// without one delivered exactly [`Self::content_length`] bytes.
    #[must_use]
    pub fn into_body(self) -> ByteStream {
        self.body
    }
}

/// Written by hand because the body is a stream, which has no rendering.
impl Debug for SessionExport {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionExport")
            .field("harness", &self.harness)
            .field("harness_session_id", &self.harness_session_id)
            .field("content_length", &self.content_length)
            .finish_non_exhaustive()
    }
}
