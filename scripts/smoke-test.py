"""Drive the containerized satellite with real protobuf requests."""

from __future__ import annotations

import sys
import time
import urllib.error
import urllib.request

sys.path.insert(0, "gen/python")

from arsox.common.v1 import common_pb2
from arsox.error.v1 import error_pb2
from arsox.settings.v1 import budget_pb2, settings_pb2
from arsox.thread.v1 import thread_pb2
from arsox.event.v1 import event_pb2
from arsox.turn.v1 import result_pb2, turn_pb2

# Unique per run, so re-running against a live container exercises a fresh
# thread rather than deduplicating onto the previous run's finished one.
RUN = str(int(time.time()))
BASE = "http://127.0.0.1:18080"
SECRET = "container-secret"

passed = 0
failed = 0


def check(label: str, condition: bool, detail: str = "") -> None:
    global passed, failed
    if condition:
        passed += 1
        print(f"  PASS  {label}")
    else:
        failed += 1
        print(f"  FAIL  {label}  {detail}")


def call(method: str, path: str, body: bytes | None = None, *, token=SECRET, content_type="application/protobuf"):
    request = urllib.request.Request(f"{BASE}{path}", data=body, method=method)
    if token is not None:
        request.add_header("Authorization", f"Bearer {token}")
    if body is not None and content_type is not None:
        request.add_header("Content-Type", content_type)
    try:
        with urllib.request.urlopen(request) as response:
            return response.status, response.read()
    except urllib.error.HTTPError as error:
        return error.code, error.read()


def decode_error(payload: bytes):
    error = error_pb2.Error()
    error.ParseFromString(payload)
    return error


def build_settings() -> settings_pb2.ThreadSettings:
    settings = settings_pb2.ThreadSettings()
    settings.idle_ttl.seconds = 7200
    settings.budget.max_tokens_per_turn.tokens = 8_000_000
    settings.budget.max_cost_per_thread.cost.currency_code = "USD"
    settings.budget.max_cost_per_thread.cost.units = 40
    return settings


print("== thread lifecycle ==")

create = thread_pb2.CreateThreadRequest(idempotency_key=f"smoke-{RUN}")
create.settings.CopyFrom(build_settings())
create.metadata["tenant"] = "acme"
create.metadata["triggered_by"] = "smoke-test"

status, payload = call("POST", "/v1/threads", create.SerializeToString())
created = thread_pb2.CreateThreadResponse()
created.ParseFromString(payload)
thread_id = created.thread.thread_id

check("create returns 200", status == 200, f"got {status}")
check("thread id is a UUIDv7", thread_id.count("-") == 4 and thread_id[14] == "7", thread_id)
check("not deduplicated on first create", not created.deduplicated)
check("metadata round-trips", dict(created.thread.metadata).get("tenant") == "acme")
check("idle ttl echoed back", created.thread.settings.idle_ttl.seconds == 7200)
check("expires_at is set from the ttl", created.thread.HasField("expires_at"))

status, payload = call("POST", "/v1/threads", create.SerializeToString())
again = thread_pb2.CreateThreadResponse()
again.ParseFromString(payload)
check("same idempotency key deduplicates", again.deduplicated)
check("and returns the original thread", again.thread.thread_id == thread_id)

status, payload = call("GET", f"/v1/threads/{thread_id}")
fetched = thread_pb2.GetThreadResponse()
fetched.ParseFromString(payload)
check("get returns the thread", fetched.thread.thread_id == thread_id)

status, payload = call("GET", "/v1/threads")
listed = thread_pb2.ListThreadsResponse()
listed.ParseFromString(payload)
check("list returns the thread", any(t.thread_id == thread_id for t in listed.threads))
check("summaries carry no settings field", not any(f.name == "settings" for f in thread_pb2.ThreadSummary.DESCRIPTOR.fields))
check("next cursor is set", listed.page.next_cursor != "")

query = thread_pb2.ListThreadsRequest()
query.metadata["tenant"] = "globex"
status, payload = call("GET", "/v1/threads", query.SerializeToString())
filtered = thread_pb2.ListThreadsResponse()
filtered.ParseFromString(payload)
check("metadata filter excludes non-matches", len(filtered.threads) == 0, f"got {len(filtered.threads)}")

print("== turn queue ==")

# Two turns: the first is claimed by the runner immediately, the second waits
# behind it. Cancelling the queued one is deterministic, where cancelling the
# running one would race the runner to a terminal state.
start = turn_pb2.StartTurnRequest(prompt="replay the probe", idempotency_key=f"turn-{RUN}")
start.metadata["request_id"] = "req-42"
status, payload = call("POST", f"/v1/threads/{thread_id}/turns", start.SerializeToString())
started = turn_pb2.StartTurnResponse()
started.ParseFromString(payload)
running_turn = started.turn.turn_id

check("start turn returns 200", status == 200, f"got {status}")
check("turn is queued", started.turn.status == turn_pb2.TURN_STATUS_QUEUED)
check("turn metadata round-trips", dict(started.turn.metadata).get("request_id") == "req-42")

