from arsox.common.v1 import common_pb2 as _common_pb2
from google.protobuf.internal import containers as _containers
from google.protobuf.internal import enum_type_wrapper as _enum_type_wrapper
from google.protobuf import descriptor as _descriptor
from google.protobuf import message as _message
from collections.abc import Iterable as _Iterable, Mapping as _Mapping
from typing import ClassVar as _ClassVar, Optional as _Optional, Union as _Union

DESCRIPTOR: _descriptor.FileDescriptor

class TurnStatus(int, metaclass=_enum_type_wrapper.EnumTypeWrapper):
    __slots__ = ()
    TURN_STATUS_UNSPECIFIED: _ClassVar[TurnStatus]
    TURN_STATUS_QUEUED: _ClassVar[TurnStatus]
    TURN_STATUS_RUNNING: _ClassVar[TurnStatus]
    TURN_STATUS_COMPLETED: _ClassVar[TurnStatus]
    TURN_STATUS_FAILED: _ClassVar[TurnStatus]
    TURN_STATUS_CANCELLED: _ClassVar[TurnStatus]
    TURN_STATUS_INTERRUPTED: _ClassVar[TurnStatus]
    TURN_STATUS_WATCHING: _ClassVar[TurnStatus]

class TurnOrder(int, metaclass=_enum_type_wrapper.EnumTypeWrapper):
    __slots__ = ()
    TURN_ORDER_UNSPECIFIED: _ClassVar[TurnOrder]
    TURN_ORDER_QUEUED: _ClassVar[TurnOrder]
    TURN_ORDER_FINISHED: _ClassVar[TurnOrder]
TURN_STATUS_UNSPECIFIED: TurnStatus
TURN_STATUS_QUEUED: TurnStatus
TURN_STATUS_RUNNING: TurnStatus
TURN_STATUS_COMPLETED: TurnStatus
TURN_STATUS_FAILED: TurnStatus
TURN_STATUS_CANCELLED: TurnStatus
TURN_STATUS_INTERRUPTED: TurnStatus
TURN_STATUS_WATCHING: TurnStatus
TURN_ORDER_UNSPECIFIED: TurnOrder
TURN_ORDER_QUEUED: TurnOrder
TURN_ORDER_FINISHED: TurnOrder

class Turn(_message.Message):
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
    PROMPT_FIELD_NUMBER: _ClassVar[int]
    SATELLITE_INITIATED_FIELD_NUMBER: _ClassVar[int]
    QUEUED_AT_FIELD_NUMBER: _ClassVar[int]
    STARTED_AT_FIELD_NUMBER: _ClassVar[int]
    FINISHED_AT_FIELD_NUMBER: _ClassVar[int]
    TRIGGERED_BY_TURN_ID_FIELD_NUMBER: _ClassVar[int]
    METADATA_FIELD_NUMBER: _ClassVar[int]
    turn_id: str
    thread_id: str
    status: TurnStatus
    prompt: str
    satellite_initiated: bool
    queued_at: _common_pb2.Timestamp
    started_at: _common_pb2.Timestamp
    finished_at: _common_pb2.Timestamp
    triggered_by_turn_id: str
    metadata: _containers.ScalarMap[str, str]
    def __init__(self, turn_id: _Optional[str] = ..., thread_id: _Optional[str] = ..., status: _Optional[_Union[TurnStatus, str]] = ..., prompt: _Optional[str] = ..., satellite_initiated: _Optional[bool] = ..., queued_at: _Optional[_Union[_common_pb2.Timestamp, _Mapping]] = ..., started_at: _Optional[_Union[_common_pb2.Timestamp, _Mapping]] = ..., finished_at: _Optional[_Union[_common_pb2.Timestamp, _Mapping]] = ..., triggered_by_turn_id: _Optional[str] = ..., metadata: _Optional[_Mapping[str, str]] = ...) -> None: ...

class TeamMember(_message.Message):
    __slots__ = ()
    MEMBER_ID_FIELD_NUMBER: _ClassVar[int]
    ROLE_FIELD_NUMBER: _ClassVar[int]
    member_id: str
    role: str
    def __init__(self, member_id: _Optional[str] = ..., role: _Optional[str] = ...) -> None: ...

class ChangedFile(_message.Message):
    __slots__ = ()
    PATH_FIELD_NUMBER: _ClassVar[int]
    INSERTIONS_FIELD_NUMBER: _ClassVar[int]
    DELETIONS_FIELD_NUMBER: _ClassVar[int]
    path: str
    insertions: int
    deletions: int
    def __init__(self, path: _Optional[str] = ..., insertions: _Optional[int] = ..., deletions: _Optional[int] = ...) -> None: ...

