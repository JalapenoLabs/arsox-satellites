// Copyright © 2026 Jalapeno Labs

//! Relayed MCP tools and workspace files, driven from a consumer's seat.
//!
//! A real satellite, the real SDK, and the stand-in harness. The relayed
//! server's address is read from the command line the harness was actually
//! launched with, so what these tests call is the endpoint an agent would have
//! been pointed at, under the turn grant it would have held.

#![cfg(feature = "test-util")]

use arsox_satellite::{ServeOptions, assemble};
use arsox_sdk::client::{RelayEvent, Satellite as Client, ThreadHandle};
use arsox_sdk::proto::common::v1::Duration as ProtoDuration;
use arsox_sdk::proto::error::v1::ErrorCode;
use arsox_sdk::proto::relay::v1::{ToolContent, ToolResult, tool_content};
use arsox_sdk::proto::settings::v1::{Budget, RelayedMcpServer, RelayedTool, ThreadSettings};
use futures_util::StreamExt as _;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

const SECRET: &str = "relay-test-secret";

/// The recorded transcript the stand-in replays once its hang is over.
const TRANSCRIPT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../arsox-harness/fixtures/claude/2.1.221/tool-call.stdout.jsonl"
);

/// How long the stand-in harness holds its turn open, so its grant stays live.
///
/// Far longer than any assertion here takes, and far shorter than the idle
/// bound, which is what would otherwise tear it down.
const HOLD_TURN_OPEN_MS: u64 = 60_000;

/// Distinguishes scratch directories created in the same clock tick.
static NEXT_SCRATCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A running satellite: where to reach it, and where its workspaces live.
struct Started {
    url: String,
    workspace_root: PathBuf,
}

