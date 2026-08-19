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

`.events.json` is generated, not written. Regenerate one by deleting its
contents, running the suite, and copying the `actual:` document the failure
prints. Review the diff: that document is the contract.

## Provenance

Every fixture says here whether it was recorded or constructed, because a
transcript nobody recorded is a guess wearing evidence's clothes.

| Fixture | Provenance |
|---|---|
| `claude/2.1.221/tool-call` | Recorded from the Claude CLI, scrubbed of the capturing machine's paths and identifiers. |
| `codex/0.147.0/auth-failure` | Recorded from Codex 0.147.0 in full, four lines, scrubbed of the thread id and the transport detail in the message. |
| `codex/0.147.0/tool-call` | **Constructed**, see below. |

`claude/2.1.221` is named for the `claude_code_version` the transcript itself
reports. `docs/core-crates.md` records a measurement against 2.1.226, a later
release of the same CLI; a directory is named for the version that produced the
bytes in it, so this one follows the recording rather than the prose.

### The constructed Codex fixture

`codex/0.147.0/tool-call.stdout.jsonl` was not captured from a working run: no
Codex credential was available on the machine that wrote it, and the run it
attempted is what `auth-failure` records. It is built instead from two things,
both pinned to 0.147.0:

- its lifecycle lines are byte-identical to what the 0.147.0 binary actually
  emitted, which is what `auth-failure` proves,
- its item and usage lines follow the event schema published in
  `@openai/codex-sdk@0.147.0`, which is the same version's own declaration of
  what `codex exec --json` writes.

It is a faithful reading of a published schema rather than a recording, and it
is structured so a recording replaces it without touching the mapper: record a
run, drop the JSONL in, regenerate `.events.json`, and read the diff.

One field is worth naming. The published `Usage` type declares
`cache_write_input_tokens`, and the fixture carries it as the zero a provider
with no cache-write concept reports. The mapper deliberately maps that zero to
an absent `cache_write_tokens`, for the reason `usage.proto` gives: claiming
"this run wrote nothing to cache" is a different and false statement from "this
harness has no cache-write concept". A count above zero is carried through.

## Recording a new one

Claude:

```bash
claude -p "<prompt>" --output-format stream-json --verbose --allowedTools "Bash" < /dev/null
```

`< /dev/null` matters. The CLI waits on stdin for a few seconds otherwise, which
looks like a hang.

Codex:

```bash
codex exec --json --skip-git-repo-check --sandbox read-only -C <scratch-dir> "<prompt>"
```

`--json` matters. Without it `codex exec` writes a human report, and a mapper
pointed at prose parses prose.

Scrub the capturing machine's paths, session identifiers, installed tooling, and
anything else that is about the machine rather than about the harness.
