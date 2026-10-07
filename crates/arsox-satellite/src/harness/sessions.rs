// Copyright © 2026 Jalapeno Labs

//! Carrying a harness session out of one thread and into another.
//!
//! A satellite owns no data: the idle TTL collects a thread, and a replaced
//! container loses every transcript the CLIs kept under the agent's home. A host
//! that wants a conversation to outlive either exports the session after a turn
//! and imports it into a fresh thread later, on any satellite, whose first turn
//! then resumes it through the ordinary resume path.
//!
//! `GET /v1/threads/{id}/session` answers a tar archive whose first entry,
//! `arsox-session.binpb`, is a `HarnessSession`, followed by the session's files
//! under `files/`, relative to where the harness keeps its sessions. The harness
//! and the session id also travel as the `Arsox-Harness` and
//! `Arsox-Harness-Session-Id` headers. `PUT` of the same archive installs the
//! files where the thread's harness will look for them and records the session
//! id, so the thread's first turn resumes it.
//!
//! # Where each harness keeps a session
//!
//! Measured against Claude 2.1.290 and Codex 0.160.0, and the reason the
//! satellite sets both CLIs' state directories on every launch: it has to find
//! the files again, so it decides where they are rather than guessing.
//!
//! | | Claude | Codex |
//! |---|---|---|
//! | state directory | `CLAUDE_CONFIG_DIR`, set to `<agent home>/.claude` | `CODEX_HOME`, set to `<agent home>/.codex` |
//! | where its sessions live | `projects/<cwd>/` | `sessions/` |
//! | one session | `<id>.jsonl`, and a `<id>/` directory when it has one | `YYYY/MM/DD/rollout-<time>-<id>.jsonl` |
//! | what resume looks for | the file in the **current** cwd's directory | the rollout, anywhere under `sessions/` |
//!
//! `<cwd>` is the absolute working directory with every character outside
//! `A-Z a-z 0-9` written as `-`. A thread's working directory differs per
//! thread, so an import places a Claude session under the new thread's
//! directory rather than the one it was exported from. Claude cuts a name past
//! 200 characters and adds a hash this does not reproduce, so a workspace path
//! that long is refused; `/workspace/<thread id>` is 47.
//!
//! # The agent's home is the agent's
//!
//! Every file is reached through [`crate::workspace::confined`] from the agent's
//! home, never through a link, because the agent can rewrite anything under it
//! and the satellite reading or writing there runs as root.

