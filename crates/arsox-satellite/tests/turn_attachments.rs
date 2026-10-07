// Copyright © 2026 Jalapeno Labs

//! Files that come with a turn's prompt, driven through the SDK.
//!
//! What is asserted is what the stand-in harness was actually handed, recorded
//! from inside the child: the stdin message Claude reads its blocks from, and
//! the command line Codex reads its images from. Asserting on the satellite's
//! intent would prove nothing about what a real CLI receives.

#![cfg(feature = "test-util")]

use arsox_satellite::{ServeOptions, assemble};
use arsox_sdk::client::{Satellite as Client, ThreadHandle, TurnOptions};
use arsox_sdk::proto::common::v1::Duration as ProtoDuration;
use arsox_sdk::proto::error::v1::ErrorCode;
use arsox_sdk::proto::harness::v1::Harness;
use arsox_sdk::proto::settings::v1::{Budget, ThreadSettings};
use arsox_sdk::proto::turn::v1::{TurnAttachment, TurnStatus};
use futures_util::StreamExt as _;
use std::time::Duration;

const SECRET: &str = "attachments-test-secret";

/// The recorded transcripts the stand-in replays, one per vocabulary.
const TRANSCRIPT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../arsox-harness/fixtures/claude/2.1.221/tool-call.stdout.jsonl"
);
const CODEX_TRANSCRIPT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../arsox-harness/fixtures/codex/0.147.0/tool-call.stdout.jsonl"
);

/// The first bytes of a PNG, which is all the satellite reads to call it one.
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01";

/// A PDF's signature.
const PDF: &[u8] = b"%PDF-1.7\n%%EOF\n";

/// Distinguishes scratch directories created in the same clock tick.
static NEXT_SCRATCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

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
            std::env::set_var("ARSOX_CODEX_BIN", env!("CARGO_BIN_EXE_arsox-fake-harness"));
            std::env::set_var("ARSOX_FAKE_CODEX_TRANSCRIPT", CODEX_TRANSCRIPT);
        }
    });

    let unique = NEXT_SCRATCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let directory = std::env::temp_dir().join(format!(
        "arsox-attachments-{unique}-{}",
        uuid::Uuid::now_v7()
    ));
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
        agent_home: None,
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

async fn upload(thread: &ThreadHandle, path: &str, contents: &'static [u8]) {
    let body = futures_util::stream::iter([Ok::<_, std::io::Error>(
        axum::body::Bytes::from_static(contents),
    )]);

    thread
        .write_file(path, contents.len() as u64, body)
        .await
        .expect("should upload");
}

/// Exactly the shape a host application submits, claims included.
fn attachment(path: &str) -> TurnAttachment {
    TurnAttachment {
        path: path.to_owned(),
        content_type: Some("image/png".into()),
        size_bytes: 1,
    }
}

async fn run_with(thread: &ThreadHandle, prompt: &str, paths: &[&str]) -> String {
    let turn = thread
        .start_turn_with(
            prompt,
            TurnOptions {
                attachments: paths.iter().map(|path| attachment(path)).collect(),
                ..TurnOptions::default()
            },
        )
        .await
        .expect("should queue");

    let result = tokio::time::timeout(Duration::from_secs(30), turn.result())
        .await
        .expect("should not time out")
        .expect("should report a result");
    assert_eq!(result.status, i32::from(TurnStatus::Completed));

    turn.id().to_owned()
}

async fn read(thread: &ThreadHandle, path: &str) -> String {
    let download = thread.read_file(path).await.expect("should download");
    let mut body = download.into_body();
    let mut bytes = Vec::new();

    while let Some(chunk) = body.next().await {
        bytes.extend_from_slice(&chunk.expect("a chunk"));
    }

    String::from_utf8(bytes).expect("the record is text")
}

#[tokio::test]
async fn claude_is_handed_an_uploaded_image_and_pdf_as_content_blocks() {
    let running = start().await;
    let thread = thread(&running, Harness::Claude).await;

    upload(&thread, "feedback/1/annotated.png", PNG).await;
    upload(&thread, "brief.pdf", PDF).await;
    upload(&thread, "data.csv", b"a,b\n").await;

    let turn_id = run_with(
        &thread,
        "make what I circled greener [[record_stdin=stdin.txt]] [[record_argv=argv.txt]]",
        &["feedback/1/annotated.png", "brief.pdf", "data.csv"],
    )
    .await;

    // Read from the child's own seat: the text block, then one block per file
    // Claude takes natively, decoded to the bytes that were uploaded.
    let blocks = read(&thread, "stdin.txt").await;
    assert_eq!(
        blocks,
        format!(
            "text\nimage image/png {}\ndocument application/pdf {}",
            PNG.len(),
            PDF.len()
        )
    );

    let argv = read(&thread, "argv.txt").await;
    assert!(argv.contains("--input-format\nstream-json"), "{argv}");
    assert!(!argv.contains("greener"), "the prompt left argv: {argv}");

    // Stored as the satellite found the files, not as the caller described
    // them, in the order they were named.
    let turns = thread.turns().await.expect("should list the turns");
    let turn = turns
        .iter()
        .find(|listed| listed.turn_id == turn_id)
        .expect("the turn is listed");
    let stored: Vec<(&str, Option<&str>, u64)> = turn
        .attachments
        .iter()
        .map(|attached| {
            (
                attached.path.as_str(),
                attached.content_type.as_deref(),
                attached.size_bytes,
            )
        })
        .collect();
    assert_eq!(
        stored,
        [
            (
                "feedback/1/annotated.png",
                Some("image/png"),
                PNG.len() as u64
            ),
            ("brief.pdf", Some("application/pdf"), PDF.len() as u64),
            ("data.csv", None, 4),
        ]
    );
}

