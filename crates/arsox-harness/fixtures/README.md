# Conformance fixtures

One directory per harness, per pinned CLI version, because an output shape
belongs to a version and to nothing else.

```
<harness>/<cli-version>/<scenario>.stdout.jsonl   the native transcript
<harness>/<cli-version>/<scenario>.events.json    the canonical output it must produce
```

The suite maps every line of the transcript and asserts the whole canonical
document against `.events.json`. Asserting field by field is how a mapper starts
dropping a field nobody wrote an assertion for, so nothing here is asserted in
prose. A version bump becomes: record new fixtures, and let the suite say
exactly what changed.

`.events.json` is generated, not written. Regenerate one by replacing its
contents with `{}`, running the suite, and copying the `actual:` document the
failure prints. Review the diff: that document is the contract. Leave `{}` there
rather than emptying the file, because an empty file fails to parse and the
suite reports that instead of printing the document you came for.

## Provenance

Every fixture says here whether it was recorded or constructed, because a
transcript nobody recorded is a guess wearing evidence's clothes.

| Fixture | Provenance |
|---|---|
| `claude/2.1.221/tool-call` | Recorded from the Claude CLI, scrubbed of the capturing machine's paths and identifiers. |
| `claude/2.1.237/plain-text` | Recorded live from Claude CLI 2.1.237, four lines, `system` init line scrubbed. See below. |
| `claude/2.1.237/tool-call` | Recorded live from Claude CLI 2.1.237, six lines, `system` init line scrubbed. |
| `claude/2.1.237/error-result` | Recorded live from Claude CLI 2.1.237, one line, unscrubbed. |
| `claude/2.1.237/multi-message` | Recorded live from Claude CLI 2.1.237, ten lines, `system` init line scrubbed. |
| `codex/0.147.0/auth-failure` | Recorded from Codex 0.147.0 in full, four lines, scrubbed of the thread id and the transport detail in the message. |
| `codex/0.147.0/tool-call` | Recorded from Codex 0.147.0 in full, nine lines, live run, unscrubbed. See below. |

`claude/2.1.221` is named for the `claude_code_version` the transcript itself
reports. `docs/core-crates.md` records a measurement against 2.1.226, a later
release of the same CLI; a directory is named for the version that produced the
bytes in it, so this one follows the recording rather than the prose.

**Both Claude versions stay.** 2.1.237 proves the CLI shipping today and 2.1.221
proves the one before it, and keeping both is what makes a shape that shifts on
upgrade a diff between two directories rather than an edit to one. The
`reasoning_output_tokens` difference below is the first thing that pair caught.

### The recorded Claude 2.1.237 fixtures

Four scenarios, recorded in one sitting against the CLI on the capturing
machine, each with a prompt small enough that the whole set cost pennies:

- **`plain-text`** is prose and nothing else, the simplest turn there is and the
  one a mapper built around tool calls is most likely to fumble.
- **`tool-call`** is a shell command, its result, and the answer after it. Same
  scenario as the 2.1.221 recording, on the newer CLI.
- **`error-result`** is a run the CLI refused to start, resumed against a session
  id that does not exist. It is one line: `type: result`, `subtype:
  error_during_execution`, `is_error: true`, and an `errors` array carrying why.
  The turn has no session, no message, and nothing the agent did. Deterministic
  and free, which is what makes it a fixture rather than a lucky capture.
- **`multi-message`** is a turn that reasoned, spoke, used a tool, and spoke
  again: six canonical events out of ten native lines.

Three things about 2.1.237 are easier to get wrong from the older recording than
from these bytes:

- **`usage.output_tokens_details.thinking_tokens` exists**, and 2.1.221 has no
  such field. This CLI does separate reasoning from output, so
  `reasoning_output_tokens` is a measurement rather than the absence the older
  recording made it look like.
- **A failed run carries `errors` and no `result`.** Reading only `result`
  reports a failed turn with an empty summary, which is the turn's own
  explanation of itself lost.
- **`system` arrives with more than one subtype.** Alongside `init` the stream
  carries `subtype: thinking_tokens` progress lines, each repeating the session
  id the init line already announced.

A recorded thinking block carries a `signature` and an **empty** `thinking`
string, because this model returns its reasoning encrypted. The canonical event
is emitted anyway: "the agent reasoned here" is true, and dropping it would make
the turn look like it acted without thinking.

