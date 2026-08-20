# Copyright © 2026 Jalapeno Labs

"""A connection to one satellite, and the threads on it."""

from __future__ import annotations

import logging
from types import TracebackType
from typing import NamedTuple

import aiohttp

from arsox_sdk.constants import SDK_PROTO_MAJOR, SDK_PROTO_MINOR
from arsox_sdk.errors import ArsoxError
from arsox_sdk.proto.arsox.common.v1 import common_pb2
from arsox_sdk.proto.arsox.harness.v1 import harness_pb2
from arsox_sdk.proto.arsox.incident.v1 import incident_pb2
from arsox_sdk.proto.arsox.satellite.v1 import satellite_pb2
from arsox_sdk.proto.arsox.settings.v1 import settings_pb2
from arsox_sdk.proto.arsox.thread.v1 import thread_pb2
from arsox_sdk.thread import ThreadHandle
from arsox_sdk.transport import Connection

logger = logging.getLogger(__name__)


class ThreadCreated(NamedTuple):
    """A newly opened thread."""

    thread: thread_pb2.Thread

    # True when an idempotency key matched an existing thread, so this is that
    # thread rather than a new one.
    deduplicated: bool

    handle: ThreadHandle


class Satellite:
    """A connection to one satellite.

    Protobuf is always on the wire, in both directions. What you hold is an
    ordinary Python object.

    Holds a connection pool, so close it when you are done with it:

    ```python
    async with await Satellite.connect(url, secret) as satellite:
        ...
    ```
    """

    def __init__(self, connection: Connection) -> None:
        """Wrap a connection. Use `connect`, which also checks the contract version."""
        self._connection = connection

    @classmethod
    async def connect(cls, url: str, secret: str) -> Satellite:
        """Connect, and refuse a satellite this SDK cannot speak to.

        The version check happens here rather than lazily, so a mismatch is
        reported at the point a human can act on it instead of surfacing as a
        decode failure three calls later.

        Raises:
            ArsoxError: when the satellite is unreachable or serves a proto major
                above this SDK's.
        """
        satellite = cls(Connection(url, secret, aiohttp.ClientSession()))

        try:
            version = await satellite.version()

            if version.proto_major > SDK_PROTO_MAJOR:
                raise ArsoxError.incompatible(version.proto_major, SDK_PROTO_MAJOR)
            if version.proto_major == SDK_PROTO_MAJOR and version.proto_minor > SDK_PROTO_MINOR:
                # Additive fields this SDK does not know about are ignored, which
                # is safe. Saying so once beats saying nothing.
                logger.warning(
                    f"[Satellite] serves proto v{version.proto_major}.{version.proto_minor} and "
                    f"this SDK was built against v{SDK_PROTO_MAJOR}.{SDK_PROTO_MINOR}; "
                    "unknown fields will be ignored"
                )
        except BaseException:
            # A satellite that was never usable must not leave its pool open.
            await satellite.close()
            raise

        return satellite

    async def version(self) -> satellite_pb2.GetVersionResponse:
        """Report the satellite version and the proto contract it serves.

        Raises:
            ArsoxError: when the satellite is unreachable.
        """
        return await self._connection.call("GET", "/v1/version", satellite_pb2.GetVersionResponse)

    async def status(self) -> satellite_pb2.GetStatusResponse:
        """Report what the satellite is currently doing.

        Raises:
            ArsoxError: when the satellite is unreachable or rejects the secret.
        """
        return await self._connection.call("GET", "/v1/status", satellite_pb2.GetStatusResponse)

    async def harness(self) -> harness_pb2.GetHarnessResponse:
        """Report which harnesses this satellite offers and what each supports.

        Worth calling before relying on a capability. Discovering that a harness
        has no plan mode by its absence, three turns into a run, is the failure
        this endpoint exists to prevent.

        Raises:
            ArsoxError: when the satellite is unreachable or rejects the secret.
        """
        return await self._connection.call("GET", "/v1/harness", harness_pb2.GetHarnessResponse)

    async def incidents(
        self, query: incident_pb2.ListIncidentsRequest | None = None
    ) -> list[incident_pb2.Incident]:
        """List incidents across every thread this satellite has held.

        Incidents outlive the threads they describe, so this answers for threads
        that were collected long ago. That is the point: "why did last night's
        run go wrong" is asked after the workspace is gone.

        Returns one page, oldest first. Raise the query's page limit or follow its
        cursor for the rest. Every filter is "match any of these", and an empty
        one does not filter rather than matching nothing, so no query at all asks
        for everything.

        Raises:
            ArsoxError: when the satellite is unreachable or rejects the secret.
        """
        request = query or incident_pb2.ListIncidentsRequest()

        response = await self._connection.call(
            "GET",
            "/v1/incidents",
            incident_pb2.ListIncidentsResponse,
            request.SerializeToString(),
        )

        return list(response.incidents)

    def threads(self) -> Threads:
        """Threads on this satellite."""
        return Threads(self._connection)

    async def close(self) -> None:
        """Release the connection pool."""
        await self._connection.close()

    async def __aenter__(self) -> Satellite:
        """Enter a block that closes the connection on the way out."""
        return self

    async def __aexit__(
        self,
        exception_type: type[BaseException] | None,
        exception: BaseException | None,
        traceback: TracebackType | None,
    ) -> None:
        """Close the connection, however the block was left."""
        await self.close()


