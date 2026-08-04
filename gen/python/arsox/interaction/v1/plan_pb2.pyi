from arsox.common.v1 import common_pb2 as _common_pb2
from google.protobuf.internal import enum_type_wrapper as _enum_type_wrapper
from google.protobuf import descriptor as _descriptor
from google.protobuf import message as _message
from collections.abc import Mapping as _Mapping
from typing import ClassVar as _ClassVar, Optional as _Optional, Union as _Union

DESCRIPTOR: _descriptor.FileDescriptor

class PlanDecision(int, metaclass=_enum_type_wrapper.EnumTypeWrapper):
    __slots__ = ()
    PLAN_DECISION_UNSPECIFIED: _ClassVar[PlanDecision]
    PLAN_DECISION_APPROVED: _ClassVar[PlanDecision]
    PLAN_DECISION_REJECTED: _ClassVar[PlanDecision]
PLAN_DECISION_UNSPECIFIED: PlanDecision
PLAN_DECISION_APPROVED: PlanDecision
PLAN_DECISION_REJECTED: PlanDecision

class Plan(_message.Message):
    __slots__ = ()
    PLAN_ID_FIELD_NUMBER: _ClassVar[int]
    BODY_FIELD_NUMBER: _ClassVar[int]
    PROPOSED_AT_FIELD_NUMBER: _ClassVar[int]
    plan_id: str
    body: str
    proposed_at: _common_pb2.Timestamp
    def __init__(self, plan_id: _Optional[str] = ..., body: _Optional[str] = ..., proposed_at: _Optional[_Union[_common_pb2.Timestamp, _Mapping]] = ...) -> None: ...

class DecidePlanRequest(_message.Message):
    __slots__ = ()
    THREAD_ID_FIELD_NUMBER: _ClassVar[int]
    PLAN_ID_FIELD_NUMBER: _ClassVar[int]
    DECISION_FIELD_NUMBER: _ClassVar[int]
    FEEDBACK_FIELD_NUMBER: _ClassVar[int]
    thread_id: str
    plan_id: str
    decision: PlanDecision
    feedback: str
    def __init__(self, thread_id: _Optional[str] = ..., plan_id: _Optional[str] = ..., decision: _Optional[_Union[PlanDecision, str]] = ..., feedback: _Optional[str] = ...) -> None: ...

class DecidePlanResponse(_message.Message):
    __slots__ = ()
    PLAN_FIELD_NUMBER: _ClassVar[int]
    DECISION_FIELD_NUMBER: _ClassVar[int]
    plan: Plan
    decision: PlanDecision
    def __init__(self, plan: _Optional[_Union[Plan, _Mapping]] = ..., decision: _Optional[_Union[PlanDecision, str]] = ...) -> None: ...
