// Copyright © 2026 Jalapeno Labs

//! Files an agent deliberately produced, announced as each turn ends.
//!
//! Every thread's workspace has an artifacts/ directory, created with the thread
//! and owned by the agent, and the header of its `AGENTS.md` says what belongs
//! there. The agent decides what is an artifact by where it puts a file; the
//! satellite decides nothing about the contents.
//!
//! # The scan announces what changed
//!
//! When a turn's work is over and its hooks have run, [`scan`] walks the
//! directory and announces every regular file that is new, or whose SHA-256
//! differs from what the previous scan recorded, as `artifact.created`. It then
//! records what it found, replacing the thread's previous record whole, so a
//! file that disappears and comes back is announced again, and a restart
//! announces nothing it already announced. The record lives in the database for
//! that reason; see [`crate::store::RecordedArtifact`].
//!
//! # A hash is reused only when the kernel vouches for it
//!
//! Hashing every artifact on every turn would read the whole directory each
//! time. A file whose inode, size, and change time all match its record still
//! has the contents that were hashed: the kernel moves the change time on every
//! write and nothing an agent can do moves it back, which is why the
//! modification time, which `touch` sets, is not what is compared.
//!
//! # The listing reads, and never records
//!
//! `GET /v1/threads/{id}/artifacts` answers what the directory holds now,
//! announced or not, with every hash. It reuses the record the same way the scan
//! does and hashes the rest on the spot, but it never writes the record: reading
//! what a thread holds must not change what its next scan announces.
//!
//! Every path goes through [`crate::workspace::confined`], so a link the agent
//! left in artifacts/ is neither followed nor listed.

use crate::api::Protobuf;
use crate::store::{RecordedArtifact, Store, StoreError};
use crate::workspace::confined::{self, Entry, FileError, Page};
use crate::workspace::files::{listing_page, thread_workspace};
use crate::{Satellite, protobuf};
use arsox_sdk::proto::artifact::v1::{Artifact, ListArtifactsRequest, ListArtifactsResponse};
use arsox_sdk::proto::common::v1::PageResponse;
use axum::Router;
use axum::extract::{Path, State};
use axum::response::Response;
use axum::routing::get;
use sha2::{Digest as _, Sha256};
use std::collections::HashMap;
use std::io::Read as _;
use std::sync::Arc;

/// The directory, relative to a thread's workspace, that holds its artifacts.
pub const DIRECTORY: &str = "artifacts";

/// How much of a file is read into memory at a time while it is hashed.
const HASH_CHUNK: usize = 64 * 1024;

