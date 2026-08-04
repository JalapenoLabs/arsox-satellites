from google.protobuf.internal import enum_type_wrapper as _enum_type_wrapper
from google.protobuf import descriptor as _descriptor
from google.protobuf import message as _message
from typing import ClassVar as _ClassVar, Optional as _Optional, Union as _Union

DESCRIPTOR: _descriptor.FileDescriptor

class AuthorKind(int, metaclass=_enum_type_wrapper.EnumTypeWrapper):
    __slots__ = ()
    AUTHOR_KIND_UNSPECIFIED: _ClassVar[AuthorKind]
    AUTHOR_KIND_AGENT: _ClassVar[AuthorKind]
    AUTHOR_KIND_COMMANDER: _ClassVar[AuthorKind]
    AUTHOR_KIND_MEMBER: _ClassVar[AuthorKind]
    AUTHOR_KIND_SUBAGENT: _ClassVar[AuthorKind]
    AUTHOR_KIND_PLANNER: _ClassVar[AuthorKind]
    AUTHOR_KIND_REVIEWER: _ClassVar[AuthorKind]
    AUTHOR_KIND_SUGGESTIONS: _ClassVar[AuthorKind]
AUTHOR_KIND_UNSPECIFIED: AuthorKind
AUTHOR_KIND_AGENT: AuthorKind
AUTHOR_KIND_COMMANDER: AuthorKind
AUTHOR_KIND_MEMBER: AuthorKind
AUTHOR_KIND_SUBAGENT: AuthorKind
AUTHOR_KIND_PLANNER: AuthorKind
AUTHOR_KIND_REVIEWER: AuthorKind
AUTHOR_KIND_SUGGESTIONS: AuthorKind

class Author(_message.Message):
    __slots__ = ()
    KIND_FIELD_NUMBER: _ClassVar[int]
    MEMBER_ID_FIELD_NUMBER: _ClassVar[int]
    ROLE_FIELD_NUMBER: _ClassVar[int]
    PARENT_MEMBER_ID_FIELD_NUMBER: _ClassVar[int]
    kind: AuthorKind
    member_id: str
    role: str
    parent_member_id: str
    def __init__(self, kind: _Optional[_Union[AuthorKind, str]] = ..., member_id: _Optional[str] = ..., role: _Optional[str] = ..., parent_member_id: _Optional[str] = ...) -> None: ...
