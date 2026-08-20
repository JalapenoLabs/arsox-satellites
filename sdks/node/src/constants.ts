// Copyright © 2026 Jalapeno Labs

/**
 * The proto major this SDK speaks.
 *
 * The SDK refuses a satellite serving a higher major rather than failing later
 * with a confusing decode error, and warns once on a higher minor before
 * proceeding, because additive fields it does not know about are safely ignored.
 */
export const SDK_PROTO_MAJOR = 1

/** The proto minor this SDK was built against. */
export const SDK_PROTO_MINOR = 0

/**
 * What the SDK sends and asks for, in both directions, always.
 *
 * JSON exists on the satellite as a debugging affordance for hand-driven
 * clients. No setting in this SDK changes what goes on the wire.
 */
export const PROTOBUF_CONTENT_TYPE = 'application/protobuf'

/**
 * How often a pending turn is re-read while waiting for it to finish.
 *
 * Polling rather than watching the stream, because a caller awaiting a result
 * has not necessarily subscribed and should not have to.
 */
export const RESULT_POLL_MILLISECONDS = 500
