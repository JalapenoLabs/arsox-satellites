// Copyright © 2026 Jalapeno Labs

//! A harness session exported from one thread and imported into another,
//! driven through the SDK across two satellites.
//!
//! The stand-in harness keeps its session where the real CLI does and refuses
//! to resume one it cannot find there, so a turn that completes after an import
//! proves the files landed where the harness looks from the new thread's
//! workspace, not merely somewhere on disk.

#![cfg(all(feature = "test-util", unix))]

use arsox_satellite::{ServeOptions, assemble};
use arsox_sdk::client::{Satellite as Client, SessionExport, ThreadHandle};
use arsox_sdk::proto::common::v1::Duration as ProtoDuration;
use arsox_sdk::proto::error::v1::ErrorCode;
use arsox_sdk::proto::harness::v1::Harness;
use arsox_sdk::proto::settings::v1::{Budget, ThreadSettings};
use arsox_sdk::proto::turn::v1::TurnStatus;
use futures_util::StreamExt as _;
use std::time::Duration;

const SECRET: &str = "sessions-test-secret";

const TRANSCRIPT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../arsox-harness/fixtures/claude/2.1.221/tool-call.stdout.jsonl"
);
const CODEX_TRANSCRIPT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../arsox-harness/fixtures/codex/0.147.0/tool-call.stdout.jsonl"
);

/// The id the Codex recording's `thread.started` announces.
const CODEX_SESSION_ID: &str = "01a01cd2-200b-77f0-b4b8-7421557ff5ed";

/// Distinguishes scratch directories created in the same clock tick.
static NEXT_SCRATCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// One satellite, with its own workspace root and its own agent home.
struct Running {
    url: String,
    workspace_root: std::path::PathBuf,
    agent_home: std::path::PathBuf,
}

