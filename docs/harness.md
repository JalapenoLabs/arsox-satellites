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
everything that can end it: the harness finishing, a cancellation, a budget
crossing, and the idle bound below. Wall clock is a deadline in that loop rather
than a check in the proxy, because the proxy sees requests and not the gaps
between them, and a harness stuck in a shell command would outlive a ceiling
counted per request. Token exhaustion arrives from the proxy on a channel, so it
is acted on the moment it happens rather than whenever the next line of output
does. Either way the harness is torn down, the work it committed survives, the
events it already produced stay in the log, and the result carries the code for
the ceiling that stopped it. See
[the proxy doc](./llm-proxy.md#counting-and-refusing).

**A harness that says nothing is stopped, not slow.** The same loop carries an
idle bound that any output on either pipe resets, and a harness that outlives it
is torn down and started once more on the same session with the same grant. A
second expiry fails the turn with `HARNESS_IDLE_TIMEOUT`. Stderr is read for the
same reason it counts as output: an unread pipe fills and blocks the process the
loop is waiting on, which would be the satellite manufacturing the hang it then
reported. See [the timeouts doc](./timeouts.md#the-idle-bound-is-measured-against-silence).

### Checkers run after the harness, and can wake it back up

A repo's `checker` is its own definition of done. It runs once the harness has
reported its result, with the working directory at that repo's checkout, under
the same barrier semantics setup commands use: newlines run in parallel and do
not fail fast, semicolons are barriers. One parser serves both ends of a turn,
in `src/commands.rs`, because the contract gives them one syntax.

This is the verification that turns "the agent said it was done" into something
checked. Repos are walked one at a time, since a checker is frequently a build
and running four at once on a satellite already bounded to four threads trades a
little wall clock for a lot of contention.

**Every command is bounded**, setup and checker alike, and one past its bound is
killed and reported as a failure that says it timed out. The failure reaches the
agent the same way any other red checker does. See
[the timeouts doc](./timeouts.md#the-exec-bound-kills-the-shell-and-only-the-shell)
for what the kill does and does not reach.

**A nonzero exit is a normal outcome, not a crash.** The agent's own session is
resumed with the failing commands, their exit codes, and the tail of their
output, and is asked to do one of two things: change the code so the command
passes, or say why the failure should stand. Both answers are named because a
checker can be red for a reason the agents deliberately accept, and an agent told
only to fix it will keep trying to fix something that is not broken.

**Resumed, not started fresh.** The agent that wrote the code is the one that can
fix it. A clean context would meet a failing lint with no idea what the work was
for.

**The loop is capped at two fix attempts.** A checker that failed, was fixed, and
failed again is saying the fix did not work, and a third attempt on the same
evidence usually reaches the same place at the cost of another session. The cap
is also what a flake cannot outlast: a test that fails at random produces new
evidence forever, and an uncapped loop would answer it until the budget was gone.
Past the cap the turn **completes** with the failures in `checker_results`, the
stage reported as `FAILED`, and a `CHECKER_FAILED` incident. A check that will
not go green is a fact about the work rather than a reason to throw the work
away, and `COMPLETED` says the turn reached the end of the stack rather than that
everything passed.

That incident is **`degraded`, not `blocked`.** `blocked` is for a permission
gate that closed as designed, and no gate closed here: the work happened and
finished with its verification missing, which is what `degraded` names. Filing it
as blocked would put a red build in the same query an operator runs to find an
allowlist that needs widening.

**A fix cycle spends the turn's budget, not a new one.** Every session in a turn
runs inside one `drive` call, sharing one proxy grant, one meter, and one wall
clock. A grant minted per session would spend past the ceiling the turn set, and
a wall clock restarted per session would let a turn with a five minute ceiling
run for fifteen while reporting the ceiling as held. Budget exhaustion during a
fix cycle ends the turn with the code for the ceiling that did it, exactly as it
would during the work itself.

The accounting from every session is folded together in
`src/harness/accounting.rs`. Reporting only the first would understate every turn
that had to fix a checker, in the number a cost reconciliation reads and the one
the thread's lifetime ceiling is enforced against. Absent counts stay absent
through the fold: two harnesses reporting no cache accounting must not start
reporting a zero because a turn ran twice.

Each command's outcome reaches the thread's stream as `checker.result` as it
finishes, so a consumer watching a long build sees it land rather than learning
about the whole stage at the end. `TurnResult.checker_results` carries the **last**
attempt only, which is the state the turn ended in; the history is on the stream.

A thread whose repos declare no checker costs one filter over its repos, and the
stage is still reported, as `SKIPPED` with a reason. "Not run" must never read as
"found nothing".

**The stage is skipped when the harness did not finish.** Checkers verify work an
agent claimed to have completed, and a turn whose harness crashed or reported an
error made no such claim. Resuming the session that just failed would spend two
more of them proving it.

`CheckerResult.skipped_by_commander` is always false today. The MCP tool that
lets a commander accept a failure is separate work, and claiming a skip nobody
asked for would misreport why a check is red.

### Permissions reach the harness as flags

A harness launched with `--print` cannot answer a permission prompt. Without
permission flags every file edit and every shell command it tries is refused, so
a non-interactive turn can talk about work but never do any. The spawn maps the
thread's permissions onto flags to close that.

**These flags are advisory.** The harness applies them to itself, exactly as
`AGENTS.md` shapes behavior without constraining it. An agent granted a shell
reaches everything the container reaches, whatever else the flags say.

The deterministic layer the README describes is separate, unbuilt work: the exec
broker that rejects a command by exact argv, the egress proxy the container has
no route around, the root-owned `pre-push` hook. **Until those land the container
is the only real boundary**, and none of them is something `--permission-mode`
can switch off, so they will enforce underneath these flags rather than through
them.

| Setting | Flag | Note |
|---|---|---|
| nothing declared | `--permission-mode bypassPermissions` | the documented default is the preset, so this is where both land |
| `exec: PRESET` | `--permission-mode bypassPermissions` | the curated list belongs to the broker, which does not exist yet |
| `exec: NONE` | `--permission-mode acceptEdits --disallowedTools Bash` | the shell goes, edits stay |
| `exec: CUSTOM` | `--permission-mode acceptEdits --allowedTools Bash(cmd),Bash(cmd *)` | one pair of rules per command |
| `allowed_commands` | the same pair of rules, added to whatever `exec` set | additive, which is the contract's own rule |
| `web`, `additional_domains` | none | the egress proxy's, not a tool rule |
| `allow_git_push`, `protected_branches` | none | the `pre-push` hook's, not a tool rule |

Three decisions in that table are worth their reasoning.

**The default is `bypassPermissions` because the narrower posture protects
nothing.** The alternative grants the shell and withholds the rest, and an agent
holding a shell reaches every byte and socket the container does, so refusing it
`WebFetch` is a formality it answers with `curl`. What the narrower posture does
buy is failure: under `--print` a gate cannot be answered, so the first tool
nobody thought to list is refused mid-turn and the agent spends the rest of the
turn working around a restriction that was never intended. The honest default
matches the boundary that actually exists.

**An allowed command becomes two rules.** The CLI matches `Bash(yarn install)`
exactly and `Bash(yarn install *)` only with something following it. A thread
that allowed `yarn install` means both, and emitting one form would refuse the
bare invocation of the command it just permitted. The CLI also accepts a `:*`
prefix form; its own validator calls that legacy, so the wildcard spelling is
what the satellite emits.

**Egress and push policy are not rendered as tool rules.** A push can be spelled
a dozen ways in argv and a protected ref is frequently not in the argv at all, so
a rule that matched the obvious spelling would advertise an enforcement a rename
defeats. Those two stay with the infrastructure that can actually hold them.

`--permission-mode` never reached the contract for the same reason. It is a
Claude spelling for an advisory gate, and `Permissions` documents itself as
deterministic controls; putting one in the other would leak a harness into the
wire format and promise an enforcement the satellite does not perform. The
posture is derived from what the contract already states, and it is derived
inside the Claude arm of the spawn, because handing `--permission-mode` to
`codex exec` would fail the launch rather than restrict it. Writing the Codex
spawn path means mapping the same posture onto Codex's own approval flags.

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

#### Declared variables add to it, and may only add

A thread's `env` list is set on top of that scrub, which is how a registry token
or a deploy target reaches an agent at all. It may only add.
`spawn::declared_key_refusal` refuses any key beginning `ARSOX_`, any key shaped
like a provider credential, and any name a process cannot carry, and the API
refuses a thread declaring one at creation with `REQUEST_FIELD_INVALID` naming
the key. A declared variable that put back what the scrub removed would hand an
agent the credential the proxy exists to hold on its behalf.

The same rule runs again in `spawn::declared_environment`, where it can only
fire for settings written before that check existed. The proxy's own variables
are applied last, so a declared `ANTHROPIC_BASE_URL` cannot route a turn around
the budget ceilings: the last value set for a key is the one the child sees.

`is_secret` is `optional bool` on the wire precisely so absent and `false` stay
distinguishable, and **absent means secret**. It decides what may be rendered,
never what an agent is given: both kinds are set on the child. `AgentVar`
carries the flag to the spawn boundary and writes its own `Debug`, so the turn's
proxy token and every declared credential are masked in any log line that
formats a command, while a value the caller marked public reads plainly.

The same rule decides what the thread's redactor indexes, so a secret an agent
prints back is masked out of every event, result, and incident the turn produces.
See [the redaction doc](./redaction.md).

The same list reaches a repo's setup commands, because `yarn install` needs the
registry token for exactly the reason the agent that runs it later does.

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

- **Crash restart-once.** A harness that died is recovered the same way a hung
  one already is, through the restart helper the idle bound uses. Today a
  nonzero exit fails the turn.
- **Bidirectional mode.** Both CLIs accept streaming input as well as emitting
  streaming output, which suits a long-lived process per thread better than a
  spawn per turn. It is also what makes cancellation and mid-turn input possible.
- **Spawning Codex.** The mapper reads `codex exec --json` today; the runner
  cannot start one. That spawn path also has to take the turn summary from the
  last `agent.message`, since Codex's closing message is an item rather than
  part of `turn.completed` and a per-line mapper holds no state to fold it in.
- **A commander that can accept a failing checker.** The MCP tool that sets
  `skipped_by_commander`, so a check the agents deliberately accept is reported
  as accepted rather than as unfixed. A skip applies to one turn and never
  carries into the next.
- **Deterministic permission enforcement.** The exec broker, the egress proxy,
  and the root-owned `pre-push` hook. The flags above are advisory until these
  exist, and they remain advisory afterward: these enforce underneath them.
- **Codex over its app-server protocol.** It exposes command, patch, and network
  approvals as first-class requests, which is a better fit for the permission
  model than a one-way event stream.
- **Pairing `tool.completed` back to `tool.started`** for the elapsed duration
  and the tool name, neither of which the native result event carries.
