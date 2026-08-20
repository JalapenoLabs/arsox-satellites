// Copyright © 2026 Jalapeno Labs

/**
 * The Node SDK driving a real satellite.
 *
 * This is the point of the exercise rather than a formality. Anything a consumer
 * needs and cannot reach from the published surface is a hole in the SDK, and a
 * test written from the consumer's seat is the only place that shows up.
 *
 * One satellite serves most of the file because starting one costs a second, not
 * because it has to be alone. Each gets a port of its own. See
 * `satellite-process.ts`.
 */

import type { RunningSatellite } from './satellite-process.js'
import type { ThreadSettingsInit } from '../src/index.js'

// Core
import { afterAll, beforeAll, describe, expect, it } from 'vitest'

// Misc
import {
  ArsoxError,
  Disposition,
  ErrorCode,
  Harness,
  Satellite,
  ThreadState,
  TurnStatus
} from '../src/index.js'
import { SECRET, startSatellite } from './satellite-process.js'

/**
 * The settings a thread must declare: an idle TTL and a budget.
 *
 * Both are required, always. The TTL is the safety net against a forgotten
 * workspace filling a disk, and a required budget is what makes an unbounded
 * spend a decision rather than a field somebody left unset.
 */
const settings: ThreadSettingsInit = {
  idleTtl: { seconds: 3600n },
  budget: {}
}

/**
 * Awaits a call that must fail, and hands back the failure it produced.
 *
 * Written out rather than reached through `rejects`, because every assertion
 * below is about the error's own shape: its code, its retryable flag, and which
 * of the two "the thread is not here" facts it reports.
 */
async function failureFrom(call: Promise<unknown>): Promise<ArsoxError> {
  try {
    await call
  }
  catch (error) {
    if (error instanceof ArsoxError) {
      return error
    }
    throw error
  }

  throw new Error('the call was expected to fail and did not')
}

