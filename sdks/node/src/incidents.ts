// Copyright © 2026 Jalapeno Labs

import type { TimestampSchema } from './proto/arsox/common/v1/common_pb.js'
import type { Disposition } from './proto/arsox/incident/v1/incident_pb.js'
import type { ErrorCode } from './proto/arsox/error/v1/error_pb.js'
import type { MessageInitShape } from '@bufbuild/protobuf'

// Core
import { ListIncidentsRequestSchema } from './proto/arsox/incident/v1/incident_pb.js'

// Misc
import { encode } from './transport.js'

/**
 * Which incidents a listing should return.
 *
 * Every filter is "match any of these", and an empty one does not filter rather
 * than matching nothing, so `{}` asks for everything.
 */
export type IncidentQuery = {
  /** Ignored by a thread's own listing, which is already scoped to one. */
  threadIds?: string[]

  turnIds?: string[]
  memberIds?: string[]
  codes?: ErrorCode[]
  dispositions?: Disposition[]

  /**
   * Half-open window: at or after `occurredAfter`, strictly before
   * `occurredBefore`, so adjacent windows tile rather than overlap.
   */
  occurredAfter?: MessageInitShape<typeof TimestampSchema>
  occurredBefore?: MessageInitShape<typeof TimestampSchema>

  /** Omitted or zero asks the satellite for its default page size. */
  limit?: number

  /** Cursor from a previous listing's `nextCursor`. Empty starts at the beginning. */
  cursor?: string
}

/**
 * Renders a query as the request the satellite reads.
 *
 * `scopedTo` is the thread a per-thread listing is already about. The path wins
 * over any `threadIds` in the query, so the URL says what it looks like it says.
 */
export function encodeIncidentQuery(query: IncidentQuery, scopedTo?: string): Uint8Array {
  return encode(ListIncidentsRequestSchema, {
    threadIds: scopedTo
      ? [ scopedTo ]
      : query.threadIds ?? [],
    turnIds: query.turnIds ?? [],
    memberIds: query.memberIds ?? [],
    codes: query.codes ?? [],
    dispositions: query.dispositions ?? [],
    occurredAfter: query.occurredAfter,
    occurredBefore: query.occurredBefore,
    page: {
      limit: query.limit ?? 0,
      cursor: query.cursor ?? ''
    }
  })
}
