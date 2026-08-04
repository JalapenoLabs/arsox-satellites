from arsox.common.v1 import common_pb2 as _common_pb2
from arsox.thread.v1 import thread_pb2 as _thread_pb2
from google.protobuf.internal import containers as _containers
from google.protobuf import descriptor as _descriptor
from google.protobuf import message as _message
from collections.abc import Iterable as _Iterable, Mapping as _Mapping
from typing import ClassVar as _ClassVar, Optional as _Optional, Union as _Union

DESCRIPTOR: _descriptor.FileDescriptor

class GetVersionResponse(_message.Message):
    __slots__ = ()
    SATELLITE_VERSION_FIELD_NUMBER: _ClassVar[int]
    PROTO_MAJOR_FIELD_NUMBER: _ClassVar[int]
    PROTO_MINOR_FIELD_NUMBER: _ClassVar[int]
    satellite_version: str
    proto_major: int
    proto_minor: int
    def __init__(self, satellite_version: _Optional[str] = ..., proto_major: _Optional[int] = ..., proto_minor: _Optional[int] = ...) -> None: ...

class ReadinessCheck(_message.Message):
    __slots__ = ()
    NAME_FIELD_NUMBER: _ClassVar[int]
    PASSING_FIELD_NUMBER: _ClassVar[int]
    DETAIL_FIELD_NUMBER: _ClassVar[int]
    name: str
    passing: bool
    detail: str
    def __init__(self, name: _Optional[str] = ..., passing: _Optional[bool] = ..., detail: _Optional[str] = ...) -> None: ...

class GetReadinessResponse(_message.Message):
    __slots__ = ()
    READY_FIELD_NUMBER: _ClassVar[int]
    CHECKS_FIELD_NUMBER: _ClassVar[int]
    ready: bool
    checks: _containers.RepeatedCompositeFieldContainer[ReadinessCheck]
    def __init__(self, ready: _Optional[bool] = ..., checks: _Optional[_Iterable[_Union[ReadinessCheck, _Mapping]]] = ...) -> None: ...

class DiskUsage(_message.Message):
    __slots__ = ()
    WORKSPACE_BYTES_FIELD_NUMBER: _ClassVar[int]
    AVAILABLE_BYTES_FIELD_NUMBER: _ClassVar[int]
    AGGREGATE_QUOTA_BYTES_FIELD_NUMBER: _ClassVar[int]
    DATABASE_BYTES_FIELD_NUMBER: _ClassVar[int]
    workspace_bytes: int
    available_bytes: int
    aggregate_quota_bytes: int
    database_bytes: int
    def __init__(self, workspace_bytes: _Optional[int] = ..., available_bytes: _Optional[int] = ..., aggregate_quota_bytes: _Optional[int] = ..., database_bytes: _Optional[int] = ...) -> None: ...

class GetStatusResponse(_message.Message):
    __slots__ = ()
    SATELLITE_VERSION_FIELD_NUMBER: _ClassVar[int]
    MAX_CONCURRENT_THREADS_FIELD_NUMBER: _ClassVar[int]
    RUNNING_THREADS_FIELD_NUMBER: _ClassVar[int]
    THREADS_FIELD_NUMBER: _ClassVar[int]
    STARTED_AT_FIELD_NUMBER: _ClassVar[int]
    INSECURE_MODE_FIELD_NUMBER: _ClassVar[int]
    DISK_FIELD_NUMBER: _ClassVar[int]
    satellite_version: str
    max_concurrent_threads: int
    running_threads: int
    threads: _containers.RepeatedCompositeFieldContainer[_thread_pb2.ThreadSummary]
    started_at: _common_pb2.Timestamp
    insecure_mode: bool
    disk: DiskUsage
    def __init__(self, satellite_version: _Optional[str] = ..., max_concurrent_threads: _Optional[int] = ..., running_threads: _Optional[int] = ..., threads: _Optional[_Iterable[_Union[_thread_pb2.ThreadSummary, _Mapping]]] = ..., started_at: _Optional[_Union[_common_pb2.Timestamp, _Mapping]] = ..., insecure_mode: _Optional[bool] = ..., disk: _Optional[_Union[DiskUsage, _Mapping]] = ...) -> None: ...
