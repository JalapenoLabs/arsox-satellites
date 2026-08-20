// Copyright © 2026 Jalapeno Labs

import type { GetHarnessResponse } from './proto/arsox/harness/v1/harness_pb.js'
import type { GetStatusResponse, GetVersionResponse } from './proto/arsox/satellite/v1/satellite_pb.js'
import type { Incident } from './proto/arsox/incident/v1/incident_pb.js'
import type { Thread, ThreadSummary } from './proto/arsox/thread/v1/thread_pb.js'
import type { ThreadSettingsSchema } from './proto/arsox/settings/v1/settings_pb.js'
import type { MessageInitShape } from '@bufbuild/protobuf'
import type { IncidentQuery } from './incidents.js'

// Core
import { GetHarnessResponseSchema } from './proto/arsox/harness/v1/harness_pb.js'
import { GetStatusResponseSchema, GetVersionResponseSchema } from './proto/arsox/satellite/v1/satellite_pb.js'
import { ListIncidentsResponseSchema } from './proto/arsox/incident/v1/incident_pb.js'
import {
  CreateThreadRequestSchema,
  CreateThreadResponseSchema,
  ListThreadsRequestSchema,
  ListThreadsResponseSchema,
  ThreadOrder
} from './proto/arsox/thread/v1/thread_pb.js'

// Misc
import { Connection, encode } from './transport.js'
import { encodeIncidentQuery } from './incidents.js'
import { ThreadHandle } from './thread.js'
import { ArsoxError } from './error.js'
import { SDK_PROTO_MAJOR } from './constants.js'

/**
 * The settings a thread declares when it is opened.
 *
 * An init shape rather than the protobuf message itself, so a caller writes an
 * ordinary object literal and never constructs a protobuf type by hand. Protobuf
 * is on the wire; what you hold is a plain object.
 */
export type ThreadSettingsInit = MessageInitShape<typeof ThreadSettingsSchema>

/** Correlation data and deduplication for one created thread. */
export type CreateThreadOptions = {
  /**
   * Deduplicates retries of this call.
   *
   * Without it, a create that times out in transit leaves the caller unable to
   * tell "not created" from "created, response lost", and the only safe move is
   * to retry and leak a whole workspace.
   */
  idempotencyKey?: string

  /**
   * Your own correlation data, stored verbatim and handed back untouched.
   * Filterable through {@link Threads.list}. Never redacted, so credentials
   * belong in `env` with `isSecret` instead.
   */
  metadata?: Record<string, string>
}

/** A newly opened thread. */
export type ThreadCreated = {
  thread: Thread

  /**
   * True when an idempotency key matched an existing thread, so this is that
   * thread rather than a new one.
   */
  deduplicated: boolean

  handle: ThreadHandle
}

/** How a listing is ordered. */
export type ListThreadsOptions = {
  /** Every entry must match for a thread to be returned. Empty does not filter. */
  metadata?: Record<string, string>

  /**
   * Creation order by default, because it is free: thread ids are UUIDv7 and
   * already sort by time. Last activity is the order an operator scanning a
   * fleet actually wants.
   */
  orderBy?: ThreadOrder

  descending?: boolean
}

/**
 * A connection to one satellite.
 *
 * Protobuf is always on the wire, in both directions. What you hold is an
 * ordinary TypeScript object.
 */
export class Satellite {
  readonly #connection: Connection

  private constructor(connection: Connection) {
    this.#connection = connection
  }

  /**
   * Connects, and refuses a satellite this SDK cannot speak to.
   *
   * The version check happens here rather than lazily, so a mismatch is reported
   * at the point a human can act on it instead of surfacing as a decode failure
   * three calls later.
   */
  static async connect(url: string, secret: string): Promise<Satellite> {
    const satellite = new Satellite(new Connection(url, secret))
    const version = await satellite.version()

    if (version.protoMajor > SDK_PROTO_MAJOR) {
      throw ArsoxError.incompatible(version.protoMajor, SDK_PROTO_MAJOR)
    }
    if (version.protoMajor === SDK_PROTO_MAJOR && version.protoMinor > 0) {
      // Additive fields this SDK does not know about are ignored, which is safe.
      // Saying so once beats saying nothing.
      console.warn(
        `arsox: satellite serves proto v${version.protoMajor}.${version.protoMinor} and this SDK `
        + `was built against v${SDK_PROTO_MAJOR}.0; unknown fields will be ignored`
      )
    }

    return satellite
  }