async fn start() -> Started {
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
        std::env::temp_dir().join(format!("arsox-relay-{unique}-{}", uuid::Uuid::now_v7()));
    tokio::fs::create_dir_all(&directory)
        .await
        .expect("should create a scratch directory");

    let assembled = assemble(ServeOptions {
        secret: Some(SECRET.to_owned()),
        allow_insecure: false,
        database_path: directory.join("arsox.db").to_string_lossy().into_owned(),
        workspace_root: directory.to_string_lossy().into_owned(),
        broker_root: directory.join("broker").to_string_lossy().into_owned(),
        max_concurrent_threads: 4,
        port: 0,
        collect_interval: Duration::from_hours(1),
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

    Started {
        url: format!("http://127.0.0.1:{port}"),
        workspace_root: directory,
    }
}

/// The settings a thread must declare, plus the relayed servers given.
fn settings(relayed: Vec<RelayedMcpServer>) -> ThreadSettings {
    ThreadSettings {
        idle_ttl: Some(ProtoDuration {
            seconds: 3600,
            nanos: 0,
        }),
        budget: Some(Budget::default()),
        relayed_mcp_servers: relayed,
        ..ThreadSettings::default()
    }
}

/// The storage server a host application like Elysium would declare.
fn storage() -> RelayedMcpServer {
    RelayedMcpServer {
        name: "storage".to_owned(),
        instructions: "Files the host application keeps.".to_owned(),
        tools: vec![RelayedTool {
            name: "upload".to_owned(),
            description: "Uploads a workspace file to storage.".to_owned(),
            input_schema_json: r#"{"type":"object","properties":{"path":{"type":"string"}}}"#
                .to_owned(),
        }],
    }
}

/// Opens a thread with the storage server and a turn holding its grant open.
///
/// Returns the handle and the URL the harness was launched with for the
/// server, read from the harness's own command line.
async fn thread_with_live_turn(started: &Started, client: &Client) -> (ThreadHandle, String) {
    let created = client
        .threads()
        .create(settings(vec![storage()]))
        .await
        .expect("should create");
    let thread = created.handle;

    thread
        .start_turn(format!(
            "[[record_argv=argv.txt]] [[hang={HOLD_TURN_OPEN_MS}]]"
        ))
        .await
        .expect("should queue");

    let recorded = started.workspace_root.join(thread.id()).join("argv.txt");
    let argv = wait_for_file(&recorded).await;

    let arguments: Vec<&str> = argv.lines().collect();
    let config_at = arguments
        .iter()
        .position(|argument| *argument == "--mcp-config")
        .expect("a thread with a relayed server launches with --mcp-config");
    let config: Value = serde_json::from_str(arguments[config_at + 1]).expect("JSON");
    assert!(arguments.contains(&"--strict-mcp-config"));

    let url = config["mcpServers"]["storage"]["url"]
        .as_str()
        .expect("the storage server has a URL")
        .to_owned();

    (thread, url)
}

async fn wait_for_file(path: &Path) -> String {
    for _attempt in 0..200 {
        if let Ok(contents) = tokio::fs::read_to_string(path).await
            && !contents.is_empty()
        {
            return contents;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    panic!("{} never appeared", path.display());
}

/// Sends one JSON-RPC message the way a harness does.
async fn rpc(url: &str, message: &Value) -> (reqwest::StatusCode, Option<Value>) {
    let response = reqwest::Client::new()
        .post(url)
        .header("Accept", "application/json, text/event-stream")
        .json(message)
        .send()
        .await
        .expect("the endpoint answers");

    let status = response.status();
    let body = response.bytes().await.expect("a body");

    (status, serde_json::from_slice(&body).ok())
}

fn call_upload(id: u64) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": { "name": "upload", "arguments": { "path": "report.pdf" } },
    })
}

fn text_result(call_id: String, text: &str) -> ToolResult {
    ToolResult {
        call_id,
        content: vec![ToolContent {
            kind: Some(tool_content::Kind::Text(text.to_owned())),
        }],
        is_error: false,
    }
}

#[tokio::test]
async fn an_agent_lists_and_calls_a_relayed_tool_the_host_application_answers() {
    let started = start().await;
    let client = Client::connect(&started.url, SECRET)
        .await
        .expect("should connect");
    let (thread, url) = thread_with_live_turn(&started, &client).await;

    // What both pinned CLIs send first, answered without a client attached.
    let (_, initialized) = rpc(
        &url,
        &json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize",
                 "params": { "protocolVersion": "2025-06-18" } }),
    )
    .await;
    let initialized = initialized.expect("a reply");
    assert_eq!(initialized["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(
        initialized["result"]["instructions"],
        "Files the host application keeps."
    );

    let (status, _) = rpc(
        &url,
        &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::ACCEPTED);

    let (_, listed) = rpc(
        &url,
        &json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
    )
    .await;
    assert_eq!(
        listed.expect("a reply")["result"]["tools"][0]["name"],
        "upload"
    );

    // No client attached yet: a tool error the agent can read, at once.
    let (_, unattached) = rpc(&url, &call_upload(2)).await;
    let unattached = unattached.expect("a reply");
    assert_eq!(unattached["result"]["isError"], true);
    assert!(
        unattached["result"]["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("not connected")),
        "{unattached}"
    );

    let mut relay = thread.relay().await.expect("should attach");
    let answerer = relay.answerer();

    let answering = tokio::spawn(async move {
        let Some(Ok(RelayEvent::Call(call))) = relay.next().await else {
            panic!("the call should arrive");
        };

        assert_eq!(call.server, "storage");
        assert_eq!(call.tool, "upload");
        assert!(!call.turn_id.is_empty());
        assert_eq!(
            serde_json::from_str::<Value>(&call.arguments_json).expect("JSON"),
            json!({ "path": "report.pdf" })
        );

        answerer
            .answer(text_result(call.call_id, "uploaded report.pdf"))
            .await
            .expect("should answer");

        relay
    });

    let (_, tool_reply) = rpc(&url, &call_upload(3)).await;
    let tool_reply = tool_reply.expect("a reply");
    assert_eq!(tool_reply["result"]["isError"], false);
    assert_eq!(
        tool_reply["result"]["content"][0]["text"],
        "uploaded report.pdf"
    );

    drop(answering.await.expect("the answering task finishes"));
}

#[tokio::test]
async fn a_second_client_replaces_the_first_and_its_calls_in_flight_fail() {
    let started = start().await;
    let client = Client::connect(&started.url, SECRET)
        .await
        .expect("should connect");
    let (thread, url) = thread_with_live_turn(&started, &client).await;

    let mut first = thread.relay().await.expect("should attach");

    let calling = tokio::spawn({
        let url = url.clone();
        async move { rpc(&url, &call_upload(1)).await }
    });

    let Some(Ok(RelayEvent::Call(call))) = first.next().await else {
        panic!("the first client should receive the call");
    };

    let mut second = thread.relay().await.expect("should attach again");

    // The first client is told why it was closed, by code.
    let replaced = first
        .next()
        .await
        .expect("a close reason")
        .expect_err("a replacement is an error");
    assert_eq!(replaced.code(), Some(ErrorCode::RelayClientReplaced));

    // The call it was holding fails as a tool error the agent reads.
    let (_, failed) = calling.await.expect("the call returns");
    let failed = failed.expect("a reply");
    assert_eq!(failed["result"]["isError"], true);
    assert!(
        failed["result"]["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("disconnected")),
        "{failed}"
    );
    drop(call);

    // And the replacement answers the next one.
    let answerer = second.answerer();
    let answering = tokio::spawn(async move {
        let Some(Ok(RelayEvent::Call(call))) = second.next().await else {
            panic!("the second client should receive the call");
        };
        answerer
            .answer(text_result(call.call_id, "second"))
            .await
            .expect("should answer");
        second
    });

    let (_, tool_reply) = rpc(&url, &call_upload(2)).await;
    assert_eq!(
        tool_reply.expect("a reply")["result"]["content"][0]["text"],
        "second"
    );
    drop(answering.await.expect("finishes"));
}

#[tokio::test]
async fn a_relay_is_refused_for_a_thread_with_nothing_to_relay_or_no_thread_at_all() {
    let started = start().await;
    let client = Client::connect(&started.url, SECRET)
        .await
        .expect("should connect");

    let plain = client
        .threads()
        .create(settings(Vec::new()))
        .await
        .expect("should create")
        .handle;

    let refused = plain.relay().await.expect_err("nothing to relay");
    assert!(refused.is_relay_not_declared(), "{refused}");
    // Permanent, so a client stops rather than reconnecting forever.
    assert!(!refused.is_retryable());

    plain.destroy().await.expect("should destroy");
    let gone = plain.relay().await.expect_err("no thread");
    assert!(gone.is_gone() || gone.is_not_found(), "{gone}");
}

#[tokio::test]
async fn a_thread_declaring_an_invalid_relayed_server_is_refused_naming_the_field() {
    let started = start().await;
    let client = Client::connect(&started.url, SECRET)
        .await
        .expect("should connect");

    let mut broken = storage();
    broken.tools[0].input_schema_json = "[]".to_owned();

    let error = client
        .threads()
        .create(settings(vec![broken]))
        .await
        .expect_err("should be refused");

    assert_eq!(error.code(), Some(ErrorCode::RequestFieldInvalid));
    assert!(
        error.to_string().contains("settings.relayed_mcp_servers"),
        "{error}"
    );
}

#[tokio::test]
async fn a_file_round_trips_through_the_workspace_by_the_sdk() {
    let started = start().await;
    let client = Client::connect(&started.url, SECRET)
        .await
        .expect("should connect");
    let thread = client
        .threads()
        .create(settings(Vec::new()))
        .await
        .expect("should create")
        .handle;

    let contents = axum::body::Bytes::from_static(b"a report the agent needs");
    let body = futures_util::stream::iter([Ok::<_, std::io::Error>(contents.clone())]);

    let written = thread
        .write_file("inbox/q3/report.txt", contents.len() as u64, body)
        .await
        .expect("should write");
    assert!(written.created);
    assert_eq!(written.size_bytes, contents.len() as u64);
    assert_eq!(written.path, "inbox/q3/report.txt");
    assert_eq!(written.sha256.len(), 64);

    let download = thread
        .read_file("inbox/q3/report.txt")
        .await
        .expect("should read");
    assert_eq!(download.content_length(), contents.len() as u64);
    assert_eq!(download.content_type(), Some("text/plain"));

    let mut received = Vec::new();
    let mut body = download.into_body();
    while let Some(chunk) = body.next().await {
        received.extend_from_slice(&chunk.expect("a chunk"));
    }
    assert_eq!(received, contents);

    // Replacing an existing file says so.
    let again = thread
        .write_file(
            "inbox/q3/report.txt",
            2,
            futures_util::stream::iter([Ok::<_, std::io::Error>(axum::body::Bytes::from_static(
                b"ok",
            ))]),
        )
        .await
        .expect("should replace");
    assert!(!again.created);

    let missing = thread
        .read_file("inbox/nothing.txt")
        .await
        .expect_err("nothing is there");
    assert_eq!(missing.code(), Some(ErrorCode::WorkspaceFileNotFound));

    let directory = thread.read_file("inbox").await.expect_err("a directory");
    assert_eq!(directory.code(), Some(ErrorCode::WorkspaceFileNotRegular));
}

#[cfg(unix)]
#[tokio::test]
async fn a_link_the_agent_left_in_the_workspace_is_never_followed() {
    let started = start().await;
    let client = Client::connect(&started.url, SECRET)
        .await
        .expect("should connect");
    let thread = client
        .threads()
        .create(settings(Vec::new()))
        .await
        .expect("should create")
        .handle;

    let outside = started.workspace_root.join("outside");
    tokio::fs::create_dir_all(&outside).await.expect("created");
    tokio::fs::write(outside.join("secret"), "outside the workspace")
        .await
        .expect("written");

    let workspace = started.workspace_root.join(thread.id());
    tokio::fs::create_dir_all(&workspace)
        .await
        .expect("created");
    std::os::unix::fs::symlink(&outside, workspace.join("escape")).expect("linked");
    std::os::unix::fs::symlink(outside.join("secret"), workspace.join("secret-link"))
        .expect("linked");

    for path in ["escape/secret", "secret-link"] {
        let error = thread.read_file(path).await.expect_err("never followed");
        assert_eq!(
            error.code(),
            Some(ErrorCode::WorkspacePathInvalid),
            "{path}"
        );
    }

    let planted = thread
        .write_file(
            "escape/planted",
            1,
            futures_util::stream::iter([Ok::<_, std::io::Error>(axum::body::Bytes::from_static(
                b"x",
            ))]),
        )
        .await
        .expect_err("never written through");
    assert_eq!(planted.code(), Some(ErrorCode::WorkspacePathInvalid));
    assert!(!outside.join("planted").exists());
}

/// Sends a raw request, for what a well-behaved HTTP client refuses to send.
async fn raw_request(url: &str, request: &str) -> String {
    let address = url.trim_start_matches("http://");
    let mut stream = tokio::net::TcpStream::connect(address)
        .await
        .expect("should connect");

    stream
        .write_all(request.as_bytes())
        .await
        .expect("should send");

    // Bytes rather than a string, because an error body is protobuf.
    let mut response = Vec::new();
    drop(stream.read_to_end(&mut response).await);
    String::from_utf8_lossy(&response).into_owned()
}

#[tokio::test]
async fn a_traversal_or_a_write_without_a_length_is_refused_on_the_wire() {
    let started = start().await;
    let client = Client::connect(&started.url, SECRET)
        .await
        .expect("should connect");
    let thread = client
        .threads()
        .create(settings(Vec::new()))
        .await
        .expect("should create")
        .handle;

    // The SDK refuses these before sending, and so does any URL library, which
    // resolves dot segments. A hand-written request is what an attacker sends.
    for traversal in [
        "a/../../arsox.db",
        "%2E%2E/arsox.db",
        "a/%2e%2e/%2e%2e/arsox.db",
    ] {
        let response = raw_request(
            &started.url,
            &format!(
                "GET /v1/threads/{}/files/{traversal} HTTP/1.1\r\nHost: satellite\r\n\
                 Authorization: Bearer {SECRET}\r\nConnection: close\r\n\r\n",
                thread.id()
            ),
        )
        .await;

        assert!(
            response.starts_with("HTTP/1.1 400") || response.starts_with("HTTP/1.1 404"),
            "{traversal}: {response}"
        );
        assert!(
            !response.contains("SQLite format"),
            "{traversal}: {response}"
        );
    }

    let chunked = raw_request(
        &started.url,
        &format!(
            "PUT /v1/threads/{}/files/inbox/a.txt HTTP/1.1\r\nHost: satellite\r\n\
             Authorization: Bearer {SECRET}\r\nTransfer-Encoding: chunked\r\n\
             Connection: close\r\n\r\n1\r\na\r\n0\r\n\r\n",
            thread.id()
        ),
    )
    .await;
    assert!(chunked.starts_with("HTTP/1.1 411"), "{chunked}");
}
