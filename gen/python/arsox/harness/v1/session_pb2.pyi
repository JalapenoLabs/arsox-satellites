from arsox.common.v1 import common_pb2 as _common_pb2
from arsox.harness.v1 import harness_pb2 as _harness_pb2
from google.protobuf.internal import containers as _containers
from google.protobuf import descriptor as _descriptor
from google.protobuf import message as _message
from collections.abc import Iterable as _Iterable, Mapping as _Mapping
from typing import ClassVar as _ClassVar, Optional as _Optional, Union as _Union

DESCRIPTOR: _descriptor.FileDescriptor

class HarnessSession(_message.Message):
    __slots__ = ()
    HARNESS_FIELD_NUMBER: _ClassVar[int]
    SESSION_ID_FIELD_NUMBER: _ClassVar[int]
    WORKSPACE_FIELD_NUMBER: _ClassVar[int]
    EXPORTED_AT_FIELD_NUMBER: _ClassVar[int]
    FORMAT_VERSION_FIELD_NUMBER: _ClassVar[int]
    harness: _harness_pb2.Harness
    session_id: str
    workspace: str
    exported_at: _common_pb2.Timestamp
    format_version: int
    def __init__(self, harness: _Optional[_Union[_harness_pb2.Harness, str]] = ..., session_id: _Optional[str] = ..., workspace: _Optional[str] = ..., exported_at: _Optional[_Union[_common_pb2.Timestamp, _Mapping]] = ..., format_version: _Optional[int] = ...) -> None: ...

class ImportHarnessSessionResponse(_message.Message):
    __slots__ = ()
    SESSION_FIELD_NUMBER: _ClassVar[int]
    FILES_FIELD_NUMBER: _ClassVar[int]
    session: HarnessSession
    files: _containers.RepeatedScalarFieldContainer[str]
    def __init__(self, session: _Optional[_Union[HarnessSession, _Mapping]] = ..., files: _Optional[_Iterable[str]] = ...) -> None: ...