class Threads:
    """Thread operations on one satellite."""

    def __init__(self, connection: Connection) -> None:
        """Wrap a connection. Obtained from `Satellite.threads`."""
        self._connection = connection

    async def create(self, settings: settings_pb2.ThreadSettings) -> ThreadCreated:
        """Open a thread.

        Raises:
            ArsoxError: when the settings are incomplete or the satellite is
                unreachable.
        """
        return await self.create_with(settings)

    async def create_with(
        self,
        settings: settings_pb2.ThreadSettings,
        idempotency_key: str | None = None,
        metadata: dict[str, str] | None = None,
    ) -> ThreadCreated:
        """Open a thread with an idempotency key and correlation metadata.

        Args:
            settings: the thread's settings. An idle TTL and a budget are both
                required, always.
            idempotency_key: deduplicates retries of this call. Without it, a
                create that times out in transit leaves the caller unable to tell
                "not created" from "created, response lost", and the only safe
                move is to retry and leak a whole workspace.
            metadata: your own correlation data, stored verbatim and handed back
                untouched. Filterable through `list`. Never redacted, so
                credentials belong in the settings' `env` with `is_secret`
                instead.

        Raises:
            ArsoxError: when the settings are incomplete or the satellite is
                unreachable.
        """
        # A None key is left unset rather than sent empty, which is the
        # difference between "deduplicate me" and "I have no key".
        request = thread_pb2.CreateThreadRequest(
            settings=settings,
            idempotency_key=idempotency_key,
            metadata=metadata or {},
        )

        response = await self._connection.call(
            "POST",
            "/v1/threads",
            thread_pb2.CreateThreadResponse,
            request.SerializeToString(),
        )

        if not response.HasField("thread"):
            raise ArsoxError.transport("the satellite created a thread without returning it")

        return ThreadCreated(
            response.thread,
            response.deduplicated,
            ThreadHandle(self._connection, response.thread.thread_id),
        )

    async def attach(self, thread_id: str) -> ThreadHandle:
        """Pick up a thread that already exists.

        There is no handoff and no lease. The process that created the thread has
        no privileged claim on it and may have exited hours ago.

        Reads the thread before returning, so attaching to a typo fails here
        rather than at the first operation on the handle.

        Raises:
            ArsoxError: when the thread is unknown or the satellite is
                unreachable.
        """
        handle = ThreadHandle(self._connection, thread_id)
        await handle.get()

        return handle

    async def list(
        self,
        metadata: dict[str, str] | None = None,
        order_by: thread_pb2.ThreadOrder = thread_pb2.THREAD_ORDER_UNSPECIFIED,
        descending: bool = False,
    ) -> list[thread_pb2.ThreadSummary]:
        """List threads. Returns one page.

        Args:
            metadata: every entry must match for a thread to be returned. Empty
                does not filter.
            order_by: creation order by default, because it is free: thread ids
                are UUIDv7 and already sort by time. Last activity is the order an
                operator scanning a fleet actually wants.
            descending: newest first when set.

        Raises:
            ArsoxError: when the satellite is unreachable.
        """
        request = thread_pb2.ListThreadsRequest(
            metadata=metadata or {},
            page=common_pb2.PageRequest(),
            order_by=order_by,
            descending=descending,
        )

        response = await self._connection.call(
            "GET",
            "/v1/threads",
            thread_pb2.ListThreadsResponse,
            request.SerializeToString(),
        )

        return list(response.threads)
