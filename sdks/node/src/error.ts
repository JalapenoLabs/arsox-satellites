// Copyright © 2026 Jalapeno Labs

import type { Error as ContractError } from './proto/arsox/error/v1/error_pb.js'
import type { JsonObject } from '@bufbuild/protobuf'

// Core
import { ErrorCode } from './proto/arsox/error/v1/error_pb.js'

/**
 * Where a failure came from.
 *
 * A discriminant rather than a subclass per case, so a caller can `switch` on it
 * without `instanceof`, and so the SDK never has to recover its own meaning by
 * reading its own message back.
 */
export type ArsoxErrorKind =
  /** The satellite answered with a contract error. `code` is populated. */
  | 'contract'
  /** The satellite could not be reached, or answered something undecodable. */
  | 'transport'
  /** The satellite serves a proto major this SDK does not. */
  | 'incompatible'

/**
 * Anything that can go wrong talking to a satellite.
 *
 * One class rather than a family, because a caller handling a failure almost
 * always wants the same three questions answered regardless of where it came
 * from: which contract code was it, is it worth retrying, and what happened.
 *
 * Match on {@link ArsoxError.code}, never on the message. Wording may change
 * within a major version; codes may not.
 */
export class ArsoxError extends Error {
  override readonly name = 'ArsoxError'

  readonly kind: ArsoxErrorKind

  /**
   * The contract code, when the satellite named one.
   *
   * Undefined for a failure that never reached a satellite, such as a refused
   * connection or a version mismatch caught before the first call.
   */
  readonly code: ErrorCode | undefined

  /**
   * Whether retrying the same request could plausibly succeed.
   *
   * Read from the satellite's own flag rather than matched against a list of
   * codes, which is what makes an older SDK safe against a newer satellite: a
   * code this build has never heard of still gets a usable answer.
   */
  readonly retryable: boolean

  /**
   * Structured context, keyed per code. `field` and `reason` for a validation
   * failure, `argv` for a denied command, `host` for a denied domain.
   */
  readonly details: JsonObject | undefined

  /**
   * Correlates the failure with the satellite's own logs. Present on `INTERNAL`
   * and absent on errors that are the caller's to fix.
   */
  readonly traceId: string | undefined

  private constructor(
    message: string,
    kind: ArsoxErrorKind,
    code: ErrorCode | undefined,
    retryable: boolean,
    details?: JsonObject,
    traceId?: string
  ) {
    super(message)
    this.kind = kind
    this.code = code
    this.retryable = retryable
    this.details = details
    this.traceId = traceId
  }

  /** The satellite answered with a contract error. */
  static contract(error: ContractError): ArsoxError {
    // Numeric enums carry a reverse mapping, but only for values this build
    // knows. Codes are additive within a proto major, so an unnamed one is
    // reported by number rather than as `undefined`.
    const named = ErrorCode[error.code] ?? `code ${error.code}`

    return new ArsoxError(
      `${named}: ${error.message}`,
      'contract',
      error.code,
      error.retryable,
      error.details,
      error.traceId
    )
  }

  /**
   * The satellite could not be reached, or answered something undecodable.
   *
   * Retryable, because a refused connection is usually a satellite that has not
   * finished starting.
   */
  static transport(message: string): ArsoxError {
    return new ArsoxError(`could not reach the satellite: ${message}`, 'transport', undefined, true)
  }

  /** The satellite serves a proto major this SDK does not. */
  static incompatible(satelliteMajor: number, sdkMajor: number): ArsoxError {
    return new ArsoxError(
      `this satellite serves proto v${satelliteMajor} and this SDK speaks v${sdkMajor}. `
      + 'Upgrade the SDK, or point at a satellite on the same major.',
      'incompatible',
      undefined,
      // A version mismatch does not resolve itself.
      false
    )
  }

  /** Whether the satellite reported that the thing asked for does not exist. */
  isNotFound(): boolean {
    return this.code === ErrorCode.THREAD_NOT_FOUND || this.code === ErrorCode.TURN_NOT_FOUND
  }

  /**
   * Whether the thread existed and no longer does.
   *
   * Distinct from {@link ArsoxError.isNotFound} on purpose. A thread that
   * expired or was destroyed is a thread your application probably has a record
   * of, and the right response is usually to open a new one and carry the work
   * over. A thread that was never found is a bad id, and opening a new one would
   * paper over the bug.
   *
   * Read {@link ArsoxError.code} when the difference between expired and
   * destroyed matters: an expired thread means the TTL was shorter than the way
   * the application actually uses it.
   */
  isGone(): boolean {
    return this.code === ErrorCode.THREAD_EXPIRED || this.code === ErrorCode.THREAD_DESTROYED
  }

  /** Whether this SDK is too old for the satellite it was pointed at. */
  isIncompatible(): boolean {
    return this.kind === 'incompatible'
  }
}
