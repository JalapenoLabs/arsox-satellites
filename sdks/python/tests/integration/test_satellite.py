# Copyright © 2026 Jalapeno Labs

"""The Python SDK driving a real satellite.

This is the point of the exercise rather than a formality. Anything a consumer
needs and cannot reach from the published surface is a hole in the SDK, and a test
written from the consumer's seat is the only place that shows up.

One satellite serves the whole file because starting one costs a second, not
because it has to be alone. Each gets a port of its own. See conftest.py.
"""

from __future__ import annotations

from collections.abc import Awaitable

from arsox_sdk import (
    ArsoxError,
    Disposition,
    ErrorCode,
    EventStream,
    Harness,
    ListIncidentsRequest,
    Satellite,
    ThreadCreated,
    ThreadEvent,
    ThreadSettings,
    ThreadState,
    TurnStatus,
)


async def failure_from(call: Awaitable[object]) -> ArsoxError:
    """Await a call that must fail, and hand back the failure it produced.

    Written out rather than reached through `pytest.raises`, because every
    assertion below is about the error's own shape: its code, its retryable flag,
    and which of the two "the thread is not here" facts it reports.
    """
    try:
        await call
    except ArsoxError as failure:
        return failure

    raise AssertionError("the call was expected to fail and did not")


async def read_until(events: EventStream, event_type: str) -> list[ThreadEvent]:
    """Read the stream up to and including the first event of `event_type`."""
    seen: list[ThreadEvent] = []
    async for event in events:
        seen.append(event)
        if event.type == event_type:
            break

    return seen


async def test_it_checks_the_contract_version_when_it_connects(client: Satellite) -> None:
    """Connect refuses a satellite this SDK cannot speak to, before anything else."""
    version = await client.version()

    assert version.proto_major == 1
    assert version.satellite_version != ""


async def test_it_reports_which_harnesses_this_satellite_offers(client: Satellite) -> None:
    """Capabilities are asked for rather than discovered by absence three turns in."""
    harness = await client.harness()

    # Claude is the harness implemented today, and the endpoint says so plainly
    # rather than implying a suite that spans several.
    assert harness.default_harness == Harness.HARNESS_CLAUDE
    assert len(harness.harnesses) == 1

    claude = harness.harnesses[0]
    assert claude.harness == Harness.HARNESS_CLAUDE
    assert claude.supports_mcp
    assert claude.reports_cache_tokens


async def test_it_rejects_a_bad_secret_with_a_code_the_caller_can_match_on(
    satellite_url: str,
) -> None:
    """A wrong credential is a contract error, not a mystery."""
    async with await Satellite.connect(satellite_url, "wrong") as wrong:
        failure = await failure_from(wrong.status())

        assert failure.code == ErrorCode.ERROR_CODE_AUTH_SECRET_INVALID
        # Wrong credentials do not become right by trying again.
        assert not failure.retryable


async def test_it_creates_reads_lists_and_destroys_a_thread(
    client: Satellite, settings: ThreadSettings
) -> None:
    """The lifecycle, including what a destroyed thread reports afterwards."""
    metadata = {"tenant": "acme"}

    created = await client.threads().create_with(
        settings, idempotency_key="python-sdk-1", metadata=metadata
    )

    assert not created.deduplicated
    assert created.thread.state == ThreadState.THREAD_STATE_IDLE

    # The key is what makes a timed-out create safe to retry: a second call
    # carrying it returns the original thread rather than leaking a workspace.
    repeat = await client.threads().create_with(
        settings, idempotency_key="python-sdk-1", metadata=metadata
    )
    assert repeat.deduplicated
    assert repeat.thread.thread_id == created.thread.thread_id

    listed = await client.threads().list(metadata=metadata)
    assert created.thread.thread_id in [ summary.thread_id for summary in listed ]

    await created.handle.destroy()

    # Gone, not missing. A destroyed thread reports what happened to it, so an
    # application can tell "my record is stale" from "my id is wrong".
    gone = await failure_from(created.handle.get())
    assert gone.is_gone()
    assert not gone.is_not_found()
    assert gone.code == ErrorCode.ERROR_CODE_THREAD_DESTROYED


