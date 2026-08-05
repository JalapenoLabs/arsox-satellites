# Harnesses

A harness is the CLI that drives an agent. Each speaks its own event vocabulary,
and a mapper turns that vocabulary into the one canonical contract, so an
application is written once and never rewritten when the harness changes.

Claude is implemented. Codex is next, and the contract is already designed
against its published schema.

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

## Where the native shape disagrees

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

`crates/arsox-satellite/fixtures/claude/` holds real transcripts captured from
the CLI, scrubbed of the capturing machine's paths, session identifiers, and
installed tooling.

Their value is that nobody wrote them from imagination. Every field, and every
place the native shape disagrees with the contract, is something the harness
actually emitted. Tests assert the exact canonical sequence a transcript
produces, so a mapper that starts dropping or reordering events fails loudly.

Capturing a new one:

```bash
claude -p "<prompt>" --output-format stream-json --verbose --allowedTools "Bash" < /dev/null
```

`< /dev/null` matters. The CLI waits on stdin for a few seconds otherwise, which
looks like a hang.

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
- **Codex.** Its app-server protocol exposes command, patch, and network
  approvals as first-class requests, which is a better fit for the permission
  model than a one-way event stream.
- **Pairing `tool.completed` back to `tool.started`** for the elapsed duration
  and the tool name, neither of which the native result event carries.
