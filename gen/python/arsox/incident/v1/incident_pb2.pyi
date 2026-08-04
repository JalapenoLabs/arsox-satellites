from arsox.common.v1 import common_pb2 as _common_pb2
from arsox.error.v1 import error_pb2 as _error_pb2
from google.protobuf import struct_pb2 as _struct_pb2
from google.protobuf.internal import containers as _containers
from google.protobuf.internal import enum_type_wrapper as _enum_type_wrapper
from google.protobuf import descriptor as _descriptor
from google.protobuf import message as _message
from collections.abc import Iterable as _Iterable, Mapping as _Mapping
from typing import ClassVar as _ClassVar, Optional as _Optional, Union as _Union

DESCRIPTOR: _descriptor.FileDescriptor

class Disposition(int, metaclass=_enum_type_wrapper.EnumTypeWrapper):
    __slots__ = ()
    DISPOSITION_UNSPECIFIED: _ClassVar[Disposition]
    DISPOSITION_FATAL: _ClassVar[Disposition]
    DISPOSITION_RECOVERED: _ClassVar[Disposition]
    DISPOSITION_DEGRADED: _ClassVar[Disposition]
    DISPOSITION_BLOCKED: _ClassVar[Disposition]
DISPOSITION_UNSPECIFIED: Disposition
DISPOSITION_FATAL: Disposition
DISPOSITION_RECOVERED: Disposition
DISPOSITION_DEGRADED: Disposition
DISPOSITION_BLOCKED: Disposition

class Incident(_message.Message):
    __slots__ = ()
    INCIDENT_ID_FIELD_NUMBER: _ClassVar[int]
    SEQUENCE_FIELD_NUMBER: _ClassVar[int]
    THREAD_ID_FIELD_NUMBER: _ClassVar[int]
    TURN_ID_FIELD_NUMBER: _ClassVar[int]
    MEMBER_ID_FIELD_NUMBER: _ClassVar[int]
    CODE_FIELD_NUMBER: _ClassVar[int]
    DISPOSITION_FIELD_NUMBER: _ClassVar[int]
    RETRYABLE_FIELD_NUMBER: _ClassVar[int]
    MESSAGE_FIELD_NUMBER: _ClassVar[int]
    DETAILS_FIELD_NUMBER: _ClassVar[int]
    OCCURRED_AT_FIELD_NUMBER: _ClassVar[int]
    incident_id: str
    sequence: int
    thread_id: str
    turn_id: str
    member_id: str
    code: _error_pb2.ErrorCode
    disposition: Disposition
    retryable: bool
    message: str
    details: _struct_pb2.Struct
    occurred_at: _common_pb2.Timestamp
    def __init__(self, incident_id: _Optional[str] = ..., sequence: _Optional[int] = ..., thread_id: _Optional[str] = ..., turn_id: _Optional[str] = ..., member_id: _Optional[str] = ..., code: _Optional[_Union[_error_pb2.ErrorCode, str]] = ..., disposition: _Optional[_Union[Disposition, str]] = ..., retryable: _Optional[bool] = ..., message: _Optional[str] = ..., details: _Optional[_Union[_struct_pb2.Struct, _Mapping]] = ..., occurred_at: _Optional[_Union[_common_pb2.Timestamp, _Mapping]] = ...) -> None: ...

class IncidentCounts(_message.Message):
    __slots__ = ()
    FATAL_FIELD_NUMBER: _ClassVar[int]
    RECOVERED_FIELD_NUMBER: _ClassVar[int]
    DEGRADED_FIELD_NUMBER: _ClassVar[int]
    BLOCKED_FIELD_NUMBER: _ClassVar[int]
    fatal: int
    recovered: int
    degraded: int
    blocked: int
    def __init__(self, fatal: _Optional[int] = ..., recovered: _Optional[int] = ..., degraded: _Optional[int] = ..., blocked: _Optional[int] = ...) -> None: ...

class ListIncidentsRequest(_message.Message):
    __slots__ = ()
    THREAD_IDS_FIELD_NUMBER: _ClassVar[int]
    TURN_IDS_FIELD_NUMBER: _ClassVar[int]
    MEMBER_IDS_FIELD_NUMBER: _ClassVar[int]
    CODES_FIELD_NUMBER: _ClassVar[int]
    DISPOSITIONS_FIELD_NUMBER: _ClassVar[int]
    OCCURRED_AFTER_FIELD_NUMBER: _ClassVar[int]
    OCCURRED_BEFORE_FIELD_NUMBER: _ClassVar[int]
    PAGE_FIELD_NUMBER: _ClassVar[int]
    thread_ids: _containers.RepeatedScalarFieldContainer[str]
    turn_ids: _containers.RepeatedScalarFieldContainer[str]
    member_ids: _containers.RepeatedScalarFieldContainer[str]
    codes: _containers.RepeatedScalarFieldContainer[_error_pb2.ErrorCode]
    dispositions: _containers.RepeatedScalarFieldContainer[Disposition]
    occurred_after: _common_pb2.Timestamp
    occurred_before: _common_pb2.Timestamp
    page: _common_pb2.PageRequest
    def __init__(self, thread_ids: _Optional[_Iterable[str]] = ..., turn_ids: _Optional[_Iterable[str]] = ..., member_ids: _Optional[_Iterable[str]] = ..., codes: _Optional[_Iterable[_Union[_error_pb2.ErrorCode, str]]] = ..., dispositions: _Optional[_Iterable[_Union[Disposition, str]]] = ..., occurred_after: _Optional[_Union[_common_pb2.Timestamp, _Mapping]] = ..., occurred_before: _Optional[_Union[_common_pb2.Timestamp, _Mapping]] = ..., page: _Optional[_Union[_common_pb2.PageRequest, _Mapping]] = ...) -> None: ...

class ListIncidentsResponse(_message.Message):
    __slots__ = ()
    INCIDENTS_FIELD_NUMBER: _ClassVar[int]
    PAGE_FIELD_NUMBER: _ClassVar[int]
    incidents: _containers.RepeatedCompositeFieldContainer[Incident]
    page: _common_pb2.PageResponse
    def __init__(self, incidents: _Optional[_Iterable[_Union[Incident, _Mapping]]] = ..., page: _Optional[_Union[_common_pb2.PageResponse, _Mapping]] = ...) -> None: ...
