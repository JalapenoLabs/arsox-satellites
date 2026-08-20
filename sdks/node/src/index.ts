// Copyright © 2026 Jalapeno Labs

/**
 * The Node client for an Arsox satellite.
 *
 * Protobuf is always on the wire, in both directions, and no setting changes
 * that. What a consumer holds is an ordinary TypeScript object.
 *
 * ```typescript
 * const satellite = await Satellite.connect(url, process.env.ARSOX_SECRET)
 * const { handle } = await satellite.threads().create({
 *   idleTtl: { seconds: 3600n },
 *   budget: {}
 * })
 *
 * const turn = await handle.startTurn('Add rate limiting to the public API.')
 * const result = await turn.result()
 * ```
 *
 * The contract itself is re-exported below for the messages this surface
 * returns. Anything else in it is reachable at its generated path, for example
 * `@jalapenolabs/arsox-sdk/proto/arsox/settings/v1/permission_pb.js`.
 */

// ///////////////////////////// //
//          The client           //
// ///////////////////////////// //

export { Satellite, Threads } from './satellite.js'
export type {
  CreateThreadOptions,
  ListThreadsOptions,
  ThreadCreated,
  ThreadSettingsInit
} from './satellite.js'

export { ThreadHandle } from './thread.js'
export type { StartTurnOptions } from './thread.js'

export { TurnHandle } from './turn.js'

export { EventStream } from './events.js'
export type { EventStreamOptions } from './events.js'

export { ArsoxError } from './error.js'
export type { ArsoxErrorKind } from './error.js'

export type { IncidentQuery } from './incidents.js'

export { SDK_PROTO_MAJOR, SDK_PROTO_MINOR } from './constants.js'

// ///////////////////////////// //
//         The contract          //
// ///////////////////////////// //

// Enums are values, so they are exported as values: a consumer matches on
// `TurnStatus.COMPLETED`, never on a string.
export { ErrorCode } from './proto/arsox/error/v1/error_pb.js'
export { Disposition } from './proto/arsox/incident/v1/incident_pb.js'
export { Harness } from './proto/arsox/harness/v1/harness_pb.js'
export { ThreadOrder, ThreadState } from './proto/arsox/thread/v1/thread_pb.js'
export { TurnOrder, TurnStatus } from './proto/arsox/turn/v1/turn_pb.js'
export { Stage, StageDisposition } from './proto/arsox/turn/v1/result_pb.js'

// Schemas, for a consumer that needs to encode or decode a message itself.
export { ThreadSettingsSchema } from './proto/arsox/settings/v1/settings_pb.js'
export { ThreadEventSchema } from './proto/arsox/event/v1/event_pb.js'

export type {
  Duration,
  Money,
  PageRequest,
  PageResponse,
  Secret,
  Timestamp
} from './proto/arsox/common/v1/common_pb.js'
export type { Error as ContractError } from './proto/arsox/error/v1/error_pb.js'
export type { ThreadEvent } from './proto/arsox/event/v1/event_pb.js'
export type { GetHarnessResponse, HarnessCapabilities } from './proto/arsox/harness/v1/harness_pb.js'
export type { Incident, IncidentCounts } from './proto/arsox/incident/v1/incident_pb.js'
export type {
  DiskUsage,
  GetStatusResponse,
  GetVersionResponse
} from './proto/arsox/satellite/v1/satellite_pb.js'
export type { ThreadSettings } from './proto/arsox/settings/v1/settings_pb.js'
export type { Thread, ThreadSummary } from './proto/arsox/thread/v1/thread_pb.js'
export type { Turn } from './proto/arsox/turn/v1/turn_pb.js'
export type { StageOutcome, TurnResult } from './proto/arsox/turn/v1/result_pb.js'
export type { CostEstimate, TokenUsage } from './proto/arsox/usage/v1/usage_pb.js'