async def test_it_fails_at_attach_rather_than_later_when_the_thread_is_unknown(
    client: Satellite,
) -> None:
    """Attaching to a typo fails at attach, not at the first operation on the handle."""
    failure = await failure_from(client.threads().attach("not-a-thread"))

    assert failure.is_not_found()


async def test_a_second_client_attaches_to_a_thread_it_did_not_create(
    satellite_url: str, secret: str, thread: ThreadCreated
) -> None:
    """The property a horizontally scaled application depends on.

    A replica that dies mid-turn costs nothing, because whichever replica comes up
    next picks the thread back up from its id alone. There is no handoff, no
    lease, and no ownership.
    """
    async with await Satellite.connect(satellite_url, secret) as other:
        attached = await other.threads().attach(thread.thread.thread_id)
        assert attached.id == thread.thread.thread_id

        # An attached handle can do everything a creating handle can.
        turn = await attached.start_turn("work through the probe")
        assert turn.queued.status == TurnStatus.TURN_STATUS_QUEUED

        result = await turn.result()
        assert result.status == TurnStatus.TURN_STATUS_COMPLETED


async def test_it_runs_a_turn_and_returns_its_result(thread: ThreadCreated) -> None:
    """A whole turn, from queued to a result a consumer can read."""
    turn = await thread.handle.start_turn(
        "replay the probe", metadata={"request_id": "req_2f8c11"}
    )

    result = await turn.result()

    assert result.status == TurnStatus.TURN_STATUS_COMPLETED
    assert result.tokens.total_tokens > 0
    # Absent rather than zero, all the way out to a consumer. A harness that
    # reports no reasoning accounting must not look like one that reported none.
    assert not result.tokens.HasField("reasoning_output_tokens")
    # Failover cost stays visible: a turn where the first endpoint burned tokens
    # failing must not look identical to a clean run on the second.
    assert len(result.by_model) >= 2

    # The turn's metadata rides along on the result rather than needing a lookup,
    # because "whose job just finished" is the question being asked at exactly
    # this moment.
    assert dict(result.metadata) == {"request_id": "req_2f8c11"}

    turns = await thread.handle.turns()
    assert turn.id in [ queued.turn_id for queued in turns ]


async def test_it_streams_the_whole_turn_over_the_socket(thread: ThreadCreated) -> None:
    """The turn watched rather than polled, in the order the satellite numbered it."""
    # Subscribed before the turn is queued, so nothing published between the two
    # is lost.
    async with await thread.handle.events() as events:
        await thread.handle.start_turn("replay the probe")

        seen = [ event.type for event in await read_until(events, "turn.completed") ]

    assert seen[0] == "turn.started"
    assert seen[-1] == "turn.completed"
    assert "tool.started" in seen