use crate::harness::spawn::AgentVar;
use crate::workspace::confined::{self, FileError};
use crate::{Satellite, contract_error, protobuf};
use arsox_sdk::proto::common::v1::Timestamp;
use arsox_sdk::proto::error::v1::ErrorCode;
use arsox_sdk::proto::harness::v1::{Harness, HarnessSession, ImportHarnessSessionResponse};
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path as UrlPath, Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures_util::StreamExt as _;
use prost::Message as _;
use std::io::{Read as _, Seek as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

/// The archive's first entry, holding the `HarnessSession` that describes it.
pub const SESSION_ENTRY: &str = "arsox-session.binpb";

/// The directory every session file sits under inside the archive.
const FILES_DIRECTORY: &str = "files/";

/// The archive layout this satellite writes and reads.
pub const FORMAT_VERSION: u32 = 1;

/// The largest archive an import accepts: 2 GiB.
///
/// A Claude transcript keeps every image it was shown inline as base64, so a
/// long conversation over drawings runs to hundreds of megabytes. The archive is
/// received into a file rather than memory, so the cap bounds disk, and two
/// gigabytes is far past any conversation a context window could still hold.
pub const MAX_ARCHIVE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// The largest the `HarnessSession` entry may be. It holds four short fields.
const MAX_SESSION_ENTRY_BYTES: u64 = 64 * 1024;

/// The longest name Claude gives a project directory without cutting it.
const CLAUDE_PROJECT_NAME_LIMIT: usize = 200;

/// The response header naming the harness that wrote an exported session.
pub const HARNESS_HEADER: &str = "arsox-harness";

/// The response header carrying an exported session's id.
pub const SESSION_ID_HEADER: &str = "arsox-harness-session-id";

/// How much of a file is read into memory at a time on its way out.
const READ_CHUNK: usize = 64 * 1024;

/// The home directory of the account agents run as.
///
/// Both CLIs keep their state under it, and the satellite names their state
/// directories explicitly on every launch so it knows where a session is. In
/// the image it is the `arsox` account's home; a satellite that cannot hand its
/// children down uses its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentHome(PathBuf);

impl AgentHome {
    /// The agent's home: `configured` when given, else the agent account's,
    /// else this process's own.
    ///
    /// `None` only when there is no home at all to be found, which leaves both
    /// CLIs their own defaults and refuses session export and import.
    #[must_use]
    pub fn resolve(configured: Option<PathBuf>) -> Option<Self> {
        let resolved = configured
            .or_else(|| {
                crate::privilege::descent()
                    .account()
                    .map(|account| account.home.clone())
            })
            .or_else(std::env::home_dir)
            .map(Self);

        if resolved.is_none() {
            tracing::warn!(
                event.name = "satellite.boot.agent_home_unknown",
                "no home directory was found for the agent account, so the harnesses keep their \
                 state where they default to and harness sessions cannot be exported or imported",
            );
        }

        resolved
    }

    /// A home at exactly this path.
    #[must_use]
    pub const fn at(path: PathBuf) -> Self {
        Self(path)
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.0
    }

    /// The variables that put both CLIs' state where the satellite will look.
    ///
    /// Set in the satellite's own layer, after the thread's declared variables,
    /// so a declared `CLAUDE_CONFIG_DIR` cannot move a session out from under
    /// its export.
    #[must_use]
    pub fn environment(&self) -> [AgentVar; 2] {
        [
            AgentVar {
                key: "CLAUDE_CONFIG_DIR".to_owned(),
                value: self.0.join(".claude").display().to_string(),
                secret: false,
            },
            AgentVar {
                key: "CODEX_HOME".to_owned(),
                value: self.0.join(".codex").display().to_string(),
                secret: false,
            },
        ]
    }
}

/// Why a session could not be exported or imported.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// The reason completes "there is no session to export: ...".
    #[error("there is no session to export: {0}")]
    NotFound(String),

    #[error(
        "the thread already has a harness session or a turn; a session is imported into a fresh \
         thread, before its first turn"
    )]
    AlreadyStarted,

    #[error("the archive holds a {archive} session and this thread runs {thread}")]
    Mismatch {
        archive: &'static str,
        thread: &'static str,
    },

    #[error("the archive is larger than the {MAX_ARCHIVE_BYTES} bytes one import may carry")]
    TooLarge,

    /// The reason completes "the archive ...".
    #[error("the archive {0}")]
    Malformed(String),

    /// The satellite cannot place a session for this thread at all.
    #[error("{0}")]
    Unplaceable(String),

    #[error("a session file could not be reached: {0}")]
    File(#[from] FileError),

    #[error("the archive could not be read or written: {0}")]
    Io(#[from] std::io::Error),
}

impl SessionError {
    /// The response a caller is given for this failure.
    fn response(&self) -> Response {
        let (status, code) = match self {
            Self::NotFound(_) => (StatusCode::NOT_FOUND, ErrorCode::HarnessSessionNotFound),
            Self::AlreadyStarted => (
                StatusCode::CONFLICT,
                ErrorCode::HarnessSessionAlreadyStarted,
            ),
            Self::Mismatch { .. } => (StatusCode::CONFLICT, ErrorCode::HarnessSessionMismatch),
            Self::TooLarge => (
                StatusCode::PAYLOAD_TOO_LARGE,
                ErrorCode::HarnessSessionTooLarge,
            ),
            Self::Malformed(_) => (StatusCode::BAD_REQUEST, ErrorCode::RequestBodyMalformed),
            Self::Unplaceable(_) | Self::File(_) | Self::Io(_) => {
                tracing::error!(
                    event.name = "harness.session.failed",
                    "a harness session could not be moved: {self}",
                );
                (StatusCode::INTERNAL_SERVER_ERROR, ErrorCode::Internal)
            }
        };

        contract_error(status, code, &self.to_string())
    }
}

/// The harness a thread runs, with the documented default for none.
fn harness_of(declared: i32) -> Harness {
    match Harness::try_from(declared) {
        Ok(Harness::Codex) => Harness::Codex,
        Ok(Harness::Claude | Harness::Unspecified) | Err(_) => Harness::Claude,
    }
}

/// The name Claude gives the project directory of a working directory.
#[must_use]
pub fn claude_project_name(working_dir: &Path) -> String {
    working_dir
        .to_string_lossy()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .collect()
}

/// Where `harness` keeps its sessions for a thread working in `working_dir`, as
/// components below the agent's home.
fn sessions_directory(harness: Harness, working_dir: &Path) -> Result<Vec<String>, SessionError> {
    if harness == Harness::Codex {
        return Ok(vec![".codex".to_owned(), "sessions".to_owned()]);
    }

    // The real path, which is what the CLI sees as its working directory.
    let real = std::fs::canonicalize(working_dir)?;
    let project = claude_project_name(&real);

    if project.len() > CLAUDE_PROJECT_NAME_LIMIT {
        return Err(SessionError::Unplaceable(format!(
            "the thread's workspace path is {} characters once encoded, and Claude names a \
             project directory past {CLAUDE_PROJECT_NAME_LIMIT} with a hash the satellite does \
             not reproduce",
            project.len()
        )));
    }

    Ok(vec![".claude".to_owned(), "projects".to_owned(), project])
}

/// Whether a file, relative to the sessions directory, belongs to `session_id`.
///
/// Only those are exported, and only those an import will install: the archive
/// carries one conversation, not whatever else the agent left beside it.
fn belongs(harness: Harness, session_id: &str, relative: &[String]) -> bool {
    match harness {
        Harness::Codex => relative.last().is_some_and(|name| {
            name.starts_with("rollout-") && name.ends_with(&format!("-{session_id}.jsonl"))
        }),
        Harness::Claude | Harness::Unspecified => match relative {
            [only] => *only == format!("{session_id}.jsonl"),
            [first, _rest @ ..] => first == session_id,
            [] => false,
        },
    }
}

/// Whether an id can name files and a command line argument safely.
///
/// Both CLIs mint UUIDs. The id becomes part of a file name and an argument, so
/// anything else in an archive is refused rather than trusted.
fn usable_session_id(session_id: &str) -> bool {
    (1..=128).contains(&session_id.len())
        && session_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// What a session export is about to send.
#[derive(Debug)]
struct Exported {
    archive: std::fs::File,
    length: u64,
}

/// Builds a thread's session archive in an anonymous file.
fn export(
    home: &AgentHome,
    harness: Harness,
    session_id: &str,
    working_dir: &Path,
) -> Result<Exported, SessionError> {
    let directory = sessions_directory(harness, working_dir)?;

    let listing = confined::list(home.path(), &directory, &confined::Page::default())?;
    let files: Vec<Vec<String>> = listing
        .entries
        .into_iter()
        .map(|entry| entry.path)
        .filter(|path| belongs(harness, session_id, &path[directory.len()..]))
        .collect();

    if files.is_empty() {
        return Err(SessionError::NotFound(format!(
            "no file of session {session_id} is where the harness keeps it"
        )));
    }

    let session = HarnessSession {
        harness: harness.into(),
        session_id: session_id.to_owned(),
        workspace: working_dir.display().to_string(),
        exported_at: Some(Timestamp::now()),
        format_version: FORMAT_VERSION,
    };

    let mut builder = tar::Builder::new(tempfile::tempfile()?);
    let encoded = session.encode_to_vec();
    builder.append_data(
        &mut entry_header(encoded.len() as u64),
        SESSION_ENTRY,
        encoded.as_slice(),
    )?;

    for path in &files {
        let (file, size) = confined::open_file(home.path(), path)?;
        let relative = path[directory.len()..].join("/");

        // Bounded by the size measured on the open handle, which is what the
        // header promised. A file the harness appends to meanwhile is cut at
        // that size; one that shrank fails the export rather than padding it.
        builder.append_data(
            &mut entry_header(size),
            format!("{FILES_DIRECTORY}{relative}"),
            file.take(size),
        )?;
    }

    let mut archive = builder.into_inner()?;
    archive.flush()?;
    let length = archive.stream_position()?;
    archive.rewind()?;

    Ok(Exported { archive, length })
}

/// A regular file entry's header.
fn entry_header(size: u64) -> tar::Header {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_size(size);
    header.set_mode(0o644);
    header.set_mtime(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or_default(),
    );
    header
}

/// Installs an archive's files where `thread_harness` will look for them.
///
/// Returns the session it carried and every file installed, relative to the
/// sessions directory. A file already at a path is replaced: a host imports the
/// newest copy it kept.
fn install(
    home: &AgentHome,
    thread_harness: Harness,
    working_dir: &Path,
    archive: std::fs::File,
) -> Result<(HarnessSession, Vec<String>), SessionError> {
    let mut archive = tar::Archive::new(archive);
    let mut entries = archive.entries()?;

    let malformed = |reason: &str| SessionError::Malformed(reason.to_owned());

    let mut first = entries
        .next()
        .ok_or_else(|| malformed("is empty"))?
        .map_err(|error| SessionError::Malformed(format!("could not be read: {error}")))?;

    if first.path_bytes().as_ref() != SESSION_ENTRY.as_bytes() {
        return Err(malformed("does not open with arsox-session.binpb"));
    }
    if first.size() > MAX_SESSION_ENTRY_BYTES {
        return Err(malformed("has an arsox-session.binpb too large to be one"));
    }

    let mut encoded = Vec::new();
    first.read_to_end(&mut encoded)?;
    let session = HarnessSession::decode(encoded.as_slice())
        .map_err(|error| SessionError::Malformed(format!("has an unreadable session: {error}")))?;

    if session.format_version != FORMAT_VERSION {
        return Err(SessionError::Malformed(format!(
            "has layout {}, and this satellite reads layout {FORMAT_VERSION}",
            session.format_version
        )));
    }

    let archived = match Harness::try_from(session.harness) {
        Ok(harness @ (Harness::Claude | Harness::Codex)) => harness,
        Ok(Harness::Unspecified) | Err(_) => return Err(malformed("names no harness")),
    };
    if archived != thread_harness {
        return Err(SessionError::Mismatch {
            archive: archived.as_str_name(),
            thread: thread_harness.as_str_name(),
        });
    }

    if !usable_session_id(&session.session_id) {
        return Err(malformed(
            "names a session id that is not 1 to 128 of A-Z a-z 0-9 - _",
        ));
    }

    let directory = sessions_directory(thread_harness, working_dir)?;
    let mut installed = Vec::new();

    for entry in entries {
        let mut entry = entry
            .map_err(|error| SessionError::Malformed(format!("could not be read: {error}")))?;

        let entry_type = entry.header().entry_type();
        if entry_type.is_dir() {
            continue;
        }
        if !entry_type.is_file() {
            return Err(malformed("holds something other than files"));
        }

        let name = String::from_utf8(entry.path_bytes().into_owned())
            .map_err(|_not_utf8| malformed("names a file that is not UTF-8"))?;
        let relative = name
            .strip_prefix(FILES_DIRECTORY)
            .ok_or_else(|| malformed("holds a file outside files/"))?;

        // The same rules a workspace path follows, so `..` and an absolute
        // path never leave the sessions directory.
        let pieces = confined::owned_components(relative)
            .map_err(|error| SessionError::Malformed(format!("names {relative:?}: {error}")))?;

        if !belongs(thread_harness, &session.session_id, &pieces) {
            return Err(SessionError::Malformed(format!(
                "holds {relative:?}, which is not a file of session {}",
                session.session_id
            )));
        }

        let target: Vec<String> = directory.iter().cloned().chain(pieces).collect();
        let (staged, mut file) = confined::stage(home.path(), &target)?;

        let size = entry.size();
        let copied = std::io::copy(&mut entry.by_ref().take(size), &mut file)?;
        if copied != size {
            return Err(malformed("ended inside a file"));
        }
        file.sync_data()?;
        staged.commit()?;

        installed.push(relative.to_owned());
    }

    if installed.is_empty() {
        return Err(malformed("carries no session files"));
    }

    Ok((session, installed))
}

/// The agent's home, or the refusal a satellite without one gives.
fn home_of(satellite: &Satellite) -> Result<AgentHome, SessionError> {
    satellite.agent_home.clone().ok_or_else(|| {
        SessionError::Unplaceable(
            "the satellite found no home directory for the agent account, so it cannot tell \
             where a harness keeps its sessions"
                .to_owned(),
        )
    })
}

/// Streams a thread's harness session out as a tar archive.
async fn export_session(
    State(satellite): State<Arc<Satellite>>,
    UrlPath(thread_id): UrlPath<String>,
) -> Response {
    let thread = match satellite.store.thread(&thread_id).await {
        Ok(thread) => thread,
        Err(error) => return crate::api::store_failure(&error),
    };

    let Some(session_id) = thread.harness_session_id.clone() else {
        return SessionError::NotFound(
            "the thread has not opened a harness session yet".to_owned(),
        )
        .response();
    };

    let harness = harness_of(
        thread
            .settings
            .as_ref()
            .map_or(0, |settings| settings.harness),
    );

    let home = match home_of(&satellite) {
        Ok(home) => home,
        Err(error) => return error.response(),
    };
    let working_dir = match crate::workspace::files::thread_workspace(&satellite, &thread_id).await
    {
        Ok(directory) => directory,
        Err(response) => return response,
    };

    let identifier = session_id.clone();
    let built =
        tokio::task::spawn_blocking(move || export(&home, harness, &identifier, &working_dir))
            .await;

    let exported = match built {
        Ok(Ok(exported)) => exported,
        Ok(Err(error)) => return error.response(),
        Err(panicked) => return SessionError::Io(std::io::Error::other(panicked)).response(),
    };

    let length = exported.length;
    let reader = tokio::fs::File::from_std(exported.archive).take(length);
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
            (header::CONTENT_TYPE, "application/x-tar".to_owned()),
            (header::CONTENT_LENGTH, length.to_string()),
            (
                header::HeaderName::from_static(HARNESS_HEADER),
                harness.as_str_name().to_owned(),
            ),
            (
                header::HeaderName::from_static(SESSION_ID_HEADER),
                session_id,
            ),
        ],
        Body::from_stream(body),
    )
        .into_response()
}

