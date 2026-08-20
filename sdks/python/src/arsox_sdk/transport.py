# Copyright © 2026 Jalapeno Labs

"""One satellite's address, credential, and connection pool."""

from __future__ import annotations

import logging
from typing import Literal, TypeVar

import aiohttp
from google.protobuf.message import DecodeError, Message

from arsox_sdk.constants import PROTOBUF_CONTENT_TYPE
from arsox_sdk.errors import ArsoxError
from arsox_sdk.proto.arsox.error.v1 import error_pb2

logger = logging.getLogger(__name__)

# The methods the satellite's HTTP surface uses.
HttpMethod = Literal["GET", "POST", "DELETE"]

# The thread stream's socket, opened with text decoding off. Frames are binary
# protobuf, always, so a text frame is never something this SDK reads.
#
# Parameterized on `bool` rather than on `Literal[False]` because that is what
# aiohttp's own overloads resolve a keyword `decode_text=False` to.
WebSocket = aiohttp.ClientWebSocketResponse[bool]

_Response = TypeVar("_Response", bound=Message)


def decode_response(message_type: type[_Response], status: int, body: bytes) -> _Response:
    """Turn a raw response into the message it carries, or the failure it describes.

    Separated from the socket work so the mapping from a status and some bytes to
    an `ArsoxError` is testable without a satellite, a port, or a mock.

    Args:
        message_type: the contract message the caller expects on success.
        status: the HTTP status the satellite answered with.
        body: the response body, protobuf either way.

    Raises:
        ArsoxError: on any status outside 2xx, and on a success body that will
            not decode.
    """
    if 200 <= status < 300:
        message = message_type()
        try:
            message.ParseFromString(body)
        except DecodeError as error:
            raise ArsoxError.transport(
                f"the satellite sent an undecodable response: {error}"
            ) from error

        return message

    # Every failure on every transport is the same shape, so an empty or
    # undecodable error body means something other than a satellite answered.
    if not body:
        raise ArsoxError.transport(f"the satellite answered {status} with no body")

    contract_error = error_pb2.Error()
    try:
        contract_error.ParseFromString(body)
    except DecodeError as error:
        raise ArsoxError.transport(f"the satellite answered {status}") from error

    raise ArsoxError.contract(contract_error)


class Connection:
    """One satellite's address, credential, and connection pool.

    Handles hold one of these and nothing else, which is what makes a handle
    disposable and a thread durable.
    """

    def __init__(self, url: str, secret: str, session: aiohttp.ClientSession) -> None:
        """Point a connection at a satellite."""
        self.base_url = url.rstrip("/")
        self.secret = secret
        self.session = session

    @property
    def socket_url(self) -> str:
        """The same satellite, addressed as a WebSocket."""
        if self.base_url.startswith("https://"):
            return f"wss://{self.base_url[len('https://'):]}"
        if self.base_url.startswith("http://"):
            return f"ws://{self.base_url[len('http://'):]}"
        return self.base_url

    async def call(
        self,
        method: HttpMethod,
        path: str,
        message_type: type[_Response],
        body: bytes | None = None,
    ) -> _Response:
        """Send one request and decode what comes back.

        `body` is optional because several endpoints take none, and present on
        GET for the listings, whose filters are a protobuf request message like
        every other request in the contract. aiohttp puts a body on a GET; the
        WHATWG fetch specification forbids one outright, which is the constraint
        that shaped the Node client and would have shaped this one.

        Raises:
            ArsoxError: when the satellite is unreachable, rejects the request,
                or answers something undecodable.
        """
        headers = {
            "Authorization": f"Bearer {self.secret}",
            "Accept": PROTOBUF_CONTENT_TYPE,
        }
        if body is not None:
            headers["Content-Type"] = PROTOBUF_CONTENT_TYPE

        try:
            async with self.session.request(
                method, self.base_url + path, headers=headers, data=body
            ) as response:
                return decode_response(message_type, response.status, await response.read())
        except aiohttp.ClientError as error:
            raise ArsoxError.transport(str(error)) from error

    async def open_socket(self, path: str) -> WebSocket:
        """Open one of the satellite's WebSockets.

        Awaited rather than lazy, so a refused handshake is reported where the
        caller asked for the stream rather than at the first event.

        Raises:
            ArsoxError: when the handshake is refused or the satellite is
                unreachable.
        """
        try:
            return await self.session.ws_connect(
                self.socket_url + path,
                headers={"Authorization": f"Bearer {self.secret}"},
                decode_text=False,
            )
        except aiohttp.ClientError as error:
            raise ArsoxError.transport(str(error)) from error

    async def close(self) -> None:
        """Release the connection pool."""
        await self.session.close()
