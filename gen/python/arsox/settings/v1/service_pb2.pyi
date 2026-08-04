from arsox.common.v1 import common_pb2 as _common_pb2
from google.protobuf.internal import enum_type_wrapper as _enum_type_wrapper
from google.protobuf import descriptor as _descriptor
from google.protobuf import message as _message
from collections.abc import Mapping as _Mapping
from typing import ClassVar as _ClassVar, Optional as _Optional, Union as _Union

DESCRIPTOR: _descriptor.FileDescriptor

class ServiceIsolation(int, metaclass=_enum_type_wrapper.EnumTypeWrapper):
    __slots__ = ()
    SERVICE_ISOLATION_UNSPECIFIED: _ClassVar[ServiceIsolation]
    SERVICE_ISOLATION_SHARED: _ClassVar[ServiceIsolation]
    SERVICE_ISOLATION_PER_MEMBER: _ClassVar[ServiceIsolation]
SERVICE_ISOLATION_UNSPECIFIED: ServiceIsolation
SERVICE_ISOLATION_SHARED: ServiceIsolation
SERVICE_ISOLATION_PER_MEMBER: ServiceIsolation

class Service(_message.Message):
    __slots__ = ()
    NAME_FIELD_NUMBER: _ClassVar[int]
    COMMAND_FIELD_NUMBER: _ClassVar[int]
    PORT_FIELD_NUMBER: _ClassVar[int]
    READY_WHEN_FIELD_NUMBER: _ClassVar[int]
    ISOLATION_FIELD_NUMBER: _ClassVar[int]
    name: str
    command: str
    port: int
    ready_when: ReadinessProbe
    isolation: ServiceIsolation
    def __init__(self, name: _Optional[str] = ..., command: _Optional[str] = ..., port: _Optional[int] = ..., ready_when: _Optional[_Union[ReadinessProbe, _Mapping]] = ..., isolation: _Optional[_Union[ServiceIsolation, str]] = ...) -> None: ...

class ReadinessProbe(_message.Message):
    __slots__ = ()
    HTTP_GET_FIELD_NUMBER: _ClassVar[int]
    TIMEOUT_FIELD_NUMBER: _ClassVar[int]
    http_get: str
    timeout: _common_pb2.Duration
    def __init__(self, http_get: _Optional[str] = ..., timeout: _Optional[_Union[_common_pb2.Duration, _Mapping]] = ...) -> None: ...
