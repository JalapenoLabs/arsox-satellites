# Persistence

SQLite through `sqlx`, in WAL mode, with migrations compiled into the binary.
There is no external database to run: a satellite is one container and one
volume.

The file lives at `/var/arsox/arsox.db`, overridable with `ARSOX_DB_PATH`.
**Mount `/var/arsox` as a named volume.** It holds threads, queued turns, event
history, and incidents, so losing it means losing every thread you intended to
resume and every record of what went wrong.

## Three decisions shape the schema

**Contract messages are stored encoded, not re-modelled.** `threads.settings`
and `turns.result` are protobuf bytes rather than a column per field. The
contract has 147 messages; mirroring it in DDL would mean a migration for every
additive proto change, and the two models would drift the first time somebody
forgot. What earns its own column is what gets queried or ordered by.

**Anything filterable gets a real table.** Thread metadata is queried by
`ListThreads`, so it is rows rather than a blob. That is the whole test for
whether something deserves columns, and it is why `settings` does not get them.

**Timestamps are nanoseconds since the epoch, as one signed integer.** The
contract carries seconds plus nanos plus a display zone. The zone is
presentational and every instant recorded is UTC, so it is reconstructed rather
than stored. A signed 64-bit nanosecond count spans 1678 to 2262.

## The queue is a query

There is no queue table. Turns run in the order they were submitted, so the next
one to run is the oldest `QUEUED` row for the thread, served by one index on
`(thread_id, status, queued_at)`.

A separate structure would be a second source of truth that could fall out of
step with the turns it describes, and reconciling the two after a crash is a
problem worth not having.

## Sequence numbers are issued inside the append

`append_event` increments `threads.latest_sequence` and inserts the row in one
transaction, using SQLite's `UPDATE ... RETURNING`.

Handing out the number first and writing afterwards would leave a hole in the
stream whenever the write failed, and a consumer replaying across that hole waits
forever for an event that does not exist. Gapless and monotonic is not a nicety
here: it is what makes `from_sequence` resumption correct.

## Idempotency

Thread keys are globally unique; turn keys are unique **per thread**. Two threads
may each retry a submit carrying the caller's own request id without colliding
with each other.

Both are checked before inserting rather than by catching the unique violation,
because the existing row has to be returned either way and a successful read is
cheaper than a failed write followed by a read.

## Incidents outlive their thread

`incidents` is deliberately not foreign-keyed to `threads`, and `thread_id` is
nullable.

Not cascading is what lets "why did last night's run go wrong" be answered after
the workspace is collected. The nullable column is what lets a failure belong to
the satellite rather than to any thread: an unreachable LLM proxy at boot, a
failed migration, a volume that will not mount.

## Durability

`synchronous = NORMAL` rather than `FULL`. The failure this database is meant to
survive is a crash of the process, which NORMAL covers. FULL would fsync on every
event append, and events arrive continuously throughout a turn.

The pool holds a single connection. SQLite takes one writer at a time regardless,
and an in-memory database is per-connection, so one connection keeps the test
path and the production path honest rather than fast in one and wrong in the
other.

## Verifying it

`cargo test` covers the store against an in-memory database, including that
events survive a reopen.

`scripts/smoke-test.py` drives a running container with real protobuf requests:
thread and turn lifecycle, idempotency, metadata filtering, and every error code
the endpoints can return. It is written in Python on purpose, because a Python
client decoding what a Rust satellite encoded is the cross-language contract
proving itself rather than being asserted.

```bash
docker build -t arsox-satellite:dev .
docker run -d --name arsox-test -p 18080:8080 \
  -e ARSOX_SECRET=container-secret \
  -v arsox-test-db:/var/arsox arsox-satellite:dev
python scripts/smoke-test.py
```

## Roadmap

- **Idle TTL collection.** `expires_at` is maintained and nothing sweeps it yet.
- **Retention for incidents and statistics**, which is separate from the thread
  TTL by design.
- **Aggregate disk accounting**, so `GetStatusResponse.disk` stops being absent.
- **A second migration** will be the first real test of the migration path; the
  initial schema proves only that migrations run.
