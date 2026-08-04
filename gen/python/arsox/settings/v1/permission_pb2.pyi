from google.protobuf.internal import containers as _containers
from google.protobuf.internal import enum_type_wrapper as _enum_type_wrapper
from google.protobuf import descriptor as _descriptor
from google.protobuf import message as _message
from collections.abc import Iterable as _Iterable
from typing import ClassVar as _ClassVar, Optional as _Optional, Union as _Union

DESCRIPTOR: _descriptor.FileDescriptor

class WebAccess(int, metaclass=_enum_type_wrapper.EnumTypeWrapper):
    __slots__ = ()
    WEB_ACCESS_UNSPECIFIED: _ClassVar[WebAccess]
    WEB_ACCESS_PRESET: _ClassVar[WebAccess]
    WEB_ACCESS_ALL: _ClassVar[WebAccess]
    WEB_ACCESS_NONE: _ClassVar[WebAccess]
    WEB_ACCESS_CUSTOM: _ClassVar[WebAccess]

class ExecAccess(int, metaclass=_enum_type_wrapper.EnumTypeWrapper):
    __slots__ = ()
    EXEC_ACCESS_UNSPECIFIED: _ClassVar[ExecAccess]
    EXEC_ACCESS_PRESET: _ClassVar[ExecAccess]
    EXEC_ACCESS_NONE: _ClassVar[ExecAccess]
    EXEC_ACCESS_CUSTOM: _ClassVar[ExecAccess]
WEB_ACCESS_UNSPECIFIED: WebAccess
WEB_ACCESS_PRESET: WebAccess
WEB_ACCESS_ALL: WebAccess
WEB_ACCESS_NONE: WebAccess
WEB_ACCESS_CUSTOM: WebAccess
EXEC_ACCESS_UNSPECIFIED: ExecAccess
EXEC_ACCESS_PRESET: ExecAccess
EXEC_ACCESS_NONE: ExecAccess
EXEC_ACCESS_CUSTOM: ExecAccess

class Permissions(_message.Message):
    __slots__ = ()
    WEB_FIELD_NUMBER: _ClassVar[int]
    ADDITIONAL_DOMAINS_FIELD_NUMBER: _ClassVar[int]
    EXEC_FIELD_NUMBER: _ClassVar[int]
    ALLOWED_COMMANDS_FIELD_NUMBER: _ClassVar[int]
    ALLOW_GIT_PUSH_FIELD_NUMBER: _ClassVar[int]
    PROTECTED_BRANCHES_FIELD_NUMBER: _ClassVar[int]
    web: WebAccess
    additional_domains: _containers.RepeatedScalarFieldContainer[str]
    exec: ExecAccess
    allowed_commands: _containers.RepeatedScalarFieldContainer[str]
    allow_git_push: bool
    protected_branches: _containers.RepeatedScalarFieldContainer[str]
    def __init__(self, web: _Optional[_Union[WebAccess, str]] = ..., additional_domains: _Optional[_Iterable[str]] = ..., exec: _Optional[_Union[ExecAccess, str]] = ..., allowed_commands: _Optional[_Iterable[str]] = ..., allow_git_push: _Optional[bool] = ..., protected_branches: _Optional[_Iterable[str]] = ...) -> None: ...
