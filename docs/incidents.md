# Incidents

Returned errors answer "why did my call fail". Most things that go wrong are not
that: a prefetched ticket 404s, a checker exits nonzero, an endpoint fails and
the next one covers, a command hits the exec allowlist. The turn continues and
nothing is returned to the caller.

Incidents are where those go, so that nothing in Arsox fails silently. They reuse
the contract's error codes rather than defining a parallel taxonomy; what differs
is the disposition.

## One call writes both copies

An incident lives in two places at once. It is a row, because "why did last
night's run go wrong" is asked long after the turn ended. It is an event, because
a consumer watching a thread has to learn about a failure while the turn is still
running.

`Store::report_incident` writes both, and it is what every caller reaches for.
The runner, the workspace provisioner, the LLM proxy's reports, and the harness
mappers all go through it.

Recording and appending separately at each call site would be two rules to
remember, and the one that gets forgotten is the stream: a failure in the
database that reached no consumer is silence to every client watching, which is
the exact defect incidents exist to prevent. `Store::record_incident` still
exists as the durable half, for the caller that has already streamed it.

**The event goes first**, so the row can carry the sequence its frame landed at.
That is what lets a query result be located in the stream, and a stream frame be
looked up again afterwards. The frame itself carries no sequence inside its
payload, because the number is issued by the append that emits it; the envelope
around it carries the same number.

**A stream that will not take the event does not cost the record.** The durable
copy is the half an operator queries later, and losing it because the thread was
collected a moment ago would be the silence this is all written against.

### Mapper incidents are not appended twice

A harness mapper emits an incident as an ordinary mapped event, so the runner's
read loop has to hand it to `report_incident` **instead of** its usual append
rather than as well as. Both would put two copies of one failure on the stream.

The mapper knows the code, the disposition, and what went wrong. It knows nothing
about which thread or turn was reading the line, so the runner fills the
attribution in rather than the mapper inventing it, and carries the mapper's own
`retryable` judgement through untouched.

## Satellite-scoped incidents reach no thread stream

`thread_id` is nullable, because a failure can belong to the satellite rather
than to any thread: an unreachable proxy at boot, a failed migration, a volume
that will not mount. Those are recorded and not streamed. The thread sockets
carry thread content only, and the control socket that would carry them does not
emit incidents yet.

## Redaction holds on both copies

An incident passes both masking doors: `append_event` masks the payload before it
is encoded, and `record_incident` masks the row. A thread's secrets are therefore
absent from the frame and from the row, and the two cannot disagree about what a
consumer was allowed to see. See [redaction](./redaction.md).

## Reading them back

| Surface | What it answers |
|---|---|
| the `incident` event on the thread socket | what is going wrong right now |
| `TurnResult.incident_counts` | how much went wrong in this turn, by disposition |
| `GET /v1/threads/{id}/incidents` | what went wrong on this thread |
| `GET /v1/incidents` | what went wrong anywhere on this satellite |

Both listings take the contract's filters: thread, turn, member, code,
disposition, and a time range. Every filter is "match any of these", and an empty
one does not filter rather than matching nothing, so the request a caller sends
first returns everything instead of nothing.

The thread-scoped route pins the thread to the path rather than reading it from
the body, so the URL says what it looks like it says. A caller wanting several
threads at once has the wide endpoint.

In the Rust SDK both are `incidents(IncidentQuery)`, on the satellite and on a
thread handle. The query takes codes and dispositions as typed enums and defaults
to asking for everything.

**The counts are a query, not a tally.** Incidents reach the database from the
runner, the harness mappers, and the LLM proxy, so a counter kept beside them
would report the subset one of the three happened to see. They are counted at the
one point every ending of a turn passes through, past the last incident any path
records, which is why a turn stopped by a ceiling and a turn whose harness never
launched both report what actually happened.

**Every report carries counts, including zeroes.** "Not counted" and "nothing
went wrong" are different facts, and an absent message would collapse them into
the first.

### Paging

Oldest first, with the sort key travelling in the cursor. Incidents order by when
they happened and a timestamp is not unique, so a cursor carrying only the time
would repeat or skip whatever shares a nanosecond with the row at a page
boundary. The pair is what makes paging total, and it is the same format thread
listings use.

Time windows are half-open: at or after `occurred_after`, strictly before
`occurred_before`. An operator walking a log an hour at a time then sees every
incident exactly once rather than seeing the boundary ones in both windows.

There is deliberately no sort order to choose. The contract defines none, and
inventing one on the satellite would be a field no SDK could name.

## Incidents outlive their thread

The table carries no foreign key to `threads` and collection never touches it.
This is the one thing in a thread's life that is deliberately not ephemeral: the
workspace, the turns, and the event log all go when the thread is collected, and
the evidence stays.

**No listing asks whether a thread is alive**, and that is the point rather than
an oversight. Filtering on an expired or destroyed thread returns its incidents
rather than `THREAD_EXPIRED`, because "why did last night's run go wrong" is
asked precisely when the workspace is already gone.

## Roadmap

- **Retention**, which is separate from the thread TTL by design and does not
  exist yet: nothing prunes incidents today.
- **Control stream incidents**, so a satellite-scoped failure reaches an operator
  live rather than only on query.
- **`details` on more incidents.** Provisioning failures carry the repo, the
  command, its exit code, and its output. The runner's own incidents carry a
  message and nothing structured.
- **`total` on a listing page**, which is absent today because counting every
  match would mean a second scan on every page.
