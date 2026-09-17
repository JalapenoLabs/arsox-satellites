// Copyright © 2026 Jalapeno Labs

//! The MCP server the agents reach for a thread's relayed tools.
//!
//! Each relayed server is served over streamable HTTP at
//! `/t/{token}/mcp/{server}` on the LLM proxy's loopback listener. The address
//! is the one the harness already reaches its model through, so an agent needs
//! no new credential, no new host in its egress allowlist, and no new port: the
//! turn's grant token in the path is the whole authorization, and the grant is
//! revoked when the turn ends exactly as it is for model traffic.
//!
//! # What is answered here, and what travels
//!
//! | Method | Answered by |
//! |---|---|
//! | `initialize` | here, with the server's instructions and the tools capability |
//! | `notifications/initialized` and every other notification | here, `202 Accepted` |
//! | `ping` | here |
//! | `tools/list` | here, from the thread's settings |
//! | `tools/call` | the host application, over the relay |
//! | anything else | here, JSON-RPC method not found |
//!
//! `tools/list` never touches the relay, so a harness starting while the host
//! application is disconnected still learns the tools exist, and a call made in
//! that window fails as a readable tool error rather than as a server the
//! harness could not load.
//!
//! # Plain JSON, no session
//!
//! Every response is one `application/json` body. The transport also permits an
//! SSE stream per request and a session id, and neither is needed by a server
//! that holds no state between requests and sends nothing unprompted. `GET`,
//! which opens a stream for server-initiated messages, answers
//! `405 Method Not Allowed`, as the transport specifies for a server that offers
//! none. Both pinned CLIs were measured accepting exactly this: Claude 2.1.235
//! and Codex 0.147.0. See `docs/relay.md`.
//!
//! # Versions
//!
//! A client's requested protocol version is echoed when it is one this server
//! speaks, and the newest one it speaks is offered otherwise, which is the
//! negotiation the specification describes. Claude 2.1.235 asks for
//! `2025-11-25` and accepts `2025-06-18` in its place.

use super::{CallFailure, CallRequest, Hub};
use crate::proxy::Grant;
use arsox_sdk::proto::relay::v1::{ToolResult, tool_content};
use arsox_sdk::proto::settings::v1::RelayedMcpServer;
use axum::body::Bytes;
use axum::http::{Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

/// The protocol versions this server speaks, newest first.
///
/// The tool surface used here, `tools/list` and `tools/call` with text content,
/// is unchanged across all three, so speaking each is a matter of echoing it.
const PROTOCOL_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

/// The largest JSON-RPC request body read.
///
/// Tool arguments are small: bytes move through the workspace file routes, and
/// a call names a path rather than carrying a file. The bound exists so a
/// runaway agent cannot make the satellite buffer without limit.
pub const MAX_REQUEST_BODY: usize = 4 * 1024 * 1024;

/// JSON-RPC's code for a body that is not JSON.
const PARSE_ERROR: i64 = -32700;

/// JSON-RPC's code for a message that is not a valid request.
const INVALID_REQUEST: i64 = -32600;

/// JSON-RPC's code for a method this server does not offer.
const METHOD_NOT_FOUND: i64 = -32601;

/// JSON-RPC's code for parameters that do not fit the method, which MCP also
/// uses for a call naming a tool the server does not have.
const INVALID_PARAMS: i64 = -32602;

/// Answers one HTTP request to a relayed server.
///
/// `grant` is the turn the token in the path belongs to, and `server` the name
/// in the path. The caller has already read the body, bounded by
/// [`MAX_REQUEST_BODY`].
pub async fn serve(
    hub: &Hub,
    grant: &Grant,
    server: &str,
    method: &Method,
    body: &Bytes,
) -> Response {
    // Refused the same way as an unknown token, one level up: a name this turn
    // has no server for says nothing about which other names exist.
    let Some(declared) = grant
        .relayed_servers()
        .iter()
        .find(|declared| declared.name == server)
    else {
        return StatusCode::NOT_FOUND.into_response();
    };

    if method != Method::POST {
        return (StatusCode::METHOD_NOT_ALLOWED, [(header::ALLOW, "POST")]).into_response();
    }

    let Ok(message) = serde_json::from_slice::<Value>(body) else {
        return json_response(&failure(&Value::Null, PARSE_ERROR, "the body is not JSON"));
    };

    let context = Context {
        hub,
        grant,
        server: declared,
    };

    // A batch is answered as a batch. The 2025-06-18 revision removed batching
    // and earlier ones allow it, so a client speaking an earlier one may send
    // one, and answering it costs a loop.
    let reply = match message {
        Value::Array(messages) => {
            let mut replies = Vec::with_capacity(messages.len());
            for message in &messages {
                if let Some(reply) = context.answer(message).await {
                    replies.push(reply);
                }
            }
            (!replies.is_empty()).then_some(Value::Array(replies))
        }
        single => context.answer(&single).await,
    };

    match reply {
        Some(reply) => json_response(&reply),
        // Nothing to say, because everything received was a notification or a
        // response, which the transport acknowledges with an empty 202.
        None => StatusCode::ACCEPTED.into_response(),
    }
}

/// What answering one message needs.
struct Context<'a> {
    hub: &'a Hub,
    grant: &'a Grant,
    server: &'a RelayedMcpServer,
}

