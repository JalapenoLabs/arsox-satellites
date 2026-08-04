from google.protobuf.internal import containers as _containers
from google.protobuf import descriptor as _descriptor
from google.protobuf import message as _message
from collections.abc import Iterable as _Iterable
from typing import ClassVar as _ClassVar, Optional as _Optional

DESCRIPTOR: _descriptor.FileDescriptor

class TeamMode(_message.Message):
    __slots__ = ()
    ENABLED_FIELD_NUMBER: _ClassVar[int]
    MAX_MEMBERS_FIELD_NUMBER: _ClassVar[int]
    SUGGESTED_ROLES_FIELD_NUMBER: _ClassVar[int]
    enabled: bool
    max_members: int
    suggested_roles: _containers.RepeatedScalarFieldContainer[str]
    def __init__(self, enabled: _Optional[bool] = ..., max_members: _Optional[int] = ..., suggested_roles: _Optional[_Iterable[str]] = ...) -> None: ...
