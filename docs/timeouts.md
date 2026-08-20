# Timeouts

Three operations in a turn have no natural end: a shell command, a model
request, and the harness itself. Each one, left unbounded, holds its thread for
the life of the process while reporting itself perfectly healthy. A hung thread
is worse than a failed one, because a failure is a fact an application can act
on and a hang is an absence it has to guess about.

So each carries a bound, and every bound is per thread.

| Operation | Default | On expiry |
|---|---|---|
| one exec command | 30 minutes | the command is killed, and its outcome returns to the agent as a failure that says it timed out |
| one model request | 10 minutes | the attempt is abandoned, its endpoint is given up on, and a `degraded` incident records it; the harness gets a 504 only once every endpoint has been tried |
| harness idle, meaning no output at all | 15 minutes | the harness is torn down and started once on the same session; a second expiry fails the turn with `HARNESS_IDLE_TIMEOUT` |

The turn wall clock is deliberately not in this table. Exceeding it is a budget
outcome rather than a hung operation, so it lives in `Budget` beside the token
and cost ceilings. See [the proxy doc](./llm-proxy.md#the-three-ceilings).

## Resolved once, enforced in three places

`src/timeouts.rs` reads `ThreadSettings.timeouts` into `Bounds`, and that is the
only place a default is written down. The three enforcement sites are far apart,
in `src/commands.rs`, `src/proxy.rs`, and `src/harness/runner.rs`, and three
copies of "absent means 30 minutes" in three files nobody edits together is
three chances to drift.

**Every field is a real bound rather than an option.** A thread that declared
nothing gets the documented default, and "no bound at all" is not expressible.
The whole point of the table is that a hung operation always ends, and a knob
that could switch that off would be a knob for producing the hang.

**Zero and negative spans fall back to the default.** A bound of zero would kill
every command as it started and restart every harness before it drew breath.
Reading it as written would turn a typo into a satellite that runs nothing, so
the resolution refuses it and the enforcement sites never have to ask whether
their bound is real.

## The exec bound kills the shell, and only the shell

Declared commands run through `sh -c`, so `yarn install` is a grandchild of the
satellite. Without a process group on Unix or a job object on Windows, a
grandchild outlives the parent killed above it, and an orphan keeps running until
the container stops.

That is a real limit rather than something to imply away. The bound reliably ends
the satellite's *wait*, and reliably ends the shell. Process-group teardown is
what would end everything below it, and it is separate work.

Both pipes are drained while the command runs rather than after it exits, which
is what makes the bound mean what it says: a pipe nobody reads fills its buffer
and blocks the writer, and a bound applied on top of that would kill a chatty
command for a hang the satellite caused.

A timed-out command carries exit code `TIMED_OUT`, distinct from the code for one
that never launched. "It never finished" and "it finished badly" send a reader to
different places. Whatever the command printed before it was killed is kept: the
tail of a hung build is the only evidence of what it was doing when it stopped.

## The request bound covers the whole relay

Not just the wait for headers. A provider that sends a token a minute for an hour
has failed in a way a bound on the handshake alone would never catch, so a
response still streaming past the bound is cut off mid-body.

The alternative, an idle bound between chunks, would let a slow drip run forever
while never technically being idle.

The cut-off arrives as a stream **error** rather than a tidy end, because a
truncated server-sent event stream that ended cleanly would reach the harness as
a completion that simply stopped.

The incident is `degraded`, not `fatal`: the timed-out attempt gives up on its
endpoint and the next endpoint is tried, so the turn goes on. That is exactly why
it is recorded. A timeout that was quietly failed over and then answered looks
identical to success, and a tax paid on every turn forever is invisible until
somebody reconciles a bill against it. See
[dispositions](../README.md#dispositions).

**The bound is per attempt rather than per harness request.** Each attempt is one
model request, which is what this row names. A timed-out attempt is not retried,
so the worst case is one bound per endpoint rather than one per attempt. See
[the proxy doc](./llm-proxy.md#endpoints-are-tried-in-order).

**The proxy holds no database, and reports rather than records.** It sees
requests and not the turn they belong to the end of, so what it finds travels a
channel to the runner, exactly as budget crossings already do. The runner drains
that channel after its read loop as well as inside it, since a timeout found on
the request that ended a session arrives when nothing is left to select on.

## The idle bound is measured against silence

Not against progress and not against elapsed time. An agent running a forty
minute build still writes tool output around it, which is what makes fifteen
minutes safe to set well inside a turn: a harness that says nothing for a quarter
of an hour has stopped rather than slowed.

**Both pipes count as output.** A harness writing progress to stderr while stdout
stays quiet is working. Reading stderr is also what keeps its pipe from filling
and blocking the process the loop is waiting on, which would be the satellite
manufacturing the hang it then reported.

Stderr never reaches the event log. It is a CLI's diagnostics rather than
anything the contract has a shape for, so it is read to count as life and traced
at debug.

The bound belongs to the session rather than to the turn, unlike the wall clock:
a fresh process that has said nothing yet has not been idle for however long its
predecessor was.

### Restarted once, never twice

A restart recovers a process that wedged, which is a real and common thing. It
does not recover a prompt, a repo, or a model that wedges every process reading
it, which is what a second hang after a clean restart says is happening. A third
attempt spends another session reaching the same place.

The restart resumes the same harness session under the same grant, meter, and
wall clock, so the second attempt continues the conversation rather than starting
one and spends against the ceilings the turn already set. The command is rebuilt
per attempt rather than reused, because that is what lets a harness that got far
enough to report its session id be resumed into it.

The restart is recorded as a `recovered` incident. A restart that worked looks
exactly like a turn that never stalled, and a harness wedging on every turn is a
pattern nobody sees unless the recovery is written down.

A second expiry ends the turn with `HARNESS_IDLE_TIMEOUT`, which is **retryable**:
the turn is worth running again, just not inside this one.

**A crash is the other ending a restart recovers**, and it belongs in the same
helper rather than in a second restart loop beside it. That is separate work and
is deliberately not done here: today a nonzero exit fails the turn exactly as it
did before.

## Testing it

Every bound is injectable, and none of it goes through a satellite-wide constant
a test would have to reach around:

- **Commands.** `Execution` carries the bound alongside the environment, so a
  test constructs one with a bound in milliseconds and runs a sleeping command.
- **The proxy.** The bound rides on the `Grant` rather than on the shared HTTP
  client, which is what stops one turn's short bound from deciding the limit for
  every other turn on the satellite, and what lets a test bound one grant to
  300ms against a stub upstream that never answers.
- **The runner.** The idle bound is a per-thread contract field, so a test
  declares it exactly as a host application would. The stand-in harness takes
  `[[hang=MS]]` and `[[hang_once=MS]]` in the prompt, which produce silence
  before the transcript. `hang_once` marks the working directory with
  `create_new`, so the first process hangs and the restart gets through, which is
  what makes "restarted once, and then the turn completed" a thing a test can
  assert.

## Roadmap

- **Process-group teardown** for exec commands, so a killed shell takes its
  children with it rather than leaving orphans.
- **Crash restart-once**, sharing the restart the idle bound already uses.
