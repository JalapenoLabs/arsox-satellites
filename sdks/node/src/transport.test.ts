// Copyright © 2026 Jalapeno Labs

import type { GetVersionResponse } from './proto/arsox/satellite/v1/satellite_pb.js'

// Core
import { describe, expect, expectTypeOf, it } from 'vitest'
import { fromBinary } from '@bufbuild/protobuf'

// Misc
import { ErrorCode, ErrorSchema } from './proto/arsox/error/v1/error_pb.js'
import { GetVersionResponseSchema } from './proto/arsox/satellite/v1/satellite_pb.js'
import { StartTurnRequestSchema } from './proto/arsox/turn/v1/turn_pb.js'
import { Connection, decodeResponse, encode } from './transport.js'
import { ArsoxError } from './error.js'

describe('encode', () => {
  it('round trips a request through the wire format', () => {
    const bytes = encode(StartTurnRequestSchema, {
      threadId: '0199-thread',
      prompt: 'replay the probe',
      metadata: { requestId: 'req_2f8c11' }
    })

    const decoded = fromBinary(StartTurnRequestSchema, bytes)

    expect(decoded.threadId).toBe('0199-thread')
    expect(decoded.prompt).toBe('replay the probe')
    expect(decoded.metadata).toEqual({ requestId: 'req_2f8c11' })
  })

  it('leaves an omitted optional field absent rather than empty', () => {
    // Absent is not zero, all the way out to the wire. `idempotency_key` has
    // explicit presence, so an empty string would be present and empty, which
    // is a different fact from never having been set.
    const decoded = fromBinary(
      StartTurnRequestSchema,
      encode(StartTurnRequestSchema, { threadId: '0199-thread', prompt: 'work' })
    )

    expect(decoded.idempotencyKey).toBeUndefined()
  })
})

describe('decodeResponse', () => {
  it('decodes a success body into the message it carries', () => {
    const body = encode(GetVersionResponseSchema, {
      satelliteVersion: '1.0.0',
      protoMajor: 1,
      protoMinor: 0
    })

    const decoded = decodeResponse(GetVersionResponseSchema, { status: 200, body })

    expectTypeOf(decoded).toEqualTypeOf<GetVersionResponse>()
    expect(decoded.satelliteVersion).toBe('1.0.0')
    expect(decoded.protoMajor).toBe(1)
  })

  it('turns a contract error into an ArsoxError carrying its code', () => {
    const body = encode(ErrorSchema, {
      code: ErrorCode.THREAD_DESTROYED,
      message: 'the thread was destroyed',
      retryable: false,
      details: { threadId: '0199-thread' }
    })

    const thrown = (): unknown => decodeResponse(GetVersionResponseSchema, { status: 410, body })

    expect(thrown).toThrowError(ArsoxError)
    try {
      thrown()
    }
    catch (error) {
      const failure = error as ArsoxError
      expect(failure.kind).toBe('contract')
      expect(failure.code).toBe(ErrorCode.THREAD_DESTROYED)
      expect(failure.retryable).toBe(false)
      expect(failure.details).toEqual({ threadId: '0199-thread' })
      // Gone, not missing. Those are different facts that lead to different
      // repairs, so the helpers must not collapse them.
      expect(failure.isGone()).toBe(true)
      expect(failure.isNotFound()).toBe(false)
      expect(failure.isIncompatible()).toBe(false)
      // The name is in the message so a log line is readable, and the code is
      // the field to match on.
      expect(failure.message).toContain('THREAD_DESTROYED')
    }
  })

  it('answers retryable for a code this build has never heard of', () => {
    // Codes are additive within a proto major, so an older SDK will meet ones it
    // cannot name. The retryable flag is always populated, which is what keeps
    // that safe.
    const body = encode(ErrorSchema, {
      // The assertion is the point of the test: this stands in for a code a
      // future satellite defines and this build's enum does not carry.
      code: 999_999 as ErrorCode,
      message: 'something new went wrong',
      retryable: true
    })

    try {
      decodeResponse(GetVersionResponseSchema, { status: 503, body })
      expect.unreachable('an error body should throw')
    }
    catch (error) {
      const failure = error as ArsoxError
      expect(failure.retryable).toBe(true)
      expect(failure.code).toBe(999_999)
      expect(failure.message).toContain('code 999999')
    }
  })

  it('reports a failure that did not come from a satellite as transport', () => {
    // An error body that will not decode means something other than a satellite
    // answered: a proxy, a load balancer, or an HTML error page.
    const notProtobuf = new TextEncoder().encode('<html>502 Bad Gateway</html>')

    try {
      decodeResponse(GetVersionResponseSchema, { status: 502, body: notProtobuf })
      expect.unreachable('an undecodable error body should throw')
    }
    catch (error) {
      const failure = error as ArsoxError
      expect(failure.kind).toBe('transport')
      expect(failure.code).toBeUndefined()
      // A refused or mangled response is usually worth trying again.
      expect(failure.retryable).toBe(true)
    }
  })

  it('names an empty error body rather than decoding it as an unspecified code', () => {
    try {
      decodeResponse(GetVersionResponseSchema, { status: 500, body: new Uint8Array() })
      expect.unreachable('an empty error body should throw')
    }
    catch (error) {
      const failure = error as ArsoxError
      expect(failure.kind).toBe('transport')
      expect(failure.message).toContain('500')
    }
  })

  it('reports an undecodable success body rather than handing back a wrong message', () => {
    // Field number 0 is not representable, so this cannot be mistaken for a
    // message with unknown fields.
    const garbage = new Uint8Array([ 0x00, 0xff, 0xff ])

    try {
      decodeResponse(GetVersionResponseSchema, { status: 200, body: garbage })
      expect.unreachable('an undecodable success body should throw')
    }
    catch (error) {
      expect((error as ArsoxError).kind).toBe('transport')
    }
  })
})

describe('Connection', () => {
  it('trims a trailing slash so paths never double up', () => {
    expect(new Connection('http://127.0.0.1:8080/', 'secret').baseUrl)
      .toBe('http://127.0.0.1:8080')
  })

  it('addresses the same satellite as a WebSocket', () => {
    expect(new Connection('http://127.0.0.1:8080', 'secret').socketUrl)
      .toBe('ws://127.0.0.1:8080')
    expect(new Connection('https://satellite.internal', 'secret').socketUrl)
      .toBe('wss://satellite.internal')
  })
})