/// Why a scan could not finish.
#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    #[error("the artifacts directory could not be read: {0}")]
    Walk(#[from] FileError),

    #[error("what the scan found could not be recorded: {0}")]
    Record(#[from] StoreError),

    #[error("the scan stopped unexpectedly: {0}")]
    Panicked(String),
}

/// A file under artifacts/ as it stands now.
#[derive(Debug, Clone)]
struct Surveyed {
    record: RecordedArtifact,
    modified_nanos: i64,
}

impl Surveyed {
    /// The file as the contract describes it.
    fn artifact(&self) -> Artifact {
        let name = self
            .record
            .path
            .rsplit('/')
            .next()
            .unwrap_or(&self.record.path)
            .to_owned();

        Artifact {
            name,
            path: self.record.path.clone(),
            size_bytes: self.record.size_bytes,
            content_type: self.record.content_type.clone(),
            sha256: self.record.sha256.clone(),
            created_at: Some(crate::store::from_nanos(self.modified_nanos)),
            // The satellite runs one agent per turn, and a file carries no
            // author to attribute it to.
            member_id: None,
        }
    }
}

/// Walks artifacts/ and announces what changed since the last scan.
///
/// Returns the artifacts that are new or whose contents changed, in path order,
/// which is what the caller puts on the stream and in the turn's result. The
/// thread's record is replaced with everything found, announced or not.
///
/// # Errors
///
/// Returns [`ScanError`] when the directory cannot be walked, a file in it
/// cannot be read, or the record cannot be written. Nothing is announced then:
/// the record is unchanged, so the next scan announces the same files.
pub async fn scan(
    store: &Store,
    workspace_root: &std::path::Path,
    thread_id: &str,
) -> Result<Vec<Artifact>, ScanError> {
    let directory = crate::workspace::thread_directory(workspace_root, thread_id)
        .map_err(|error| ScanError::Walk(FileError::Io(std::io::Error::other(error))))?;

    let recorded: HashMap<String, RecordedArtifact> = store
        .recorded_artifacts(thread_id)
        .await?
        .into_iter()
        .map(|record| (record.path.clone(), record))
        .collect();

    // What the previous scan recorded, by path, to decide what changed once the
    // survey has consumed the map as its hash cache.
    let previous: HashMap<String, String> = recorded
        .iter()
        .map(|(path, record)| (path.clone(), record.sha256.clone()))
        .collect();

    let (found, _more) =
        tokio::task::spawn_blocking(move || survey(&directory, &recorded, &Page::default()))
            .await
            .map_err(|panicked| ScanError::Panicked(panicked.to_string()))??;

    store
        .replace_artifacts(
            thread_id,
            &found
                .iter()
                .map(|surveyed| surveyed.record.clone())
                .collect::<Vec<_>>(),
        )
        .await?;

    Ok(found
        .iter()
        .filter(|surveyed| {
            previous
                .get(&surveyed.record.path)
                .is_none_or(|sha256| *sha256 != surveyed.record.sha256)
        })
        .map(Surveyed::artifact)
        .collect())
}

/// Reads what artifacts/ holds, hashing only what `recorded` cannot vouch for.
///
/// Blocking: a walk and a hash are system calls and reads, and the caller hands
/// this to the blocking pool.
fn survey(
    thread_directory: &std::path::Path,
    recorded: &HashMap<String, RecordedArtifact>,
    page: &Page,
) -> Result<(Vec<Surveyed>, bool), FileError> {
    let listing = confined::list(thread_directory, &[DIRECTORY.to_owned()], page)?;
    let mut surveyed = Vec::with_capacity(listing.entries.len());

    for entry in listing.entries {
        let path = entry.path[1..].join("/");

        let record = match recorded.get(&path) {
            Some(record) if vouches_for(record, &entry) => record.clone(),
            _unknown_or_changed => hashed(thread_directory, &entry, path)?,
        };

        surveyed.push(Surveyed {
            record,
            modified_nanos: entry.modified_nanos,
        });
    }

    Ok((surveyed, listing.more))
}

/// Whether a record's hash still describes the file a listing found.
fn vouches_for(record: &RecordedArtifact, entry: &Entry) -> bool {
    record.inode == entry.inode
        && record.size_bytes == entry.size_bytes
        && record.changed_nanos == entry.changed_nanos
}

/// Hashes one file, and labels it from its first bytes and its name.
///
/// Exactly the size the listing measured is read, so the hash describes the
/// bytes `size_bytes` reports. A file written to after the listing stat it has a
/// later change time than the one recorded here, which is what makes the next
/// look at it hash it again rather than trust this.
fn hashed(
    thread_directory: &std::path::Path,
    entry: &Entry,
    path: String,
) -> Result<RecordedArtifact, FileError> {
    let (file, _size) = confined::open_file(thread_directory, &entry.path)?;
    let mut reader = file.take(entry.size_bytes);
    let mut hasher = Sha256::new();
    let mut head = Vec::with_capacity(crate::media::SNIFF_BYTES);
    let mut buffer = vec![0; HASH_CHUNK];

    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }

        let chunk = &buffer[..read];
        let wanted = crate::media::SNIFF_BYTES.saturating_sub(head.len());
        head.extend_from_slice(&chunk[..wanted.min(read)]);
        hasher.update(chunk);
    }

    let name = entry.path.last().map_or("", String::as_str);

    Ok(RecordedArtifact {
        content_type: crate::media::content_type(&head, name),
        sha256: format!("{:x}", hasher.finalize()),
        path,
        size_bytes: entry.size_bytes,
        inode: entry.inode,
        changed_nanos: entry.changed_nanos,
    })
}

