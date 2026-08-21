# Quickstart

One satellite, one real turn, five minutes. Everything on this page works
today. [What is not here yet](#what-is-not-here-yet) says so at the bottom.

## 1. Build and run a satellite

The image is not published to docker.io yet, so build it from a clone.

```bash
git clone https://github.com/JalapenoLabs/arsox-satellites.git
cd arsox-satellites
docker build --tag arsox-satellite:dev .
```

A satellite holds two credentials. `ARSOX_SECRET` is the bearer token your
application authenticates with, and it never reaches an agent. The model
credential never reaches one either: requests go through the satellite's own
proxy, which attaches the real credential on the way out.

Set whichever model credential you have. `ANTHROPIC_AUTH_TOKEN` takes a
subscription token, which `claude setup-token` mints; `ANTHROPIC_API_KEY` takes
an API key from the [Anthropic console](https://console.anthropic.com/).

```bash
export ARSOX_SECRET="$(openssl rand -hex 32)"
export ANTHROPIC_AUTH_TOKEN="..."   # or ANTHROPIC_API_KEY

docker run --detach --name arsox \
  --publish 127.0.0.1:8080:8080 \
  --env ARSOX_SECRET \
  --env ANTHROPIC_AUTH_TOKEN \
  --volume arsox-db:/var/arsox \
  --volume arsox-workspace:/workspace \
  arsox-satellite:dev
```

Both volumes are named on purpose. `/var/arsox` holds the database of threads,
queued turns, event history, and incidents; `/workspace` holds the checkouts.
Nothing under either survives a container replacement otherwise, which silently
breaks thread resumption.

The satellite fails to start with `ARSOX_SECRET` unset. Failing closed is
deliberate: an accidentally unauthenticated satellite is a remote shell with
your credentials in it.

## 2. Prove it is up

Two unauthenticated GETs, in this order:

```bash
curl --silent http://127.0.0.1:8080/healthz
# ok

curl --silent --output /dev/null --write-out '%{http_code} %{content_type}\n' \
  http://127.0.0.1:8080/v1/version
# 200 application/protobuf
```

`/healthz` is liveness and never touches the database. `/v1/version` reports the
satellite version and the proto contract it serves. Both answer before you hold
a credential, and neither exposes thread content or configuration.

The version body is protobuf, which is why the check above reads the status
rather than the bytes. An SDK decodes it at `connect` and refuses a satellite
serving a proto major it cannot speak. The JSON rendering of the contract is
documented in the README and is not served yet, so read a response through an
SDK rather than by eye.

## 3. A real turn, in TypeScript

The Node SDK is not on npm yet. Build it in the clone:

```bash
cd sdks/node
corepack yarn install
corepack yarn build
```

Then depend on that directory from your own project. It needs Node 22.23.2 or
newer, the version the image ships.

```bash
yarn add @jalapenolabs/arsox-sdk@portal:/path/to/arsox-satellites/sdks/node
```

```typescript
import type { ThreadSettingsInit } from '@jalapenolabs/arsox-sdk'

// Core
import { Satellite, TurnStatus } from '@jalapenolabs/arsox-sdk'

// Environment variables cross a runtime boundary, so they get a real check.
const secret = process.env.ARSOX_SECRET
if (!secret) {
  console.error('ARSOX_SECRET is not set, refusing to start')
  process.exit(1)
}

// Reads /v1/version and refuses a satellite serving a proto major this SDK
// cannot speak, rather than failing later with a decode error.
const satellite = await Satellite.connect('http://127.0.0.1:8080', secret)

const settings: ThreadSettingsInit = {
  // Required. Idle time, not wall clock from creation: the clock resets on
  // every turn, so a thread working for three days is never collected.
  idleTtl: { seconds: 3600n },

  // Required. Each ceiling is a case rather than a number, so an unbounded
  // spend has to be typed out.
  budget: {
    maxTokensPerTurn: { ceiling: { case: 'tokens', value: 2_000_000n } },
    maxCostPerThread: {
      ceiling: { case: 'cost', value: { currencyCode: 'USD', units: 5n, nanos: 0 } }
    },
    maxWallClockPerTurn: { ceiling: { case: 'unlimited', value: {} } }
  },

  // Optional. Drop this field for a first run that needs no checkout.
  repos: [
    {
      name: 'api',
      url: 'https://github.com/your-org/api.git',
      setupCommands: 'yarn install',
      // Newlines run in parallel, semicolons are barriers.
      checker: 'yarn lint\nyarn typecheck'
    }
  ],

  // Absent `isSecret` means secret, because defaulting to secret fails safe.
  env: [
    { key: 'CI', value: { value: 'true' }, isSecret: false }
  ],

  // Advisory. It shapes behavior and never constrains it.
  prompt: 'Never use em dashes in user-facing text.'
}

const { thread, handle } = await satellite.threads().create(settings)
console.log(`thread ${thread.threadId}`)

// Subscribe before queueing, so nothing published between the two is lost.
const events = await handle.events()

const turn = await handle.startTurn('Add per-endpoint rate limiting to the public API.')

for await (const event of events) {
  console.log(`${event.sequence} ${event.type}`)
  if (event.type === 'turn.completed') {
    break
  }
}

const result = await turn.result()
console.log(result.summary)
console.log(result.status === TurnStatus.COMPLETED, result.tokens?.totalTokens)

// Or leave it to expire through the idle TTL. Incidents survive either way.
await handle.destroy()
```

A repo needs a URL the satellite can reach, and a private one needs
[credentials](../README.md#repo-settings). Repos are optional: drop the field
and the agent works in an empty workspace, which is the shortest path to a first
turn.

## 4. The same turn, in Rust

`arsox-sdk` is not on crates.io and carries no tag yet, so pin a commit.
[Tagged git dependencies](./core-crates.md#publishing) are the plan.
`futures-util` comes along because the event stream is a `Stream`.

```toml
[dependencies]
arsox-sdk = { git = "https://github.com/JalapenoLabs/arsox-satellites", rev = "<commit>" }
futures-util = "0.3"
tokio = { version = "1", features = [ "rt-multi-thread", "macros" ] }
```

```rust
use arsox_sdk::client::Satellite;
use arsox_sdk::proto::common::v1::{
    CostCeiling, Duration, DurationCeiling, Money, Secret, TokenCeiling, Unlimited, cost_ceiling,
    duration_ceiling, token_ceiling,
};
use arsox_sdk::proto::settings::v1::{Budget, EnvVar, Repo, ThreadSettings};
use arsox_sdk::proto::turn::v1::TurnStatus;
use futures_util::StreamExt as _;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Reads /v1/version and refuses a satellite serving a proto major this SDK
    // cannot speak, rather than failing later with a decode error.
    let satellite =
        Satellite::connect("http://127.0.0.1:8080", std::env::var("ARSOX_SECRET")?).await?;

    let settings = ThreadSettings {
        // Required. Idle time, not wall clock from creation.
        idle_ttl: Some(Duration {
            seconds: 3600,
            nanos: 0,
        }),

        // Required. Each ceiling is a case rather than a number, so an
        // unbounded spend has to be typed out.
        budget: Some(Budget {
            max_tokens_per_turn: Some(TokenCeiling {
                ceiling: Some(token_ceiling::Ceiling::Tokens(2_000_000)),
            }),
            max_cost_per_thread: Some(CostCeiling {
                ceiling: Some(cost_ceiling::Ceiling::Cost(Money::usd(5, 0))),
            }),
            max_wall_clock_per_turn: Some(DurationCeiling {
                ceiling: Some(duration_ceiling::Ceiling::Unlimited(Unlimited {})),
            }),
        }),

        // Optional. Drop this field for a first run that needs no checkout.
        repos: vec![Repo {
            name: "api".to_owned(),
            url: "https://github.com/your-org/api.git".to_owned(),
            setup_commands: "yarn install".to_owned(),
            // Newlines run in parallel, semicolons are barriers.
            checker: "yarn lint\nyarn typecheck".to_owned(),
            ..Repo::default()
        }],

        // Absent `is_secret` means secret, because defaulting to secret fails safe.
        env: vec![EnvVar {
            key: "CI".to_owned(),
            value: Some(Secret {
                value: Some("true".to_owned()),
                display: None,
            }),
            is_secret: Some(false),
        }],

        // Advisory. It shapes behavior and never constrains it.
        prompt: "Never use em dashes in user-facing text.".to_owned(),

        ..ThreadSettings::default()
    };

    let created = satellite.threads().create(settings).await?;
    println!("thread {}", created.thread.thread_id);

    // Subscribe before queueing, so nothing published between the two is lost.
    let mut events = created.handle.events().await?;

    let turn = created
        .handle
        .start_turn("Add per-endpoint rate limiting to the public API.")
        .await?;

    while let Some(event) = events.next().await {
        let event = event?;
        println!("{} {}", event.sequence, event.r#type);
        if event.r#type == "turn.completed" {
            break;
        }
    }

    let result = turn.result().await?;
    println!("{}", result.summary);
    println!("completed: {}", result.status == i32::from(TurnStatus::Completed));

    // Or leave it to expire through the idle TTL. Incidents survive either way.
    created.handle.destroy().await?;

    Ok(())
}
```

## 5. What you just got

**Events.** Every event carries a `sequence` that is monotonic per thread and is
persisted before it is sent, so reconnecting with `fromSequence` replays exactly
what you missed. See [streaming](./streaming.md).

**Incidents.** A prefetch that 404s, an endpoint that failed before the next one
covered, a checker that went red: each is recorded, streamed, and queryable
after the thread is gone, because nothing in Arsox fails silently. See
[incidents](./incidents.md).

**Budgets.** The ceilings above are enforced by the proxy every model request
passes through and by the runner's own deadline, so an agent cannot talk its way
past one. See [the LLM proxy](./llm-proxy.md).

**Checkers.** A repo's checker runs once the agent reports its work complete, and
a nonzero exit wakes that same session back up to fix it or to justify letting it
stand. See [the harness doc](./harness.md#checkers-run-after-the-harness-and-can-wake-it-back-up).

Turns are also bounded: a shell command, a model request, and a silent harness
each carry a per-thread [timeout](./timeouts.md).

## What is not here yet

The README describes the whole design. These parts of it are roadmap rather than
behavior, and this page shows none of them:

| Not yet | Where it lands |
|---|---|
| The image on docker.io, so `FROM jalapenolabs/arsox-satellite` works | [Distribution](https://github.com/JalapenoLabs/arsox-satellites/milestone/5) |
| The Node SDK on npm, and a Python SDK at all | [Distribution](https://github.com/JalapenoLabs/arsox-satellites/milestone/5) |
| A published tag for the Rust SDK, so a git dependency can pin one | [Distribution](https://github.com/JalapenoLabs/arsox-satellites/milestone/5) |
| Team mode, plan approval, question answering, and artifacts | [the SDK doc's roadmap](./sdk.md#what-the-aspirational-example-still-needs) |
| Deterministic permissions beyond the exec broker, the `pre-push` hook, and the egress proxy: filesystem scope per member, and the redaction kill switch | [Enforcement](https://github.com/JalapenoLabs/arsox-satellites/milestone/4) |

The three gates that exist engage only on a satellite that separates privilege,
which means the published image rather than a `cargo run` on a laptop. Off it,
thread permissions reach the harness as advisory flags and the container is the
real boundary, so give a satellite the network reach and the credentials you
would give the agent running inside it. The egress proxy's allowlist also wants
the [route closure](./enforcement.md#the-route-closure-is-deployment-configuration),
which is deployment configuration rather than something the satellite applies to
itself.
