from arsox.common.v1 import common_pb2 as _common_pb2
from arsox.interaction.v1 import plan_pb2 as _plan_pb2
from arsox.interaction.v1 import question_pb2 as _question_pb2
from arsox.settings.v1 import settings_pb2 as _settings_pb2
from google.protobuf.internal import containers as _containers
from google.protobuf.internal import enum_type_wrapper as _enum_type_wrapper
from google.protobuf import descriptor as _descriptor
from google.protobuf import message as _message
from collections.abc import Iterable as _Iterable, Mapping as _Mapping
from typing import ClassVar as _ClassVar, Optional as _Optional, Union as _Union

DESCRIPTOR: _descriptor.FileDescriptor

class ThreadState(int, metaclass=_enum_type_wrapper.EnumTypeWrapper):
    __slots__ = ()
    THREAD_STATE_UNSPECIFIED: _ClassVar[ThreadState]
    THREAD_STATE_PROVISIONING: _ClassVar[ThreadState]
    THREAD_STATE_IDLE: _ClassVar[ThreadState]
    THREAD_STATE_RUNNING: _ClassVar[ThreadState]
    THREAD_STATE_AWAITING_INPUT: _ClassVar[ThreadState]
    THREAD_STATE_WATCHING: _ClassVar[ThreadState]
    THREAD_STATE_EXPIRED: _ClassVar[ThreadState]
    THREAD_STATE_DESTROYED: _ClassVar[ThreadState]
THREAD_STATE_UNSPECIFIED: ThreadState
THREAD_STATE_PROVISIONING: ThreadState
THREAD_STATE_IDLE: ThreadState
THREAD_STATE_RUNNING: ThreadState
THREAD_STATE_AWAITING_INPUT: ThreadState
THREAD_STATE_WATCHING: ThreadState
THREAD_STATE_EXPIRED: ThreadState
THREAD_STATE_DESTROYED: ThreadState

class Thread(_message.Message):
    __slots__ = ()
    THREAD_ID_FIELD_NUMBER: _ClassVar[int]
    STATE_FIELD_NUMBER: _ClassVar[int]
    SETTINGS_FIELD_NUMBER: _ClassVar[int]
    CREATED_AT_FIELD_NUMBER: _ClassVar[int]
    LAST_ACTIVITY_AT_FIELD_NUMBER: _ClassVar[int]
    EXPIRES_AT_FIELD_NUMBER: _ClassVar[int]
    QUEUE_DEPTH_FIELD_NUMBER: _ClassVar[int]
    CURRENT_TURN_ID_FIELD_NUMBER: _ClassVar[int]
    PENDING_QUESTIONS_FIELD_NUMBER: _ClassVar[int]
    PENDING_PLAN_FIELD_NUMBER: _ClassVar[int]
    LATEST_SEQUENCE_FIELD_NUMBER: _ClassVar[int]
    thread_id: str
    state: ThreadState
    settings: _settings_pb2.ThreadSettings
    created_at: _common_pb2.Timestamp
    last_activity_at: _common_pb2.Timestamp
    expires_at: _common_pb2.Timestamp
    queue_depth: int
    current_turn_id: str
    pending_questions: _question_pb2.QuestionSet
    pending_plan: _plan_pb2.Plan
    latest_sequence: int
    def __init__(self, thread_id: _Optional[str] = ..., state: _Optional[_Union[ThreadState, str]] = ..., settings: _Optional[_Union[_settings_pb2.ThreadSettings, _Mapping]] = ..., created_at: _Optional[_Union[_common_pb2.Timestamp, _Mapping]] = ..., last_activity_at: _Optional[_Union[_common_pb2.Timestamp, _Mapping]] = ..., expires_at: _Optional[_Union[_common_pb2.Timestamp, _Mapping]] = ..., queue_depth: _Optional[int] = ..., current_turn_id: _Optional[str] = ..., pending_questions: _Optional[_Union[_question_pb2.QuestionSet, _Mapping]] = ..., pending_plan: _Optional[_Union[_plan_pb2.Plan, _Mapping]] = ..., latest_sequence: _Optional[int] = ...) -> None: ...

class CreateThreadRequest(_message.Message):
    __slots__ = ()
    SETTINGS_FIELD_NUMBER: _ClassVar[int]
    IDEMPOTENCY_KEY_FIELD_NUMBER: _ClassVar[int]
    settings: _settings_pb2.ThreadSettings
    idempotency_key: str
    def __init__(self, settings: _Optional[_Union[_settings_pb2.ThreadSettings, _Mapping]] = ..., idempotency_key: _Optional[str] = ...) -> None: ...

class CreateThreadResponse(_message.Message):
    __slots__ = ()
    THREAD_FIELD_NUMBER: _ClassVar[int]
    DEDUPLICATED_FIELD_NUMBER: _ClassVar[int]
    thread: Thread
    deduplicated: bool
    def __init__(self, thread: _Optional[_Union[Thread, _Mapping]] = ..., deduplicated: _Optional[bool] = ...) -> None: ...

class GetThreadRequest(_message.Message):
    __slots__ = ()
    THREAD_ID_FIELD_NUMBER: _ClassVar[int]
    thread_id: str
    def __init__(self, thread_id: _Optional[str] = ...) -> None: ...

class GetThreadResponse(_message.Message):
    __slots__ = ()
    THREAD_FIELD_NUMBER: _ClassVar[int]
    thread: Thread
    def __init__(self, thread: _Optional[_Union[Thread, _Mapping]] = ...) -> None: ...

class ListThreadsRequest(_message.Message):
    __slots__ = ()
    STATES_FIELD_NUMBER: _ClassVar[int]
    PAGE_FIELD_NUMBER: _ClassVar[int]
    states: _containers.RepeatedScalarFieldContainer[ThreadState]
    page: _common_pb2.PageRequest
    def __init__(self, states: _Optional[_Iterable[_Union[ThreadState, str]]] = ..., page: _Optional[_Union[_common_pb2.PageRequest, _Mapping]] = ...) -> None: ...

class ListThreadsResponse(_message.Message):
    __slots__ = ()
    THREADS_FIELD_NUMBER: _ClassVar[int]
    PAGE_FIELD_NUMBER: _ClassVar[int]
    threads: _containers.RepeatedCompositeFieldContainer[Thread]
    page: _common_pb2.PageResponse
    def __init__(self, threads: _Optional[_Iterable[_Union[Thread, _Mapping]]] = ..., page: _Optional[_Union[_common_pb2.PageResponse, _Mapping]] = ...) -> None: ...

class DestroyThreadRequest(_message.Message):
    __slots__ = ()
    THREAD_ID_FIELD_NUMBER: _ClassVar[int]
    thread_id: str
    def __init__(self, thread_id: _Optional[str] = ...) -> None: ...

class DestroyThreadResponse(_message.Message):
    __slots__ = ()
    THREAD_FIELD_NUMBER: _ClassVar[int]
    thread: Thread
    def __init__(self, thread: _Optional[_Union[Thread, _Mapping]] = ...) -> None: ...