#### What the scrub touched

Only the `system` init line, and only the parts of it that describe the person
capturing rather than the harness. Its `tools`, `mcp_servers`, `slash_commands`,
`skills`, `agents`, `plugins`, and `memory_paths` are that machine's install, and
`plugins` and `memory_paths` carry home directory paths outright. Each is
replaced with what a satellite would report: the core tool set, the built-in
slash commands, and nothing else.

Everything else is the bytes the CLI wrote, `error-result` included, which is
unscrubbed in full. The session ids, the scratch `cwd`, the message ids, and the
thinking signature are all about the run rather than about a person.

### The recorded Codex fixture

`codex/0.147.0/tool-call.stdout.jsonl` is nine lines of stdout from a live
`codex exec --json` run, driven by the 0.147.0 binary against a throwaway git
repo holding one file. The prompt asked it to write `hello.txt` and print the
file back, so the transcript carries a patch, a shell command, and the two agent
messages that bracket them.

Nothing in it is scrubbed. The scratch path and the thread id are the bytes the
CLI wrote, and neither names the capturing machine's user.

Four things about the stream are easier to get wrong from the published schema
than from these bytes, so they are worth stating before anyone reads the next
recording:

- **`item.started` announces work, not prose.** A patch and a command each open
  with one; an agent message arrives only as an `item.completed`.
- **`item.updated` never appears at all.** The exec stream sends starts and
  completions and nothing between them.
- **A command's `item.started` carries `"exit_code": null`** rather than
  omitting the key until the command has exited.
- **A turn emits several `agent_message` items**, a preamble and then the
  answer, which is what makes "the last one is the summary" load-bearing rather
  than incidental.

Two shapes the mapper handles are absent from this recording, and each is
asserted instead as a unit test in `codex.rs` that says which of the two it is:
a `reasoning` item, because the turn reasoned nothing, and an `item.updated`. A
fixture is not staged into producing a shape a real run did not produce.

Two usage fields are worth naming, because they are zero for opposite reasons.
The run reported `cache_write_input_tokens: 0`, and the mapper maps that zero to
an absent `cache_write_tokens`, for the reason `usage.proto` gives: claiming
"this run wrote nothing to cache" is a different and false statement from "this
harness has no cache-write concept". A count above zero is carried through. It
also reported `reasoning_output_tokens: 0`, and that zero is kept, because there
the concept exists and nothing is what the turn measured.

## Recording a new one

Claude:

```bash
claude -p "<prompt>" --output-format stream-json --verbose --allowedTools "Bash" < /dev/null
```

`< /dev/null` matters. The CLI waits on stdin for a few seconds otherwise, which
looks like a hang.

Name the directory for the `claude_code_version` the init line reports, and
**add** a directory rather than overwriting one. A newer CLI proves the newer
CLI; the recording already there goes on proving the version it came from.

A turn that ends in a failure needs no error to be arranged. Resuming a session
id that does not exist produces one, deterministically and without a model call:

```bash
claude -p "hi" --resume 00000000-0000-4000-8000-0000000000ff \
  --output-format stream-json --verbose < /dev/null
```

Codex:

```bash
npm install --prefix <scratch> @openai/codex@<version>
cd <scratch-repo>
<scratch>/node_modules/.bin/codex exec --json --ignore-user-config --ignore-rules \
  -s danger-full-access -c approval_policy="never" "<prompt>" < /dev/null
```

Every flag there is load-bearing:

- `--json`. Without it `codex exec` writes a human report, and a mapper pointed
  at prose parses prose.
- The pinned binary in a scratch prefix. Whatever `codex` is on the capturing
  machine's `PATH` is some other version, and a fixture directory is named for
  the version that produced its bytes.
- `--ignore-user-config` and `--ignore-rules`. A developer's `config.toml` can
  turn on features that change which item shapes the stream carries, and the
  satellite spawns the CLI with its own configuration rather than theirs.
- A sandbox and an approval policy that permit whatever the prompt asks for.
  Under the defaults a write is refused, and the transcript records the refusal
  rather than the tool call you wanted. Point it at a throwaway directory.
- `< /dev/null`. Otherwise `exec` waits on stdin for a prompt it already has.

Scrub what is about the person capturing rather than about the harness:
credentials, home directory paths, and their installed tooling. A scratch path
and a run's own thread id are about the run, and a fixture is evidence before it
is tidy.
