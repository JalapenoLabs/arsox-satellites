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
    name: str
    url: str
    headers: _containers.MessageMap[str, _common_pb2.Secret]
    def __init__(self, name: _Optional[str] = ..., url: _Optional[str] = ..., headers: _Optional[_Mapping[str, _common_pb2.Secret]] = ...) -> None: ...

class VirtualBrowser(_message.Message):
    __slots__ = ()
    ENABLED_FIELD_NUMBER: _ClassVar[int]
    ALLOWED_ROLES_FIELD_NUMBER: _ClassVar[int]
    VIEWPORTS_FIELD_NUMBER: _ClassVar[int]
    enabled: bool
    allowed_roles: _containers.RepeatedScalarFieldContainer[str]
    viewports: _containers.RepeatedScalarFieldContainer[Viewport]
    def __init__(self, enabled: _Optional[bool] = ..., allowed_roles: _Optional[_Iterable[str]] = ..., viewports: _Optional[_Iterable[_Union[Viewport, str]]] = ...) -> None: ...
