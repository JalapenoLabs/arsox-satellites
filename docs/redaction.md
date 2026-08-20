# Secret redaction

A thread runs with credentials in its environment, and everything it produces is
text: an agent's message, a checker's output, an incident's evidence, a turn's
summary. Somewhere in that text a credential will appear, and it will appear on
the day nobody is watching.

So masking is not a rule applied where somebody remembered it. It is one engine,
in `src/redaction.rs`, applied at the doors text goes out through.

## What counts as a secret

`Redactor::for_thread` reads them from the thread's settings:

- Every declared `EnvVar` whose `is_secret` resolves true. **Absent means
  secret**, which is the same rule `spawn::declared_environment` applies, for the
  same reason: the cost of needlessly masking a public value is a confusing log
  line, and the cost of the reverse is a leaked credential.
- Every credential the caller handed the satellite: each repo's personal access
  token or SSH private key, the agents repo's, the GitHub and Jira tokens, and
  every LLM endpoint's key, access token, and refresh token.

**The second group is not optional.** None of it came from an `EnvVar` and none
of it carries an `is_secret` flag, so a thread has no way to ask for its own
deploy key to be printed. A public SSH key is not a credential and is left alone.

### A very short value is not indexed

A declared variable inherits `is_secret` when the caller says nothing, so a
thread that sets `DEBUG=1` has declared a one character secret. Indexing it would
mask every `1` in every event, every path, and every exit code, which destroys
the stream the masking exists to protect while hiding nothing anybody would call
a credential.

Values shorter than four characters are therefore not indexed, and a declared one
gets a `redaction.secret.too_short` warning naming the key and never the value.

## The modes, and the two rules above them

`mask` implements the contract's four reveal modes with the contract's own
formulas. For a secret of length `L`, revealing `N`:

| Mode | Rule | `sk_ant_12345` (L=12) |
|---|---|---|
| `ANONYMOUS` | reveal nothing | `******` |
| `PREFIX_SHOWN` | first `N`, `N = floor(min(8, 0.20 × L))` | `sk******` |
| `POSTFIX_SHOWN` | last `N`, same `N` | `******45` |
| `HYBRID_SHOWN` | first and last `N`, `N = max(1, floor(min(5, 0.10 × L)))` | `s******5` |

`REDACTION_MODE_UNSPECIFIED` reads as anonymous. A caller who set no mode asked
for redaction and said nothing about how much of it, and reading proto3's zero
value as a reveal would turn silence into a disclosure.

Two rules outrank every mode, and they live inside `reveal` rather than at its
call sites so no mode added later can escape them:

- **A secret shorter than 8 characters is always anonymous.** There is no safe
  prefix of a short secret.
- **A reveal that would expose more than half falls back to anonymous.** No mode
  in the contract can reach this today, because the widest of them reveals a
  fifth. It is checked because it is the rule that has to hold when a fifth mode
  is written by somebody who did not read this far.

### Star count

Absent gives six stars, fixed rather than mirrored, because mirroring leaks the
length and a length is real information about a credential. A positive integer is
honoured exactly, `0` clamps to `1` because a mask with no stars is invisible,
and `MirrorLength` gives one star per hidden character.

A count above 256 is clamped. The contract accepts any positive integer and a
mask is read by a human, so a count past that is a mistake or an attack, and
either way it must not become a multi-megabyte allocation per masked occurrence
on a stream that never stops.

## One pass, however many secrets

A thread may declare many secrets and a busy thread emits events continuously, so
the scan is a single Aho-Corasick pass rather than a `String::replace` per secret
per event. Overlapping secrets resolve longest-first, so a token that contains a
shorter one masks as the token rather than being chopped in half by its own
substring and leaving its tail in the clear.

**A thread with no secrets costs one branch.** The automaton is absent rather
than empty, and every entry point returns before it looks at the text. Text that
contains no secret is borrowed rather than copied, which is the overwhelmingly
common case on a stream.

The redactor is compiled **once per thread**: at `claim_next_turn`, where the
settings are already decoded, and at the start of provisioning. It then travels
with the work, on `ClaimedTurn`, on `TurnContext`, and on `Execution`.

## Where it is applied

Four doors, chosen because each is the only way its kind of text gets out.

| Door | What it masks |
|---|---|
| `Store::append_event` | every stream event and its payload, before the encode |
| `Store::record_incident` | an incident's message and its evidence |
| `commands::execute` | a setup or checker command's captured output, and the command text itself |
| `api::scrubbed` | every credential in the settings a `Thread` response carries |

**Masking at the door rather than above it** is what makes this a property rather
than a convention. `append_event` masks before the encode, so the row on disk and
the frame on the wire are the same bytes and a reconnecting consumer cannot be
told a different story than one that stayed connected. A new place that appends
an event has to name a redactor, because `AppendEvent.redactor` is a required
field rather than an optional one.

