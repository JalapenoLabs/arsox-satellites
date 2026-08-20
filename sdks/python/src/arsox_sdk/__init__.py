# Copyright © 2026 Jalapeno Labs

"""The Python client for an Arsox satellite.

Protobuf is always on the wire, in both directions, and no setting changes that.
What a consumer holds is the generated contract message, typed by its `.pyi`
stub, rather than a second hand-written copy of a contract that only ever grows.

```python
import asyncio
from arsox_sdk import Duration, Satellite, ThreadSettings

async def main() -> None:
    async with await Satellite.connect(url, secret) as satellite:
        created = await satellite.threads().create(
            ThreadSettings(idle_ttl=Duration(seconds=3600), budget={})
        )

        turn = await created.handle.start_turn("Add rate limiting to the public API.")
        result = await turn.result()

asyncio.run(main())
```

The rest of the contract is reachable at its generated path, for example
`from arsox_sdk.proto.arsox.settings.v1 import permission_pb2`.
"""

from __future__ import annotations

# ####################### #
#       THE CLIENT        #
# ####################### #
from arsox_sdk.constants import (
    PROTOBUF_CONTENT_TYPE,
    RESULT_POLL_SECONDS,
    SDK_PROTO_MAJOR,
    SDK_PROTO_MINOR,
)
from arsox_sdk.errors import ArsoxError, ArsoxErrorKind
from arsox_sdk.events import EventStream

# ####################### #
#      THE CONTRACT       #
# ####################### #
#
# The messages and enums this surface returns, re-exported so the common case
# needs one import. Enums are module-level constants in generated Python, so a
# consumer matches on `TURN_STATUS_COMPLETED` and never on a string.
from arsox_sdk.proto.arsox.common.v1.common_pb2 import (
    Duration,
    Money,
    PageRequest,
    PageResponse,
    Secret,
    Timestamp,
)
from arsox_sdk.proto.arsox.error.v1.error_pb2 import Error as ContractError
from arsox_sdk.proto.arsox.error.v1.error_pb2 import ErrorCode
from arsox_sdk.proto.arsox.event.v1.event_pb2 import ThreadEvent
from arsox_sdk.proto.arsox.harness.v1.harness_pb2 import (
    GetHarnessResponse,
    Harness,
    HarnessCapabilities,
)
from arsox_sdk.proto.arsox.incident.v1.incident_pb2 import (
    Disposition,
    Incident,
    IncidentCounts,
    ListIncidentsRequest,
)
from arsox_sdk.proto.arsox.satellite.v1.satellite_pb2 import (
    DiskUsage,
    GetStatusResponse,
    GetVersionResponse,
)
from arsox_sdk.proto.arsox.settings.v1.settings_pb2 import ThreadSettings
from arsox_sdk.proto.arsox.thread.v1.thread_pb2 import (
    Thread,
    ThreadOrder,
    ThreadState,
    ThreadSummary,
)
from arsox_sdk.proto.arsox.turn.v1.result_pb2 import (
    Stage,
    StageDisposition,
    StageOutcome,
    TurnResult,
)
from arsox_sdk.proto.arsox.turn.v1.turn_pb2 import Turn, TurnOrder, TurnStatus
from arsox_sdk.proto.arsox.usage.v1.usage_pb2 import CostEstimate, TokenUsage
from arsox_sdk.satellite import Satellite, ThreadCreated, Threads
from arsox_sdk.thread import ThreadHandle
from arsox_sdk.turn import TurnHandle

__all__ = [
    # The client.
    "ArsoxError",
    "ArsoxErrorKind",
    "EventStream",
    "Satellite",
    "ThreadCreated",
    "ThreadHandle",
    "Threads",
    "TurnHandle",
    "PROTOBUF_CONTENT_TYPE",
    "RESULT_POLL_SECONDS",
    "SDK_PROTO_MAJOR",
    "SDK_PROTO_MINOR",
    # The contract.
    "ContractError",
    "CostEstimate",
    "DiskUsage",
    "Disposition",
    "Duration",
    "ErrorCode",
    "GetHarnessResponse",
    "GetStatusResponse",
    "GetVersionResponse",
    "Harness",
    "HarnessCapabilities",
    "Incident",
    "IncidentCounts",
    "ListIncidentsRequest",
    "Money",
    "PageRequest",
    "PageResponse",
    "Secret",
    "Stage",
    "StageDisposition",
    "StageOutcome",
    "Thread",
    "ThreadEvent",
    "ThreadOrder",
    "ThreadSettings",
    "ThreadState",
    "ThreadSummary",
    "Timestamp",
    "TokenUsage",
    "Turn",
    "TurnOrder",
    "TurnResult",
    "TurnStatus",
]