class IntegrationRecord(_message.Message):
    __slots__ = ()
    MEMBER_ID_FIELD_NUMBER: _ClassVar[int]
    REPO_FIELD_NUMBER: _ClassVar[int]
    BRANCH_FIELD_NUMBER: _ClassVar[int]
    SUMMARY_FIELD_NUMBER: _ClassVar[int]
    LANDED_AT_FIELD_NUMBER: _ClassVar[int]
    member_id: str
    repo: str
    branch: str
    summary: str
    landed_at: _common_pb2.Timestamp
    def __init__(self, member_id: _Optional[str] = ..., repo: _Optional[str] = ..., branch: _Optional[str] = ..., summary: _Optional[str] = ..., landed_at: _Optional[_Union[_common_pb2.Timestamp, _Mapping]] = ...) -> None: ...

class CheckerResult(_message.Message):
    __slots__ = ()
    COMMAND_FIELD_NUMBER: _ClassVar[int]
    EXIT_CODE_FIELD_NUMBER: _ClassVar[int]
    OUTPUT_FIELD_NUMBER: _ClassVar[int]
    SKIPPED_BY_COMMANDER_FIELD_NUMBER: _ClassVar[int]
    command: str
    exit_code: int
    output: str
    skipped_by_commander: bool
    def __init__(self, command: _Optional[str] = ..., exit_code: _Optional[int] = ..., output: _Optional[str] = ..., skipped_by_commander: _Optional[bool] = ...) -> None: ...

class StartTurnRequest(_message.Message):
    __slots__ = ()
    class MetadataEntry(_message.Message):
        __slots__ = ()
        KEY_FIELD_NUMBER: _ClassVar[int]
        VALUE_FIELD_NUMBER: _ClassVar[int]
        key: str
        value: str
        def __init__(self, key: _Optional[str] = ..., value: _Optional[str] = ...) -> None: ...
    THREAD_ID_FIELD_NUMBER: _ClassVar[int]
    PROMPT_FIELD_NUMBER: _ClassVar[int]
    IDEMPOTENCY_KEY_FIELD_NUMBER: _ClassVar[int]
    METADATA_FIELD_NUMBER: _ClassVar[int]
    thread_id: str
    prompt: str
    idempotency_key: str
    metadata: _containers.ScalarMap[str, str]
    def __init__(self, thread_id: _Optional[str] = ..., prompt: _Optional[str] = ..., idempotency_key: _Optional[str] = ..., metadata: _Optional[_Mapping[str, str]] = ...) -> None: ...

class StartTurnResponse(_message.Message):
    __slots__ = ()
    TURN_FIELD_NUMBER: _ClassVar[int]
    turn: Turn
    def __init__(self, turn: _Optional[_Union[Turn, _Mapping]] = ...) -> None: ...

class CancelTurnRequest(_message.Message):
    __slots__ = ()
    THREAD_ID_FIELD_NUMBER: _ClassVar[int]
    TURN_ID_FIELD_NUMBER: _ClassVar[int]
    thread_id: str
    turn_id: str
    def __init__(self, thread_id: _Optional[str] = ..., turn_id: _Optional[str] = ...) -> None: ...

class CancelTurnResponse(_message.Message):
    __slots__ = ()
    TURN_FIELD_NUMBER: _ClassVar[int]
    turn: Turn
    def __init__(self, turn: _Optional[_Union[Turn, _Mapping]] = ...) -> None: ...

class ListTurnsRequest(_message.Message):
    __slots__ = ()
    THREAD_ID_FIELD_NUMBER: _ClassVar[int]
    STATUSES_FIELD_NUMBER: _ClassVar[int]
    PAGE_FIELD_NUMBER: _ClassVar[int]
    ORDER_BY_FIELD_NUMBER: _ClassVar[int]
    DESCENDING_FIELD_NUMBER: _ClassVar[int]
    thread_id: str
    statuses: _containers.RepeatedScalarFieldContainer[TurnStatus]
    page: _common_pb2.PageRequest
    order_by: TurnOrder
    descending: bool
    def __init__(self, thread_id: _Optional[str] = ..., statuses: _Optional[_Iterable[_Union[TurnStatus, str]]] = ..., page: _Optional[_Union[_common_pb2.PageRequest, _Mapping]] = ..., order_by: _Optional[_Union[TurnOrder, str]] = ..., descending: _Optional[bool] = ...) -> None: ...

class ListTurnsResponse(_message.Message):
    __slots__ = ()
    TURNS_FIELD_NUMBER: _ClassVar[int]
    PAGE_FIELD_NUMBER: _ClassVar[int]
    turns: _containers.RepeatedCompositeFieldContainer[Turn]
    page: _common_pb2.PageResponse
    def __init__(self, turns: _Optional[_Iterable[_Union[Turn, _Mapping]]] = ..., page: _Optional[_Union[_common_pb2.PageResponse, _Mapping]] = ...) -> None: ...