An incident passes two of those doors rather than one, because `report_incident`
writes the row and emits the frame in a single call. Both are masked by the same
automaton, so neither can carry a credential the other hid. See
[incidents](./incidents.md).

`commands::execute` masks at the moment of capture, so the incident, the checker
result, the turn report, and the prompt that hands a failure back to the agent
all carry the same masked text from one scan instead of four places each having
to remember. The command text is masked alongside its output, because a checker
spelled `deploy --token ghp_...` puts the credential in the command rather than
in what it printed.

The turn result is masked once in the runner, ahead of both `finish_turn` and the
`turn.completed` event, so a summary quoting a credential cannot survive into one
of them because a consumer was wired later.

The payload visitor is an exhaustive match on `thread_event::Payload`. A payload
added to the contract stops compiling until somebody says what its secrets are,
which is the only way a contract that grows every release keeps this honest.
Identifiers the satellite or the harness generates are deliberately left alone: a
UUID and a tool call id cannot carry a credential, and scanning them buys nothing.

### Settings on their way back out

The first three doors mask text. The fourth masks a typed field, and it is a
different problem: `Secret.value` is the plaintext a caller sent up, and the
contract says the satellite never populates it on a response, at any endpoint,
at any authentication level.

`redaction::scrub_settings` moves each plaintext into `display` as the mask that
stands in for it and clears `value`, under the thread's own mode. A value the
caller marked public makes the same move and its rendering is the plaintext, so
one rule covers the whole surface: `value` goes up, `display` comes back.

**It runs at the response boundary, not where settings are decoded.** The stored
settings are read for two purposes. One is a response. The other is work:
`Provisioner::resume_interrupted` re-reads them to re-clone private repos after a
restart, and `spawn` reads them to build an agent's environment. Masking at
hydration would serve the first and silently break the second, leaving a restart
cloning with `******` for a token. So `api::scrubbed` masks the copy already on
its way to a client, and every handler returning a whole `Thread` goes through
it: create, get, pause, resume, destroy.

`ListThreads` is not among them and needs no masking. It returns `ThreadSummary`,
which carries no settings at all, which is the stronger answer: an operational
listing has no configuration in it to leak.

Two mechanisms keep the walk honest as the contract grows:

- **Every message on it is destructured by name**, so a field added to one of
  them stops compiling until somebody says what it is. `clippy::unneeded_field_pattern`
  is expected away there with that reason, because `..` is exactly what would let
  a new credential through.
- **A guard test reads the proto sources** for every message that declares a
  `Secret` at all, so one added to a message the walk never reaches fails a test
  rather than leaving a plaintext on a response.

### Two maskings that are not the engine, and stay

`AgentVar` writes its own `Debug`, so a declared credential and the turn's proxy
token are masked in any log line that formats a harness command. That is a
property of the type rather than of a scan, and a type that cannot print its own
secret is stronger than one that is scanned afterwards.

`Credentials::redact` in `workspace/repos.rs` masks the token a clone was lent,
in git's own output. The thread's redactor already knows that token, so it finds
nothing in the ordinary case. It stays because it is the guarantee that belongs
to the code that staged the credential: a caller passing a redactor built from
different settings still cannot print the token this clone was handed. Its mask
comes from `redaction::mask`, so one implementation decides what a masked secret
looks like.

`redact_url` beside it is a different problem and not this engine's: it masks a
credential somebody wrote into a repo URL themselves, which the thread never
declared and the redactor has therefore never heard of.

## What is not built yet

The README lists the full set of channels redaction covers. These are the ones
that exist:

- stream events and their payloads
- turn results, summaries, checker output, and command logs
- incident messages and their evidence
- captured setup and checker output
- credentials in the settings every thread response carries

These do not exist yet, because the features they belong to do not:

- **File contents in commits, and commit messages.** The root-owned `pre-push`
  hook is the hard gate here and is separate work. Until it lands, nothing stops
  an agent committing a credential.
- **PR titles, bodies, review comments, and Jira comments.** There is no `gh` or
  `jira` broker yet.
- **Artifact contents at upload time**, and **suggestion bodies**. Neither stage
  exists.
- **Team chat and direct messages.** Team mode is unbuilt. The payloads are
  already handled by the visitor, so they are masked the moment something emits
  one.

The **`override_redaction` MCP tool** and its `allowRedactionOverride` kill switch
are also unbuilt. `Redaction.allow_redaction_override` is carried in settings and
read by nothing, which means redaction is currently absolute for everything it
covers: there is no way for an agent to move it, deliberately or otherwise.

## Roadmap

- The `pre-push` hook, which refuses a push carrying an unredacted secret. See
  the [workspace doc](./workspace.md) for where credentials live today.
- The `override_redaction` MCP tool, scoped to one secret and one operation,
  emitting a high-priority stream event and appearing in the turn report with the
  justification the agent gave. `allowRedactionOverride: false` unregisters it
  entirely rather than gating its behaviour.
- Masking on the way into a commit, a pull request, and an artifact, alongside
  the features that produce them.
