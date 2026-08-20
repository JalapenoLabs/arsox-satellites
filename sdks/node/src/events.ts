// Copyright © 2026 Jalapeno Labs

import type { ThreadEvent } from './proto/arsox/event/v1/event_pb.js'

// Core
import WebSocket from 'ws'
import { fromBinary } from '@bufbuild/protobuf'

// Misc
import { ThreadEventSchema } from './proto/arsox/event/v1/event_pb.js'
import { ArsoxError } from './error.js'

/** How a consumer asks for the part of the stream it has not seen. */
export type EventStreamOptions = {
  /**
   * Resume after this sequence.
   *
   * Exclusive: pass the last sequence you actually handled and receive
   * everything since. This is how a replica that died mid-turn picks up without
   * losing an event. Omit it to start at the beginning of retained history.
   */
  fromSequence?: number | bigint
}

/** Someone waiting on `next()` with nothing buffered to give them. */
type Waiter = {
  resolve: (result: IteratorResult<ThreadEvent, undefined>) => void
  reject: (error: unknown) => void
}

/**
 * One thread's events, in sequence order.
 *
 * An async iterator rather than an emitter, so the ordinary
 * `for await (const event of stream)` reads events in the order the satellite
 * numbered them. Leaving the loop closes the socket.
 *
 * The socket is unidirectional, server to client. Nothing is ever sent up it:
 * every command is an ordinary HTTP request, and this only reports what
 * happened.
 */
export class EventStream implements AsyncIterableIterator<ThreadEvent, undefined> {
  readonly #socket: WebSocket
  readonly #buffered: ThreadEvent[] = []
  readonly #waiting: Waiter[] = []
  readonly #handshake: Promise<void>

  #failure: ArsoxError | undefined
  #finished = false

  constructor(url: string, secret: string) {
    // `ws` rather than Node's global WebSocket. The stream sits behind the same
    // bearer check as every other authenticated route, and the WHATWG WebSocket
    // API has no way to set a request header on the handshake.
    this.#socket = new WebSocket(url, {
      headers: { Authorization: `Bearer ${secret}` }
    })

    this.#handshake = new Promise((resolve, reject) => {
      this.#socket.once('open', () => resolve())
      this.#socket.once('error', (error: Error) => reject(ArsoxError.transport(error.message)))
    })

    this.#socket.on('message', (data: Buffer, isBinary: boolean) => {
      if (!isBinary) {
        // Text frames are not part of the contract. The JSON subprotocol is a
        // debugging affordance for hand-driven clients and is never what an SDK
        // negotiates.
        console.debug('arsox: ignoring a non-binary frame on the thread stream')
        return
      }

      try {
        this.#deliver(fromBinary(ThreadEventSchema, data))
      }
      catch (error) {
        this.#fail(ArsoxError.transport(`undecodable event frame: ${String(error)}`))
      }
    })

    this.#socket.on('error', (error: Error) => this.#fail(ArsoxError.transport(error.message)))

    this.#socket.on('close', (_code: number, reason: Buffer) => {
      // A close frame carries the contract code in its reason, so a lagging
      // consumer learns it was dropped rather than watching the stream stop.
      const said = reason.toString()
      if (said) {
        this.#fail(ArsoxError.transport(`stream closed: ${said}`))
        return
      }
      this.#finish()
    })
  }

  /**
   * Resolves once the socket is open, so a refused handshake is reported where
   * the caller asked for the stream rather than at the first `next()`.
   */
  async opened(): Promise<void> {
    await this.#handshake
  }

  [Symbol.asyncIterator](): AsyncIterableIterator<ThreadEvent, undefined> {
    return this
  }

  async next(): Promise<IteratorResult<ThreadEvent, undefined>> {
    // Buffered events are delivered even after a failure or a close, because
    // they are events the satellite already sent and the consumer has not read.
    const buffered = this.#buffered.shift()
    if (buffered) {
      return { value: buffered, done: false }
    }

    if (this.#failure) {
      throw this.#failure
    }
    if (this.#finished) {
      return { value: undefined, done: true }
    }

    return new Promise<IteratorResult<ThreadEvent, undefined>>((resolve, reject) => {
      this.#waiting.push({ resolve, reject })
    })
  }

  /** Called when a `for await` loop is left, which is what closes the socket. */
  async return(): Promise<IteratorResult<ThreadEvent, undefined>> {
    this.close()
    return { value: undefined, done: true }
  }

  /** Stops reading and closes the socket. */
  close(): void {
    this.#finish()
    this.#socket.close()
  }

  #deliver(event: ThreadEvent): void {
    const waiter = this.#waiting.shift()
    if (waiter) {
      waiter.resolve({ value: event, done: false })
      return
    }

    this.#buffered.push(event)
  }

  #fail(error: ArsoxError): void {
    if (this.#failure || this.#finished) {
      return
    }

    this.#failure = error
    for (const waiter of this.#waiting.splice(0)) {
      waiter.reject(error)
    }
  }

  #finish(): void {
    if (this.#finished) {
      return
    }

    this.#finished = true
    for (const waiter of this.#waiting.splice(0)) {
      waiter.resolve({ value: undefined, done: true })
    }
  }
}