/// Installs an exported session into a fresh thread.
async fn import_session(
    State(satellite): State<Arc<Satellite>>,
    UrlPath(thread_id): UrlPath<String>,
    request: Request,
) -> Response {
    let declared = request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|text| text.parse::<u64>().ok());

    let Some(declared) = declared else {
        return contract_error(
            StatusCode::LENGTH_REQUIRED,
            ErrorCode::RequestFieldMissing,
            "an import needs a Content-Length, so its size can be checked before it is received",
        );
    };
    if declared > MAX_ARCHIVE_BYTES {
        return SessionError::TooLarge.response();
    }

    let thread = match satellite.store.thread(&thread_id).await {
        Ok(thread) => thread,
        Err(error) => return crate::api::store_failure(&error),
    };

    // Checked before a byte is received, so a refusal costs nothing. The
    // record below is made in one conditional statement that checks again.
    let started = match satellite.store.has_turns(&thread_id).await {
        Ok(started) => started || thread.harness_session_id.is_some(),
        Err(error) => return crate::api::store_failure(&error),
    };
    if started {
        return SessionError::AlreadyStarted.response();
    }

    let harness = harness_of(
        thread
            .settings
            .as_ref()
            .map_or(0, |settings| settings.harness),
    );

    let home = match home_of(&satellite) {
        Ok(home) => home,
        Err(error) => return error.response(),
    };
    let working_dir = match crate::workspace::files::thread_workspace(&satellite, &thread_id).await
    {
        Ok(directory) => directory,
        Err(response) => return response,
    };

    let archive = match receive(request.into_body(), declared).await {
        Ok(archive) => archive,
        Err(response) => return response,
    };

    let installed =
        tokio::task::spawn_blocking(move || install(&home, harness, &working_dir, archive)).await;

    let (session, files) = match installed {
        Ok(Ok(installed)) => installed,
        Ok(Err(error)) => return error.response(),
        Err(panicked) => return SessionError::Io(std::io::Error::other(panicked)).response(),
    };

    // Files first, then the record, so no turn can resume a session whose files
    // are not there yet. A turn queued in between wins, and the files it beat
    // stay where they are, inert: nothing resumes a session no thread names.
    match satellite
        .store
        .adopt_harness_session(&thread_id, &session.session_id)
        .await
    {
        Ok(true) => protobuf(&ImportHarnessSessionResponse {
            session: Some(session),
            files,
        }),
        Ok(false) => SessionError::AlreadyStarted.response(),
        Err(error) => crate::api::store_failure(&error),
    }
}

