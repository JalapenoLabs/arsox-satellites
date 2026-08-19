# Harnesses

A harness is the CLI that drives an agent. Each speaks its own event vocabulary,
and a mapper turns that vocabulary into the one canonical contract, so an
application is written once and never rewritten when the harness changes.

Two mappers exist: Claude and Codex. A satellite spawns Claude only, so the
Codex mapper is knowledge the runner cannot reach yet, and the spawn path is
what remains.

## Lenient in, strict out

Mappers read native events as loose JSON rather than into strict structs.

That is deliberate, and it is not a lowering of standards. A harness adds fields
on its own schedule, and a strict deserializer turns every such addition into a
hard failure in a satellite that worked yesterday. What must be rigid is what
*leaves* the mapper, and that is a generated protobuf type the compiler checks.

The asymmetry is the point: be permissive about what you accept from a
dependency you do not control, and exact about what you promise to consumers.

## Nothing is dropped

A native event a mapper does not recognize becomes a `degraded` incident. So does
a line that is not valid JSON, which is what a harness writing a partial line as
it crashes produces.

Neither is an error, because a harness adding an event type must not take a
satellite down. Neither is a silent skip, because a mapping layer that quietly
discards what it does not understand looks correct right up until somebody
reconciles a bill against it.

## Where the Claude shape disagrees

Three mismatches in the Claude vocabulary are worth knowing, because each is a
place a naive mapper would produce something wrong rather than something missing.

**A tool result arrives as a `user` event.** The call goes out as an `assistant`
line carrying a `tool_use` block, and the outcome comes back as a **`user`** line
carrying a `tool_result` block. The harness models a tool's output as something
the user said. The contract models it as `tool.completed`, paired to its start by
`tool_call_id`. A mapper that trusted the `user` label would emit the tool output
as a human message.

**One native line becomes several canonical events.** A single `assistant` line
carries a `content` array, so prose and a tool call arrive together. The mapping
is one-to-many by nature, and forcing it to be one-to-one is how a harness's
envelope leaks into the contract.

**A sub-agent is identified by the tool call that spawned it.** There is no
member id in the native stream, so one is derived from `parent_tool_use_id`.
Deriving is what keeps that field out of the contract: a consumer sees a stable
member id and never learns which harness produced its stream.

## Where the Codex shape disagrees

Codex models a run as a thread holding turns, and a turn as a list of *items*.
Lifecycle events bracket the run and the items carry the work, which is a
different envelope from Claude's and disagrees with the contract in three of its
own places.

**`codex exec` emits JSONL only with `--json`.** Without the flag it writes a
human report, and a mapper pointed at that parses prose as protocol. The flag is
part of the spawn and the consequence belongs to the mapper.

**The usage event names no model.** `turn.completed` carries token counts and
nothing else. A mapper that required a model and a count on one object would
discard every count Codex reports, so usage is mapped without one and `by_model`
is left to the runner, which knows the endpoint that answered.

**`input_tokens` includes what came from cache**, with the cached count broken
out beside it. Anthropic excludes it and the canonical shape follows Anthropic,
so the cached count is subtracted. Copying the field across overstates a cached
run by most of its prompt, in a number nobody checks until a reconciliation.

Two smaller decisions are worth stating because both are deliberate:

- **`cache_write_tokens` is absent, not zero.** Codex reports a zero, and the
  provider behind it has no cache-write concept, so the zero is a structural
  placeholder rather than a measurement. `usage.proto` names this case. A count
  above zero is carried through, so a provider that grows the concept is already
  mapped.
- **A to-do list, a patch, and a web search are items rather than tool calls.**
  Claude delivers all three as tool calls and the contract has one shape for a
  tool call, so they map to `tool.started` and `tool.completed` rather than to
  nothing.

Codex also stamps `client_metadata` on its API requests, carrying `session_id`,
`thread_id`, `turn_id`, and `turn_started_at_unix_ms`. None of it reaches stdout,
which is why the harness session id comes from `thread.started`, the same id
`codex exec resume` takes.

## Names that had to change

`num_turns` becomes `TurnTiming.model_round_trips`. A harness counts one request
and its response as a turn; Arsox counts a whole unit of work as a turn. Carrying
the harness's word would have put two meanings on one name in one contract.

The distinction is not academic. A run that answers a question directly reports
one round trip; the same run made to use a tool reports two, because the model is
called again after seeing the result. Both are a single Arsox turn.

## Cost crosses from float to integer exactly once

The harness reports cost as an `f64`. That is the last place a float is allowed
to exist: `money_from_usd` converts it to `Money`'s integer units and billionths,
and everything downstream accumulates in integers. A ceiling that drifts from the
invoice it was meant to predict is the failure that shape exists to prevent.

## Conformance fixtures

