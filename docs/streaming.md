# Event streaming

Two WebSockets, both unidirectional and server to client. Nothing is ever sent up
them: every command is an ordinary HTTP request, and the socket only reports what
happened.

| Socket | Carries |
|---|---|
| `wss /v1/threads/{id}/stream` | everything that happens inside one thread |
| `wss /v1/stream` | satellite lifecycle only, never thread content |

Frames are binary protobuf. Both sit behind the same bearer check as every other
authenticated route, so a socket is not a way around it.

## Subscribe before you replay

This is the one ordering that matters, and getting it backwards produces a bug
that is close to invisible.

A consumer wants everything it missed followed by everything that happens next.
The obvious implementation reads history from the database and then subscribes to
live events. Anything published between those two steps is lost, and the loss
leaves no trace: the sequence numbers the consumer receives are contiguous with
what it read, just missing the middle.

So the handler subscribes first, replays from the database second, and then
discards any live event the replay already covered. There is a test that appends
events specifically in that window and asserts the delivered sequence is
`[1, 2, 3, 4, 5]` with no gap.

## Lagging is loud

The broadcast channel is bounded. A consumer that falls too far behind is dropped
by the channel itself, which surfaces as an error rather than as silence, and the
socket closes carrying `STREAM_CONSUMER_LAGGED`. Reconnecting with
`from_sequence` costs it nothing.

A consumer asking to resume from before retained history gets
`STREAM_SEQUENCE_EXPIRED` rather than a stream that quietly skips what it will
never see.

Close reasons carry the contract error code by name, so a client matches on the
same vocabulary it uses everywhere else instead of on a numeric close code that
means something different in every protocol.

## The bus is in-process

One broadcast channel carries every thread's events and subscribers filter by
thread. A channel per thread would avoid waking uninterested subscribers, at the
cost of a map created, reference counted, and torn down in step with thread
lifetimes. With a per-satellite concurrency cap in the single digits, that
bookkeeping costs more than the wakeups it saves.

**Live delivery does not cross processes.** The bus lives beside one `Store`
handle. A second handle opened on the same database file writes the same rows and
publishes to nobody, so its events would appear on replay and never live. That is
correct for a satellite, which is one process per database, and it is a trap for
anything that assumes otherwise. `assemble` returns the store it wired for
exactly this reason, and a test that opened its own handle is how the trap was
found.

## Published after the write commits

An event reaches the bus only once its row is committed. Publishing first would
let a live consumer see something a reconnecting consumer could never replay,
which is two views of the same stream disagreeing.

The store owns the publish rather than leaving it to callers, because a publish
that can be forgotten at one call site is a subscriber that silently misses
events.

## Incidents ride the same append

Every incident is written to the database and put on its thread's stream by one
store call, so a failure cannot land in one and miss the other. The event goes
first, which is what lets the row carry the sequence its frame landed at.

Satellite-scoped incidents reach no thread socket, because they belong to no
thread and these sockets carry thread content only. See
[incidents](./incidents.md).

## JSON frames

Documented in the README as a debugging affordance for hand-driven clients, and
not implemented. A handshake requesting the `arsox.json.v1` subprotocol is
refused with `STREAM_SUBPROTOCOL_UNSUPPORTED` rather than being ignored and
handed protobuf, which would have the client decoding binary as text.

It needs protobuf-to-JSON, which prost does not provide. `neoeinstein-prost-serde`
is the candidate.

## Roadmap

- **JSON frames**, as above.
- **Control stream coverage.** Thread created and destroyed are published today.
  Queue depth, health transitions, and budget warnings are defined in the
  contract and not yet emitted.
- **Retention.** Nothing prunes the event log, so `STREAM_SEQUENCE_EXPIRED`
  cannot currently fire. The check exists so the behaviour is right the day
  pruning lands.
