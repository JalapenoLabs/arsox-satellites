from arsox.artifact.v1 import artifact_pb2 as _artifact_pb2
from arsox.common.v1 import common_pb2 as _common_pb2
from arsox.error.v1 import error_pb2 as _error_pb2
from arsox.incident.v1 import incident_pb2 as _incident_pb2
from arsox.interaction.v1 import question_pb2 as _question_pb2
from arsox.suggestion.v1 import suggestion_pb2 as _suggestion_pb2
from arsox.turn.v1 import turn_pb2 as _turn_pb2
from arsox.usage.v1 import usage_pb2 as _usage_pb2
from google.protobuf.internal import containers as _containers
from google.protobuf.internal import enum_type_wrapper as _enum_type_wrapper
from google.protobuf import descriptor as _descriptor
from google.protobuf import message as _message
from collections.abc import Iterable as _Iterable, Mapping as _Mapping
from typing import ClassVar as _ClassVar, Optional as _Optional, Union as _Union

DESCRIPTOR: _descriptor.FileDescriptor

class Stage(int, metaclass=_enum_type_wrapper.EnumTypeWrapper):
    __slots__ = ()
    STAGE_UNSPECIFIED: _ClassVar[Stage]
    STAGE_PLAN: _ClassVar[Stage]
    STAGE_TEAM_WORK: _ClassVar[Stage]
    STAGE_CHECKERS: _ClassVar[Stage]
    STAGE_SELF_REVIEW: _ClassVar[Stage]
    STAGE_MERGE: _ClassVar[Stage]
    STAGE_ARTIFACTS: _ClassVar[Stage]
    STAGE_SUGGESTIONS: _ClassVar[Stage]
    STAGE_PULL_REQUEST_WATCH: _ClassVar[Stage]

class StageDisposition(int, metaclass=_enum_type_wrapper.EnumTypeWrapper):
    __slots__ = ()
    STAGE_DISPOSITION_UNSPECIFIED: _ClassVar[StageDisposition]
    STAGE_DISPOSITION_RAN: _ClassVar[StageDisposition]
    STAGE_DISPOSITION_SKIPPED: _ClassVar[StageDisposition]
    STAGE_DISPOSITION_FAILED: _ClassVar[StageDisposition]

class StopReason(int, metaclass=_enum_type_wrapper.EnumTypeWrapper):
    __slots__ = ()
    STOP_REASON_UNSPECIFIED: _ClassVar[StopReason]
    STOP_REASON_END_TURN: _ClassVar[StopReason]
    STOP_REASON_MAX_TOKENS: _ClassVar[StopReason]
    STOP_REASON_STOP_SEQUENCE: _ClassVar[StopReason]
    STOP_REASON_REFUSAL: _ClassVar[StopReason]
STAGE_UNSPECIFIED: Stage
STAGE_PLAN: Stage
STAGE_TEAM_WORK: Stage
STAGE_CHECKERS: Stage
STAGE_SELF_REVIEW: Stage
STAGE_MERGE: Stage
STAGE_ARTIFACTS: Stage
STAGE_SUGGESTIONS: Stage
STAGE_PULL_REQUEST_WATCH: Stage
STAGE_DISPOSITION_UNSPECIFIED: StageDisposition
STAGE_DISPOSITION_RAN: StageDisposition
STAGE_DISPOSITION_SKIPPED: StageDisposition
STAGE_DISPOSITION_FAILED: StageDisposition
STOP_REASON_UNSPECIFIED: StopReason
STOP_REASON_END_TURN: StopReason
STOP_REASON_MAX_TOKENS: StopReason
STOP_REASON_STOP_SEQUENCE: StopReason
STOP_REASON_REFUSAL: StopReason

class StageOutcome(_message.Message):
    __slots__ = ()
    STAGE_FIELD_NUMBER: _ClassVar[int]
    DISPOSITION_FIELD_NUMBER: _ClassVar[int]
    REASON_FIELD_NUMBER: _ClassVar[int]
    ELAPSED_FIELD_NUMBER: _ClassVar[int]
    stage: Stage
    disposition: StageDisposition
    reason: str
    elapsed: _common_pb2.Duration
    def __init__(self, stage: _Optional[_Union[Stage, str]] = ..., disposition: _Optional[_Union[StageDisposition, str]] = ..., reason: _Optional[str] = ..., elapsed: _Optional[_Union[_common_pb2.Duration, _Mapping]] = ...) -> None: ...

