# Copyright © 2026 Jalapeno Labs

"""What a consumer holds: the generated contract, and its presence rules.

These are tests of the SDK's central claim rather than of protobuf. A consumer
that reads `reasoning_output_tokens` without asking whether it is there turns
"this harness has no reasoning accounting" into "this run reasoned for nothing",
which is a silent defect in a billing-adjacent number.
"""

from __future__ import annotations

from arsox_sdk import (
    Duration,
    McpServer,
    ReadinessProbe,
    Service,
    ServiceEndpoint,
    ServiceIsolation,
    ThreadEvent,
    ThreadSettings,
    TokenUsage,
    TurnStatus,
)
from arsox_sdk.proto.arsox.turn.v1 import result_pb2


def test_settings_survive_the_wire_unchanged() -> None:
    """What the client sends is what a satellite would read back."""
    sent = ThreadSettings(
        idle_ttl=Duration(seconds=7200),
        budget={"max_tokens_per_turn": {"tokens": 8_000_000}},
        prompt="Keep the diff small.",
    )

    received = ThreadSettings()
    received.ParseFromString(sent.SerializeToString())

    assert received == sent
    assert received.idle_ttl.seconds == 7200
    assert received.budget.max_tokens_per_turn.tokens == 8_000_000


def test_an_unreported_optional_is_absent_rather_than_zero() -> None:
    """A harness that reports no cache accounting must not look like one reporting none."""
    reported = TokenUsage(input_tokens=120, output_tokens=40, total_tokens=160)

    received = TokenUsage()
    received.ParseFromString(reported.SerializeToString())

    assert not received.HasField("cache_read_tokens")
    assert not received.HasField("reasoning_output_tokens")
    # Reading it anyway hands back the same zero either way, which is exactly why
    # the question has to be asked first.
    assert received.cache_read_tokens == 0


def test_a_reported_zero_stays_a_reported_zero() -> None:
    """A genuine zero survives the round trip as a value, not as an absence."""
    reported = TokenUsage(input_tokens=120, cache_read_tokens=0)

    received = TokenUsage()
    received.ParseFromString(reported.SerializeToString())

    assert received.HasField("cache_read_tokens")
    assert received.cache_read_tokens == 0


def test_a_result_round_trips_with_its_absent_fields_intact() -> None:
    """The presence rule holds through a whole message, not only a bare field."""
    finished = result_pb2.TurnResult(
        turn_id="turn-1",
        status=TurnStatus.TURN_STATUS_COMPLETED,
        summary="done",
        tokens=TokenUsage(input_tokens=10, output_tokens=2, total_tokens=12),
    )

    received = result_pb2.TurnResult()
    received.ParseFromString(finished.SerializeToString())

    assert received.status == TurnStatus.TURN_STATUS_COMPLETED
    assert received.tokens.total_tokens == 12
    assert not received.tokens.HasField("reasoning_output_tokens")
    # A turn that failed carries an error; this one did not, and says so.
    assert not received.HasField("error")


def test_an_event_frame_round_trips() -> None:
    """The stream's frames are ordinary contract messages, in sequence order."""
    published = ThreadEvent(
        sequence=42,
        thread_id="thread-1",
        type="tool.started",
        tool_started={"tool_name": "Bash"},
    )

    received = ThreadEvent()
    received.ParseFromString(published.SerializeToString())

    assert received.sequence == 42
    assert received.type == "tool.started"
    assert received.tool_started.tool_name == "Bash"
    # The payload is a oneof in everything but name: only the field matching the
    # type is set.
    assert not received.HasField("tool_completed")


def test_a_thread_service_and_the_mcp_server_it_runs_survive_the_wire() -> None:
    """A helper process, and an MCP server addressed through it rather than a URL."""
    sent = ThreadSettings(
        idle_ttl=Duration(seconds=3600),
        services=[
            Service(
                name="blender-mcp",
                command='blender --background --python bridge.py -- --port "$PORT"',
                ready_when=ReadinessProbe(http_get="/health", timeout=Duration(seconds=120)),
                isolation=ServiceIsolation.SERVICE_ISOLATION_SHARED,
            )
        ],
        mcp_servers=[
            McpServer(
                name="blender",
                service=ServiceEndpoint(service="blender-mcp", path="/mcp"),
            )
        ],
    )

    received = ThreadSettings()
    received.ParseFromString(sent.SerializeToString())

    assert received == sent
    service = received.services[0]
    assert service.ready_when.http_get == "/health"
    # An undeclared port stays absent, which asks the satellite to assign one.
    # A port of zero would be a declared port the satellite refuses.
    assert not service.HasField("port")

    server = received.mcp_servers[0]
    assert server.HasField("service")
    assert server.service.service == "blender-mcp"
    # Exactly one of the two addresses: a server reached through a service
    # leaves its url empty.
    assert server.url == ""