status, payload = call("POST", f"/v1/threads/{thread_id}/turns", start.SerializeToString())
repeat = turn_pb2.StartTurnResponse()
repeat.ParseFromString(payload)
check("turn idempotency deduplicates", repeat.turn.turn_id == running_turn)

queued = turn_pb2.StartTurnRequest(prompt="wait your turn")
status, payload = call("POST", f"/v1/threads/{thread_id}/turns", queued.SerializeToString())
second = turn_pb2.StartTurnResponse()
second.ParseFromString(payload)
queued_turn = second.turn.turn_id

status, payload = call("POST", f"/v1/threads/{thread_id}/turns/{queued_turn}/cancel")
cancelled = turn_pb2.CancelTurnResponse()
cancelled.ParseFromString(payload)
check("a queued turn cancels", cancelled.turn.status == turn_pb2.TURN_STATUS_CANCELLED)

status, payload = call("POST", f"/v1/threads/{thread_id}/turns/{queued_turn}/cancel")
check("cancelling twice is a no-op rather than an error", status == 200, f"got {status}")

print("== the turn actually runs ==")

deadline = time.time() + 30
final = None
while time.time() < deadline:
    status, payload = call("GET", f"/v1/threads/{thread_id}/turns/{running_turn}")
    fetched = result_pb2.GetTurnResponse()
    fetched.ParseFromString(payload)
    if fetched.turn.status not in (turn_pb2.TURN_STATUS_QUEUED, turn_pb2.TURN_STATUS_RUNNING):
        final = fetched
        break
    time.sleep(0.5)

check("the turn reached a terminal state", final is not None)
if final is not None:
    check(
        "the turn completed",
        final.turn.status == turn_pb2.TURN_STATUS_COMPLETED,
        turn_pb2.TurnStatus.Name(final.turn.status),
    )
    check("a result was recorded", final.HasField("result"))
    check("usage was captured", final.result.tokens.total_tokens > 0)
    check("cost is Money, not a float", final.result.cost.amount.currency_code == "USD")
    check("usage is split per model", len(final.result.by_model) >= 2)
    check(
        "reasoning tokens absent rather than zero",
        not final.result.tokens.HasField("reasoning_output_tokens"),
    )
    skipped = [s for s in final.result.stages if s.disposition == result_pb2.STAGE_DISPOSITION_SKIPPED]
    check("unimplemented stages report as skipped", len(skipped) >= 5)

status, payload = call("GET", f"/v1/threads/{thread_id}")
after = thread_pb2.GetThreadResponse()
after.ParseFromString(payload)
check("the thread returned to idle", after.thread.state == thread_pb2.THREAD_STATE_IDLE)
check("the queue drained", after.thread.queue_depth == 0)
check("the harness session was recorded", after.thread.harness_session_id != "")
check("the event log advanced", after.thread.latest_sequence >= 6)

print("== error contract ==")

status, payload = call("GET", "/v1/threads/does-not-exist")
check("unknown thread is 404", status == 404, f"got {status}")
check(
    "and carries THREAD_NOT_FOUND",
    decode_error(payload).code == error_pb2.ERROR_CODE_THREAD_NOT_FOUND,
)
check("marked not retryable", not decode_error(payload).retryable)

status, payload = call("GET", f"/v1/threads/{thread_id}", token=None)
check("missing auth is 401", status == 401, f"got {status}")
check(
    "and carries AUTH_HEADER_MISSING",
    decode_error(payload).code == error_pb2.ERROR_CODE_AUTH_HEADER_MISSING,
)

status, payload = call("POST", "/v1/threads", b"{}", content_type="application/json")
check("a JSON body is refused by name", status == 415, f"got {status}")
check(
    "and carries REQUEST_CONTENT_TYPE_UNSUPPORTED",
    decode_error(payload).code == error_pb2.ERROR_CODE_REQUEST_CONTENT_TYPE_UNSUPPORTED,
)

status, payload = call("POST", "/v1/threads", b"\xff\xff\xff\xff")
check("a malformed body is 400", status == 400, f"got {status}")
check(
    "and carries REQUEST_BODY_MALFORMED",
    decode_error(payload).code == error_pb2.ERROR_CODE_REQUEST_BODY_MALFORMED,
)

bare = thread_pb2.CreateThreadRequest()
bare.settings.CopyFrom(settings_pb2.ThreadSettings())
status, payload = call("POST", "/v1/threads", bare.SerializeToString())
check("a thread with no idle TTL is refused", status == 400, f"got {status}")
check(
    "and carries REQUEST_FIELD_MISSING",
    decode_error(payload).code == error_pb2.ERROR_CODE_REQUEST_FIELD_MISSING,
)

print(f"\n{passed} passed, {failed} failed")
with open("smoke-thread-id.txt", "w", encoding="utf-8") as handle:  # noqa: for the restart check
    handle.write(thread_id)
sys.exit(1 if failed else 0)