class TurnTiming(_message.Message):
    __slots__ = ()
    TOTAL_FIELD_NUMBER: _ClassVar[int]
    LLM_FIELD_NUMBER: _ClassVar[int]
    TIME_TO_FIRST_TOKEN_FIELD_NUMBER: _ClassVar[int]
    MODEL_ROUND_TRIPS_FIELD_NUMBER: _ClassVar[int]
    total: _common_pb2.Duration
    llm: _common_pb2.Duration
    time_to_first_token: _common_pb2.Duration
    model_round_trips: int
    def __init__(self, total: _Optional[_Union[_common_pb2.Duration, _Mapping]] = ..., llm: _Optional[_Union[_common_pb2.Duration, _Mapping]] = ..., time_to_first_token: _Optional[_Union[_common_pb2.Duration, _Mapping]] = ..., model_round_trips: _Optional[int] = ...) -> None: ...

class PullRequestWatchReport(_message.Message):
    __slots__ = ()
    PULL_REQUEST_NUMBER_FIELD_NUMBER: _ClassVar[int]
    ATTEMPTS_USED_FIELD_NUMBER: _ClassVar[int]
    CHECKS_PASSED_FIELD_NUMBER: _ClassVar[int]
    TERMINATION_REASON_FIELD_NUMBER: _ClassVar[int]
    pull_request_number: int
    attempts_used: int
    checks_passed: bool
    termination_reason: str
    def __init__(self, pull_request_number: _Optional[int] = ..., attempts_used: _Optional[int] = ..., checks_passed: _Optional[bool] = ..., termination_reason: _Optional[str] = ...) -> None: ...

