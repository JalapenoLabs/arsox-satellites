# Copyright © 2026 Jalapeno Labs

"""Turning a status and some bytes into a message, or into a failure.

No satellite, no port, and no mock: the mapping is a function, so it is tested as
one.
"""

from __future__ import annotations

import aiohttp
import pytest

from arsox_sdk import ArsoxError
from arsox_sdk.proto.arsox.error.v1 import error_pb2
from arsox_sdk.proto.arsox.satellite.v1 import satellite_pb2
from arsox_sdk.transport import Connection, decode_response

# Not protobuf under any reading: field 15 with wire type 7, which no decoder
# accepts.
UNDECODABLE = b"\xff\xff\xff\xff"


def test_a_success_body_decodes_to_the_message_it_carries() -> None:
    """The round trip a consumer depends on, in one line each way."""
    sent = satellite_pb2.GetVersionResponse(
        satellite_version="0.1.0", proto_major=1, proto_minor=0
    )

    received = decode_response(
        satellite_pb2.GetVersionResponse, 200, sent.SerializeToString()
    )

    assert received == sent
    assert received.satellite_version == "0.1.0"


def test_an_undecodable_success_body_is_a_transport_failure() -> None:
    """A satellite that answers 200 with nonsense is not a contract failure."""
    with pytest.raises(ArsoxError) as raised:
        decode_response(satellite_pb2.GetVersionResponse, 200, UNDECODABLE)

    assert raised.value.kind == "transport"
    assert "undecodable" in str(raised.value)


def test_a_failure_body_becomes_the_error_it_describes() -> None:
    """Every failure on every transport is the same shape, so it decodes as one."""
    sent = error_pb2.Error(
        code=error_pb2.ERROR_CODE_AUTH_SECRET_INVALID,
        message="the bearer token does not match",
        retryable=False,
    )

    with pytest.raises(ArsoxError) as raised:
        decode_response(satellite_pb2.GetStatusResponse, 401, sent.SerializeToString())

    assert raised.value.kind == "contract"
    assert raised.value.code == error_pb2.ERROR_CODE_AUTH_SECRET_INVALID
    assert not raised.value.retryable


def test_an_empty_failure_body_says_so() -> None:
    """Something other than a satellite answered, and the message says which."""
    with pytest.raises(ArsoxError) as raised:
        decode_response(satellite_pb2.GetStatusResponse, 502, b"")

    assert raised.value.kind == "transport"
    assert "502" in str(raised.value)
    assert raised.value.code is None


def test_an_undecodable_failure_body_reports_the_status() -> None:
    """A proxy answering HTML on the satellite's behalf is not a contract error."""
    with pytest.raises(ArsoxError) as raised:
        decode_response(satellite_pb2.GetStatusResponse, 503, UNDECODABLE)

    assert raised.value.kind == "transport"
    assert "503" in str(raised.value)


@pytest.mark.parametrize(
    ("url", "expected"),
    [
        ("http://127.0.0.1:8080", "ws://127.0.0.1:8080"),
        ("https://satellite.internal", "wss://satellite.internal"),
        ("https://satellite.internal/", "wss://satellite.internal"),
    ],
)
async def test_the_socket_url_follows_the_base_url(url: str, expected: str) -> None:
    """The stream reaches the same satellite over the same scheme's socket."""
    async with aiohttp.ClientSession() as session:
        assert Connection(url, "secret", session).socket_url == expected
