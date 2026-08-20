# arsox-sdk

The Python client for an [Arsox](https://github.com/JalapenoLabs/arsox-satellites)
satellite. Protobuf on the wire, ordinary objects in your hands.

```bash
pip install arsox-sdk
```

## Usage

```python
import asyncio

from arsox_sdk import Duration, Satellite, ThreadSettings, TurnStatus


async def main() -> None:
    async with await Satellite.connect("http://127.0.0.1:8080", secret) as satellite:
        # An idle TTL and a budget are both required, always. The TTL is the
        # safety net against a forgotten workspace filling a disk, and a required
        # budget is what makes an unbounded spend a decision rather than a field
        # somebody left unset.
        created = await satellite.threads().create(
            ThreadSettings(idle_ttl=Duration(seconds=3600), budget={})
        )

        # Subscribe before you queue, so nothing published between the two is
        # lost.
        async with await created.handle.events() as events:
            turn = await created.handle.start_turn("Add rate limiting to the public API.")

            async for event in events:
                print(event.type)
                if event.type == "turn.completed":
                    break

        result = await turn.result()
        if result.status == TurnStatus.TURN_STATUS_COMPLETED:
            print(result.summary)

        await created.handle.destroy()


asyncio.run(main())
```

Everything is `async`. The client holds one connection pool, so close it when you
are done: `async with`, or `await satellite.close()`.

## What the client reaches today

`version`, `status`, `harness`, threads, turns, the thread event stream, and
incidents. Streaming settings toggles, text and markdown reports, artifacts, plan
approval, question answering, and the control socket are not there yet.
[`examples/example.py`](../../examples/example.py) at the repository root
describes the full surface from the README and is a design target rather than a
description of this package.

## What you hold

The generated contract message, typed by its `.pyi` stub. `Thread`, `Turn`,
`TurnResult`, `Incident`, and `ThreadEvent` are protobuf messages; nothing in the
package parses a `.proto` at runtime, and nothing hands you a dictionary of
unnamed keys.

The messages the surface returns are re-exported from the package root, so the
common case needs one import:

```python
from arsox_sdk import Disposition, ErrorCode, Incident, ThreadEvent, TurnStatus
```

The rest of the contract is reachable at its generated path:

```python
from arsox_sdk.proto.arsox.settings.v1 import permission_pb2
```

Message constructors take nested dictionaries, so settings read as ordinary
literals rather than a tree of constructors:

```python
ThreadSettings(
    idle_ttl={"seconds": 7200},
    budget={"max_tokens_per_turn": {"tokens": 8_000_000}},
)
```

Build messages that way rather than by assigning fields afterwards. The
generated stubs declare `__slots__ = ()`, which mypy reads literally and rejects
`message.field = value` under it. Constructor keywords, repeated-field methods,
and `CopyFrom` all typecheck.

**Absent is not zero.** A field a harness might not report is `optional` in the
contract, so ask before you read it:

```python
if result.tokens.HasField("reasoning_output_tokens"):
    ...
```

A harness with no reasoning accounting reports nothing, and a run that genuinely
reasoned for zero tokens reports zero. Reading the field without asking collapses
the two.

## Errors

One `ArsoxError`, carrying the contract's own answer to the three questions a
caller has:

```python
from arsox_sdk import ArsoxError, ErrorCode

try:
    await handle.get()
except ArsoxError as failure:
    if failure.is_gone():
        ...  # the thread existed and no longer does: open a new one
    elif failure.is_not_found():
        ...  # the id is wrong: opening a new thread would paper over the bug
    elif failure.retryable:
        ...
```

Match on `failure.code`, never on the message. Codes are additive within a proto
major, so a satellite may name one this build has never heard of; `retryable` is
always populated and answers for it.

## Development

```bash
python -m venv .venv
.venv/bin/pip install -e ".[dev]"

python scripts/sync_proto.py   # before lint, typecheck, or test
ruff check .
mypy
pytest tests/unit               # no network, no satellite
pytest tests/integration        # builds and drives a real satellite
```

`scripts/sync_proto.py` copies the checked-in contract from `gen/python` into
`src/arsox_sdk/proto`, which is git ignored. `gen/python` is the one committed
copy, so the contract can never be current in one tree and stale in the other.
Run it after any `buf generate`.

The integration suite builds `arsox-satellite` with `--features test-util` and
drives the real binary with a fake harness replaying a recorded transcript. No
network and no model, but the same satellite a consumer would run: anything the
SDK needs and cannot reach shows up there rather than in somebody's application.
