# Blender

The Blender variant of the satellite image gives agents Blender and Blender Lab's
MCP integration, so a thread can model, render, and inspect `.blend` files
whenever its task calls for it. It is an image that extends the base one, not a
satellite feature: the satellite binary knows nothing about Blender, and a host
application opts a thread in by declaring one MCP server.

## Building it

```sh
docker build --tag arsox-satellite:<tag> .
docker build --file docker/blender/Dockerfile \
  --build-arg BASE_IMAGE=arsox-satellite:<tag> --tag arsox-satellite:<tag>-blender .
```

`BASE_IMAGE` has no default, so the variant always names the exact satellite it
wraps. Blender adds about two gigabytes, which is why it is a variant rather than
part of every image.

| File | What it does |
|---|---|
| `docker/blender/Dockerfile` | pins every version and checksum, runs the setup script, sets the entrypoint |
| `docker/blender/install.sh` | the setup script: Blender, the MCP server, and the extension, at build time |
| `docker/blender/configure_preferences.py` | saves the agent account's Blender preferences |
| `docker/blender/requirements.txt` | the MCP server's dependencies, hash-locked |
| `docker/blender/entrypoint.sh` | starts the Blender services, then becomes the satellite |

## What is installed

| Piece | Where | Pinned by |
|---|---|---|
| Blender | `/opt/blender`, linked as `blender` | release tarball version and SHA-256 |
| The MCP server (`blender-mcp`) | a venv at `/opt/blender-mcp` | repository commit, dependencies by hash |
| The bridge extension (`mcp`) | the agent account's Blender profile, enabled | release zip version and SHA-256 |

The MCP server is installed from Blender Lab's repository at an exact commit
because its release archives sit behind a bot check that refuses scripted
downloads. A commit hash names its content, so it is as strong a pin as a
checksum. The server builds without isolation, with its build backend in the same
hash-locked file, so pip never fetches anything the lock does not name.

The extension's preferences are saved explicitly: enabled, online access allowed
(the extension declares the network permission and will not listen without it),
and auto start on. Blender writes preferences only when saved, so a setting that
was changed and not saved would be gone at the next launch.

## What runs

The entrypoint starts two services before it `exec`s the satellite:

| Service | Listens on | What it is |
|---|---|---|
| the bridge | `127.0.0.1:9876` | Blender in background mode with the extension serving `--command blender_mcp` |
| the server | `127.0.0.1:9877` | the MCP server over streamable HTTP, at path `/` |

The server forwards live-session tools such as `execute_blender_code` to the
bridge, and runs each `*_for_cli` tool as its own `blender -b` on the file it is
given. Each service is restarted two seconds after it exits, so a crash does not
leave later threads without tools.

**Both run as the agent account.** The bridge executes whatever Python it is
sent. As root, that would be a way past every enforcement point in
[the enforcement doc](./enforcement.md); as the agent account, it is exactly the
reach the agent's own shell already has.

**Both start from an empty environment.** The entrypoint holds `ARSOX_SECRET` and
any provider credential the container was given, and an agent that can run
Python inside Blender can read Blender's environment. They get `HOME`, `PATH`,
`LANG`, and the server's own three variables, nothing else.

**The satellite is still PID 1, as root,** by `exec`, so its enforcement posture
and the image checks that assert it are unchanged.

## Declaring it on a thread

A host application gives a thread the tools by declaring one entry in
`settings.mcp_servers`:

| Field | Value |
|---|---|
| `name` | `blender` |
| `url` | `http://127.0.0.1:9877/` |

The tools reach the agent as `mcp__blender__<tool>`. The server is on loopback,
so `NO_PROXY` keeps its traffic off the egress proxy, and it needs no header.
A thread declared against a satellite that is not this variant gets a server
that fails to connect, and the turn carries on without Blender.

The two ports are a contract between the entrypoint and every host that declares
the server. Change them together.

## What it does not do

- **One Blender session is shared by every thread.** The live-session tools act
  on whatever scene the bridge holds, so two concurrent threads modelling at once
  see each other's objects. Agents that save their work to a `.blend` under their
  workspace and reopen it are unaffected, and the `*_for_cli` tools are per call.
- **There is no window.** The screenshot and navigation tools
  (`get_screenshot_of_*`, `jump_to_*`) need one and fail in background mode.
  Rendering works: `render_thumbnail_to_path` and scripted renders both run
  headless.
- **The bridge is an exec path the broker does not see.** A thread whose exec
  policy is `NONE` or `CUSTOM` can still run arbitrary Python through
  `execute_blender_code` when the host declared the server. Declare it only on
  threads that may run code.

## Verified

The Docker workflow builds the variant on the image it just built, boots it,
lists the server's tools, runs code in the live session through the bridge, and
asserts that the satellite is PID 1 as root while every Blender process runs as
the agent account. See [the CI doc](./ci.md#docker).
