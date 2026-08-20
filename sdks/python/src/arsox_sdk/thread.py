# Copyright © 2026 Jalapeno Labs

"""A handle to one thread."""

from __future__ import annotations

from arsox_sdk.errors import ArsoxError
from arsox_sdk.events import EventStream
from arsox_sdk.proto.arsox.common.v1 import common_pb2
from arsox_sdk.proto.arsox.incident.v1 import incident_pb2
from arsox_sdk.proto.arsox.thread.v1 import thread_pb2
from arsox_sdk.proto.arsox.turn.v1 import turn_pb2
from arsox_sdk.transport import Connection
from arsox_sdk.turn import TurnHandle


class ThreadHandle:
    """A handle to one thread.

    Holds an id and a connection. All the state lives on the satellite, which is
    what makes a handle disposable and a thread durable. Any process with the
    URL, the secret, and the id can attach and do everything the process that
    created the thread could: there is no handoff, no lease, and no ownership.
    """

    def __init__(self, connection: Connection, thread_id: str) -> None:
        """Point a handle at a thread. Obtained from `Threads.create` or `Threads.attach`."""
        self._connection = connection
        self.id = thread_id

    async def get(self) -> thread_pb2.Thread:
        """Read the thread's current state.

        Raises:
            ArsoxError: when the thread is unknown, expired, destroyed, or the
                satellite is unreachable.
        """
        response = await self._connection.call(
            "GET", f"/v1/threads/{self.id}", thread_pb2.GetThreadResponse
        )

        if not response.HasField("thread"):
            raise ArsoxError.transport("the satellite returned a thread with no thread in it")

        return response.thread

    async def start_turn(
        self,
        prompt: str,
        idempotency_key: str | None = None,
        metadata: dict[str, str] | None = None,
    ) -> TurnHandle:
        """Queue a turn.

        Returns as soon as the turn is queued. Await `TurnHandle.result` for the
        outcome.

        Args:
            prompt: what the thread is being asked to do.
            idempotency_key: deduplicates retries of this call. A second request
                carrying a key the satellite has already seen returns the
                original turn rather than queueing a second one.
            metadata: your own correlation data, stored verbatim and handed back
                on the result. The satellite never reads it and it never reaches
                an agent.

        Raises:
            ArsoxError: when the queue is full, the thread is unknown, or the
                satellite is unreachable.
        """
        # A None key is left unset rather than sent empty, which is the
        # difference between "deduplicate me" and "I have no key".
        request = turn_pb2.StartTurnRequest(
            thread_id=self.id,
            prompt=prompt,
            idempotency_key=idempotency_key,
            metadata=metadata or {},
        )

        response = await self._connection.call(
            "POST",
            f"/v1/threads/{self.id}/turns",
            turn_pb2.StartTurnResponse,
            request.SerializeToString(),
        )

        if not response.HasField("turn"):
            raise ArsoxError.transport("the satellite queued a turn without returning it")

        return TurnHandle(self._connection, self.id, response.turn)

    async def turns(self) -> list[turn_pb2.Turn]:
        """List this thread's turns, oldest first. Returns one page.

        Raises:
            ArsoxError: when the thread is unknown or the satellite is
                unreachable.
        """
        request = turn_pb2.ListTurnsRequest(
            thread_id=self.id,
            page=common_pb2.PageRequest(),
            order_by=turn_pb2.TURN_ORDER_UNSPECIFIED,
            descending=False,
        )

        response = await self._connection.call(
            "GET",
            f"/v1/threads/{self.id}/turns",
            turn_pb2.ListTurnsResponse,
            request.SerializeToString(),
        )

        return list(response.turns)

    async def incidents(
        self, query: incident_pb2.ListIncidentsRequest | None = None
    ) -> list[incident_pb2.Incident]:
        """List this thread's incidents, oldest first. Returns one page.

        Answers after the thread is expired or destroyed, because incidents carry
        their own retention and the workspace's collection never touches them.
        `thread_ids` on the query is ignored: this listing is already scoped, and
        the path wins so the URL says what it looks like it says.

        Raises:
            ArsoxError: when the satellite is unreachable or rejects the secret.
        """
        request = incident_pb2.ListIncidentsRequest()
        if query is not None:
            request.CopyFrom(query)
        del request.thread_ids[:]
        request.thread_ids.append(self.id)

        response = await self._connection.call(
            "GET",
            f"/v1/threads/{self.id}/incidents",
            incident_pb2.ListIncidentsResponse,
            request.SerializeToString(),
        )

        return list(response.incidents)

    async def destroy(self) -> thread_pb2.Thread:
        """Destroy the thread and everything under it.

        Incidents survive on their own retention, because "why did last night go
        wrong" is asked after the workspace is gone.

        Raises:
            ArsoxError: when the thread is unknown or the satellite is
                unreachable.
        """
        response = await self._connection.call(
            "DELETE", f"/v1/threads/{self.id}", thread_pb2.DestroyThreadResponse
        )

        if not response.HasField("thread"):
            raise ArsoxError.transport("the satellite destroyed a thread without saying so")

        return response.thread

    async def pause(self) -> thread_pb2.Thread:
        """Stop the thread claiming queued work, without losing anything.

        Turns may still be submitted and still queue; the queue simply does not
        move until the thread resumes. This is the state an operator reaches for
        when destroying the thread would lose the workspace.

        Raises:
            ArsoxError: when the thread is unknown or the satellite is
                unreachable.
        """
        response = await self._connection.call(
            "POST", f"/v1/threads/{self.id}/pause", thread_pb2.PauseThreadResponse
        )

        if not response.HasField("thread"):
            raise ArsoxError.transport("the satellite paused a thread without saying so")

        return response.thread

    async def resume(self) -> thread_pb2.Thread:
        """Return a paused thread to service.

        Raises:
            ArsoxError: when the thread is unknown or the satellite is
                unreachable.
        """
        response = await self._connection.call(
            "POST", f"/v1/threads/{self.id}/resume", thread_pb2.ResumeThreadResponse
        )

        if not response.HasField("thread"):
            raise ArsoxError.transport("the satellite resumed a thread without saying so")

        return response.thread

    async def drain(self) -> list[str]:
        """Cancel every queued turn, leaving any running turn alone.

        One call rather than a loop, because cancelling turns one at a time races
        the runner claiming them, and that is a race an operator should not have
        to win. Pause first if the intent is to stop the thread rather than clear
        a backlog.

        Returns the ids it cancelled, so a caller that needs the thread fully
        stopped can see there is still something running and cancel it
        explicitly.

        Raises:
            ArsoxError: when the thread is unknown or the satellite is
                unreachable.
        """
        response = await self._connection.call(
            "POST", f"/v1/threads/{self.id}/drain", thread_pb2.DrainThreadResponse
        )

        return list(response.cancelled_turn_ids)

    async def events(self, from_sequence: int = 0) -> EventStream:
        """Stream this thread's events.

        Resolves once the socket is open, so a refused handshake is reported here
        rather than at the first event.

        Args:
            from_sequence: resume after this sequence. Exclusive: pass the last
                sequence you actually handled and receive everything since. This
                is how a replica that died mid-turn picks up without losing an
                event. Zero starts at the beginning of retained history.

        Raises:
            ArsoxError: when the socket cannot be opened, including when the
                requested sequence is older than retained history.
        """
        socket = await self._connection.open_socket(
            f"/v1/threads/{self.id}/stream?from_sequence={from_sequence}"
        )

        return EventStream(socket)
