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

## Incidents outlive their thread

The table carries no foreign key to `threads` and collection never touches it.
This is the one thing in a thread's life that is deliberately not ephemeral: the
workspace, the turns, and the event log all go when the thread is collected, and
the evidence stays.

## Roadmap

- **Retention**, which is separate from the thread TTL by design and does not
  exist yet: nothing prunes incidents today.
- **Control stream incidents**, so a satellite-scoped failure reaches an operator
  live rather than only on query.
- **`details` on more incidents.** Provisioning failures carry the repo, the
  command, its exit code, and its output. The runner's own incidents carry a
  message and nothing structured.