/// Receives a request body into an anonymous file, holding it to `declared`.
async fn receive(body: Body, declared: u64) -> Result<std::fs::File, Response> {
    let io_failure = |error: std::io::Error| SessionError::Io(error).response();

    let anonymous = tokio::task::spawn_blocking(tempfile::tempfile)
        .await
        .map_err(|panicked| io_failure(std::io::Error::other(panicked)))?
        .map_err(io_failure)?;
    let mut file = tokio::fs::File::from_std(anonymous);

    let mut received: u64 = 0;
    let mut chunks = body.into_data_stream();

    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.map_err(|error| {
            contract_error(
                StatusCode::BAD_REQUEST,
                ErrorCode::RequestBodyMalformed,
                &format!("the body failed before it finished arriving: {error}"),
            )
        })?;

        received = received.saturating_add(chunk.len() as u64);
        if received > declared {
            return Err(contract_error(
                StatusCode::BAD_REQUEST,
                ErrorCode::RequestBodyMalformed,
                "the body carried more bytes than its Content-Length declared",
            ));
        }

        file.write_all(&chunk).await.map_err(io_failure)?;
    }

    if received != declared {
        return Err(contract_error(
            StatusCode::BAD_REQUEST,
            ErrorCode::RequestBodyMalformed,
            &format!(
                "the body carried {received} bytes and its Content-Length declared {declared}"
            ),
        ));
    }

    let mut file = file.into_std().await;
    file.rewind().map_err(io_failure)?;

    Ok(file)
}

