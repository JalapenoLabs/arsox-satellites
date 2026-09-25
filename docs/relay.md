# The tool relay and workspace files

A host application often cannot be reached from its satellites. It has no public
address, and nothing between the two was set up for a satellite to dial in. The
host can still offer the agents tools of its own, and move files in and out of a
workspace, because both travel in the direction every SDK call already goes:
host to satellite.

| Surface | Route | Carries |
|---|---|---|
| the relay socket | `wss /v1/threads/{id}/relay` | tool calls down, results up |
| the agent-facing MCP server | `http://127.0.0.1:<proxy>/t/{token}/mcp/{server}` | the relayed tools, to the harness |
| workspace files | `GET` and `PUT /v1/threads/{id}/files/{path}` | one file's bytes, either way |
| workspace listings | `GET /v1/threads/{id}/files` and `GET /v1/threads/{id}/artifacts` | one page of paths, sizes, and hashes |

The code is `src/relay.rs`, `src/relay/socket.rs`, `src/relay/mcp.rs`,
`src/workspace/files.rs`, `src/workspace/confined.rs`, and `src/artifacts.rs`.

A relayed server is the choice when the tools belong to the host application.
When they have to run beside the workspace instead, such as an MCP server driving
an editor that works on the thread's files, the thread declares a
[service](./services.md) and an MCP server that names it, and the satellite runs
both for each turn.

## Declaring relayed tools

A thread lists them in `ThreadSettings.relayed_mcp_servers`. Each
`RelayedMcpServer` has a name, optional instructions, and its `RelayedTool`s,
each with a name, a description, and an input schema as JSON.

Refused at thread creation with `REQUEST_FIELD_INVALID`, naming
`settings.relayed_mcp_servers` and the server:

| Field | Rule | Why |
|---|---|---|
| the list | at most 16 servers | the same bound declared servers have |
| `name` | the `McpServer` name rule: 1 to 64 of `A-Z a-z 0-9 _ -`, not starting with `arsox` | it travels the same places: a Codex config key, a Claude JSON key, every tool name |
| `name`, across both lists | unique ignoring case across `mcp_servers` and `relayed_mcp_servers` | both reach the agent as `mcp__<name>`, and a CLI holds one server per name |
| `instructions` | at most 8192 bytes | shown to the agent when its harness connects |
| `tools` | at most 64 per server | every tool is described to the model on every request |
| a tool `name` | 1 to 64 of `A-Z a-z 0-9 _ -`, unique within its server | it is the second half of `mcp__<server>__<tool>` |
| a tool `description` | at most 4096 bytes | |
| `input_schema_json` | at most 64 KiB, a JSON object whose `type` is `"object"` | the only shape MCP allows, and a CLI handed another refuses the whole server |

None of this reaches a command line. A relayed server adds one short URL to the
launch and nothing else, so the argument budget the declared servers are bounded
by is not what bounds these.

## The relay socket

`GET /v1/threads/{id}/relay`, upgraded to a WebSocket behind the same bearer
check as every other authenticated route. Frames are binary protobuf, one
message per frame: `SatelliteRelayFrame` down (`ToolCall` or
`ToolCallCancelled`) and `ClientRelayFrame` up (`ToolResult`). The JSON
subprotocol is refused by name, as on the event streams.

Refused before the upgrade, as ordinary contract errors:

| Condition | Answer |
|---|---|
| unknown thread | `404 THREAD_NOT_FOUND` |
| collected thread | `410 THREAD_EXPIRED` or `THREAD_DESTROYED` |
| the thread declared no relayed servers | `409 RELAY_NOT_DECLARED` |

`RELAY_NOT_DECLARED` is permanent. A thread's settings never change, so a client
that meets it should stop reconnecting to that thread. The Rust SDK reads it as
`Error::is_relay_not_declared`.

### One client per thread

