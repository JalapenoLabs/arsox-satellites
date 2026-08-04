# CLI notes

Design notes for the `arsox` CLI, which will be built in
[`JalapenoLabs/arsox-cli`](https://github.com/JalapenoLabs/arsox-cli) and is not built here.

**This file is a handoff, not a spec.** It captures decisions made while designing the satellite so
they are not lost between now and starting the CLI. When that repo exists, move this file into it
and delete it from here. The CLI's source of truth belongs next to the CLI.

The one thing that stays in this repo's README is the [What Arsox ships](../README.md) table, which
records that the CLI exists and where it is built.

## Shape

`arsox` is an interactive terminal, not a subcommand tool. You launch it with a bare `arsox` and
then you talk to it. If you have used the Claude Code CLI you already know the shape: a prompt, a
scrollback, slash commands for control. The difference is that nothing runs on your machine.

**Typing a message is how you start work.** There is no create-thread step to remember. Your first
message opens a thread on a satellite and becomes its first turn, and the conversation grows from
there.

## Slash commands

| Command | Does |
|---|---|
| `/new` | Start a fresh thread; your next message opens it |
| `/clear` | Clear the screen and start over on a new thread |
| `/resume` | Pick an existing thread and rejoin it |
| `/satellite` | Switch which satellite you are talking to |
| `/settings` | View and edit this thread's settings |
| `/status` | Satellite state and queue depth |
| `/incidents` | What went wrong, by disposition |
| `/artifacts` | List and download |
| `/stats` | Lifetime statistics |
| `/quit` | Leave. The thread keeps running. |

Settings, threads, and satellites are all managed in the terminal rather than through flags.
Configuration persists to `~/.arsox`, written automatically on first run, relocatable with
`ARSOX_HOME` or `--config`.

## `/resume` is not replaying a transcript

This is where a remote client stops resembling a local one, and it is the strongest single line in
the pitch.

Claude Code resumes a session that ended. **Arsox rejoins work that may still be running.** Close
your laptop mid-turn, open it tomorrow, `/resume`, and the team is either still going or finished
hours ago without you. The thread lives on the satellite; the terminal is a window onto it, and
closing the window does not stop the work.

The same property means **two people can attach to the same thread and watch it together**, since
the socket already accepts many consumers. One engineer starts an overnight run, another picks it
up in the morning, and both see the same stream.

Neither of these is a feature to build. Both fall out of the satellite architecture already
described in the README.

## Why a terminal earns its keep

Human in the loop is the feature that most needs a real interface. The commander can ask up to five
questions at once, each with up to five options and sub-descriptions, and the whole set must be
answered together. In an SDK that is a callback you have to build a UI for. In a terminal it is a
picker, which is the natural home for it.

## Headless

`arsox -p "<prompt>"` runs one turn without the terminal, for CI and scripts. Same binary, same
config, exit code reflects the turn.

It is the exception rather than the surface. An application that wants programmatic control should
use an SDK rather than shelling out.

## Compatibility

Same rule as the SDKs. Refuse a satellite serving a higher proto major with a clear message rather
than a decode error, and warn once on a higher minor.

## Keep it off the satellite

The CLI is not installed in the satellite images, and should not be added to one.

A client on an agent's `PATH` plus a reachable `ARSOX_SECRET` is a privilege escalation: an agent
could destroy threads, read other threads' artifacts and incidents, or rewrite its own permissions.
The secret is already withheld from agent environments, and the exec allowlist would block an
unknown binary, but the cleanest defense is that the binary is not there at all. `arsox` is an
operator tool, and the operator is outside the satellite.

## Do not fork Claude Code

Checked on 2026-08-04. `anthropics/claude-code` is the issue tracker, plugins directory, and docs
hub. The application ships as a compiled binary through Homebrew, an install script, or a
deprecated npm package. Its `LICENSE.md` sits alongside references to Anthropic's
[Commercial Terms of Service](https://www.anthropic.com/legal/commercial-terms), not an OSS
license. There is no source to fork.

The architectural reason matters more than the legal one. Claude Code is large because it **runs
the agent**: the agent loop, tool execution, the permission system, context management, sub-agents,
MCP hosting, harness lifecycle. Arsox deliberately puts all of that on the satellite. Forking would
import every subsystem the architecture exists to move remotely, and the work would be deleting
them and then chasing upstream changes to code being removed.

The Claude Agent SDK is the sanctioned way to build on Claude Code, but it is for building agents,
and this client does not run one. Also the wrong tool.

**Build it fresh in Rust with `ratatui`, on the Arsox Rust SDK.** The client surface is genuinely
small: text input, scrollback rendering, slash commands, and a WebSocket consumer receiving events
that are already normalized.

## Why a separate repo

A separate repo can only reach the public API, so anything the CLI needs and cannot get is a hole
in the SDK rather than something a sibling crate quietly reaches around. A workspace member could
depend on internals, `pub(crate)` escape hatches, and unpublished helpers, and nobody would notice
the published crate was insufficient.

That makes the CLI the Rust SDK's first real consumer and the first honest test of whether the
published surface is enough to build something with.

## Open questions

**How does a team-mode thread render?** This is the hardest one. With team mode on, a commander and
up to eight members all emit into one event stream. A single scrollback would be unreadable. Some
options: default to commander and team chat only with members behind a toggle, per-member panes or
tabs, or a collapsible tree keyed on `memberId`. Worth prototyping against a real transcript before
committing, because it shapes the whole renderer.

**Can `/settings` edit a running thread?** The satellite currently has no documented endpoint for
updating thread settings after creation. Either the CLI restricts `/settings` to viewing plus
authoring the next thread, or the satellite grows a scoped update endpoint. Decide before building
the editor.

**One thread per terminal, or many?** Threads run concurrently on a satellite and the socket
supports many consumers. Whether one `arsox` session can hold several threads at once, tabbed, is
unresolved.

**How does a secret get in the first time?** `/satellite` needs an add flow. `ARSOX_SECRET` covers
scripted use, but the first interactive run needs somewhere to put a URL and a token, and it should
not be a flag that lands in shell history.
