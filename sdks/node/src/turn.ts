// Copyright © 2026 Jalapeno Labs

import type { Turn } from './proto/arsox/turn/v1/turn_pb.js'
import type { TurnResult } from './proto/arsox/turn/v1/result_pb.js'
import type { Connection } from './transport.js'

// Core
import { setTimeout as sleep } from 'node:timers/promises'

// Misc
import { CancelTurnResponseSchema } from './proto/arsox/turn/v1/turn_pb.js'
import { GetTurnResponseSchema } from './proto/arsox/turn/v1/result_pb.js'
import { TurnStatus } from './proto/arsox/turn/v1/turn_pb.js'
import { ArsoxError } from './error.js'
import { RESULT_POLL_MILLISECONDS } from './constants.js'

/** A handle to one turn. */
export class TurnHandle {
  readonly id: string

  /** The turn as it was when queued. */
  readonly queued: Turn

  readonly #connection: Connection
  readonly #threadId: string

  constructor(connection: Connection, threadId: string, turn: Turn) {
    this.#connection = connection
    this.#threadId = threadId
    this.id = turn.turnId
    this.queued = turn
  }

  /** Reads the turn's current state. */
  async get(): Promise<Turn> {
    const response = await this.#connection.call(
      'GET',
      `/v1/threads/${this.#threadId}/turns/${this.id}`,
      GetTurnResponseSchema
    )

    if (!response.turn) {
      throw ArsoxError.transport('the satellite returned a turn with no turn in it')
    }

    return response.turn
  }

  /**
   * Waits for the turn to reach a terminal state and returns its result.
   *
   * Polls rather than watching the stream, because a caller awaiting a result
   * has not necessarily subscribed and should not have to.
   */
  async result(): Promise<TurnResult> {
    for (;;) {
      const response = await this.#connection.call(
        'GET',
        `/v1/threads/${this.#threadId}/turns/${this.id}`,
        GetTurnResponseSchema
      )

      const status = response.turn?.status ?? TurnStatus.UNSPECIFIED
      if (status !== TurnStatus.QUEUED && status !== TurnStatus.RUNNING) {
        if (!response.result) {
          throw ArsoxError.transport(
            'the satellite finished a turn without recording a result'
          )
        }
        return response.result
      }

      await sleep(RESULT_POLL_MILLISECONDS)
    }
  }

  /**
   * Asks the satellite to stop this turn.
   *
   * A running turn is asked to stop cooperatively first. Work already committed
   * to a branch survives either way.
   */
  async cancel(): Promise<Turn> {
    const response = await this.#connection.call(
      'POST',
      `/v1/threads/${this.#threadId}/turns/${this.id}/cancel`,
      CancelTurnResponseSchema
    )

    if (!response.turn) {
      throw ArsoxError.transport('the satellite cancelled a turn without saying so')
    }

    return response.turn
  }
}