#[tokio::test]
async fn codex_is_handed_images_by_path_and_told_about_the_rest() {
    let running = start().await;
    let thread = thread(&running, Harness::Codex).await;

    upload(&thread, "sketch.png", PNG).await;
    upload(&thread, "brief.pdf", PDF).await;

    run_with(
        &thread,
        "use these [[record_argv=argv.txt]]",
        &["sketch.png", "brief.pdf"],
    )
    .await;

    let argv: Vec<String> = read(&thread, "argv.txt")
        .await
        .lines()
        .map(str::to_owned)
        .collect();

    let image = argv
        .iter()
        .position(|argument| argument == "-i")
        .expect("the image is passed with -i");
    let expected = running.workspace_root.join(thread.id()).join("sketch.png");
    assert_eq!(argv[image + 1], expected.to_string_lossy());

    let prompt = argv.join("\n");
    assert!(prompt.contains("`brief.pdf` (application/pdf"), "{prompt}");
}

#[tokio::test]
async fn a_turn_without_attachments_launches_claude_as_it_always_has() {
    let running = start().await;
    let thread = thread(&running, Harness::Claude).await;

    run_with(&thread, "plain [[record_argv=argv.txt]]", &[]).await;

    let argv = read(&thread, "argv.txt").await;
    assert!(argv.contains("--print\nplain"), "{argv}");
    assert!(!argv.contains("--input-format"), "{argv}");
}

#[tokio::test]
async fn attachments_that_break_a_rule_are_refused_before_anything_is_queued() {
    let running = start().await;
    let thread = thread(&running, Harness::Claude).await;
    upload(&thread, "one.png", PNG).await;

    let cases: [(Vec<&str>, ErrorCode); 3] = [
        (vec!["missing.png"], ErrorCode::WorkspaceFileNotFound),
        (vec!["../outside.png"], ErrorCode::WorkspacePathInvalid),
        (vec!["one.png"; 9], ErrorCode::RequestFieldInvalid),
    ];

    for (paths, code) in cases {
        let refused = thread
            .start_turn_with(
                "never queued",
                TurnOptions {
                    attachments: paths.iter().map(|path| attachment(path)).collect(),
                    ..TurnOptions::default()
                },
            )
            .await
            .expect_err("the submission breaks a rule");

        assert_eq!(refused.code(), Some(code), "{paths:?}: {refused}");
    }

    std::fs::create_dir_all(running.workspace_root.join(thread.id()).join("folder"))
        .expect("should create a directory");
    let folder = thread
        .start_turn_with(
            "never queued",
            TurnOptions {
                attachments: vec![attachment("folder")],
                ..TurnOptions::default()
            },
        )
        .await
        .expect_err("a directory is not an attachment");
    assert_eq!(folder.code(), Some(ErrorCode::WorkspaceFileNotRegular));

    assert!(
        thread.turns().await.expect("should list").is_empty(),
        "a refused turn is never queued"
    );
}

#[tokio::test]
async fn a_file_replaced_after_queueing_is_named_rather_than_followed() {
    let running = start().await;
    let thread = thread(&running, Harness::Claude).await;
    upload(&thread, "sketch.png", PNG).await;

    // Paused, so the file can be swapped between submission and spawn, which
    // is the window the agent owns.
    thread.pause().await.expect("should pause");

    let turn = thread
        .start_turn_with(
            "look [[record_argv=argv.txt]]",
            TurnOptions {
                attachments: vec![attachment("sketch.png")],
                ..TurnOptions::default()
            },
        )
        .await
        .expect("should queue");

    let path = running.workspace_root.join(thread.id()).join("sketch.png");
    std::fs::remove_file(&path).expect("should remove the upload");
    #[cfg(unix)]
    std::os::unix::fs::symlink("/etc/passwd", &path).expect("should plant a link");

    thread.resume().await.expect("should resume");

    let result = tokio::time::timeout(Duration::from_secs(30), turn.result())
        .await
        .expect("should not time out")
        .expect("should report a result");
    assert_eq!(result.status, i32::from(TurnStatus::Completed));

    // No native block was left to hand over, so the prompt went back to argv
    // and says what happened to the file.
    let argv = read(&thread, "argv.txt").await;
    assert!(argv.contains("`sketch.png` could not be read"), "{argv}");

    let incidents = thread
        .incidents(arsox_sdk::client::IncidentQuery::default())
        .await
        .expect("should list incidents");
    assert!(
        incidents
            .iter()
            .any(|incident| incident.code == i32::from(ErrorCode::WorkspaceFileNotFound)),
        "{incidents:?}"
    );
}
