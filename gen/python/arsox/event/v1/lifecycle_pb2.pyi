from arsox.artifact.v1 import artifact_pb2 as _artifact_pb2
from arsox.event.v1 import author_pb2 as _author_pb2
from arsox.interaction.v1 import plan_pb2 as _plan_pb2
from arsox.interaction.v1 import question_pb2 as _question_pb2
from arsox.turn.v1 import result_pb2 as _result_pb2
from arsox.turn.v1 import turn_pb2 as _turn_pb2
from arsox.usage.v1 import usage_pb2 as _usage_pb2
from google.protobuf.internal import containers as _containers
from google.protobuf.internal import enum_type_wrapper as _enum_type_wrapper
from google.protobuf import descriptor as _descriptor
from google.protobuf import message as _message
from collections.abc import Iterable as _Iterable, Mapping as _Mapping
from typing import ClassVar as _ClassVar, Optional as _Optional, Union as _Union

DESCRIPTOR: _descriptor.FileDescriptor

class Ceiling(int, metaclass=_enum_type_wrapper.EnumTypeWrapper):
    __slots__ = ()
    CEILING_UNSPECIFIED: _ClassVar[Ceiling]
    CEILING_TOKENS_PER_TURN: _ClassVar[Ceiling]
    CEILING_COST_PER_THREAD: _ClassVar[Ceiling]
    CEILING_WALL_CLOCK_PER_TURN: _ClassVar[Ceiling]
CEILING_UNSPECIFIED: Ceiling
CEILING_TOKENS_PER_TURN: Ceiling
CEILING_COST_PER_THREAD: Ceiling
CEILING_WALL_CLOCK_PER_TURN: Ceiling

class BudgetWarning(_message.Message):
    __slots__ = ()
    CEILING_FIELD_NUMBER: _ClassVar[int]
    PERCENT_USED_FIELD_NUMBER: _ClassVar[int]
    ceiling: Ceiling
    percent_used: int
    def __init__(self, ceiling: _Optional[_Union[Ceiling, str]] = ..., percent_used: _Optional[int] = ...) -> None: ...

class PlanProposed(_message.Message):
    __slots__ = ()
    PLAN_FIELD_NUMBER: _ClassVar[int]
    plan: _plan_pb2.Plan
    def __init__(self, plan: _Optional[_Union[_plan_pb2.Plan, _Mapping]] = ...) -> None: ...

class PlanDecided(_message.Message):
    __slots__ = ()
    PLAN_ID_FIELD_NUMBER: _ClassVar[int]
    DECISION_FIELD_NUMBER: _ClassVar[int]
    AUTO_APPROVED_FIELD_NUMBER: _ClassVar[int]
    plan_id: str
    decision: _plan_pb2.PlanDecision
    auto_approved: bool
    def __init__(self, plan_id: _Optional[str] = ..., decision: _Optional[_Union[_plan_pb2.PlanDecision, str]] = ..., auto_approved: _Optional[bool] = ...) -> None: ...

class QuestionAsked(_message.Message):
    __slots__ = ()
    QUESTION_SET_FIELD_NUMBER: _ClassVar[int]
    question_set: _question_pb2.QuestionSet
    def __init__(self, question_set: _Optional[_Union[_question_pb2.QuestionSet, _Mapping]] = ...) -> None: ...

class QuestionAnswered(_message.Message):
    __slots__ = ()
    QUESTION_SET_ID_FIELD_NUMBER: _ClassVar[int]
    ANSWERS_FIELD_NUMBER: _ClassVar[int]
    TIMED_OUT_FIELD_NUMBER: _ClassVar[int]
    question_set_id: str
    answers: _containers.RepeatedCompositeFieldContainer[_question_pb2.QuestionAnswer]
    timed_out: bool
    def __init__(self, question_set_id: _Optional[str] = ..., answers: _Optional[_Iterable[_Union[_question_pb2.QuestionAnswer, _Mapping]]] = ..., timed_out: _Optional[bool] = ...) -> None: ...

class ArtifactCreated(_message.Message):
    __slots__ = ()
    ARTIFACT_FIELD_NUMBER: _ClassVar[int]
    artifact: _artifact_pb2.Artifact
    def __init__(self, artifact: _Optional[_Union[_artifact_pb2.Artifact, _Mapping]] = ...) -> None: ...

class RedactionOverridden(_message.Message):
    __slots__ = ()
    AUTHOR_FIELD_NUMBER: _ClassVar[int]
    SECRET_KEY_FIELD_NUMBER: _ClassVar[int]
    JUSTIFICATION_FIELD_NUMBER: _ClassVar[int]
    OPERATION_FIELD_NUMBER: _ClassVar[int]
    author: _author_pb2.Author
    secret_key: str
    justification: str
    operation: str
    def __init__(self, author: _Optional[_Union[_author_pb2.Author, _Mapping]] = ..., secret_key: _Optional[str] = ..., justification: _Optional[str] = ..., operation: _Optional[str] = ...) -> None: ...

class TurnStarted(_message.Message):
    __slots__ = ()
    TURN_FIELD_NUMBER: _ClassVar[int]
    turn: _turn_pb2.Turn
    def __init__(self, turn: _Optional[_Union[_turn_pb2.Turn, _Mapping]] = ...) -> None: ...

class TurnCompleted(_message.Message):
    __slots__ = ()
    RESULT_FIELD_NUMBER: _ClassVar[int]
    result: _result_pb2.TurnResult
    def __init__(self, result: _Optional[_Union[_result_pb2.TurnResult, _Mapping]] = ...) -> None: ...

class StatisticsUpdated(_message.Message):
    __slots__ = ()
    STATISTICS_FIELD_NUMBER: _ClassVar[int]
    statistics: _usage_pb2.LifetimeStatistics
    def __init__(self, statistics: _Optional[_Union[_usage_pb2.LifetimeStatistics, _Mapping]] = ...) -> None: ...
