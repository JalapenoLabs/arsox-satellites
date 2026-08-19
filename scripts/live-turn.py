"""Drive one real turn through a live satellite and print what came back.

The smoke test proves the machinery with a stand-in harness. This script is for
the other half: a satellite whose container carries the real Claude CLI and a
real credential. It creates a thread, runs one turn, and prints the reply with
its token accounting.
"""

from __future__ import annotations

import os
import sys
import time
import urllib.error
import urllib.request

sys.path.insert(0, "gen/python")

from arsox.settings.v1 import settings_pb2
from arsox.thread.v1 import thread_pb2
from arsox.turn.v1 import result_pb2, turn_pb2

BASE = os.environ.get("ARSOX_SMOKE_BASE", "http://127.0.0.1:18081")
SECRET = os.environ.get("ARSOX_SMOKE_SECRET", "container-secret")

# A short deterministic prompt by default; pass your own as the first argument.
PROMPT = sys.argv[1] if len(sys.argv) > 1 else "Reply with exactly: ARSOX LIVE"

TURN_TIMEOUT_SECONDS = 300


def call(method: str, path: str, body: bytes | None = None) -> tuple[int, bytes]:
    """One authenticated protobuf request against the satellite."""
    request = urllib.request.Request(f"{BASE}{path}", data=body, method=method)
    request.add_header("Authorization", f"Bearer {SECRET}")
    if body is not None:
        request.add_header("Content-Type", "application/protobuf")
    try:
        with urllib.request.urlopen(request) as response:
            return response.status, response.read()
    except urllib.error.HTTPError as error:
        return error.code, error.read()


create = thread_pb2.CreateThreadRequest()
create.settings.idle_ttl.seconds = 3600
create.settings.budget.max_tokens_per_turn.tokens = 2_000_000
create.settings.budget.max_cost_per_thread.cost.currency_code = "USD"
create.settings.budget.max_cost_per_thread.cost.units = 5

status, payload = call("POST", "/v1/threads", create.SerializeToString())
if status != 200:
    raise SystemExit(f"could not create a thread: {status} {payload!r}")

created = thread_pb2.CreateThreadResponse()
created.ParseFromString(payload)
thread_id = created.thread.thread_id
print(f"thread: {thread_id}")

start = turn_pb2.StartTurnRequest(prompt=PROMPT)
status, payload = call(
    "POST", f"/v1/threads/{thread_id}/turns", start.SerializeToString()
)
if status != 200:
    raise SystemExit(f"could not start the turn: {status} {payload!r}")

started = turn_pb2.StartTurnResponse()
started.ParseFromString(payload)
turn_id = started.turn.turn_id
print(f"turn:   {turn_id}")
print(f"prompt: {PROMPT}")

deadline = time.time() + TURN_TIMEOUT_SECONDS
final = None
while time.time() < deadline:
    status, payload = call("GET", f"/v1/threads/{thread_id}/turns/{turn_id}")
    fetched = result_pb2.GetTurnResponse()
    fetched.ParseFromString(payload)
    if fetched.turn.status not in (
        turn_pb2.TURN_STATUS_QUEUED,
        turn_pb2.TURN_STATUS_RUNNING,
    ):
        final = fetched
        break
    time.sleep(1)

if final is None:
    raise SystemExit(f"the turn did not finish within {TURN_TIMEOUT_SECONDS}s")

print(f"\nstatus: {turn_pb2.TurnStatus.Name(final.turn.status)}")

if final.HasField("result"):
    result = final.result
    print(f"\n--- reply ---\n{result.summary}\n-------------")
    print(
        f"tokens: {result.tokens.total_tokens} total "
        f"({result.tokens.input_tokens} in, {result.tokens.output_tokens} out)"
    )
    for per_model in result.by_model:
        print(f"  {per_model.model}: {per_model.tokens.total_tokens} tokens")
    if result.HasField("error"):
        print(f"error: {result.error.code} {result.error.message}")
