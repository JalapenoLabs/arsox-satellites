// Copyright © 2026 Jalapeno Labs

// Core
import { describe, expect, it } from 'vitest'
import { create } from '@bufbuild/protobuf'

// Misc
import { ErrorCode, ErrorSchema } from './proto/arsox/error/v1/error_pb.js'
import { ArsoxError } from './error.js'
import { SDK_PROTO_MAJOR } from './constants.js'

describe('ArsoxError.contract', () => {
  it('reads retryable from the satellite rather than from the code', () => {
    // The flag is what makes an older SDK safe against a newer satellite. A code
    // matched against a hardcoded list would answer wrongly for anything added
    // after this build shipped.
    const failure = ArsoxError.contract(create(ErrorSchema, {
      code: ErrorCode.THREAD_LIMIT_REACHED,
      message: 'the satellite is at its concurrency cap',
      retryable: true
    }))

    expect(failure.retryable).toBe(true)
    expect(failure.code).toBe(ErrorCode.THREAD_LIMIT_REACHED)
  })

  it('carries the trace id that correlates an internal failure with the logs', () => {
    const failure = ArsoxError.contract(create(ErrorSchema, {
      code: ErrorCode.INTERNAL,
      message: 'a satellite bug',
      retryable: true,
      traceId: 'trace-0199'
    }))

    expect(failure.traceId).toBe('trace-0199')
  })

  it('separates a thread that is gone from one that was never found', () => {
    const destroyed = ArsoxError.contract(create(ErrorSchema, {
      code: ErrorCode.THREAD_DESTROYED,
      message: 'destroyed'
    }))
    const expired = ArsoxError.contract(create(ErrorSchema, {
      code: ErrorCode.THREAD_EXPIRED,
      message: 'expired'
    }))
    const missing = ArsoxError.contract(create(ErrorSchema, {
      code: ErrorCode.THREAD_NOT_FOUND,
      message: 'not found'
    }))

    expect([ destroyed.isGone(), expired.isGone(), missing.isGone() ])
      .toEqual([ true, true, false ])
    expect([ destroyed.isNotFound(), expired.isNotFound(), missing.isNotFound() ])
      .toEqual([ false, false, true ])
  })

  it('treats a missing turn as not found alongside a missing thread', () => {
    const failure = ArsoxError.contract(create(ErrorSchema, {
      code: ErrorCode.TURN_NOT_FOUND,
      message: 'unknown turn'
    }))

    expect(failure.isNotFound()).toBe(true)
    expect(failure.isGone()).toBe(false)
  })
})

describe('ArsoxError.transport', () => {
  it('names no code and is worth retrying', () => {
    // A failure that never reached a satellite has no contract code to report,
    // and a refused connection is usually a satellite that has not finished
    // starting.
    const failure = ArsoxError.transport('connect ECONNREFUSED 127.0.0.1:8080')

    expect(failure.kind).toBe('transport')
    expect(failure.code).toBeUndefined()
    expect(failure.retryable).toBe(true)
    expect(failure.isNotFound()).toBe(false)
    expect(failure.isGone()).toBe(false)
  })
})

describe('ArsoxError.incompatible', () => {
  it('does not resolve itself, so it is not retryable', () => {
    const failure = ArsoxError.incompatible(SDK_PROTO_MAJOR + 1, SDK_PROTO_MAJOR)

    expect(failure.isIncompatible()).toBe(true)
    expect(failure.retryable).toBe(false)
    expect(failure.code).toBeUndefined()
    expect(failure.message).toContain(`v${SDK_PROTO_MAJOR + 1}`)
  })

  it('is an Error, so it survives an ordinary catch and log', () => {
    const failure = ArsoxError.incompatible(2, 1)

    expect(failure).toBeInstanceOf(Error)
    expect(failure.name).toBe('ArsoxError')
  })
})
