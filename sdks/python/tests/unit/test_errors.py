# Copyright © 2026 Jalapeno Labs

"""What a caller can ask an ArsoxError, without a satellite to ask it about."""

from __future__ import annotations

from typing import cast

from arsox_sdk import ArsoxError, ErrorCode
from arsox_sdk.proto.arsox.error.v1 import error_pb2


def test_a_contract_error_carries_the_code_and_the_flag() -> None:
    """A contract failure reports what the satellite said about it."""
    failure = ArsoxError.contract(
        error_pb2.Error(
            code=ErrorCode.ERROR_CODE_TURN_QUEUE_FULL,
            message="the queue is full",
            retryable=True,
        )
    )

    assert failure.kind == "contract"
    assert failure.code == error_pb2.ERROR_CODE_TURN_QUEUE_FULL
    assert failure.retryable
    assert "ERROR_CODE_TURN_QUEUE_FULL" in str(failure)
    assert "the queue is full" in str(failure)


def test_details_and_trace_id_come_through() -> None:
    """Structured context reaches the caller as an ordinary dictionary."""
    error = error_pb2.Error(
        code=error_pb2.ERROR_CODE_PERMISSION_COMMAND_DENIED,
        message="denied",
        retryable=False,
        trace_id="trace-9",
    )
    error.details["argv"] = "docker build ."

    failure = ArsoxError.contract(error)

    assert failure.details == {"argv": "docker build ."}
    assert failure.trace_id == "trace-9"


def test_absent_details_are_absent_rather_than_empty() -> None:
    """An error carrying no context reports none, not an empty dictionary."""
    failure = ArsoxError.contract(
        error_pb2.Error(code=error_pb2.ERROR_CODE_THREAD_NOT_FOUND, message="nope")
    )

    assert failure.details is None
    assert failure.trace_id is None


def test_an_unknown_code_falls_back_to_the_retryable_flag() -> None:
    """A code this build has never heard of is still handled.

    Codes are additive within a proto major, so an older SDK will meet codes a
    newer satellite names. Reporting the number keeps the specificity, and
    `retryable` is populated regardless of whether the name is known.
    """
    # Deliberately outside the enum, which is the whole point: proto3 enums are
    # open, so the wire carries it and the generated stub cannot name it.
    unknown = cast(error_pb2.ErrorCode, 999_999)

    failure = ArsoxError.contract(
        error_pb2.Error(code=unknown, message="something new went wrong", retryable=True)
    )

    assert failure.code == unknown
    assert failure.retryable
    assert "code 999999" in str(failure)
    # Nothing about an unknown code makes it one of the cases worth branching on.
    assert not failure.is_not_found()
    assert not failure.is_gone()
    assert not failure.is_incompatible()


def test_missing_and_gone_are_different_questions() -> None:
    """A destroyed thread is not a wrong id, and the two lead somewhere different."""
    destroyed = ArsoxError.contract(
        error_pb2.Error(code=error_pb2.ERROR_CODE_THREAD_DESTROYED, message="gone")
    )
    expired = ArsoxError.contract(
        error_pb2.Error(code=error_pb2.ERROR_CODE_THREAD_EXPIRED, message="collected")
    )
    missing = ArsoxError.contract(
        error_pb2.Error(code=error_pb2.ERROR_CODE_THREAD_NOT_FOUND, message="who?")
    )

    assert destroyed.is_gone() and not destroyed.is_not_found()
    assert expired.is_gone() and not expired.is_not_found()
    assert missing.is_not_found() and not missing.is_gone()


def test_a_transport_failure_is_retryable_and_names_no_code() -> None:
    """A refused connection is usually a satellite that has not finished starting."""
    failure = ArsoxError.transport("connection refused")

    assert failure.kind == "transport"
    assert failure.code is None
    assert failure.retryable


def test_a_version_mismatch_does_not_resolve_itself() -> None:
    """An incompatible satellite is not worth retrying, and says which majors clashed."""
    failure = ArsoxError.incompatible(2, 1)

    assert failure.is_incompatible()
    assert not failure.retryable
    assert "v2" in str(failure)
    assert "v1" in str(failure)