impl Context<'_> {
    /// The reply to one JSON-RPC message, or nothing for a notification.
    async fn answer(&self, message: &Value) -> Option<Value> {
        let Some(object) = message.as_object() else {
            return Some(failure(
                &Value::Null,
                INVALID_REQUEST,
                "a message is a JSON object",
            ));
        };

        // No id is a notification, and no method is a response to something
        // this server never asked. Neither gets a reply.
        let id = object.get("id")?;
        let method = object.get("method").and_then(Value::as_str)?;

        let params = object.get("params").unwrap_or(&Value::Null);

        let outcome = match method {
            "initialize" => Ok(self.initialize(params)),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(self.list_tools()),
            "tools/call" => self.call_tool(params).await,
            _unknown => Err((
                METHOD_NOT_FOUND,
                format!("method {method:?} is not offered"),
            )),
        };

        Some(match outcome {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err((code, message)) => failure(id, code, &message),
        })
    }

    fn initialize(&self, params: &Value) -> Value {
        let requested = params.get("protocolVersion").and_then(Value::as_str);
        let version = requested
            .filter(|requested| PROTOCOL_VERSIONS.contains(requested))
            .unwrap_or(PROTOCOL_VERSIONS[0]);

        let mut result = json!({
            "protocolVersion": version,
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": {
                "name": self.server.name,
                "version": env!("CARGO_PKG_VERSION"),
            },
        });

        if !self.server.instructions.is_empty() {
            result["instructions"] = Value::String(self.server.instructions.clone());
        }

        result
    }

    fn list_tools(&self) -> Value {
        let tools: Vec<Value> = self
            .server
            .tools
            .iter()
            .map(|tool| {
                // Validated as a JSON object when the thread was created, so a
                // schema that no longer parses is settings stored before that
                // check. An empty object schema keeps the tool callable rather
                // than hiding it.
                let schema = serde_json::from_str::<Value>(&tool.input_schema_json)
                    .ok()
                    .filter(Value::is_object)
                    .unwrap_or_else(|| json!({ "type": "object" }));

                json!({
                    "name": tool.name,
                    "description": tool.description,
                    "inputSchema": schema,
                })
            })
            .collect();

        json!({ "tools": tools })
    }

    async fn call_tool(&self, params: &Value) -> Result<Value, (i64, String)> {
        let Some(tool) = params.get("name").and_then(Value::as_str) else {
            return Err((INVALID_PARAMS, "tools/call needs a tool name".to_owned()));
        };

        if !self
            .server
            .tools
            .iter()
            .any(|declared| declared.name == tool)
        {
            return Err((
                INVALID_PARAMS,
                format!("this server has no tool named {tool:?}"),
            ));
        }

        // Absent arguments are an empty object, which is what a tool taking
        // none is called with.
        let arguments = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));

        let request = CallRequest {
            server: self.server.name.clone(),
            tool: tool.to_owned(),
            arguments_json: arguments.to_string(),
            turn_id: self.grant.turn_id().to_owned(),
        };

        // Held for exactly as long as the call waits, so the runner does not
        // mistake an agent waiting on the host application for a harness that
        // stopped. See `docs/timeouts.md`.
        let _waiting = self.grant.relay_calls().start();

        let outcome = self.hub.call(self.grant.thread_id(), request).await;

        Ok(match outcome {
            Ok(result) => tool_result(&result),
            Err(failure) => failed_call(failure),
        })
    }
}

