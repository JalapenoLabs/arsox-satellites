# The LLM proxy

Every model request an agent makes traverses a proxy the satellite owns. This
is the chokepoint that makes credentials and ceilings enforceable rather than
requested.

## Why a proxy rather than configuration

Two things need to be true between an agent and its model, and neither can be
had by asking the agent nicely.

**A provider key in the agent's environment is a key the agent can read.** It
can be printed into a log, committed to a repo, or spent outside the satellite
entirely. Configuration cannot prevent that, because the agent is the thing
being configured.

**A budget written into a prompt is a suggestion.** An agent under pressure
routes around a suggestion. A budget enforced at the socket the completions
travel over is arithmetic, and there is nothing to route around.

Both are the same shape of problem: a control that lives where the agent can
reach it is not a control.

## The grant

One token per turn, minted when the turn starts and withdrawn when it ends.

It identifies the turn rather than authenticating a user. That distinction is
what lets the proxy attribute tokens and cost to the right thread without
trusting the agent to say who it is.

It is a UUID rather than anything derived from the turn, because a token an
agent can predict is a token it can mint for a turn that is not its own.

It is carried twice: in the URL path, which routes the request, and as the API
key, which is what the CLI believes it is sending. Both must match. One carrier
would be enough to route, so requiring both is deliberate: a process that
guessed the URL still needs the token, and the CLI has a credential-shaped thing
to send so it does not refuse to start.

**Revocation is a guard, not a call.** A turn can leave the runner by
cancellation, by a harness crash, or by any error added later, and each of those
is a path somebody could forget. `RevokeOnDrop` withdraws the grant however the
turn ends, so a token cannot outlive the work it was issued for.

## The agent never holds a credential

The proxy strips whatever the caller presented and attaches the real credential
itself. Stripped rather than overwritten, so a request cannot arrive carrying
two and have the upstream pick the agent's.

An API key is sent as `x-api-key` and a subscription or OAuth token as
`Authorization: Bearer`. They are not interchangeable: sending a subscription
token in `x-api-key` is rejected in a way that looks like a bad credential
rather than a mis-shaped request. An OAuth credential presents its access token
only; the refresh token never leaves the satellite.

A `Secret` carrying a `display` and no `value` presents nothing. Settings that
round-tripped through a response have the redacted rendering and no plaintext,
and sending that would forward `sk******23` as if it were the key.