async def test_it_resumes_a_stream_from_a_sequence_without_losing_an_event(
    thread: ThreadCreated,
) -> None:
    """Resumption is exclusive: hand back the last sequence handled, get the rest."""
    turn = await thread.handle.start_turn("replay the probe")
    await turn.result()

    async with await thread.handle.events() as events:
        everything = await read_until(events, "turn.completed")

    assert len(everything) > 2
    midpoint = everything[len(everything) // 2]

    # This is how a replica that died mid-turn picks back up.
    async with await thread.handle.events(from_sequence=midpoint.sequence) as resumed:
        after = await read_until(resumed, "turn.completed")

    assert after[0].sequence == midpoint.sequence + 1
    assert after[-1].type == "turn.completed"


async def test_it_pauses_resumes_and_drains_a_thread(thread: ThreadCreated) -> None:
    """Three verbs, because they answer three questions."""
    paused = await thread.handle.pause()
    assert paused.state == ThreadState.THREAD_STATE_PAUSED

    # The queue still accepts work while paused. It simply does not move.
    first = await thread.handle.start_turn("one")
    second = await thread.handle.start_turn("two")

    cancelled = await thread.handle.drain()
    assert set(cancelled) >= {first.id, second.id}

    resumed = await thread.handle.resume()
    assert resumed.state == ThreadState.THREAD_STATE_IDLE


async def test_it_cancels_a_queued_turn_without_touching_the_one_that_is_running(
    thread: ThreadCreated,
) -> None:
    """Cancelling one turn leaves the turn in flight alone."""
    # Queued behind a running turn, so cancelling is deterministic rather than a
    # race with the runner reaching a terminal state first.
    in_flight = await thread.handle.start_turn("replay the probe")
    queued = await thread.handle.start_turn("wait your turn")

    stopped = await queued.cancel()
    assert stopped.status == TurnStatus.TURN_STATUS_CANCELLED

    result = await in_flight.result()
    assert result.status == TurnStatus.TURN_STATUS_COMPLETED


async def test_it_lists_incidents_per_thread_and_per_satellite(
    client: Satellite, thread: ThreadCreated
) -> None:
    """Incidents are recorded, filterable, and outlive the thread they describe."""
    # The stand-in stops after three lines, which is a harness that exited without
    # saying what it did. The satellite restarts it once and it does the same
    # thing again, so the turn ends with two incidents against it.
    turn = await thread.handle.start_turn("replay the probe [[truncate=3]]")
    result = await turn.result()

    assert result.status == TurnStatus.TURN_STATUS_FAILED
    # The counts ride along on the report, so the common case needs no query.
    assert result.incident_counts.recovered == 1
    assert result.incident_counts.fatal == 1

    # The recovery is recorded because it happened. A restart that worked looks
    # exactly like a turn that never stalled, and a harness wedging on every turn
    # is a pattern nobody sees unless the recovery is written down.
    listed = await thread.handle.incidents()
    assert [ incident.disposition for incident in listed ] == [
        Disposition.DISPOSITION_RECOVERED,
        Disposition.DISPOSITION_FATAL,
    ]
    assert [ incident.code for incident in listed ] == [
        ErrorCode.ERROR_CODE_HARNESS_CRASHED,
        ErrorCode.ERROR_CODE_HARNESS_CRASHED,
    ]
    assert [ incident.turn_id for incident in listed ] == [ turn.id, turn.id ]

    # A filter narrows. A disposition nothing carries returns nothing rather than
    # falling back to everything.
    blocked = await thread.handle.incidents(
        ListIncidentsRequest(dispositions=[ Disposition.DISPOSITION_BLOCKED ])
    )
    assert blocked == []

    # The satellite-wide listing finds the same incidents without being told which
    # thread to look at.
    fleet = await client.incidents(
        ListIncidentsRequest(codes=[ ErrorCode.ERROR_CODE_HARNESS_CRASHED ])
    )
    assert thread.thread.thread_id in [ incident.thread_id for incident in fleet ]

    # Destroying the thread takes its workspace, its turns, and its events. The
    # evidence stays, which is the whole reason incidents are not ephemeral.
    await thread.handle.destroy()

    after_teardown = await client.incidents(
        ListIncidentsRequest(thread_ids=[ thread.thread.thread_id ])
    )
    assert len(after_teardown) == 2


async def test_it_reports_what_it_is_holding(client: Satellite, thread: ThreadCreated) -> None:
    """Status names the threads the satellite holds, and stops naming a destroyed one."""
    status = await client.status()

    assert status.max_concurrent_threads == 2
    assert thread.thread.thread_id in [ summary.thread_id for summary in status.threads ]
    assert status.disk.available_bytes > 0

    await thread.handle.destroy()

    settled = await client.status()
    assert thread.thread.thread_id not in [ summary.thread_id for summary in settled.threads ]
