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
| `codex/0.147.0/auth-failure` | Recorded from Codex 0.147.0 in full, four lines, scrubbed of the thread id and the transport detail in the message. |
| `codex/0.147.0/tool-call` | Recorded from Codex 0.147.0 in full, nine lines, live run, unscrubbed. See below. |

`claude/2.1.221` is named for the `claude_code_version` the transcript itself
reports. `docs/core-crates.md` records a measurement against 2.1.226, a later
release of the same CLI; a directory is named for the version that produced the
bytes in it, so this one follows the recording rather than the prose.

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
