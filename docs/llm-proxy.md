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

## Verifying it

`tests/llm_proxy.rs` drives the proxy over a real socket against a stub
upstream, and asserts from the upstream's seat. That is the only vantage point
that can see what was attached after the agent let go of the request.

Covered: the real credential arrives and the turn token does not, an ungranted
token is refused without reaching the provider, a revoked token stops working, a
path and key that disagree are refused, and a credential the agent supplied is
replaced rather than forwarded alongside ours.

## Roadmap

Everything above is the chokepoint. What it exists to carry is still to come:

- Token and cost accounting per thread, turn, and model, which is what
  `GET /v1/statistics` reports.
- Ceilings: `BUDGET_TOKENS_EXHAUSTED`, `BUDGET_COST_EXHAUSTED`, and the
  `budget_warning` event at 80% of any ceiling.
- Endpoint failover in the documented order, with each endpoint's own retry
  policy, and an incident recorded for every endpoint given up on.
- OAuth refresh, so an endpoint whose access token expires mid-thread recovers
  rather than failing over.