async fn start() -> Running {
    static CONFIGURE: std::sync::Once = std::sync::Once::new();
    CONFIGURE.call_once(|| {
        // SAFETY: runs once, before any child is spawned, and writes values that
        // never change for the lifetime of the process.
        unsafe {
            std::env::set_var("ARSOX_CLAUDE_BIN", env!("CARGO_BIN_EXE_arsox-fake-harness"));
            std::env::set_var("ARSOX_FAKE_TRANSCRIPT", TRANSCRIPT);
            std::env::set_var("ARSOX_CODEX_BIN", env!("CARGO_BIN_EXE_arsox-fake-harness"));
            std::env::set_var("ARSOX_FAKE_CODEX_TRANSCRIPT", CODEX_TRANSCRIPT);
        }
    });

    let unique = NEXT_SCRATCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let directory =
        std::env::temp_dir().join(format!("arsox-sessions-{unique}-{}", uuid::Uuid::now_v7()));
    let workspace_root = directory.join("workspace");
    let agent_home = directory.join("home");
    for created in [&workspace_root, &agent_home] {
        tokio::fs::create_dir_all(created)
            .await
            .expect("should create a scratch directory");
    }

    let assembled = assemble(ServeOptions {
        secret: Some(SECRET.to_owned()),
        allow_insecure: false,
        database_path: directory.join("arsox.db").to_string_lossy().into_owned(),
        workspace_root: workspace_root.to_string_lossy().into_owned(),
        broker_root: directory.join("broker").to_string_lossy().into_owned(),
        max_concurrent_threads: 2,
        port: 0,
        collect_interval: std::time::Duration::from_hours(1),
        agent_home: Some(agent_home.clone()),
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
        workspace_root,
        agent_home,
    }
}

async fn thread(running: &Running, harness: Harness) -> ThreadHandle {
    Client::connect(&running.url, SECRET)
        .await
        .expect("should connect")
        .threads()
        .create(ThreadSettings {
            idle_ttl: Some(ProtoDuration {
                seconds: 3600,
                nanos: 0,
            }),
            budget: Some(Budget::default()),
            harness: harness.into(),
            ..ThreadSettings::default()
        })
        .await
        .expect("should create a thread")
        .handle
}

async fn complete(thread: &ThreadHandle, prompt: &str) {
    let turn = thread.start_turn(prompt).await.expect("should queue");
    let result = tokio::time::timeout(Duration::from_secs(30), turn.result())
        .await
        .expect("should not time out")
        .expect("should report a result");

    assert_eq!(
        result.status,
        i32::from(TurnStatus::Completed),
        "{prompt}: {:?}",
        result.error
    );
}

async fn collect(export: SessionExport) -> Vec<u8> {
    let length = export.content_length();
    let mut body = export.into_body();
    let mut bytes = Vec::new();

    while let Some(chunk) = body.next().await {
        bytes.extend_from_slice(&chunk.expect("a chunk"));
    }

    assert_eq!(
        bytes.len() as u64,
        length,
        "the body is as long as declared"
    );
    bytes
}

async fn import(
    thread: &ThreadHandle,
    archive: Vec<u8>,
) -> arsox_sdk::client::Result<arsox_sdk::proto::harness::v1::ImportHarnessSessionResponse> {
    let length = archive.len() as u64;
    let body =
        futures_util::stream::iter([Ok::<_, std::io::Error>(axum::body::Bytes::from(archive))]);

    thread.import_session(length, body).await
}

/// The names of an archive's entries, in order.
fn entry_names(archive: &[u8]) -> Vec<String> {
    tar::Archive::new(archive)
        .entries()
        .expect("readable")
        .map(|entry| String::from_utf8_lossy(&entry.expect("an entry").path_bytes()).into_owned())
        .collect()
}

/// Every line a stand-in session file holds, found anywhere under `root`.
fn session_lines(root: &std::path::Path, file_name_suffix: &str) -> Vec<String> {
    let mut stack = vec![root.to_path_buf()];

    while let Some(directory) = stack.pop() {
        for entry in std::fs::read_dir(&directory)
            .into_iter()
            .flatten()
            .flatten()
        {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.to_string_lossy().ends_with(file_name_suffix) {
                return std::fs::read_to_string(path)
                    .expect("readable")
                    .lines()
                    .map(str::to_owned)
                    .collect();
            }
        }
    }

    Vec::new()
}

#[tokio::test]
async fn a_claude_session_continues_in_a_new_thread_on_another_satellite() {
    let first = start().await;
    let origin = thread(&first, Harness::Claude).await;
    complete(&origin, "[[keep_session]] the first turn").await;

    let export = origin.export_session().await.expect("should export");
    assert_eq!(export.harness, Harness::Claude);
    // Claude opens a thread's session under the thread's own id.
    assert_eq!(export.harness_session_id, origin.id());
    let session_id = export.harness_session_id.clone();
    let archive = collect(export).await;

    assert_eq!(
        entry_names(&archive),
        [
            "arsox-session.binpb".to_owned(),
            format!("files/{session_id}.jsonl")
        ]
    );

    let second = start().await;
    let destination = thread(&second, Harness::Claude).await;

    let imported = import(&destination, archive).await.expect("should import");
    assert_eq!(
        imported.session.expect("the session").session_id,
        session_id
    );
    assert_eq!(imported.files, [format!("{session_id}.jsonl")]);

    let adopted = destination.get().await.expect("should read the thread");
    assert_eq!(
        adopted.harness_session_id.as_deref(),
        Some(session_id.as_str())
    );

    // The stand-in resumes only a session it finds where Claude would look
    // from this thread's own workspace, so completing is the proof.
    complete(
        &destination,
        "[[keep_session]] [[record_argv=argv.txt]] the second turn",
    )
    .await;

    let argv = std::fs::read_to_string(
        second
            .workspace_root
            .join(destination.id())
            .join("argv.txt"),
    )
    .expect("recorded");
    assert!(argv.contains(&format!("--resume\n{session_id}")), "{argv}");

    let lines = session_lines(
        &second.agent_home.join(".claude").join("projects"),
        &format!("{session_id}.jsonl"),
    );
    assert_eq!(lines.len(), 2, "both turns are one conversation: {lines:?}");
    assert!(lines[0].contains("the first turn"));
    assert!(lines[1].contains("the second turn"));
}

#[tokio::test]
async fn a_codex_session_continues_the_same_way() {
    let first = start().await;
    let origin = thread(&first, Harness::Codex).await;
    complete(&origin, "[[keep_session]] the first turn").await;

    let export = origin.export_session().await.expect("should export");
    assert_eq!(export.harness, Harness::Codex);
    assert_eq!(export.harness_session_id, CODEX_SESSION_ID);
    let archive = collect(export).await;

    let second = start().await;
    let destination = thread(&second, Harness::Codex).await;
    import(&destination, archive).await.expect("should import");

    complete(
        &destination,
        "[[keep_session]] [[record_argv=argv.txt]] the second turn",
    )
    .await;

    let argv = std::fs::read_to_string(
        second
            .workspace_root
            .join(destination.id())
            .join("argv.txt"),
    )
    .expect("recorded");
    assert!(
        argv.starts_with(&format!(
            "{}\nexec\nresume",
            env!("CARGO_BIN_EXE_arsox-fake-harness")
        )),
        "{argv}"
    );
    assert!(
        argv.contains(&format!("--\n{CODEX_SESSION_ID}\n")),
        "{argv}"
    );

    let lines = session_lines(
        &second.agent_home.join(".codex").join("sessions"),
        &format!("-{CODEX_SESSION_ID}.jsonl"),
    );
    assert_eq!(lines.len(), 2, "{lines:?}");
}

#[tokio::test]
async fn a_thread_with_no_session_has_nothing_to_export() {
    let running = start().await;
    let fresh = thread(&running, Harness::Claude).await;

    let refused = fresh
        .export_session()
        .await
        .expect_err("there is no session yet");
    assert!(refused.is_session_not_found(), "{refused}");
    assert!(!refused.is_not_found(), "the thread itself is there");
}

#[tokio::test]
async fn an_import_is_refused_unless_the_thread_is_fresh_and_runs_the_same_harness() {
    let running = start().await;
    let origin = thread(&running, Harness::Claude).await;
    complete(&origin, "[[keep_session]] the first turn").await;
    let archive = collect(origin.export_session().await.expect("should export")).await;

    // A thread that has run a turn already has a conversation of its own.
    let used = thread(&running, Harness::Claude).await;
    complete(&used, "something else").await;
    let refused = import(&used, archive.clone())
        .await
        .expect_err("the thread is not fresh");
    assert_eq!(
        refused.code(),
        Some(ErrorCode::HarnessSessionAlreadyStarted)
    );

    // So does the one that exported it.
    let refused = import(&origin, archive.clone())
        .await
        .expect_err("the thread has a session");
    assert_eq!(
        refused.code(),
        Some(ErrorCode::HarnessSessionAlreadyStarted)
    );

    let codex = thread(&running, Harness::Codex).await;
    let refused = import(&codex, archive.clone())
        .await
        .expect_err("a Claude session cannot resume under Codex");
    assert_eq!(refused.code(), Some(ErrorCode::HarnessSessionMismatch));
    assert_eq!(
        codex.get().await.expect("readable").harness_session_id,
        None,
        "a refused import records nothing"
    );

    let garbage = import(
        &thread(&running, Harness::Claude).await,
        b"not a tar".to_vec(),
    )
    .await
    .expect_err("not an archive");
    assert_eq!(garbage.code(), Some(ErrorCode::RequestBodyMalformed));
}

#[tokio::test]
async fn an_archive_past_the_cap_is_refused_before_it_is_received() {
    let running = start().await;
    let fresh = thread(&running, Harness::Claude).await;

    // Declared past the cap, so the satellite answers on the header alone.
    let response = reqwest::Client::new()
        .put(format!("{}/v1/threads/{}/session", running.url, fresh.id()))
        .header("Authorization", format!("Bearer {SECRET}"))
        .header("Content-Length", (3_u64 << 30).to_string())
        .send()
        .await
        .expect("the satellite answers");

    assert_eq!(response.status(), reqwest::StatusCode::PAYLOAD_TOO_LARGE);
}