  /** Reports the satellite version and the proto contract it serves. */
  async version(): Promise<GetVersionResponse> {
    return this.#connection.call('GET', '/v1/version', GetVersionResponseSchema)
  }

  /** Reports what the satellite is currently doing. */
  async status(): Promise<GetStatusResponse> {
    return this.#connection.call('GET', '/v1/status', GetStatusResponseSchema)
  }

  /**
   * Reports which harnesses this satellite offers and what each supports.
   *
   * Worth calling before relying on a capability. Discovering that a harness has
   * no plan mode by its absence, three turns into a run, is the failure this
   * endpoint exists to prevent.
   */
  async harness(): Promise<GetHarnessResponse> {
    return this.#connection.call('GET', '/v1/harness', GetHarnessResponseSchema)
  }

  /**
   * Lists incidents across every thread this satellite has held.
   *
   * Incidents outlive the threads they describe, so this answers for threads
   * that were collected long ago. That is the point: "why did last night's run
   * go wrong" is asked after the workspace is gone.
   *
   * Returns one page, oldest first. Raise `limit` or page with `cursor`.
   */
  async incidents(query: IncidentQuery = {}): Promise<Incident[]> {
    const response = await this.#connection.call(
      'GET',
      '/v1/incidents',
      ListIncidentsResponseSchema,
      encodeIncidentQuery(query)
    )

    return response.incidents
  }

  /** Threads on this satellite. */
  threads(): Threads {
    return new Threads(this.#connection)
  }
}

/** Thread operations on one satellite. */
export class Threads {
  readonly #connection: Connection

  constructor(connection: Connection) {
    this.#connection = connection
  }

  /** Opens a thread. */
  async create(settings: ThreadSettingsInit): Promise<ThreadCreated> {
    return this.createWith(settings, {})
  }

  /** Opens a thread with an idempotency key and correlation metadata. */
  async createWith(
    settings: ThreadSettingsInit,
    options: CreateThreadOptions
  ): Promise<ThreadCreated> {
    const response = await this.#connection.call(
      'POST',
      '/v1/threads',
      CreateThreadResponseSchema,
      encode(CreateThreadRequestSchema, {
        settings,
        idempotencyKey: options.idempotencyKey,
        metadata: options.metadata ?? {}
      })
    )

    if (!response.thread) {
      throw ArsoxError.transport('the satellite created a thread without returning it')
    }

    return {
      thread: response.thread,
      deduplicated: response.deduplicated,
      handle: new ThreadHandle(this.#connection, response.thread.threadId)
    }
  }

  /**
   * Picks up a thread that already exists.
   *
   * There is no handoff and no lease. The process that created the thread has no
   * privileged claim on it and may have exited hours ago.
   *
   * Reads the thread before returning, so attaching to a typo fails here rather
   * than at the first operation on the handle.
   */
  async attach(threadId: string): Promise<ThreadHandle> {
    const handle = new ThreadHandle(this.#connection, threadId)
    await handle.get()

    return handle
  }

  /** Lists threads. Returns one page. */
  async list(options: ListThreadsOptions = {}): Promise<ThreadSummary[]> {
    const response = await this.#connection.call(
      'GET',
      '/v1/threads',
      ListThreadsResponseSchema,
      encode(ListThreadsRequestSchema, {
        states: [],
        metadata: options.metadata ?? {},
        page: {},
        orderBy: options.orderBy ?? ThreadOrder.UNSPECIFIED,
        descending: options.descending ?? false
      })
    )

    return response.threads
  }
}
