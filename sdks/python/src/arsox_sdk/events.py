# Copyright © 2026 Jalapeno Labs

"""One thread's event stream."""

from __future__ import annotations

import logging
from types import TracebackType

import aiohttp
from google.protobuf.message import DecodeError

from arsox_sdk.errors import ArsoxError
from arsox_sdk.proto.arsox.event.v1 import event_pb2
from arsox_sdk.transport import WebSocket

logger = logging.getLogger(__name__)


class EventStream:
    """One thread's events, in sequence order.

    An async iterator rather than a callback registry, so the ordinary
    `async for event in stream` reads events in the order the satellite numbered
    them. Frames are binary protobuf, always.

    The socket is unidirectional, server to client. Nothing is ever sent up it:
    every command is an ordinary HTTP request, and this only reports what
    happened.
    """

    def __init__(self, socket: WebSocket) -> None:
        """Wrap an open socket. Obtained from `ThreadHandle.events`."""
        self._socket = socket

    def __aiter__(self) -> EventStream:
        """Iterate the stream."""
        return self

    async def __anext__(self) -> event_pb2.ThreadEvent:
        """Read the next event.

        Raises:
            ArsoxError: when a frame will not decode, or when the satellite
                closes the socket with a reason. A close reason carries the
                contract code by name, so a lagging consumer learns it was
                dropped rather than watching the stream stop.
            StopAsyncIteration: when the socket closes cleanly.
        """
        while True:
            message = await self._socket.receive()

            match message.type:
                case aiohttp.WSMsgType.BINARY:
                    event = event_pb2.ThreadEvent()
                    try:
                        event.ParseFromString(message.data)
                    except DecodeError as error:
                        raise ArsoxError.transport(f"undecodable event frame: {error}") from error

                    return event

                case aiohttp.WSMsgType.TEXT:
                    # Text frames are not part of the contract. The JSON
                    # subprotocol is a debugging affordance for hand-driven
                    # clients and is never what an SDK negotiates.
                    logger.debug("[EventStream] ignoring a non-binary frame on the thread stream")

                case aiohttp.WSMsgType.ERROR:
                    raise ArsoxError.transport(str(self._socket.exception()))

                case aiohttp.WSMsgType.CLOSE | aiohttp.WSMsgType.CLOSING | aiohttp.WSMsgType.CLOSED:
                    reason = str(message.extra or "")
                    if reason:
                        raise ArsoxError.transport(f"stream closed: {reason}")

                    raise StopAsyncIteration

    async def aclose(self) -> None:
        """Stop reading and close the socket."""
        await self._socket.close()

    async def __aenter__(self) -> EventStream:
        """Enter a block that closes the socket on the way out."""
        return self

    async def __aexit__(
        self,
        exception_type: type[BaseException] | None,
        exception: BaseException | None,
        traceback: TracebackType | None,
    ) -> None:
        """Close the socket, however the block was left."""
        await self.aclose()
