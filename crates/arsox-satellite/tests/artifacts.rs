// Copyright © 2026 Jalapeno Labs

//! Artifacts and the workspace listings, driven through the SDK.
//!
//! A turn's agent is the stand-in harness, told by a `[[write=PATH]]` directive
//! to leave a file where a real agent would. Everything is read back from a
//! consumer's seat: the listings, the stream, and the turn's result.

#![cfg(all(feature = "test-util", unix))]

use arsox_satellite::{ServeOptions, assemble};
use arsox_sdk::client::{Satellite as Client, ThreadHandle};
use arsox_sdk::proto::common::v1::{Duration as ProtoDuration, PageRequest};
use arsox_sdk::proto::error::v1::ErrorCode;
use arsox_sdk::proto::event::v1::thread_event::Payload;
use arsox_sdk::proto::settings::v1::{Budget, ThreadSettings};
use arsox_sdk::proto::turn::v1::{Stage, StageDisposition, TurnStatus};
use futures_util::StreamExt as _;
use sha2::{Digest as _, Sha256};

const SECRET: &str = "artifacts-test-secret";

/// The recorded transcript the stand-in replays.
const TRANSCRIPT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../arsox-harness/fixtures/claude/2.1.221/tool-call.stdout.jsonl"
);

/// Distinguishes scratch directories created in the same clock tick.
static NEXT_SCRATCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A running satellite: the URL a client is given and the root it keeps
/// workspaces under.
struct Running {
    url: String,
    workspace_root: std::path::PathBuf,
}

async fn start() -> Running {
    static CONFIGURE: std::sync::Once = std::sync::Once::new();
    CONFIGURE.call_once(|| {
        // SAFETY: runs once, before any child is spawned, and writes values that
        // never change for the lifetime of the process.
        unsafe {
            std::env::set_var("ARSOX_CLAUDE_BIN", env!("CARGO_BIN_EXE_arsox-fake-harness"));
            std::env::set_var("ARSOX_FAKE_TRANSCRIPT", TRANSCRIPT);
        }
    });

    let unique = NEXT_SCRATCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let directory =
        std::env::temp_dir().join(format!("arsox-artifacts-{unique}-{}", uuid::Uuid::now_v7()));
    tokio::fs::create_dir_all(&directory)
        .await
        .expect("should create a scratch directory");

    let assembled = assemble(ServeOptions {
        secret: Some(SECRET.to_owned()),
        allow_insecure: false,
        database_path: directory.join("arsox.db").to_string_lossy().into_owned(),
        workspace_root: directory.to_string_lossy().into_owned(),
        broker_root: directory.join("broker").to_string_lossy().into_owned(),
        max_concurrent_threads: 2,
        port: 0,
        collect_interval: std::time::Duration::from_hours(1),
    })
    .await
    .expect("should assemble");

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("should bind");
    let port = listener.local_addr().expect("an address").port();

    tokio::spawn(async move {
        drop(axum::serve(listener, assembled.router).await);
    });

    Running {
        url: format!("http://127.0.0.1:{port}"),
        workspace_root: directory,
    }
}

fn settings() -> ThreadSettings {
    ThreadSettings {
        idle_ttl: Some(ProtoDuration {
            seconds: 3600,
            nanos: 0,
        }),
        budget: Some(Budget::default()),
        ..ThreadSettings::default()
    }
}

async fn thread(running: &Running) -> ThreadHandle {
    Client::connect(&running.url, SECRET)
        .await
        .expect("should connect")
        .threads()
        .create(settings())
        .await
        .expect("should create a thread")
        .handle
}

async fn upload(thread: &ThreadHandle, path: &str, contents: &'static [u8]) {
    let body = futures_util::stream::iter([Ok::<_, std::io::Error>(
        axum::body::Bytes::from_static(contents),
    )]);

    thread
        .write_file(path, contents.len() as u64, body)
        .await
        .expect("should upload");
}

#[tokio::test]
async fn every_thread_starts_with_an_artifacts_directory_its_agent_is_told_about() {
    let running = start().await;
    let thread = thread(&running).await;

    let workspace = running.workspace_root.join(thread.id());
    assert!(workspace.join("artifacts").is_dir());

    let instructions =
        std::fs::read_to_string(workspace.join("AGENTS.md")).expect("the instructions exist");
    assert!(
        instructions.contains(&format!("{}/artifacts/", workspace.display())),
        "{instructions}"
    );
}

