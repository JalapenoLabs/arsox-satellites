# Copyright © 2026 Jalapeno Labs

"""What the client can fail with."""

from __future__ import annotations

from typing import Any, Literal

from google.protobuf.json_format import MessageToDict

from arsox_sdk.proto.arsox.error.v1 import error_pb2

# Where a failure came from.
#
# A discriminant rather than a subclass per case, so a caller can match on it
# without isinstance, and so the SDK never has to recover its own meaning by
# reading its own message back.
ArsoxErrorKind = Literal["contract", "transport", "incompatible"]


class ArsoxError(Exception):
    """Anything that can go wrong talking to a satellite.

    One class rather than a family, because a caller handling a failure almost
    always wants the same three questions answered regardless of where it came
    from: which contract code was it, is it worth retrying, and what happened.

    Match on `code`, never on the message. Wording may change within a major
    version; codes may not.
    """

    def __init__(
        self,
        message: str,
        kind: ArsoxErrorKind,
        code: error_pb2.ErrorCode | None,
        retryable: bool,
        details: dict[str, Any] | None = None,
        trace_id: str | None = None,
    ) -> None:
        """Build a failure. Prefer the classmethods below."""
        super().__init__(message)
        self.kind = kind

        # The contract code, when the satellite named one. None for a failure
        # that never reached a satellite, such as a refused connection or a
        # version mismatch caught before the first call.
        self.code = code

        # Whether retrying the same request could plausibly succeed.
        #
        # Read from the satellite's own flag rather than matched against a list
        # of codes, which is what makes an older SDK safe against a newer
        # satellite: a code this build has never heard of still gets a usable
        # answer.
        self.retryable = retryable

        # Structured context, keyed per code. `field` and `reason` for a
        # validation failure, `argv` for a denied command, `host` for a denied
        # domain.
        self.details = details

        # Correlates the failure with the satellite's own logs. Present on
        # INTERNAL and absent on errors that are the caller's to fix.
        self.trace_id = trace_id

    @classmethod
    def contract(cls, error: error_pb2.Error) -> ArsoxError:
        """Report a contract error the satellite answered with."""
        # Codes are additive within a proto major, so a satellite may name one
        # this build has never heard of. Report it by number rather than
        # dropping it: the specificity is the point of the enum.
        try:
            named = error_pb2.ErrorCode.Name(error.code)
        except ValueError:
            named = f"code {error.code}"

        return cls(
            f"{named}: {error.message}",
            "contract",
            error.code,
            error.retryable,
            MessageToDict(error.details) if error.HasField("details") else None,
            error.trace_id if error.HasField("trace_id") else None,
        )

    @classmethod
    def transport(cls, message: str) -> ArsoxError:
        """Report a satellite that could not be reached or could not be read.

        Retryable, because a refused connection is usually a satellite that has
        not finished starting.
        """
        return cls(f"could not reach the satellite: {message}", "transport", None, True)

    @classmethod
    def incompatible(cls, satellite_major: int, sdk_major: int) -> ArsoxError:
        """Report a satellite serving a proto major this SDK does not speak."""
        return cls(
            f"this satellite serves proto v{satellite_major} and this SDK speaks "
            f"v{sdk_major}. Upgrade the SDK, or point at a satellite on the same major.",
            "incompatible",
            None,
            # A version mismatch does not resolve itself.
            False,
        )

    def is_not_found(self) -> bool:
        """Whether the satellite reported that the thing asked for does not exist."""
        return self.code in (
            error_pb2.ERROR_CODE_THREAD_NOT_FOUND,
            error_pb2.ERROR_CODE_TURN_NOT_FOUND,
        )

    def is_gone(self) -> bool:
        """Whether the thread existed and no longer does.

        Distinct from `is_not_found` on purpose. A thread that expired or was
        destroyed is a thread your application probably has a record of, and the
        right response is usually to open a new one and carry the work over. A
        thread that was never found is a bad id, and opening a new one would
        paper over the bug.

        Read `code` when the difference between expired and destroyed matters: an
        expired thread means the TTL was shorter than the way the application
        actually uses it.
        """
        return self.code in (
            error_pb2.ERROR_CODE_THREAD_EXPIRED,
            error_pb2.ERROR_CODE_THREAD_DESTROYED,
        )

    def is_incompatible(self) -> bool:
        """Whether this SDK is too old for the satellite it was pointed at."""
        return self.kind == "incompatible"