The harness environment is scrubbed to match. See
[the harness doc](./harness.md#the-agents-environment-is-built-not-inherited):
no `ARSOX_*` variable and no provider credential reaches an agent, so the proxy
is not merely the preferred path to the model but the only one with a
credential on it.

## The chokepoint is universal

A thread that declares no endpoint still goes through the proxy, using whatever
credential the satellite itself holds.

The alternative, bypassing the proxy when nothing is configured, would mean the
default thread is the one whose spending is unmeasured and whose credential sits
in the agent's environment. Every guarantee here would then hold only for
threads that opted into it, which is the wrong way round.

## What streams and what does not

The response streams; the request does not.

A completion request is one bounded JSON document, and buffering it costs a copy
while avoiding chunked-encoding differences between providers. The response is
server-sent events, and collecting one before returning it would turn a
streaming API into a blocking one and defeat every event the harness emits as it
goes.

## Every request is bounded

An endpoint that accepted a request and then stopped answering leaves the harness
blocked on a socket, which from the satellite's side is indistinguishable from a
model thinking hard. So a request past the thread's `llm_request` bound, ten
minutes by default, is abandoned and answered with a 504 shaped like the
provider's own, which is what the harness's retry policy is written against.

The bound covers the whole relay rather than the wait for headers, so a response
still streaming past it is cut off mid-body, as a stream error rather than a tidy
end. Every request that ends this way records a `degraded` `LLM_ENDPOINT_TIMEOUT`
incident, which is what makes a failover tax visible instead of invisible.

The bound rides on the grant rather than on the shared HTTP client, so one turn's
setting cannot decide the limit for every other turn on the satellite. The
reasoning behind each of those choices is in
[the timeouts doc](./timeouts.md#the-request-bound-covers-the-whole-relay).

## Errors are shaped like the provider's

The harness on the other side of this speaks one provider's error format and
nothing else. Returning the satellite's own contract error would reach an agent
that cannot read it, and surface to the operator as an unexplained CLI failure.

An unknown token and a revoked one give the same answer. Distinguishing them
would tell a caller whether it had guessed a real turn.

## Binding

Loopback, on a port the kernel chooses.

The listener has no authentication beyond the per-turn token and it holds a real
provider credential, so it must never be reachable off the container. The port
is never configured by an operator and nothing outside the container connects to
it, so a fixed port would only be a collision waiting to happen on a host
running several satellites.

## Counting, and refusing

The chokepoint exists to carry ceilings, and this is them.

**Who counts and who decides.** The proxy counts. The runner decides how a turn
ends. A `Meter` is shared between them: the proxy adds up what each response
reported and refuses the next request once the ceiling is reached, and the
crossings it finds travel a channel to the runner, which owns the turn's
lifetime and is the only thing that writes to the event log. An accountant that
also emitted events would be emitting them for turns it cannot see the end of.

**What is counted.** `input_tokens + output_tokens`, exactly the `total_tokens`
the canonical `TokenUsage` carries. Anthropic's `input_tokens` excludes what came
from cache, so a heavily cached turn spends fewer budgeted tokens than it sent to
the model. That is consistency chosen over strictness: counting cache reads here
would make the ceiling disagree with the total the same turn reports in its
result, and an operator meeting `BUDGET_TOKENS_EXHAUSTED` at four thousand tokens
while the report says two thousand has been handed a contradiction rather than a
limit.

**Where it is read from.** Both shapes the provider answers in. A streamed
response reports usage on `message_start` and again, cumulatively, on
`message_delta`; a non-streamed one reports it once at the top level of the body.
Which is arriving is decided by the response's own `Content-Type` rather than
guessed at from the bytes, because a JSON document and a `data:` line are only
distinguishable by luck. The body is still relayed as it arrives; the scan reads
the bytes on their way past rather than collecting them.

**When the count lands.** After the response finishes, because that is when the
provider has finished saying what it cost. The request that crosses the ceiling
is therefore always allowed to complete, and the next one is refused: cutting a
completion off mid-stream to save the overshoot would discard tokens already paid
for. The total commits on drop as well as on a clean end, so a harness that hangs
up mid-response is still charged for what it generated.

**What a refusal looks like.** A 403 shaped like the provider's own
`permission_error`, not a 429. Both stop the request and only one of them tells
the CLI to retry against a wall that will not move before the turn ends.

### The three ceilings

| Ceiling | Enforced by | When |
|---|---|---|
| `maxTokensPerTurn` | the proxy | per request, from usage the provider reported |
| `maxWallClockPerTurn` | the runner | a deadline in the loop reading harness output |
| `maxCostPerThread` | the runner | at turn boundaries, from cost the harness reported |

Wall clock is the runner's because the proxy sees requests and not the gaps
between them: a harness stuck in a shell command never reaches the proxy and
would outlive any ceiling the proxy counted.

**Cost is the honest exception, and worth stating plainly.** Nothing in the
contract publishes a price. `ModelEndpoint` carries a name, a model, a base URL,
auth and a retry policy, and no rate, so the proxy cannot price a request without
a table of vendor prices invented here and stale within a quarter. What the
satellite does know is what each harness reports its turn cost, which is measured
rather than guessed. So the thread's cost ceiling is summed from finished turn
results and checked before a turn starts: a thread that has spent its ceiling
runs no further turns, and the turn that crosses it completes. Absent is not
zero here either, a thread whose turns reported no priced cost has nothing
comparable and is not enforced against rather than being treated as free.

Per-request cost enforcement needs pricing in the contract. That is a proto
change, not a proxy change.

### Warnings

At 80% of any ceiling a `budget.warning` event reaches the thread's stream
carrying which ceiling and how much of it is gone, so a host application can
react before the wall. Once per ceiling per turn: a warning on every request past
the threshold is noise rather than a signal.

A ceiling actually reached emits no warning. It ends the turn, and the turn's own
error names which one did it: `BUDGET_TOKENS_EXHAUSTED`, `BUDGET_COST_EXHAUSTED`,
or `BUDGET_WALL_CLOCK_EXHAUSTED`, alongside a `fatal` incident. The stop is
graceful, so work already committed survives and the events the harness produced
stay in the log.

## Verifying it

`tests/llm_proxy.rs` drives the proxy over a real socket against a stub
upstream, and asserts from the upstream's seat. That is the only vantage point
that can see what was attached after the agent let go of the request.

Covered: the real credential arrives and the turn token does not, an ungranted
token is refused without reaching the provider, a revoked token stops working, a
path and key that disagree are refused, a credential the agent supplied is
replaced rather than forwarded alongside ours, usage is counted from both a
streamed and a non-streamed response, the warning arrives at 80%, and a turn past
its ceiling is refused **without the request reaching the provider**, which is
the assertion the whole feature rests on.

The request bound is driven against two stub upstreams that fail in the two ways
that matter: one that accepts a request and never answers, and one that starts
streaming and then stops without ending. A third asserts that healthy traffic
inside its bound reports nothing, because a bound that fired on a good request
would fill the incident log with the one thing an operator most needs to trust.

## Roadmap

- Cost per request, once the contract carries model pricing.
- Endpoint failover in the documented order, with each endpoint's own retry
  policy, and an incident recorded for every endpoint given up on.
- OAuth refresh, so an endpoint whose access token expires mid-thread recovers
  rather than failing over.
- `GET /v1/statistics`, which is where lifetime totals per model and per thread
  surface.