/// The session routes, to be mounted behind authentication.
pub(crate) fn routes() -> Router<Arc<Satellite>> {
    Router::new().route(
        "/v1/threads/{thread_id}/session",
        get(export_session).put(import_session),
    )
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// A scratch agent home and workspace, removed when the test ends.
    struct Scratch {
        root: PathBuf,
    }

    impl Scratch {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("arsox-sessions-{}", uuid::Uuid::now_v7()));
            std::fs::create_dir_all(root.join("home")).expect("should create the home");
            std::fs::create_dir_all(root.join("workspace").join("thread"))
                .expect("should create the workspace");
            Self { root }
        }

        fn home(&self) -> AgentHome {
            AgentHome::at(self.root.join("home"))
        }

        fn workspace(&self) -> PathBuf {
            self.root.join("workspace").join("thread")
        }

        fn write(&self, relative: &str, contents: &str) {
            let path = self.root.join("home").join(relative);
            std::fs::create_dir_all(path.parent().expect("a parent")).expect("should create");
            std::fs::write(path, contents).expect("should write");
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            drop(std::fs::remove_dir_all(&self.root));
        }
    }

    /// The Claude project directory for a scratch workspace, as a relative path.
    fn project(scratch: &Scratch) -> String {
        let real = std::fs::canonicalize(scratch.workspace()).expect("exists");
        format!(".claude/projects/{}", claude_project_name(&real))
    }

    fn entries(archive: std::fs::File) -> Vec<(String, Vec<u8>)> {
        let mut archive = tar::Archive::new(archive);
        archive
            .entries()
            .expect("readable")
            .map(|entry| {
                let mut entry = entry.expect("an entry");
                let name = String::from_utf8_lossy(&entry.path_bytes()).into_owned();
                let mut bytes = Vec::new();
                entry.read_to_end(&mut bytes).expect("readable");
                (name, bytes)
            })
            .collect()
    }

    #[test]
    fn claude_names_a_project_by_writing_every_other_character_as_a_dash() {
        // Measured against Claude 2.1.290: `odd.dir_name x` became
        // `odd-dir-name-x`, and the slashes became dashes too.
        assert_eq!(
            claude_project_name(Path::new("/workspace/0199b000-0000-7000-8000-000000000001")),
            "-workspace-0199b000-0000-7000-8000-000000000001"
        );
        assert_eq!(
            claude_project_name(Path::new("/tmp/odd.dir_name x")),
            "-tmp-odd-dir-name-x"
        );
    }

    #[test]
    fn a_claude_export_carries_the_session_and_nothing_beside_it() {
        let scratch = Scratch::new();
        let project = project(&scratch);
        scratch.write(&format!("{project}/abc.jsonl"), "{\"turn\":1}\n");
        scratch.write(&format!("{project}/abc/subagents/agent-1.jsonl"), "{}\n");
        scratch.write(&format!("{project}/other.jsonl"), "not ours\n");
        scratch.write(&format!("{project}/memory/notes.md"), "not ours\n");

        let exported = export(
            &scratch.home(),
            Harness::Claude,
            "abc",
            &scratch.workspace(),
        )
        .expect("the session is there");
        let length = exported.length;
        let entries = entries(exported.archive);

        let names: Vec<&str> = entries.iter().map(|(name, _bytes)| name.as_str()).collect();
        assert_eq!(
            names,
            [
                SESSION_ENTRY,
                // Listing order, bytewise: `abc/` sorts before `abc.jsonl`.
                "files/abc/subagents/agent-1.jsonl",
                "files/abc.jsonl",
            ]
        );
        assert!(length > 0);

        let session = HarnessSession::decode(entries[0].1.as_slice()).expect("decodes");
        assert_eq!(session.harness, i32::from(Harness::Claude));
        assert_eq!(session.session_id, "abc");
        assert_eq!(session.format_version, FORMAT_VERSION);
        assert_eq!(entries[2].1, b"{\"turn\":1}\n");
    }

    #[test]
    fn a_codex_export_finds_the_rollout_under_any_date() {
        let scratch = Scratch::new();
        scratch.write(
            ".codex/sessions/2026/10/06/rollout-2026-10-06T18-13-28-0199-abc.jsonl",
            "{}\n",
        );
        scratch.write(
            ".codex/sessions/2026/10/06/rollout-2026-10-06T18-13-28-xyz.jsonl",
            "{}\n",
        );

        let exported = export(
            &scratch.home(),
            Harness::Codex,
            "0199-abc",
            &scratch.workspace(),
        )
        .expect("the rollout is there");
        let names: Vec<String> = entries(exported.archive)
            .into_iter()
            .map(|(name, _bytes)| name)
            .collect();

        assert_eq!(
            names,
            [
                SESSION_ENTRY.to_owned(),
                "files/2026/10/06/rollout-2026-10-06T18-13-28-0199-abc.jsonl".to_owned()
            ]
        );
    }

    #[test]
    fn nothing_on_disk_is_not_found_rather_than_an_empty_archive() {
        let scratch = Scratch::new();
        let missing = export(
            &scratch.home(),
            Harness::Claude,
            "abc",
            &scratch.workspace(),
        );

        assert!(matches!(missing, Err(SessionError::NotFound(_))));
    }

    #[test]
    fn a_link_where_a_session_should_be_is_never_followed() {
        let scratch = Scratch::new();
        let project = project(&scratch);
        std::fs::create_dir_all(scratch.root.join("home").join(&project)).expect("created");
        std::os::unix::fs::symlink(
            "/etc/passwd",
            scratch.root.join("home").join(&project).join("abc.jsonl"),
        )
        .expect("linked");

        let exported = export(
            &scratch.home(),
            Harness::Claude,
            "abc",
            &scratch.workspace(),
        );
        assert!(matches!(exported, Err(SessionError::NotFound(_))));
    }

    /// Builds an archive the way a host would hand one back.
    fn archive(session: &HarnessSession, files: &[(&str, &str)]) -> std::fs::File {
        let mut builder = tar::Builder::new(tempfile::tempfile().expect("a file"));
        let encoded = session.encode_to_vec();
        builder
            .append_data(
                &mut entry_header(encoded.len() as u64),
                SESSION_ENTRY,
                encoded.as_slice(),
            )
            .expect("appended");
        for (name, contents) in files {
            // The name is written into the header by hand, because the writer
            // refuses to build the hostile names a test has to send.
            let mut header = entry_header(contents.len() as u64);
            let field = &mut header.as_gnu_mut().expect("a GNU header").name;
            field[..name.len()].copy_from_slice(name.as_bytes());
            header.set_cksum();
            builder
                .append(&header, contents.as_bytes())
                .expect("appended");
        }
        let mut file = builder.into_inner().expect("finished");
        file.rewind().expect("rewound");
        file
    }

    fn claude_session(session_id: &str) -> HarnessSession {
        HarnessSession {
            harness: Harness::Claude.into(),
            session_id: session_id.to_owned(),
            workspace: "/workspace/elsewhere".to_owned(),
            exported_at: None,
            format_version: FORMAT_VERSION,
        }
    }

    #[test]
    fn an_import_places_claude_files_under_the_new_threads_project() {
        let scratch = Scratch::new();

        let (session, files) = install(
            &scratch.home(),
            Harness::Claude,
            &scratch.workspace(),
            archive(
                &claude_session("abc"),
                &[("files/abc.jsonl", "{\"turn\":1}\n")],
            ),
        )
        .expect("installs");

        assert_eq!(session.session_id, "abc");
        assert_eq!(files, ["abc.jsonl"]);

        let placed = scratch
            .root
            .join("home")
            .join(project(&scratch))
            .join("abc.jsonl");
        assert_eq!(
            std::fs::read_to_string(placed).expect("placed"),
            "{\"turn\":1}\n"
        );
    }

    #[test]
    fn an_import_refuses_what_is_not_one_session_of_this_harness() {
        let scratch = Scratch::new();
        let install_one = |session: &HarnessSession, files: &[(&str, &str)]| {
            install(
                &scratch.home(),
                Harness::Claude,
                &scratch.workspace(),
                archive(session, files),
            )
        };

        let codex = HarnessSession {
            harness: Harness::Codex.into(),
            ..claude_session("abc")
        };
        assert!(matches!(
            install_one(&codex, &[("files/rollout-x-abc.jsonl", "{}")]),
            Err(SessionError::Mismatch { .. })
        ));

        let future = HarnessSession {
            format_version: 2,
            ..claude_session("abc")
        };
        for (session, files) in [
            (&future, vec![("files/abc.jsonl", "{}")]),
            (&claude_session("../abc"), vec![("files/abc.jsonl", "{}")]),
            (&claude_session("abc"), vec![("files/../../.bashrc", "{}")]),
            (&claude_session("abc"), vec![("files/settings.json", "{}")]),
            (&claude_session("abc"), vec![("elsewhere/abc.jsonl", "{}")]),
            (&claude_session("abc"), vec![]),
        ] {
            assert!(
                matches!(
                    install_one(session, &files),
                    Err(SessionError::Malformed(_))
                ),
                "{files:?}"
            );
        }
    }
}
