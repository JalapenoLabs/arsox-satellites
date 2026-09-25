from arsox.common.v1 import common_pb2 as _common_pb2
from google.protobuf.internal import containers as _containers
from google.protobuf import descriptor as _descriptor
from google.protobuf import message as _message
from collections.abc import Iterable as _Iterable, Mapping as _Mapping
from typing import ClassVar as _ClassVar, Optional as _Optional, Union as _Union

DESCRIPTOR: _descriptor.FileDescriptor

class TurnEndHook(_message.Message):
    __slots__ = ()
    NAME_FIELD_NUMBER: _ClassVar[int]
    ARGV_FIELD_NUMBER: _ClassVar[int]
    TIMEOUT_FIELD_NUMBER: _ClassVar[int]
    name: str
    argv: _containers.RepeatedScalarFieldContainer[str]
    timeout: _common_pb2.Duration
    def __init__(self, name: _Optional[str] = ..., argv: _Optional[_Iterable[str]] = ..., timeout: _Optional[_Union[_common_pb2.Duration, _Mapping]] = ...) -> None: ...
