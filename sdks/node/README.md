# @jalapenolabs/arsox-sdk

The Node client for an [Arsox](../../README.md) satellite.

Protobuf is always on the wire, in both directions, and no setting changes that.
What you hold is an ordinary TypeScript object.

Not published yet. Build it from this directory with `yarn build`.

## Install

```bash
yarn add @jalapenolabs/arsox-sdk
```

Requires Node 22.23.2 or newer, the version the satellite image ships.

## Use

```typescript
import type { ThreadSettingsInit } from '@jalapenolabs/arsox-sdk'

import { ArsoxError, Satellite, TurnStatus } from '@jalapenolabs/arsox-sdk'

// Environment variables cross a runtime boundary, so they get a real check.
const secret = process.env.ARSOX_SECRET
if (!secret) {
  console.error('ARSOX_SECRET is not set, refusing to start')
  process.exit(1)
}

// Refuses a satellite serving a higher proto major rather than failing later
// with a confusing decode error. A higher minor warns once and proceeds.
const satellite = await Satellite.connect('https://satellite-01.internal', secret)

const settings: ThreadSettingsInit = {
  // The satellite collects the workspace after this much inactivity. The clock
  // resets on every turn, so a thread working for three days is never
  // collected. Required, always, as the safety net against forgotten
  // workspaces. Every time span in the contract is a Duration.
  idleTtl: { seconds: 7200n },

  // Required. Each ceiling is a case rather than a number, so `unlimited` has
  // to be typed out and an unbounded spend is a decision rather than an
  // oversight. There is no sentinel: 0 does not mean unlimited, it means zero.
  budget: {
    maxTokensPerTurn: { ceiling: { case: 'tokens', value: 8_000_000n } },
    // Money on the wire, never a float. A single request can cost a fraction of
    // a cent, and accumulating those in a float is how a ceiling drifts away
    // from the invoice it was meant to predict.
    maxCostPerThread: {
      ceiling: { case: 'cost', value: { currencyCode: 'USD', units: 40n, nanos: 0 } }
    },
    maxWallClockPerTurn: { ceiling: { case: 'unlimited', value: {} } }
  },

  // Advisory. It shapes behavior but never constrains it. Anything that must
  // hold belongs in `permissions`.
  prompt: 'Never use em dashes in user-facing text.'
}

const { thread, handle, deduplicated } = await satellite.threads().createWith(settings, {
  // What makes a timed-out create safe to retry. Without it, a response lost in
  // transit is indistinguishable from a thread that was never created, and the
  // only safe move is to retry and leak a whole workspace.
  idempotencyKey: 'rate-limiting-2026-08-04',
  // Your own correlation data, stored verbatim and handed back untouched.
  metadata: { tenantId: 'acme-corp', jobRowId: '41ff9c2e' }
})
console.log(`thread ${thread.threadId} ${deduplicated ? 'reused' : 'created'}`)

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
console.log(result.summary, result.status === TurnStatus.COMPLETED)

// Or leave it to expire through the idle TTL. Incidents survive either way, on
// their own retention.
await handle.destroy()
```

### Attaching from another process

A thread lives entirely on the satellite. The client holds a handle, never any
thread state, so any process with the URL, the secret, and the thread id can pick
the work back up. There is no handoff, no lease, and no ownership.

```typescript
const handle = await satellite.threads().attach(threadId)

// Exclusive: pass the last sequence you actually handled and receive everything
// since. A replica that died mid-turn loses nothing.
for await (const event of await handle.events({ fromSequence: lastSeenSequence })) {
  await record(event)
}
```

### Errors

One error class rather than a family. Match on `code`, never on the message:
wording may change within a major version, codes may not.

```typescript
try {
  await handle.startTurn('...')
}
catch (error) {
  if (!(error instanceof ArsoxError)) {
    throw error
  }

  // The thread existed and no longer does, which is a different fact from a bad
  // id and leads to a different repair.
  if (error.isGone()) {
    return openAReplacementThread()
  }

  // Read from the satellite's own flag rather than matched against a list of
  // codes, which is what keeps this build safe against a newer satellite.
  if (error.retryable) {
    return retryLater()
  }

  throw error
}
```

## What exists today

| Surface | Status |
|---|---|
| `Satellite.connect`, `version`, `status`, `harness` | done |
| `threads().create`, `createWith`, `attach`, `list` | done |
| thread `get`, `destroy`, `pause`, `resume`, `drain`, `turns` | done |
| `startTurn`, `startTurnWith`, turn `get`, `result`, `cancel` | done |
| `events()` over the thread socket, with `fromSequence` resumption | done |
| `incidents()` on the satellite and on a thread | done |
| Streaming settings toggles, text and markdown reports, artifacts | not yet |
| Plan approval, question answering, the control socket | not yet |

`examples/example.ts` at the repository root describes the full surface from the
README, most of which no satellite serves yet. It is the design target, not a
description of this package.

## Working on it

```bash
corepack yarn install
corepack yarn build
corepack yarn test
```

`yarn build`, `yarn typecheck`, and `yarn test` each sync the generated protobuf
contract from `gen/ts` into `src/proto` first. That directory is git ignored:
`gen/ts` is the one checked-in generated tree, and a second copy could be stale
while that one is current. npm can only publish files beneath a package
directory, which is why the copy exists at all, and it is the same constraint
that puts the Rust output inside `arsox-sdk` rather than under `gen/`.

Nothing reads a `.proto` at runtime. The contract is checked by the compiler.

### The integration suite

`tests/satellite.test.ts` builds and spawns the real satellite binary with the
`test-util` fake harness, which replays a recorded Claude transcript rather than
calling a model. No network, no token budget, and a deterministic turn.

It needs a Rust toolchain and **port 8080 free**. The satellite binds that port
with no override, deliberately: the container's port mapping is where its
reachable address is decided, and a second knob would only be a way for the two
to disagree. The suite therefore runs one satellite at a time and skips itself
with a clear message when something else holds the port.

Run only the tests that need neither with `yarn test:unit`.
