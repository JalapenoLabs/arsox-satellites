from arsox.event.v1 import author_pb2 as _author_pb2
from google.protobuf import descriptor as _descriptor
from google.protobuf import message as _message
from collections.abc import Mapping as _Mapping
from typing import ClassVar as _ClassVar, Optional as _Optional, Union as _Union

DESCRIPTOR: _descriptor.FileDescriptor

class TeamMemberSpawned(_message.Message):
    __slots__ = ()
    MEMBER_ID_FIELD_NUMBER: _ClassVar[int]
    ROLE_FIELD_NUMBER: _ClassVar[int]
    RESUMED_CONTEXT_FIELD_NUMBER: _ClassVar[int]
    member_id: str
    role: str
    resumed_context: bool
    def __init__(self, member_id: _Optional[str] = ..., role: _Optional[str] = ..., resumed_context: _Optional[bool] = ...) -> None: ...

class TeamMemberDespawned(_message.Message):
    __slots__ = ()
    MEMBER_ID_FIELD_NUMBER: _ClassVar[int]
    ROLE_FIELD_NUMBER: _ClassVar[int]
    member_id: str
    role: str
    def __init__(self, member_id: _Optional[str] = ..., role: _Optional[str] = ...) -> None: ...

class TeamChat(_message.Message):
    __slots__ = ()
    AUTHOR_FIELD_NUMBER: _ClassVar[int]
    TEXT_FIELD_NUMBER: _ClassVar[int]
    author: _author_pb2.Author
    text: str
    def __init__(self, author: _Optional[_Union[_author_pb2.Author, _Mapping]] = ..., text: _Optional[str] = ...) -> None: ...

class TeamDirectMessage(_message.Message):
    __slots__ = ()
    AUTHOR_FIELD_NUMBER: _ClassVar[int]
    TO_MEMBER_ID_FIELD_NUMBER: _ClassVar[int]
    TEXT_FIELD_NUMBER: _ClassVar[int]
    author: _author_pb2.Author
    to_member_id: str
    text: str
    def __init__(self, author: _Optional[_Union[_author_pb2.Author, _Mapping]] = ..., to_member_id: _Optional[str] = ..., text: _Optional[str] = ...) -> None: ...