describe('the Node SDK against a running satellite', () => {
  let running: RunningSatellite
  let client: Satellite

  beforeAll(async () => {
    running = await startSatellite()
    client = await Satellite.connect(running.url, SECRET)
  })

  afterAll(async () => {
    await running?.stop()
  })

  it('checks the contract version when it connects', async () => {
    const version = await client.version()

    expect(version.protoMajor).toBe(1)
    expect(version.satelliteVersion).not.toBe('')
  })

  it('reports which harnesses this satellite offers', async () => {
    const harness = await client.harness()

    // Claude is the harness implemented today, and the endpoint says so plainly
    // rather than implying a suite that spans several.
    expect(harness.defaultHarness).toBe(Harness.CLAUDE)
    expect(harness.harnesses).toHaveLength(1)

    const [ claude ] = harness.harnesses
    expect(claude?.harness).toBe(Harness.CLAUDE)
    expect(claude?.supportsMcp).toBe(true)
    expect(claude?.reportsCacheTokens).toBe(true)
  })

  it('rejects a bad secret with a code the caller can match on', async () => {
    const wrong = await Satellite.connect(running.url, 'wrong')

    const failure = await failureFrom(wrong.status())

    expect(failure.code).toBe(ErrorCode.AUTH_SECRET_INVALID)
    // Wrong credentials do not become right by trying again.
    expect(failure.retryable).toBe(false)
  })

  it('creates, reads, lists, and destroys a thread', async () => {
    const metadata = { tenant: 'acme' }

    const created = await client.threads().createWith(settings, {
      idempotencyKey: 'node-sdk-1',
      metadata
    })

    expect(created.deduplicated).toBe(false)
    expect(created.thread.state).toBe(ThreadState.IDLE)

    const repeat = await client.threads().createWith(settings, {
      idempotencyKey: 'node-sdk-1',
      metadata
    })
    expect(repeat.deduplicated).toBe(true)
    expect(repeat.thread.threadId).toBe(created.thread.threadId)

    const listed = await client.threads().list({ metadata })
    expect(listed.map((summary) => summary.threadId)).toContain(created.thread.threadId)

    await created.handle.destroy()

    // Gone, not missing. A destroyed thread reports what happened to it, so an
    // application can tell "my record is stale" from "my id is wrong".
    const gone = await failureFrom(created.handle.get())
    expect(gone.isGone()).toBe(true)
    expect(gone.isNotFound()).toBe(false)
    expect(gone.code).toBe(ErrorCode.THREAD_DESTROYED)
  })

  it('fails at attach rather than later when the thread is unknown', async () => {
    const failure = await failureFrom(client.threads().attach('not-a-thread'))

    expect(failure.isNotFound()).toBe(true)
  })

  it('lets a second client attach to a thread it did not create', async () => {
    // The property a horizontally scaled application depends on: a replica that
    // dies mid-turn costs nothing, because whichever replica comes up next can
    // pick the thread back up from its id alone.
    const created = await client.threads().create(settings)

    const other = await Satellite.connect(running.url, SECRET)
    const attached = await other.threads().attach(created.thread.threadId)

    expect(attached.id).toBe(created.thread.threadId)

    // An attached handle can do everything a creating handle can.
    const turn = await attached.startTurn('work through the probe')
    expect(turn.queued.status).toBe(TurnStatus.QUEUED)

    await turn.result()
    await created.handle.destroy()
  })

  it('runs a turn and returns its result', async () => {
    const created = await client.threads().create(settings)

    const turn = await created.handle.startTurnWith('replay the probe', {
      metadata: { requestId: 'req_2f8c11' }
    })

    const result = await turn.result()

    expect(result.status).toBe(TurnStatus.COMPLETED)
    expect(result.tokens?.totalTokens).toBeGreaterThan(0)
    // Absent rather than zero, all the way out to a consumer. A harness that
    // reports no reasoning accounting must not look like one that reported none.
    expect(result.tokens?.reasoningOutputTokens).toBeUndefined()
    // Failover cost stays visible: a turn where the first endpoint burned tokens
    // failing must not look identical to a clean run on the second.
    expect(result.byModel.length).toBeGreaterThanOrEqual(2)

    // The turn's metadata rides along on the result rather than needing a
    // lookup, because "whose job just finished" is the question being asked at
    // exactly this moment.
    expect(result.metadata).toEqual({ requestId: 'req_2f8c11' })

    const turns = await created.handle.turns()
    expect(turns.map((queued) => queued.turnId)).toContain(turn.id)

    await created.handle.destroy()
  })

  it('streams the whole turn over the socket', async () => {
    const created = await client.threads().create(settings)

    // Subscribed before the turn is queued, so nothing published between the two
    // is lost.
    const events = await created.handle.events()

    await created.handle.startTurn('replay the probe')

    const seen: string[] = []
    for await (const event of events) {
      seen.push(event.type)
      expect(event.threadId).toBe(created.thread.threadId)
      if (event.type === 'turn.completed') {
        break
      }
    }

    // The whole turn, watched rather than polled.
    expect(seen.at(0)).toBe('turn.started')
    expect(seen.at(-1)).toBe('turn.completed')
    expect(seen).toContain('tool.started')

    await created.handle.destroy()
  })

  it('resumes a stream from a sequence without losing an event', async () => {
    const created = await client.threads().create(settings)
    const turn = await created.handle.startTurn('replay the probe')
    await turn.result()

    const everything = await created.handle.events()
    const all = []
    for await (const event of everything) {
      all.push(event)
      if (event.type === 'turn.completed') {
        break
      }
    }

    // Exclusive: pass the last sequence actually handled and receive everything
    // since. This is how a replica that died mid-turn picks back up.
    const midpoint = all[Math.floor(all.length / 2)]
    expect(midpoint).toBeDefined()

    const resumed = await created.handle.events({ fromSequence: midpoint?.sequence })
    const after = []
    for await (const event of resumed) {
      after.push(event)
      if (event.type === 'turn.completed') {
        break
      }
    }

    expect(after.at(0)?.sequence).toBe((midpoint?.sequence ?? 0n) + 1n)
    expect(after.at(-1)?.type).toBe('turn.completed')

    await created.handle.destroy()
  })

  it('pauses, resumes, and drains a thread', async () => {
    const created = await client.threads().create(settings)

    const paused = await created.handle.pause()
    expect(paused.state).toBe(ThreadState.PAUSED)

    // The queue still accepts work while paused. It simply does not move.
    const first = await created.handle.startTurn('one')
    const second = await created.handle.startTurn('two')

    const cancelled = await created.handle.drain()
    expect(cancelled).toEqual(expect.arrayContaining([ first.id, second.id ]))

    const resumed = await created.handle.resume()
    expect(resumed.state).toBe(ThreadState.IDLE)

    await created.handle.destroy()
  })

  it('cancels a queued turn without touching the one that is running', async () => {
    const created = await client.threads().create(settings)

    // Queued behind a running turn, so cancelling is deterministic rather than a
    // race with the runner reaching a terminal state first.
    const inFlight = await created.handle.startTurn('replay the probe')
    const queued = await created.handle.startTurn('wait your turn')

    const stopped = await queued.cancel()
    expect(stopped.status).toBe(TurnStatus.CANCELLED)

    const result = await inFlight.result()
    expect(result.status).toBe(TurnStatus.COMPLETED)

    await created.handle.destroy()
  })

  it('lists incidents per thread and per satellite, and outlives the thread', async () => {
    const created = await client.threads().create(settings)

    // The stand-in stops after three lines, which is a harness that exited
    // without saying what it did. The satellite restarts it once and it does the
    // same thing again, so the turn ends with two incidents against it.
    const turn = await created.handle.startTurn('replay the probe [[truncate=3]]')
    const result = await turn.result()

    expect(result.status).toBe(TurnStatus.FAILED)
    // The counts ride along on the report, so the common case needs no query.
    expect(result.incidentCounts?.recovered).toBe(1)
    expect(result.incidentCounts?.fatal).toBe(1)

    // The recovery is recorded because it happened. A restart that worked looks
    // exactly like a turn that never stalled, and a harness wedging on every
    // turn is a pattern nobody sees unless the recovery is written down.
    const listed = await created.handle.incidents()
    expect(listed).toHaveLength(2)
    expect(listed.map((incident) => incident.disposition))
      .toEqual([ Disposition.RECOVERED, Disposition.FATAL ])
    expect(listed.map((incident) => incident.code))
      .toEqual([ ErrorCode.HARNESS_CRASHED, ErrorCode.HARNESS_CRASHED ])
    expect(listed.map((incident) => incident.turnId)).toEqual([ turn.id, turn.id ])

    // A filter narrows. A disposition nothing carries returns nothing rather
    // than falling back to everything.
    const blocked = await created.handle.incidents({ dispositions: [ Disposition.BLOCKED ] })
    expect(blocked).toHaveLength(0)

    // The satellite-wide listing finds the same incidents without being told
    // which thread to look at.
    const fleet = await client.incidents({ codes: [ ErrorCode.HARNESS_CRASHED ] })
    expect(fleet.map((incident) => incident.threadId)).toContain(created.thread.threadId)

    // Destroying the thread takes its workspace, its turns, and its events. The
    // evidence stays, which is the whole reason incidents are not ephemeral.
    await created.handle.destroy()

    const afterTeardown = await client.incidents({ threadIds: [ created.thread.threadId ] })
    expect(afterTeardown).toHaveLength(2)
  })

  it('runs alongside a second satellite on the same host', async () => {
    // What `ARSOX_PORT` bought. Before the override every satellite bound 8080,
    // so a second one on the same machine failed to start and this suite had to
    // check the port and skip itself.
    const second = await startSatellite()

    try {
      expect(second.url).not.toBe(running.url)

      const other = await Satellite.connect(second.url, SECRET)
      expect((await other.version()).protoMajor).toBe(1)

      // Two satellites, two databases. Neither sees the other's threads.
      const created = await other.threads().create(settings)
      const mine = await client.threads().list()
      expect(mine.map((summary) => summary.threadId)).not.toContain(created.thread.threadId)

      await created.handle.destroy()
    }
    finally {
      await second.stop()
    }
  })

  it('reports what it is holding, and stops holding a destroyed thread', async () => {
    const created = await client.threads().create(settings)

    const status = await client.status()
    expect(status.maxConcurrentThreads).toBe(2)
    expect(status.threads.map((summary) => summary.threadId)).toContain(created.thread.threadId)
    expect(status.disk?.availableBytes).toBeGreaterThan(0n)

    await created.handle.destroy()

    const settled = await client.status()
    expect(settled.threads.map((summary) => summary.threadId))
      .not.toContain(created.thread.threadId)
  })
})