Attaching replaces whichever client was attached. The replaced socket is closed
with code `4000` and the reason `RELAY_CLIENT_REPLACED`, from the range RFC 6455
leaves to applications, because no registered code means "another client took
your place". The SDK surfaces that as an error whose code is
`RELAY_CLIENT_REPLACED`.

Replacement rather than refusal, because the second client is almost always the
first one reconnecting after a network blip, and refusing it would leave the
thread without a client until the dead socket timed out. Two clients answering
at once would race to answer every call.

### Live only

Nothing is persisted and nothing is replayed. A tool call means something only
while an agent is waiting on it: a call replayed to a client that attached an
hour later would be work done for nobody. So a call made while no client is
attached fails at once, and the agent reads a tool error saying the host
application is not connected. Waiting for a client instead would spend the
turn's wall clock on something that may never come.

`tools/list` never touches the relay, so a harness that starts while the host
application is away still sees the tools, and learns the host is away only if
it calls one.

### How a call ends

| Ending | The agent reads | The client receives |
|---|---|---|
| the client answers | the `ToolResult`: its text content and `is_error` | |
| no client attached | a tool error: not connected, nothing was done | nothing |
| the client detaches or is replaced first | a tool error: disconnected, outcome unknown | the socket closes |
| 15 minutes pass | a tool error: timed out, outcome unknown | `ToolCallCancelled` |
| the agent stops waiting: its turn ended or its harness hung up | nothing, it is gone | `ToolCallCancelled` |

Every failure reaches the agent as a tool error, `isError: true` with a sentence
of text, rather than as a JSON-RPC error. A tool error goes to the model, which
decides what to do next; a protocol error tells the harness the server is broken.

A result naming a call nothing is waiting on, whether late, duplicated, or
invented, is dropped with a debug log. Closing the socket over it would fail
every other call the client is in the middle of. A frame that does not decode
is skipped with a warning for the same reason.

**The deadline is 15 minutes**, `relay::CALL_DEADLINE`. One call can carry a
whole file transfer, since an upload tool answers only once the object is
stored, and a multi-gigabyte object takes minutes. The bound exists so a host
that accepted a call and stopped answering cannot hold an agent forever. The
`ToolCall.deadline` a client receives is that instant on the wall clock.

