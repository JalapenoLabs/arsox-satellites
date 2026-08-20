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

## Pausing and provisioning are enforced in the claim, not the runner

`claim_next_turn` joins `threads` and excludes any thread in `PAUSED` or
`PROVISIONING`, in the same statement that takes the turn.

Putting the check in the runner instead would make it a rule the runner has to
remember, and a second runner appearing later would not know about it. In the
claim it is structural: whatever does the claiming inherits the guarantee. This
is the same reasoning that keeps one-turn-at-a-time in the subquery rather than
in Rust.

`PROVISIONING` rides on the same mechanism, which is what makes "no turn ever
runs in a half-cloned workspace" a property of the database. A thread that
declared repos opens in that state, accepts queued turns while its clones run,
and is released to `IDLE` when they finish. See
[the workspace](./workspace.md).

Draining is one `UPDATE ... RETURNING` over the thread's queued rows for the same
reason. Cancelling turns in a loop races the runner claiming the next one, and an
operator clearing a backlog should not have to win that race.

## A listing cursor carries its sort key

Ordering by creation gets uniqueness for free, since thread ids are UUIDv7. Last
activity does not: a timestamp is not unique, and a cursor holding only the
timestamp would repeat or skip every thread sharing a value with the row at the
page boundary.

So the cursor is `sort_key|row_id` and the comparison is a row value,
`(last_activity_at, thread_id) > (?, ?)`. The composite is what makes paging
total: every thread appears exactly once, which is asserted by a test that pages
a listing to exhaustion and compares the result against the full set.

Incident listings page the same way and share the format, because incidents order
by when they happened and a timestamp is even less unique there: a turn can record
several in one nanosecond.

The sort direction and column are chosen from a fixed set of literals. Every
value is still bound, never interpolated.

## A collected thread is a tombstone

Collection deletes a thread's turns, events, and metadata, and leaves the
`threads` row carrying `EXPIRED` or `DESTROYED`.

Deleting the row instead would make `THREAD_EXPIRED` and `THREAD_DESTROYED`
unreachable, since there would be nothing left to distinguish them from an id
that was never valid. The contract defines all three separately because a caller
acts on each differently, so the schema has to be able to tell them apart.

The expiry is cleared at the same time. A tombstone that kept its `expires_at`
would be selected by every subsequent sweep, and the collector would remove an
already-removed workspace once a minute forever.

Incidents have no foreign key to `threads` and are never touched by collection.
They carry their own retention, because "why did last night's run go wrong" is a
question asked after the workspace is gone. No incident query asks whether a
thread is alive, so filtering on a tombstone returns its evidence rather than
`THREAD_EXPIRED`. See [incidents](./incidents.md).

## The workspace goes before the tombstone

`Collector::collect` removes the directory first and writes the tombstone
second.

Reversed, a process that died between the two steps would leave a thread reading
as collected while its files remained, and nothing would ever look at that
thread again: a leak no later sweep can find. In this order the same crash
leaves a workspace whose thread is still expired, which the next sweep picks
straight back up.

The removal is deliberately outside the database transaction. A slow unlink
inside one would hold a write lock against every other thread on the satellite.

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
thread and turn lifecycle, pause, drain, resume, ordering, collection, expiry,
idempotency, metadata filtering, the incident listings, and every error code the
endpoints can return. It is written in Python on purpose, because a Python client
decoding what a Rust satellite encoded is the cross-language contract proving
itself rather than being asserted.

The incident checks drive a turn with `[[unrecognized=1]]`, which makes the
stand-in harness emit an event type nothing maps: a degraded incident, a turn
that still completes, and evidence that outlives the thread when it is destroyed.
Both stand-ins implement that directive, so the same check runs against the
mounted shell one and against the Rust one a `cargo run` satellite spawns.

The image ships no harness, so the run mounts a fake one. Both mounts are
required: without the fixture the harness starts and immediately fails, taking
the turn checks with it. The short collect interval is what makes the expiry
checks finish in seconds rather than a minute.

```bash
docker build -t arsox-satellite:dev .
docker run -d --name arsox-test -p 18080:8080 \
  -e ARSOX_SECRET=container-secret \
  -e ARSOX_CLAUDE_BIN=/opt/arsox/fake-harness.sh \
  -e ARSOX_COLLECT_INTERVAL=2 \
  -v "$(pwd)/scripts/fake-harness.sh:/opt/arsox/fake-harness.sh:ro" \
  -v "$(pwd)/crates/arsox-harness/fixtures/claude/2.1.221:/fixtures:ro" \
  -v arsox-test-db:/var/arsox arsox-satellite:dev
python scripts/smoke-test.py
```

On Windows under Git Bash, prefix the `docker run` with `MSYS_NO_PATHCONV=1` or
the container-side mount paths are rewritten into Windows paths.

## Roadmap

- **Retention for incidents and statistics**, which is separate from the thread
  TTL by design.
- **Aggregate disk accounting**, so `GetStatusResponse.disk` stops being absent.
