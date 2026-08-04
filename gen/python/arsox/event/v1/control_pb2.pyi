from arsox.common.v1 import common_pb2 as _common_pb2
from arsox.event.v1 import lifecycle_pb2 as _lifecycle_pb2
from arsox.thread.v1 import thread_pb2 as _thread_pb2
from google.protobuf.internal import enum_type_wrapper as _enum_type_wrapper
from google.protobuf import descriptor as _descriptor
from google.protobuf import message as _message
from collections.abc import Mapping as _Mapping
from typing import ClassVar as _ClassVar, Optional as _Optional, Union as _Union

DESCRIPTOR: _descriptor.FileDescriptor

class ThreadEndReason(int, metaclass=_enum_type_wrapper.EnumTypeWrapper):
    __slots__ = ()
    THREAD_END_REASON_UNSPECIFIED: _ClassVar[ThreadEndReason]
    THREAD_END_REASON_DESTROYED: _ClassVar[ThreadEndReason]
    THREAD_END_REASON_EXPIRED: _ClassVar[ThreadEndReason]
    THREAD_END_REASON_COMPLETED: _ClassVar[ThreadEndReason]
THREAD_END_REASON_UNSPECIFIED: ThreadEndReason
THREAD_END_REASON_DESTROYED: ThreadEndReason
THREAD_END_REASON_EXPIRED: ThreadEndReason
THREAD_END_REASON_COMPLETED: ThreadEndReason

class ControlEvent(_message.Message):
    __slots__ = ()
    SEQUENCE_FIELD_NUMBER: _ClassVar[int]
    OCCURRED_AT_FIELD_NUMBER: _ClassVar[int]
    TYPE_FIELD_NUMBER: _ClassVar[int]
    THREAD_CREATED_FIELD_NUMBER: _ClassVar[int]
    THREAD_STATE_CHANGED_FIELD_NUMBER: _ClassVar[int]
    THREAD_DESTROYED_FIELD_NUMBER: _ClassVar[int]
    QUEUE_DEPTH_CHANGED_FIELD_NUMBER: _ClassVar[int]
    HEALTH_CHANGED_FIELD_NUMBER: _ClassVar[int]
    BUDGET_WARNING_FIELD_NUMBER: _ClassVar[int]
    sequence: int
    occurred_at: _common_pb2.Timestamp
    type: str
    thread_created: ThreadCreated
    thread_state_changed: ThreadStateChanged
    thread_destroyed: ThreadDestroyed
    queue_depth_changed: QueueDepthChanged
    health_changed: HealthChanged
    budget_warning: ControlBudgetWarning
    def __init__(self, sequence: _Optional[int] = ..., occurred_at: _Optional[_Union[_common_pb2.Timestamp, _Mapping]] = ..., type: _Optional[str] = ..., thread_created: _Optional[_Union[ThreadCreated, _Mapping]] = ..., thread_state_changed: _Optional[_Union[ThreadStateChanged, _Mapping]] = ..., thread_destroyed: _Optional[_Union[ThreadDestroyed, _Mapping]] = ..., queue_depth_changed: _Optional[_Union[QueueDepthChanged, _Mapping]] = ..., health_changed: _Optional[_Union[HealthChanged, _Mapping]] = ..., budget_warning: _Optional[_Union[ControlBudgetWarning, _Mapping]] = ...) -> None: ...

class ThreadCreated(_message.Message):
    __slots__ = ()
    THREAD_ID_FIELD_NUMBER: _ClassVar[int]
    thread_id: str
    def __init__(self, thread_id: _Optional[str] = ...) -> None: ...

class ThreadStateChanged(_message.Message):
    __slots__ = ()
    THREAD_ID_FIELD_NUMBER: _ClassVar[int]
    PREVIOUS_FIELD_NUMBER: _ClassVar[int]
    CURRENT_FIELD_NUMBER: _ClassVar[int]
    thread_id: str
    previous: _thread_pb2.ThreadState
    current: _thread_pb2.ThreadState
    def __init__(self, thread_id: _Optional[str] = ..., previous: _Optional[_Union[_thread_pb2.ThreadState, str]] = ..., current: _Optional[_Union[_thread_pb2.ThreadState, str]] = ...) -> None: ...

class ThreadDestroyed(_message.Message):
    __slots__ = ()
    THREAD_ID_FIELD_NUMBER: _ClassVar[int]
    REASON_FIELD_NUMBER: _ClassVar[int]
    thread_id: str
    reason: ThreadEndReason
    def __init__(self, thread_id: _Optional[str] = ..., reason: _Optional[_Union[ThreadEndReason, str]] = ...) -> None: ...

class QueueDepthChanged(_message.Message):
    __slots__ = ()
    THREAD_ID_FIELD_NUMBER: _ClassVar[int]
    QUEUE_DEPTH_FIELD_NUMBER: _ClassVar[int]
    thread_id: str
    queue_depth: int
    def __init__(self, thread_id: _Optional[str] = ..., queue_depth: _Optional[int] = ...) -> None: ...

class HealthChanged(_message.Message):
    __slots__ = ()
    READY_FIELD_NUMBER: _ClassVar[int]
    CHECK_NAME_FIELD_NUMBER: _ClassVar[int]
    DETAIL_FIELD_NUMBER: _ClassVar[int]
    ready: bool
    check_name: str
    detail: str
    def __init__(self, ready: _Optional[bool] = ..., check_name: _Optional[str] = ..., detail: _Optional[str] = ...) -> None: ...

class ControlBudgetWarning(_message.Message):
    __slots__ = ()
    THREAD_ID_FIELD_NUMBER: _ClassVar[int]
    CEILING_FIELD_NUMBER: _ClassVar[int]
    PERCENT_USED_FIELD_NUMBER: _ClassVar[int]
    thread_id: str
    ceiling: _lifecycle_pb2.Ceiling
    percent_used: int
    def __init__(self, thread_id: _Optional[str] = ..., ceiling: _Optional[_Union[_lifecycle_pb2.Ceiling, str]] = ..., percent_used: _Optional[int] = ...) -> None: ...