While an agent waits on a relayed call, its harness is silent, and the idle
bound does not treat that as a hang. See
[the timeouts doc](./timeouts.md#a-relayed-call-is-not-silence).

## The agent-facing MCP server

Each relayed server is served over streamable HTTP at `{grant}/mcp/{server}`,
where `{grant}` is the turn's proxy base URL, `http://127.0.0.1:<port>/t/{token}`.
It is the address the harness already reaches its model through, so no new
credential, host, or port is involved: the agent reaches loopback directly,
exempt from the egress proxy exactly as its model traffic is.

| Method | Answered by |
|---|---|
| `initialize` | the satellite: the tools capability, `serverInfo`, and the server's `instructions` |
| notifications, such as `notifications/initialized` | the satellite: `202 Accepted`, no body |
| `ping` | the satellite |
| `tools/list` | the satellite, from settings |
| `tools/call` | the host application, over the relay |
| anything else | JSON-RPC `-32601`, method not found |

A call naming a tool the server does not declare answers `-32602`. `GET`, which
opens a stream for server-initiated messages, answers `405` with `Allow: POST`,
which is what the transport specifies for a server that sends nothing
unprompted. A batch is answered as a batch.

**Responses are plain `application/json`, with no SSE and no session id.** The
server holds no state between requests, so neither buys anything.

**Protocol versions** `2025-06-18`, `2025-03-26`, and `2024-11-05` are echoed
when asked for; anything else is offered `2025-06-18`, as the specification's
negotiation describes.

### What the pinned CLIs were measured doing

Against a stub, then end to end through a real satellite with a stub model
endpoint asking for the tool and the Rust SDK answering it on the relay:

| | Claude 2.1.235 | Codex 0.147.0 |
|---|---|---|
| first request | `server/discover` at protocol `2026-07-28`; accepts method not found and falls back | `initialize` |
| `initialize` asks for | `2025-11-25`, and accepts `2025-06-18` in its place | `2025-06-18` |
| `GET` for a stream | yes, once; accepts `405` | no |
| plain JSON responses | accepted | accepted |
| session id | none sent, none needed | none sent, none needed |
| the tool reaches the model as | `mcp__<server>__<tool>`, instructions in the system prompt | a `mcp__<server>` namespace whose description is the instructions |
| `tools/call` round trip | verified | verified, with `model = "gpt-5"`; the default model calls tools through code mode, which a stub cannot drive |
| tool call timeout | about 27 hours by default; nothing set | 60 seconds by its documentation; `tool_timeout_sec` set to 960 |

Measured with a delayed answer: Codex honouring `tool_timeout_sec` by returning
a 75 second answer. The 60 second default is Codex's documented value and was
not measured here. See the roadmap for what else is not measured.

### Authorization

**The token in the path is the whole authorization.** Model traffic must also
present the token as its API key, and neither CLI sends one to an MCP server.
The URL carrying the token reaches the harness by the same route the model base
URL does, so a second carrier would be a header to configure that adds nothing
a process holding the URL lacks. No budget is checked: a tool call spends no
tokens.

An unknown or revoked token, and a server name the grant does not carry, both
answer `404`. Model traffic gets a `401` for a bad token, but under the MCP
authorization spec a `401` from an MCP server tells the client to start OAuth
discovery, a detour to nowhere. A `404`
still says nothing about whether a token was ever real.

**The token is visible to the agent**, as it already was: it is in
`ANTHROPIC_BASE_URL`, in Codex's command line, and now in Claude's
`--mcp-config`. It authorizes only what the turn's agents may already do, and it
is revoked when the turn ends.

### How the launch carries them

`harness::mcp::Launch` renders relayed servers beside the declared ones, so both
harnesses get them through the same flags. See
[the harness doc](./harness.md#mcp-servers-reach-the-harness-as-launch-arguments).

## Workspace files

`GET /v1/threads/{id}/files/{path}` streams a file out, with `Content-Length`
and a media type guessed from the extension. `PUT` streams one in and answers
`WorkspaceFileWritten` as protobuf: the path, the size, the SHA-256 in hex, and
whether the write created the file. `201 Created` when it did, `200 OK` when it
replaced one.

`path` is relative to the thread's workspace root, `/workspace/<thread-id>`.

### Every path is hostile

The workspace belongs to the agent, which can create any file, directory, or
link in it at any moment, including between two system calls the satellite
makes. A path resolved, checked, and then opened by name is a race the agent can
win, and the satellite runs as root.

So nothing opens a path by name. The thread directory is opened once, and each
component below it is opened relative to the directory handle above it with
`O_NOFOLLOW`. A link anywhere on the way is refused whatever it points at, and a
component swapped after it was opened changes nothing a handle refers to.

One module does this, `workspace::confined`, and everything that reaches into a
tree an agent owns goes through it: these routes, the listings below, the
artifact scan, turn attachments, and harness session export and import. A rule
about links then holds everywhere or nowhere.

| Refused | Answer |
|---|---|
| empty, absolute, an empty, `.`, or `..` component, or a NUL | `400 WORKSPACE_PATH_INVALID` |
| a symbolic link anywhere, including the file itself | `400 WORKSPACE_PATH_INVALID` |
| nothing there, on a read | `404 WORKSPACE_FILE_NOT_FOUND` |
| a directory, FIFO, socket, or device at the path or in its way | `409 WORKSPACE_FILE_NOT_REGULAR` |
| a write with no `Content-Length` | `411 WORKSPACE_FILE_LENGTH_REQUIRED` |
| a write over 5 GiB | `413 WORKSPACE_FILE_TOO_LARGE` |
| a body that carries a different number of bytes than it declared | `400 REQUEST_BODY_MALFORMED` |

A read opens the file with `O_NONBLOCK`, because a FIFO where a file was expected
would otherwise block the open forever waiting for a writer. The type is checked
on the open handle. The response streams exactly the size measured then, so a
file the agent grows mid-download sends what was declared.

The Rust SDK refuses a path with an empty, `.`, or `..` component before sending
it, with the same `WORKSPACE_PATH_INVALID`. It has to: a URL library resolves a
dot segment, even a percent-encoded one, so `a/../secret` would arrive as
`secret`. The satellite refuses them regardless, which a test proves with a
hand-written request.

### A write lands whole or not at all

The body streams into `.arsox-upload-<uuid>` beside the destination, created
with `O_EXCL`. Once every declared byte has arrived it is synced and renamed over
the destination, so a reader never sees half a file and a failed transfer leaves
the destination untouched. A staged file that never reaches the rename is
removed. Missing directories on the way are created.

**Everything a write creates belongs to the agent account**, changed on the open
handle rather than by path, because a path in the agent's workspace can be
swapped for a link between creating an inode and changing its owner. Directories
are `0755` and files `0644`. On a satellite that is not root there is no agent
account and ownership stays with the satellite, as it does for every other
workspace write.

**`Content-Length` is required** so the 5 GiB ceiling is checked before a byte is
written, and the bytes are counted as they arrive against it. Five GiB is the
largest object one request uploads to the object stores a host application moves
these files onward to, so a larger file could not travel on whole anyway.

On a platform without a descriptor-relative walk, which is anything but Unix,
every one of these routes answers `INTERNAL`.

### Listing what is there

`GET /v1/threads/{id}/files` answers `ListWorkspaceFilesResponse` and
`GET /v1/threads/{id}/artifacts` answers `ListArtifactsResponse`. Both carry
their request as a protobuf body on the `GET`, as every listing in the contract
does, and both page with `common.PageRequest`: 50 by default and at most 500.

**Order is by path, compared one component at a time.** That is the order a
depth-first walk visits files in when each directory's names are sorted, so
`a/b` comes before `a-c` even though a string comparison says otherwise. It is
what lets a page start after its cursor without walking everything before it:
a directory that sorts before the cursor and does not lead to it is skipped
whole. The cursor is the last path of the previous page.

Only regular files are listed. A symbolic link is neither followed nor
reported, and neither is a FIFO, a socket, a device, a staged write, or a name
that is not UTF-8, which the contract cannot spell.

| Listing | `path_prefix` | A path is relative to | Carries |
|---|---|---|---|
| files | a directory, checked like a file route's path; one trailing `/` is forgiven | the workspace root | size, modification time, whether it is under artifacts/ |
| artifacts | none: always artifacts/ | artifacts/ | size, SHA-256, content type, modification time |

A prefix naming a directory that does not exist lists nothing, and one naming a
file answers `409 WORKSPACE_FILE_NOT_REGULAR`. A cursor that is not a relative
path answers `400 REQUEST_FIELD_INVALID`.

The artifacts listing hashes every file it returns. It reuses a hash the last
artifact scan recorded when the file's inode, size, and change time still match
it, and hashes the rest on the spot, and it never writes the record: reading
what a thread holds must not change what its next scan announces. See
[the workspace doc](./workspace.md#artifacts).

## Roadmap

- **Content other than text.** `ToolContent` is a oneof with one arm today;
  images and embedded resources are additive.
- **Node and Python clients** for the relay and the file transfer routes. The
  contract is already in both packages, and both list files and artifacts.
- **Measuring Claude under the narrow posture** with a relayed server, where the
  `mcp__<name>` allow rule is what lets the call through. The rule is rendered
  the same way for both kinds of server and unit tested, and the declared-server
  measurement covers the flag itself.
