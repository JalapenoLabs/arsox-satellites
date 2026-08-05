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

## Roadmap

- **The turn runner**: spawn the harness, feed it a prompt, assemble
  `HarnessResult` and the surrounding stages into a full `TurnResult`.
- **Bidirectional mode.** Both CLIs accept streaming input as well as emitting
  streaming output, which suits a long-lived process per thread better than a
  spawn per turn. It is also what makes cancellation and mid-turn input possible.
- **Codex.** Its app-server protocol exposes command, patch, and network
  approvals as first-class requests, which is a better fit for the permission
  model than a one-way event stream.
- **Pairing `tool.completed` back to `tool.started`** for the elapsed duration
  and the tool name, neither of which the native result event carries.