`crates/arsox-harness/fixtures/` holds transcripts captured from the CLIs,
scrubbed of the capturing machine's paths, session identifiers, and installed
tooling. One directory per harness per pinned CLI version, because an output
shape belongs to a version and to nothing else:

```
<harness>/<cli-version>/<scenario>.stdout.jsonl   the native transcript
<harness>/<cli-version>/<scenario>.events.json    the canonical output it must produce
```

The suite asserts the whole canonical document rather than a field at a time,
which is what stops a mapper from dropping a field nobody wrote an assertion for.
A version bump becomes: record new fixtures, and let the suite say exactly what
changed.

Their value is that nobody wrote them from imagination, so each one states
whether it was recorded or constructed and `fixtures/README.md` keeps the table.
Every field a recording carries is something the harness actually emitted. The
one constructed fixture is Codex's successful run, built from the event schema
published with the same pinned CLI version, and it is shaped so a recording drops
in as a replacement.

Capturing a new one is documented with the fixtures, in
`crates/arsox-harness/fixtures/README.md`.

## The runner

The runner claims a queued turn, spawns the harness, streams its stdout through
the mapper, appends the canonical events to the log, and records the result.

**One turn at a time per thread is enforced by the claim query**, not by the
runner remembering to check. The claim is a single conditional update whose
subquery excludes any thread that already has something running, so two runners
racing produce one winner and one `None` rather than two processes driving one
conversation.

**A thread resumes its harness session.** The first turn opens a session under an
id the satellite chooses; later turns pass `--resume`. Without that a thread
would be a series of unrelated turns rather than a conversation.

**Cancellation is a database write, not a signal.** It arrives as an ordinary
HTTP request, so the runner learns about it by asking every so often rather than
being interrupted. Checking every line would be a query per line of output.

**A clean exit with no result line fails the turn.** The harness ended without
saying what it did, and reporting that as success is exactly the silent failure
the incident system exists to prevent.

**A ceiling ends a turn gracefully.** Reading harness output is a `select!` over
the three things that can end it: the harness finishing, a cancellation, and a
budget crossing. Wall clock is a deadline in that loop rather than a check in the
proxy, because the proxy sees requests and not the gaps between them, and a
harness stuck in a shell command would outlive a ceiling counted per request.
Token exhaustion arrives from the proxy on a channel, so it is acted on the
moment it happens rather than whenever the next line of output does. Either way
the harness is torn down, the work it committed survives, the events it already
produced stay in the log, and the result carries the code for the ceiling that
stopped it. See [the proxy doc](./llm-proxy.md#counting-and-refusing).

### The agent's environment is built, not inherited

**No `ARSOX_*` variable reaches an agent.** A spawned process inherits its
parent's environment by default, and the satellite's holds `ARSOX_SECRET`. An
agent that could read it could command its own satellite: destroy threads, read
another thread's artifacts, rewrite its own permissions.

The rule is written over the whole prefix rather than as a list of names,
because a denylist is one forgotten entry away from leaking the next setting
somebody adds. Everything an agent is meant to have is instead listed
explicitly on `HarnessCommand::env`.

It lives in `spawn::process_for`, which is the only way the runner builds a
process. Putting the scrub in the runner would make it a step somebody could
forget on the next spawn site; putting it here means a new spawn site inherits
the guarantee by construction.

The test spawns a real child and asks it what it can see, because the only
vantage point that can answer "what does an agent actually get" is the agent's.
Asserting on the `Command`'s own bookkeeping would be asserting on intent.

### Testing it without a model

Two stand-in harnesses replay a recorded transcript in place of a real CLI:
`src/bin/fake_harness.rs` for Rust tests, gated behind the `test-util` feature
so it cannot reach a published image, and `scripts/fake-harness.sh` for smoke
testing a built image without rebuilding it.

Both take per-run behaviour from `[[key=value]]` directives **in the prompt**
rather than from environment variables. That is not a stylistic choice: process
environment is global, tests run in parallel in one process, and an env-var knob
is a race in which one test silently reconfigures another. It was one, until two
tests started failing for reasons that had nothing to do with the code under
test.

## Roadmap

- **Bidirectional mode.** Both CLIs accept streaming input as well as emitting
  streaming output, which suits a long-lived process per thread better than a
  spawn per turn. It is also what makes cancellation and mid-turn input possible.
- **Spawning Codex.** The mapper reads `codex exec --json` today; the runner
  cannot start one. That spawn path also has to take the turn summary from the
  last `agent.message`, since Codex's closing message is an item rather than
  part of `turn.completed` and a per-line mapper holds no state to fold it in.
- **Codex over its app-server protocol.** It exposes command, patch, and network
  approvals as first-class requests, which is a better fit for the permission
  model than a one-way event stream.
- **Pairing `tool.completed` back to `tool.started`** for the elapsed duration
  and the tool name, neither of which the native result event carries.