#[tokio::test]
async fn the_artifacts_listing_answers_what_is_there_with_every_hash() {
    let running = start().await;
    let thread = thread(&running).await;

    upload(
        &thread,
        "artifacts/renders/front.png",
        b"\x89PNG\r\n\x1a\nfront",
    )
    .await;
    upload(&thread, "artifacts/model.glb", b"glTF\x02\0\0\0").await;
    upload(&thread, "scratch/notes.txt", b"not an artifact").await;

    let artifacts = thread.artifacts().await.expect("should list");

    let paths: Vec<&str> = artifacts
        .iter()
        .map(|artifact| artifact.path.as_str())
        .collect();
    assert_eq!(paths, ["model.glb", "renders/front.png"]);

    let render = &artifacts[1];
    assert_eq!(render.name, "front.png");
    assert_eq!(render.content_type.as_deref(), Some("image/png"));
    assert_eq!(
        render.sha256,
        format!("{:x}", Sha256::digest(b"\x89PNG\r\n\x1a\nfront"))
    );

    // Paged one at a time, the same files in the same order.
    let first = thread
        .artifacts_page(PageRequest {
            limit: 1,
            cursor: String::new(),
        })
        .await
        .expect("should list a page");
    let cursor = first.page.expect("a page").next_cursor;
    assert_eq!(cursor, "model.glb");

    let second = thread
        .artifacts_page(PageRequest { limit: 1, cursor })
        .await
        .expect("should list a page");
    assert_eq!(second.artifacts[0].path, "renders/front.png");
    assert!(second.page.expect("a page").next_cursor.is_empty());
}

#[tokio::test]
async fn the_workspace_listing_pages_a_directory_and_marks_artifacts() {
    let running = start().await;
    let thread = thread(&running).await;

    upload(&thread, "artifacts/out.txt", b"out").await;
    upload(&thread, "repos/api/a.txt", b"a").await;
    upload(&thread, "repos/api/b/c.txt", b"c").await;

    let listing = thread
        .workspace_files("repos/api/", PageRequest::default())
        .await
        .expect("should list");
    let paths: Vec<&str> = listing
        .files
        .iter()
        .map(|file| file.path.as_str())
        .collect();
    assert_eq!(paths, ["repos/api/a.txt", "repos/api/b/c.txt"]);
    assert!(listing.files.iter().all(|file| !file.is_artifact));

    let everything = thread
        .workspace_files("", PageRequest::default())
        .await
        .expect("should list");
    let artifact = everything
        .files
        .iter()
        .find(|file| file.path == "artifacts/out.txt")
        .expect("the artifact is listed");
    assert!(artifact.is_artifact);
    assert!(
        everything.files.iter().any(|file| file.path == "AGENTS.md"),
        "the whole workspace is listed"
    );

    let refused = thread
        .workspace_files("repos/../..", PageRequest::default())
        .await
        .expect_err("a prefix that climbs out is refused");
    assert_eq!(refused.code(), Some(ErrorCode::WorkspacePathInvalid));
}

#[tokio::test]
async fn a_turn_announces_the_artifacts_its_agent_left_and_then_only_what_changed() {
    let running = start().await;
    let thread = thread(&running).await;
    let mut events = thread.events().await.expect("should subscribe");

    let first = thread
        .start_turn("[[write=artifacts/render.png]] [[write=artifacts/scene/model.glb]] [[write=scratch.txt]]")
        .await
        .expect("should queue")
        .result()
        .await
        .expect("should finish");
    assert_eq!(first.status(), TurnStatus::Completed);

    let announced: Vec<&str> = first
        .artifacts
        .iter()
        .map(|artifact| artifact.path.as_str())
        .collect();
    assert_eq!(announced, ["render.png", "scene/model.glb"]);

    let stage = first
        .stages
        .iter()
        .find(|stage| stage.stage() == Stage::Artifacts)
        .expect("the artifact stage is reported");
    assert_eq!(stage.disposition(), StageDisposition::Ran);

    // The same files, one event each, before the turn completed.
    let mut streamed = Vec::new();
    while let Some(event) = events.next().await {
        match event.expect("a frame").payload {
            Some(Payload::ArtifactCreated(created)) => {
                streamed.push(created.artifact.expect("an artifact").path);
            }
            Some(Payload::TurnCompleted(_)) => break,
            _other => {}
        }
    }
    assert_eq!(streamed, ["render.png", "scene/model.glb"]);

    // Nothing changed, so a second turn announces nothing; a changed file is
    // announced again.
    let unchanged = thread
        .start_turn("[[write=artifacts/render.png]]")
        .await
        .expect("should queue")
        .result()
        .await
        .expect("should finish");
    assert!(unchanged.artifacts.is_empty(), "{:?}", unchanged.artifacts);

    let changed = thread
        .start_turn("[[write=artifacts/render.png=a second render]]")
        .await
        .expect("should queue")
        .result()
        .await
        .expect("should finish");
    assert_eq!(changed.artifacts.len(), 1);
    assert_eq!(
        changed.artifacts[0].sha256,
        format!("{:x}", Sha256::digest(b"a second render"))
    );
}
