// Copyright © 2026 Jalapeno Labs

import type { Incident } from './proto/arsox/incident/v1/incident_pb.js'
import type { Thread } from './proto/arsox/thread/v1/thread_pb.js'
import type { Turn } from './proto/arsox/turn/v1/turn_pb.js'
import type { Connection } from './transport.js'
import type { EventStreamOptions } from './events.js'
import type { IncidentQuery } from './incidents.js'

// Core
import {
  DestroyThreadResponseSchema,
  DrainThreadResponseSchema,
  GetThreadResponseSchema,
  PauseThreadResponseSchema,
  ResumeThreadResponseSchema
} from './proto/arsox/thread/v1/thread_pb.js'
import {
  ListTurnsRequestSchema,
  ListTurnsResponseSchema,
  StartTurnRequestSchema,
  StartTurnResponseSchema,
  TurnOrder
} from './proto/arsox/turn/v1/turn_pb.js'
import { ListIncidentsResponseSchema } from './proto/arsox/incident/v1/incident_pb.js'

// Misc
import { encode } from './transport.js'
import { encodeIncidentQuery } from './incidents.js'
import { EventStream } from './events.js'
import { TurnHandle } from './turn.js'
import { ArsoxError } from './error.js'

/** Correlation data and deduplication for one queued turn. */
export type StartTurnOptions = {
  /**
   * Deduplicates retries of this call. A second request carrying a key the
   * satellite has already seen returns the original turn rather than queueing a
   * second one.
   */
  idempotencyKey?: string

  /**
   * Your own correlation data, stored verbatim and handed back on the result.
   * The satellite never reads it and it never reaches an agent.
   */
  metadata?: Record<string, string>
}

/**
 * A handle to one thread.
 *
 * Holds an id and a connection. All the state lives on the satellite, which is
 * what makes a handle disposable and a thread durable. Any process with the URL,
 * the secret, and the id can attach and do everything the process that created
 * the thread could: there is no handoff, no lease, and no ownership.
 */
export class ThreadHandle {
  readonly id: string

  readonly #connection: Connection

  constructor(connection: Connection, threadId: string) {
    this.#connection = connection
    this.id = threadId
  }

  /** Reads the thread's current state. */
  async get(): Promise<Thread> {
    const response = await this.#connection.call(
      'GET',
      `/v1/threads/${this.id}`,
      GetThreadResponseSchema
    )

    if (!response.thread) {
      throw ArsoxError.transport('the satellite returned a thread with no thread in it')
    }

    return response.thread
  }

  /**
   * Queues a turn.
   *
   * Returns as soon as the turn is queued. Await {@link TurnHandle.result} for
   * the outcome.
   */
  async startTurn(prompt: string): Promise<TurnHandle> {
    return this.startTurnWith(prompt, {})
  }

  /** Queues a turn with an idempotency key and correlation metadata. */
  async startTurnWith(prompt: string, options: StartTurnOptions): Promise<TurnHandle> {
    const response = await this.#connection.call(
      'POST',
      `/v1/threads/${this.id}/turns`,
      StartTurnResponseSchema,
      encode(StartTurnRequestSchema, {
        threadId: this.id,
        prompt,
        idempotencyKey: options.idempotencyKey,
        metadata: options.metadata ?? {}
      })
    )

    if (!response.turn) {
      throw ArsoxError.transport('the satellite queued a turn without returning it')
    }

    return new TurnHandle(this.#connection, this.id, response.turn)
  }

  /** Lists this thread's turns, oldest first. Returns one page. */
  async turns(): Promise<Turn[]> {
    const response = await this.#connection.call(
      'GET',
      `/v1/threads/${this.id}/turns`,
      ListTurnsResponseSchema,
      encode(ListTurnsRequestSchema, {
        threadId: this.id,
        statuses: [],
        page: {},
        orderBy: TurnOrder.UNSPECIFIED,
        descending: false
      })
    )

    return response.turns
  }

  /**
   * Lists this thread's incidents, oldest first.
   *
   * Answers after the thread is expired or destroyed, because incidents carry
   * their own retention and the workspace's collection never touches them.
   * `threadIds` on the query is ignored: this listing is already scoped.
   */
  async incidents(query: IncidentQuery = {}): Promise<Incident[]> {
    const response = await this.#connection.call(
      'GET',
      `/v1/threads/${this.id}/incidents`,
      ListIncidentsResponseSchema,
      encodeIncidentQuery(query, this.id)
    )

    return response.incidents
  }

  /**
   * Destroys the thread and everything under it.
   *
   * Incidents survive on their own retention, because "why did last night go
   * wrong" is asked after the workspace is gone.
   */
  async destroy(): Promise<Thread> {
    const response = await this.#connection.call(
      'DELETE',
      `/v1/threads/${this.id}`,
      DestroyThreadResponseSchema
    )

    if (!response.thread) {
      throw ArsoxError.transport('the satellite destroyed a thread without saying so')
    }

    return response.thread
  }

  /**
   * Stops the thread claiming queued work, without losing anything.
   *
   * Turns may still be submitted and still queue; the queue simply does not move
   * until the thread resumes. This is the state an operator reaches for when
   * destroying the thread would lose the workspace.
   */
  async pause(): Promise<Thread> {
    const response = await this.#connection.call(
      'POST',
      `/v1/threads/${this.id}/pause`,
      PauseThreadResponseSchema
    )

    if (!response.thread) {
      throw ArsoxError.transport('the satellite paused a thread without saying so')
    }

    return response.thread
  }

  /** Returns a paused thread to service. */
  async resume(): Promise<Thread> {
    const response = await this.#connection.call(
      'POST',
      `/v1/threads/${this.id}/resume`,
      ResumeThreadResponseSchema
    )

    if (!response.thread) {
      throw ArsoxError.transport('the satellite resumed a thread without saying so')
    }

    return response.thread
  }

  /**
   * Cancels every queued turn, leaving any running turn alone.
   *
   * One call rather than a loop, because cancelling turns one at a time races
   * the runner claiming them, and that is a race an operator should not have to
   * win. Pause first if the intent is to stop the thread rather than clear a
   * backlog.
   *
   * Returns the ids it cancelled, so a caller that needs the thread fully
   * stopped can see there is still something running and cancel it explicitly.
   */
  async drain(): Promise<string[]> {
    const response = await this.#connection.call(
      'POST',
      `/v1/threads/${this.id}/drain`,
      DrainThreadResponseSchema
    )

    return response.cancelledTurnIds
  }

  /**
   * Streams this thread's events.
   *
   * Resolves once the socket is open, so a refused handshake is reported here
   * rather than at the first event. Pass `fromSequence` to resume after the last
   * sequence a previous consumer handled.
   */
  async events(options: EventStreamOptions = {}): Promise<EventStream> {
    const fromSequence = options.fromSequence ?? 0
    const stream = new EventStream(
      `${this.#connection.socketUrl}/v1/threads/${this.id}/stream?from_sequence=${fromSequence}`,
      this.#connection.secret
    )

    await stream.opened()

    return stream
  }
}