/// A host application's answer, as MCP renders a tool result.
///
/// A piece with no kind, which is how a content kind added to the contract
/// after this build decodes, is dropped: inventing a rendering for it would put
/// words in the tool's mouth the agent would trust. A kind added to this build's
/// contract stops this compiling until somebody says how MCP renders it.
fn tool_result(result: &ToolResult) -> Value {
    let content: Vec<Value> = result
        .content
        .iter()
        .filter_map(|piece| {
            piece
                .kind
                .as_ref()
                .map(|tool_content::Kind::Text(text)| json!({ "type": "text", "text": text }))
        })
        .collect();

    json!({ "content": content, "isError": result.is_error })
}

/// A call that ended without an answer, as a tool error the agent reads.
///
/// A tool error rather than a JSON-RPC error, because the specification keeps
/// protocol errors for a request the server could not process and tool errors
/// for a tool that ran and failed. The distinction matters to the harness: a
/// tool error reaches the model, which can decide what to do next, where a
/// protocol error is reported as a broken server.
fn failed_call(failure: CallFailure) -> Value {
    json!({
        "content": [{ "type": "text", "text": failure.message() }],
        "isError": true,
    })
}

fn failure(id: &Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
}

fn json_response(body: &Value) -> Response {
    (
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::budget::{Ceilings, Meter};
    use crate::proxy::failover::Route;
    use arsox_sdk::proto::relay::v1::ToolContent;
    use arsox_sdk::proto::settings::v1::RelayedTool;
    use std::sync::Arc;

    const THREAD: &str = "019fd32f-a25f-7611-a4fe-c93cc2a6d782";

    fn storage() -> RelayedMcpServer {
        RelayedMcpServer {
            name: "storage".to_owned(),
            instructions: "Files the host application keeps.".to_owned(),
            tools: vec![RelayedTool {
                name: "upload".to_owned(),
                description: "Uploads a workspace file.".to_owned(),
                input_schema_json: r#"{"type":"object","properties":{"path":{"type":"string"}}}"#
                    .to_owned(),
            }],
        }
    }

    fn grant() -> Grant {
        let (crossings, _unheard) = tokio::sync::mpsc::unbounded_channel();

        Grant::new(
            THREAD,
            "the-turn",
            Route::resolve(&[]),
            Arc::new(Meter::new(&Ceilings::default(), crossings)),
        )
        .relaying(vec![storage()])
    }

    async fn post(hub: &Hub, grant: &Grant, body: &Value) -> (StatusCode, Option<Value>) {
        let response = serve(
            hub,
            grant,
            "storage",
            &Method::POST,
            &Bytes::from(body.to_string()),
        )
        .await;

        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("a body");

        (status, serde_json::from_slice(&bytes).ok())
    }

    #[tokio::test]
    async fn initialize_echoes_a_known_version_and_offers_the_newest_otherwise() {
        let hub = Hub::new();
        let grant = grant();

        let (_, known) = post(
            &hub,
            &grant,
            &json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize",
                     "params": { "protocolVersion": "2025-03-26" } }),
        )
        .await;
        let known = known.expect("a reply");
        assert_eq!(known["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(
            known["result"]["capabilities"]["tools"]["listChanged"],
            false
        );
        assert_eq!(
            known["result"]["instructions"],
            "Files the host application keeps."
        );

        // What Claude 2.1.235 asks for, and accepts an older version in place of.
        let (_, newer) = post(
            &hub,
            &grant,
            &json!({ "jsonrpc": "2.0", "id": "a", "method": "initialize",
                     "params": { "protocolVersion": "2025-11-25" } }),
        )
        .await;
        let newer = newer.expect("a reply");
        assert_eq!(newer["id"], "a");
        assert_eq!(newer["result"]["protocolVersion"], PROTOCOL_VERSIONS[0]);
    }

    #[tokio::test]
    async fn a_notification_is_accepted_with_nothing_to_say() {
        let (status, body) = post(
            &Hub::new(),
            &grant(),
            &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
        )
        .await;

        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(body, None);
    }

    #[tokio::test]
    async fn tools_are_listed_from_settings_with_no_client_attached() {
        let (_, body) = post(
            &Hub::new(),
            &grant(),
            &json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
        )
        .await;

        let tools = &body.expect("a reply")["result"]["tools"];
        assert_eq!(tools[0]["name"], "upload");
        assert_eq!(tools[0]["description"], "Uploads a workspace file.");
        assert_eq!(
            tools[0]["inputSchema"]["properties"]["path"]["type"],
            "string"
        );
    }

    #[tokio::test]
    async fn an_unknown_method_is_method_not_found_and_ping_is_empty() {
        let hub = Hub::new();
        let grant = grant();

        let (_, unknown) = post(
            &hub,
            &grant,
            &json!({ "jsonrpc": "2.0", "id": 3, "method": "server/discover" }),
        )
        .await;
        assert_eq!(unknown.expect("a reply")["error"]["code"], METHOD_NOT_FOUND);

        let (_, ping) = post(
            &hub,
            &grant,
            &json!({ "jsonrpc": "2.0", "id": 4, "method": "ping" }),
        )
        .await;
        assert_eq!(ping.expect("a reply")["result"], json!({}));
    }

    #[tokio::test]
    async fn a_call_with_no_client_attached_is_a_tool_error_saying_so() {
        let (_, body) = post(
            &Hub::new(),
            &grant(),
            &json!({ "jsonrpc": "2.0", "id": 5, "method": "tools/call",
                     "params": { "name": "upload", "arguments": { "path": "a.txt" } } }),
        )
        .await;

        let result = &body.expect("a reply")["result"];
        assert_eq!(result["isError"], true);
        assert!(
            result["content"][0]["text"]
                .as_str()
                .is_some_and(|text| text.contains("not connected")),
            "{result}"
        );
    }

    #[tokio::test]
    async fn a_call_naming_an_undeclared_tool_is_invalid_params() {
        let (_, body) = post(
            &Hub::new(),
            &grant(),
            &json!({ "jsonrpc": "2.0", "id": 6, "method": "tools/call",
                     "params": { "name": "delete_everything" } }),
        )
        .await;

        assert_eq!(body.expect("a reply")["error"]["code"], INVALID_PARAMS);
    }

    #[tokio::test]
    async fn a_call_is_relayed_and_its_answer_returned() {
        let hub = Hub::new();
        let grant = grant();
        let mut connection = hub.attach(THREAD);

        let answering = {
            let hub = hub.clone();
            async move {
                let Some(super::super::Outbound::Frame(frame)) = connection.outbound.recv().await
                else {
                    panic!("the call should arrive");
                };
                let Some(arsox_sdk::proto::relay::v1::satellite_relay_frame::Frame::Call(call)) =
                    frame.frame
                else {
                    panic!("the frame should be a call");
                };

                assert_eq!(call.tool, "upload");
                assert_eq!(call.turn_id, "the-turn");
                assert_eq!(
                    serde_json::from_str::<Value>(&call.arguments_json).expect("JSON"),
                    json!({ "path": "a.txt" })
                );

                hub.deliver(
                    THREAD,
                    connection.id,
                    ToolResult {
                        call_id: call.call_id,
                        content: vec![ToolContent {
                            kind: Some(tool_content::Kind::Text("uploaded".to_owned())),
                        }],
                        is_error: false,
                    },
                );
            }
        };

        let request = json!({ "jsonrpc": "2.0", "id": 7, "method": "tools/call",
                              "params": { "name": "upload", "arguments": { "path": "a.txt" } } });
        let calling = post(&hub, &grant, &request);

        let ((_, body), ()) = tokio::join!(calling, answering);
        let result = &body.expect("a reply")["result"];

        assert_eq!(result["isError"], false);
        assert_eq!(result["content"][0]["text"], "uploaded");
    }

    #[tokio::test]
    async fn a_get_is_refused_and_an_undeclared_server_is_not_found() {
        let hub = Hub::new();
        let grant = grant();

        let get = serve(&hub, &grant, "storage", &Method::GET, &Bytes::new()).await;
        assert_eq!(get.status(), StatusCode::METHOD_NOT_ALLOWED);

        let unknown = serve(&hub, &grant, "other", &Method::POST, &Bytes::new()).await;
        assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
    }
}