/// Lists what a thread's artifacts/ directory holds now, one page at a time.
async fn list_artifacts(
    State(satellite): State<Arc<Satellite>>,
    Path(thread_id): Path<String>,
    Protobuf(request): Protobuf<ListArtifactsRequest>,
) -> Response {
    let directory = match thread_workspace(&satellite, &thread_id).await {
        Ok(directory) => directory,
        Err(response) => return response,
    };

    // A cursor is an artifact's own path, relative to artifacts/, and the walk
    // compares from the workspace root.
    let page = match listing_page(request.page, &[DIRECTORY.to_owned()]) {
        Ok(page) => page,
        Err(response) => return *response,
    };

    let recorded: HashMap<String, RecordedArtifact> =
        match satellite.store.recorded_artifacts(&thread_id).await {
            Ok(recorded) => recorded
                .into_iter()
                .map(|record| (record.path.clone(), record))
                .collect(),
            Err(error) => return crate::api::store_failure(&error),
        };

    let surveyed = tokio::task::spawn_blocking(move || survey(&directory, &recorded, &page)).await;

    let (found, more) = match surveyed {
        Ok(Ok(surveyed)) => surveyed,
        Ok(Err(error)) => return error.response(DIRECTORY),
        Err(panicked) => {
            return FileError::Io(std::io::Error::other(panicked)).response(DIRECTORY);
        }
    };

    let next_cursor = if more {
        found
            .last()
            .map(|surveyed| surveyed.record.path.clone())
            .unwrap_or_default()
    } else {
        String::new()
    };

    protobuf(&ListArtifactsResponse {
        artifacts: found.iter().map(Surveyed::artifact).collect(),
        page: Some(PageResponse {
            next_cursor,
            // Counting would walk the whole directory on every page.
            total: None,
        }),
    })
}

