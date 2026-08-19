# Status and disk

`GET /v1/status` is the operational snapshot: what the satellite is holding, what
is running on it, and how much room is left on the volume. It is authenticated,
because it names threads.

The unauthenticated endpoints beside it stay deliberately thin. `/healthz` is
liveness and never touches the database, `/readyz` reports the checks readiness
depends on, and `/v1/version` reports the contract. None of them expose thread
content or configuration, so an orchestrator can call them before it holds a
credential.

## What status reports

| Field | Source |
|---|---|
| `running_threads` | `count(DISTINCT thread_id)` over turns in `RUNNING` |
| `threads` | the thread listing, filtered to the states a live thread can be in |
| `disk` | the cached disk measurement |
| `max_concurrent_threads`, `insecure_mode`, `started_at` | boot configuration |

**`running_threads` is counted from turns, not from the thread state column.** A
thread blocked on a question or watching a pull request still holds a turn in
flight, and `ARSOX_MAX_CONCURRENT_THREADS` caps turns rather than states. This is
the same set the claim query excludes when it looks for work, so the number
reported is the number the runner is arbitrating against.

**Tombstones are excluded from the listing.** An expired or destroyed thread
holds no workspace, no queue, and no events. Listing it among the threads the
satellite still holds would misreport the fleet to whatever is deciding whether
to start another satellite. `GET /v1/threads` still returns them when asked.

**The listing is capped at 500 threads.** Status gets polled, and a satellite
holding thousands of idle threads should not answer every poll with thousands of
summaries. `GET /v1/threads` pages properly for the caller that wants all of
them.

## Disk is measured, cached, and honest about what it cannot answer

There is no cheap way to size a directory tree. A workspace holding a few repo
checkouts is tens of thousands of `stat` calls, so the measurement is taken at
most once every 10 seconds and served from memory in between. The walk runs on
the blocking pool: it is filesystem I/O measured in seconds on a large workspace,
and running it on an async worker would stall every other request in flight.

**The tradeoff is staleness, and it is deliberate.** A status response can report
disk figures up to one refresh interval old. Nothing in the satellite decides
anything from these numbers, so the cost is an operator seeing a slightly old
figure, never an enforcement gate opening or closing on one. Per-thread quotas
are enforced against their own thread's subtree elsewhere.

One walk answers every question at once. `by_thread` keys the subtrees directly
under the workspace root by directory name, which is the thread id, so each
`ThreadSummary.workspace_bytes` comes out of the same walk rather than costing a
walk of its own. The store fills that field with zero and leaves it to the
handler: sizing a subtree is filesystem work, and a query that touches no disk
should not start a `stat` storm.

Sizes are apparent rather than allocated, the number `du --apparent-size` gives.
Symlinks are counted as the links they are rather than followed, so a link into
another thread's workspace cannot be counted twice and a cycle cannot run
forever. Entries that vanish mid-walk are skipped, because a thread being
collected while the walk runs is normal.

`database_bytes` includes the `-wal` and `-shm` sidecars. The write-ahead log
holds every committed page until a checkpoint folds it back, and on a satellite
appending events continuously it can be the larger of the two files.

**Free space needs a platform call.** `std` has no free-space API, so this is
`statvfs` on unix and `GetDiskFreeSpaceExW` on Windows, reached through `libc`
and `windows-sys` rather than a hand-written `extern` block: the `statvfs` layout
differs by architecture, and getting that wrong is undefined behavior rather than
a wrong number. On unix it reads `f_bavail` rather than `f_bfree`, because the
difference is the reserve only root may write into and agents do not run as root.

**The whole `disk` message is absent when free space cannot be read.** Every
other field is measured, but a `DiskUsage` reporting zero available bytes claims
the satellite is out of room and about to fail every write, which is a worse
statement than "not measured". `aggregate_quota_bytes` is absent for a different
reason: no satellite-wide ceiling exists to report, and absent already means the
volume's own capacity is the only limit.
