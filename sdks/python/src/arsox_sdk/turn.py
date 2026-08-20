# Copyright © 2026 Jalapeno Labs

"""A handle to one turn."""

from __future__ import annotations

import asyncio

from arsox_sdk.constants import RESULT_POLL_SECONDS
from arsox_sdk.errors import ArsoxError
from arsox_sdk.proto.arsox.turn.v1 import result_pb2, turn_pb2
from arsox_sdk.transport import Connection


class TurnHandle:
    """A handle to one turn."""

    def __init__(self, connection: Connection, thread_id: str, turn: turn_pb2.Turn) -> None:
        """Wrap a queued turn. Obtained from `ThreadHandle.start_turn`."""
        self._connection = connection
        self._thread_id = thread_id
        self.id = turn.turn_id

        # The turn as it was when queued.
        self.queued = turn

    async def get(self) -> turn_pb2.Turn:
        """Read the turn's current state.

        Raises:
            ArsoxError: when the turn is unknown or the satellite is unreachable.
        """
        response = await self._connection.call(
            "GET",
            f"/v1/threads/{self._thread_id}/turns/{self.id}",
            result_pb2.GetTurnResponse,
        )

        if not response.HasField("turn"):
            raise ArsoxError.transport("the satellite returned a turn with no turn in it")

        return response.turn

    async def result(self) -> result_pb2.TurnResult:
        """Wait for the turn to reach a terminal state and return its result.

        Polls rather than watching the stream, because a caller awaiting a result
        has not necessarily subscribed and should not have to.

        Raises:
            ArsoxError: when the turn is unknown, the satellite is unreachable,
                or a finished turn carries no result.
        """
        while True:
            response = await self._connection.call(
                "GET",
                f"/v1/threads/{self._thread_id}/turns/{self.id}",
                result_pb2.GetTurnResponse,
            )

            status = response.turn.status if response.HasField("turn") else turn_pb2.TURN_STATUS_UNSPECIFIED
            if status not in (turn_pb2.TURN_STATUS_QUEUED, turn_pb2.TURN_STATUS_RUNNING):
                if not response.HasField("result"):
                    raise ArsoxError.transport(
                        "the satellite finished a turn without recording a result"
                    )

                return response.result

            await asyncio.sleep(RESULT_POLL_SECONDS)

    async def cancel(self) -> turn_pb2.Turn:
        """Ask the satellite to stop this turn.

        A running turn is asked to stop cooperatively first. Work already
        committed to a branch survives either way.

        Raises:
            ArsoxError: when the turn is unknown or the satellite is unreachable.
        """
        response = await self._connection.call(
            "POST",
            f"/v1/threads/{self._thread_id}/turns/{self.id}/cancel",
            turn_pb2.CancelTurnResponse,
        )

        if not response.HasField("turn"):
            raise ArsoxError.transport("the satellite cancelled a turn without saying so")

        return response.turn