class TurnResult(_message.Message):
    __slots__ = ()
    class MetadataEntry(_message.Message):
        __slots__ = ()
        KEY_FIELD_NUMBER: _ClassVar[int]
        VALUE_FIELD_NUMBER: _ClassVar[int]
        key: str
        value: str
        def __init__(self, key: _Optional[str] = ..., value: _Optional[str] = ...) -> None: ...
    TURN_ID_FIELD_NUMBER: _ClassVar[int]
    THREAD_ID_FIELD_NUMBER: _ClassVar[int]
    STATUS_FIELD_NUMBER: _ClassVar[int]
    SUMMARY_FIELD_NUMBER: _ClassVar[int]
    TOKENS_FIELD_NUMBER: _ClassVar[int]
    COST_FIELD_NUMBER: _ClassVar[int]
    BY_MODEL_FIELD_NUMBER: _ClassVar[int]
    ERROR_FIELD_NUMBER: _ClassVar[int]
    INCIDENT_COUNTS_FIELD_NUMBER: _ClassVar[int]
    MEMBERS_FIELD_NUMBER: _ClassVar[int]
    CHANGED_FILES_FIELD_NUMBER: _ClassVar[int]
    INTEGRATIONS_FIELD_NUMBER: _ClassVar[int]
    CHECKER_RESULTS_FIELD_NUMBER: _ClassVar[int]
    ARTIFACTS_FIELD_NUMBER: _ClassVar[int]
    SUGGESTIONS_FIELD_NUMBER: _ClassVar[int]
    STAGES_FIELD_NUMBER: _ClassVar[int]
    UNANSWERED_QUESTIONS_FIELD_NUMBER: _ClassVar[int]
    WATCH_FIELD_NUMBER: _ClassVar[int]
    AGENTS_REPO_COMMIT_FIELD_NUMBER: _ClassVar[int]
    METADATA_FIELD_NUMBER: _ClassVar[int]
    TIMING_FIELD_NUMBER: _ClassVar[int]
    STOP_REASON_FIELD_NUMBER: _ClassVar[int]
    RATE_LIMITS_FIELD_NUMBER: _ClassVar[int]
    turn_id: str
    thread_id: str
    status: _turn_pb2.TurnStatus
    summary: str
    tokens: _usage_pb2.TokenUsage
    cost: _usage_pb2.CostEstimate
    by_model: _containers.RepeatedCompositeFieldContainer[_usage_pb2.ModelStatistics]
    error: _error_pb2.Error
    incident_counts: _incident_pb2.IncidentCounts
    members: _containers.RepeatedCompositeFieldContainer[_turn_pb2.TeamMember]
    changed_files: _containers.RepeatedCompositeFieldContainer[_turn_pb2.ChangedFile]
    integrations: _containers.RepeatedCompositeFieldContainer[_turn_pb2.IntegrationRecord]
    checker_results: _containers.RepeatedCompositeFieldContainer[_turn_pb2.CheckerResult]
    artifacts: _containers.RepeatedCompositeFieldContainer[_artifact_pb2.Artifact]
    suggestions: _suggestion_pb2.SuggestionReport
    stages: _containers.RepeatedCompositeFieldContainer[StageOutcome]
    unanswered_questions: _containers.RepeatedCompositeFieldContainer[_question_pb2.QuestionSet]
    watch: PullRequestWatchReport
    agents_repo_commit: str
    metadata: _containers.ScalarMap[str, str]
    timing: TurnTiming
    stop_reason: StopReason
    rate_limits: _usage_pb2.RateLimitStatus
    def __init__(self, turn_id: _Optional[str] = ..., thread_id: _Optional[str] = ..., status: _Optional[_Union[_turn_pb2.TurnStatus, str]] = ..., summary: _Optional[str] = ..., tokens: _Optional[_Union[_usage_pb2.TokenUsage, _Mapping]] = ..., cost: _Optional[_Union[_usage_pb2.CostEstimate, _Mapping]] = ..., by_model: _Optional[_Iterable[_Union[_usage_pb2.ModelStatistics, _Mapping]]] = ..., error: _Optional[_Union[_error_pb2.Error, _Mapping]] = ..., incident_counts: _Optional[_Union[_incident_pb2.IncidentCounts, _Mapping]] = ..., members: _Optional[_Iterable[_Union[_turn_pb2.TeamMember, _Mapping]]] = ..., changed_files: _Optional[_Iterable[_Union[_turn_pb2.ChangedFile, _Mapping]]] = ..., integrations: _Optional[_Iterable[_Union[_turn_pb2.IntegrationRecord, _Mapping]]] = ..., checker_results: _Optional[_Iterable[_Union[_turn_pb2.CheckerResult, _Mapping]]] = ..., artifacts: _Optional[_Iterable[_Union[_artifact_pb2.Artifact, _Mapping]]] = ..., suggestions: _Optional[_Union[_suggestion_pb2.SuggestionReport, _Mapping]] = ..., stages: _Optional[_Iterable[_Union[StageOutcome, _Mapping]]] = ..., unanswered_questions: _Optional[_Iterable[_Union[_question_pb2.QuestionSet, _Mapping]]] = ..., watch: _Optional[_Union[PullRequestWatchReport, _Mapping]] = ..., agents_repo_commit: _Optional[str] = ..., metadata: _Optional[_Mapping[str, str]] = ..., timing: _Optional[_Union[TurnTiming, _Mapping]] = ..., stop_reason: _Optional[_Union[StopReason, str]] = ..., rate_limits: _Optional[_Union[_usage_pb2.RateLimitStatus, _Mapping]] = ...) -> None: ...

class GetTurnRequest(_message.Message):
    __slots__ = ()
    THREAD_ID_FIELD_NUMBER: _ClassVar[int]
    TURN_ID_FIELD_NUMBER: _ClassVar[int]
    thread_id: str
    turn_id: str
    def __init__(self, thread_id: _Optional[str] = ..., turn_id: _Optional[str] = ...) -> None: ...

class GetTurnResponse(_message.Message):
    __slots__ = ()
    TURN_FIELD_NUMBER: _ClassVar[int]
    RESULT_FIELD_NUMBER: _ClassVar[int]
    turn: _turn_pb2.Turn
    result: TurnResult
    def __init__(self, turn: _Optional[_Union[_turn_pb2.Turn, _Mapping]] = ..., result: _Optional[_Union[TurnResult, _Mapping]] = ...) -> None: ...
