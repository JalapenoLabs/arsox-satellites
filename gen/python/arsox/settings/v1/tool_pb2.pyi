from arsox.common.v1 import common_pb2 as _common_pb2
from google.protobuf.internal import containers as _containers
from google.protobuf.internal import enum_type_wrapper as _enum_type_wrapper
from google.protobuf import descriptor as _descriptor
from google.protobuf import message as _message
from collections.abc import Iterable as _Iterable, Mapping as _Mapping
from typing import ClassVar as _ClassVar, Optional as _Optional, Union as _Union

DESCRIPTOR: _descriptor.FileDescriptor

class Viewport(int, metaclass=_enum_type_wrapper.EnumTypeWrapper):
    __slots__ = ()
    VIEWPORT_UNSPECIFIED: _ClassVar[Viewport]
    VIEWPORT_MOBILE: _ClassVar[Viewport]
    VIEWPORT_TABLET: _ClassVar[Viewport]
    VIEWPORT_DESKTOP: _ClassVar[Viewport]
VIEWPORT_UNSPECIFIED: Viewport
VIEWPORT_MOBILE: Viewport
VIEWPORT_TABLET: Viewport
VIEWPORT_DESKTOP: Viewport

class McpServer(_message.Message):
    __slots__ = ()
    class HeadersEntry(_message.Message):
        __slots__ = ()
        KEY_FIELD_NUMBER: _ClassVar[int]
        VALUE_FIELD_NUMBER: _ClassVar[int]
        key: str
        value: _common_pb2.Secret
        def __init__(self, key: _Optional[str] = ..., value: _Optional[_Union[_common_pb2.Secret, _Mapping]] = ...) -> None: ...
    NAME_FIELD_NUMBER: _ClassVar[int]
    URL_FIELD_NUMBER: _ClassVar[int]
    HEADERS_FIELD_NUMBER: _ClassVar[int]
    SERVICE_FIELD_NUMBER: _ClassVar[int]
    name: str
    url: str
    headers: _containers.MessageMap[str, _common_pb2.Secret]
    service: ServiceEndpoint
    def __init__(self, name: _Optional[str] = ..., url: _Optional[str] = ..., headers: _Optional[_Mapping[str, _common_pb2.Secret]] = ..., service: _Optional[_Union[ServiceEndpoint, _Mapping]] = ...) -> None: ...

class ServiceEndpoint(_message.Message):
    __slots__ = ()
    SERVICE_FIELD_NUMBER: _ClassVar[int]
    PATH_FIELD_NUMBER: _ClassVar[int]
    service: str
    path: str
    def __init__(self, service: _Optional[str] = ..., path: _Optional[str] = ...) -> None: ...

class RelayedMcpServer(_message.Message):
    __slots__ = ()
    NAME_FIELD_NUMBER: _ClassVar[int]
    INSTRUCTIONS_FIELD_NUMBER: _ClassVar[int]
    TOOLS_FIELD_NUMBER: _ClassVar[int]
    name: str
    instructions: str
    tools: _containers.RepeatedCompositeFieldContainer[RelayedTool]
    def __init__(self, name: _Optional[str] = ..., instructions: _Optional[str] = ..., tools: _Optional[_Iterable[_Union[RelayedTool, _Mapping]]] = ...) -> None: ...

class RelayedTool(_message.Message):
    __slots__ = ()
    NAME_FIELD_NUMBER: _ClassVar[int]
    DESCRIPTION_FIELD_NUMBER: _ClassVar[int]
    INPUT_SCHEMA_JSON_FIELD_NUMBER: _ClassVar[int]
    name: str
    description: str
    input_schema_json: str
    def __init__(self, name: _Optional[str] = ..., description: _Optional[str] = ..., input_schema_json: _Optional[str] = ...) -> None: ...

class VirtualBrowser(_message.Message):
    __slots__ = ()
    ENABLED_FIELD_NUMBER: _ClassVar[int]
    ALLOWED_ROLES_FIELD_NUMBER: _ClassVar[int]
    VIEWPORTS_FIELD_NUMBER: _ClassVar[int]
    enabled: bool
    allowed_roles: _containers.RepeatedScalarFieldContainer[str]
    viewports: _containers.RepeatedScalarFieldContainer[Viewport]
    def __init__(self, enabled: _Optional[bool] = ..., allowed_roles: _Optional[_Iterable[str]] = ..., viewports: _Optional[_Iterable[_Union[Viewport, str]]] = ...) -> None: ...