/// The artifact routes, to be mounted behind authentication.
pub(crate) fn routes() -> Router<Arc<Satellite>> {
    Router::new().route("/v1/threads/{thread_id}/artifacts", get(list_artifacts))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use arsox_sdk::proto::settings::v1::ThreadSettings;

    /// A satellite's workspace and database, and one thread in both.
    struct Fixture {
        store: Store,
        root: std::path::PathBuf,
        thread_id: String,
    }

    impl Fixture {
        async fn new() -> Self {
            let store = Store::open_in_memory().await.expect("should open");
            let thread_id = store
                .create_thread(crate::store::NewThread {
                    settings: ThreadSettings::default(),
                    metadata: std::collections::BTreeMap::new(),
                    idempotency_key: None,
                })
                .await
                .expect("should create a thread")
                .thread
                .thread_id;

            let root =
                std::env::temp_dir().join(format!("arsox-artifacts-{}", uuid::Uuid::now_v7()));
            std::fs::create_dir_all(root.join(&thread_id).join(DIRECTORY)).expect("created");

            Self {
                store,
                root,
                thread_id,
            }
        }

        fn write(&self, path: &str, contents: &[u8]) {
            let full = self.root.join(&self.thread_id).join(DIRECTORY).join(path);
            std::fs::create_dir_all(full.parent().expect("a parent")).expect("created");
            std::fs::write(full, contents).expect("written");
        }

        async fn scan(&self) -> Vec<String> {
            scan(&self.store, &self.root, &self.thread_id)
                .await
                .expect("should scan")
                .into_iter()
                .map(|artifact| artifact.path)
                .collect()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            drop(std::fs::remove_dir_all(&self.root));
        }
    }

    #[tokio::test]
    async fn a_new_file_is_announced_once_and_a_changed_one_again() {
        let fixture = Fixture::new().await;
        fixture.write("renders/front.png", b"\x89PNG\r\n\x1a\nfront");
        fixture.write("model.glb", b"glTF\x02\0\0\0");

        let first = scan(&fixture.store, &fixture.root, &fixture.thread_id)
            .await
            .expect("should scan");
        assert_eq!(
            first
                .iter()
                .map(|artifact| artifact.path.as_str())
                .collect::<Vec<_>>(),
            ["model.glb", "renders/front.png"]
        );

        let model = &first[0];
        assert_eq!(model.name, "model.glb");
        assert_eq!(model.content_type.as_deref(), Some("model/gltf-binary"));
        assert_eq!(model.size_bytes, 8);
        assert_eq!(
            model.sha256,
            format!("{:x}", Sha256::digest(b"glTF\x02\0\0\0")),
            "the hash is of the file's bytes"
        );

        assert!(fixture.scan().await.is_empty(), "nothing changed");

        fixture.write("model.glb", b"glTF\x02\0\0\0changed");
        assert_eq!(fixture.scan().await, ["model.glb"]);
    }

    #[tokio::test]
    async fn rewriting_the_same_bytes_announces_nothing() {
        let fixture = Fixture::new().await;
        fixture.write("notes.txt", b"same");
        assert_eq!(fixture.scan().await, ["notes.txt"]);

        // A new inode and a new change time, and the same contents: the record
        // no longer vouches for the hash, so it is taken again and matches.
        std::fs::remove_file(
            fixture
                .root
                .join(&fixture.thread_id)
                .join(DIRECTORY)
                .join("notes.txt"),
        )
        .expect("removed");
        fixture.write("notes.txt", b"same");

        assert!(fixture.scan().await.is_empty());
    }

    #[tokio::test]
    async fn a_file_that_went_and_came_back_is_announced_again() {
        let fixture = Fixture::new().await;
        let path = fixture
            .root
            .join(&fixture.thread_id)
            .join(DIRECTORY)
            .join("report.pdf");

        fixture.write("report.pdf", b"%PDF-1.7");
        assert_eq!(fixture.scan().await, ["report.pdf"]);

        std::fs::remove_file(&path).expect("removed");
        assert!(fixture.scan().await.is_empty());

        fixture.write("report.pdf", b"%PDF-1.7");
        assert_eq!(fixture.scan().await, ["report.pdf"]);
    }

    #[tokio::test]
    async fn the_record_survives_a_new_store_handle_as_it_would_a_restart() {
        let fixture = Fixture::new().await;
        fixture.write("a.txt", b"a");
        assert_eq!(fixture.scan().await, ["a.txt"]);

        // What decides is the database, not anything this process holds.
        let recorded = fixture
            .store
            .recorded_artifacts(&fixture.thread_id)
            .await
            .expect("readable");
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].path, "a.txt");
    }

    #[tokio::test]
    async fn a_link_in_artifacts_is_neither_followed_nor_announced() {
        let fixture = Fixture::new().await;
        let outside = std::env::temp_dir().join(format!("arsox-outside-{}", uuid::Uuid::now_v7()));
        std::fs::write(&outside, "a secret").expect("written");

        std::os::unix::fs::symlink(
            &outside,
            fixture
                .root
                .join(&fixture.thread_id)
                .join(DIRECTORY)
                .join("leak.txt"),
        )
        .expect("linked");

        assert!(fixture.scan().await.is_empty());
        drop(std::fs::remove_file(outside));
    }

    #[tokio::test]
    async fn a_thread_without_the_directory_has_nothing_to_announce() {
        let fixture = Fixture::new().await;
        std::fs::remove_dir(fixture.root.join(&fixture.thread_id).join(DIRECTORY))
            .expect("removed");

        assert!(fixture.scan().await.is_empty());
    }
}
