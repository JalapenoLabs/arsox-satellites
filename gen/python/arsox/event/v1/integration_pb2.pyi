from arsox.turn.v1 import turn_pb2 as _turn_pb2
from google.protobuf.internal import containers as _containers
from google.protobuf import descriptor as _descriptor
from google.protobuf import message as _message
from collections.abc import Iterable as _Iterable, Mapping as _Mapping
from typing import ClassVar as _ClassVar, Optional as _Optional, Union as _Union

DESCRIPTOR: _descriptor.FileDescriptor

class IntegrationRequested(_message.Message):
    __slots__ = ()
    MEMBER_ID_FIELD_NUMBER: _ClassVar[int]
    REPO_FIELD_NUMBER: _ClassVar[int]
    BRANCH_FIELD_NUMBER: _ClassVar[int]
    SUMMARY_FIELD_NUMBER: _ClassVar[int]
    member_id: str
    repo: str
    branch: str
    summary: str
    def __init__(self, member_id: _Optional[str] = ..., repo: _Optional[str] = ..., branch: _Optional[str] = ..., summary: _Optional[str] = ...) -> None: ...

class IntegrationLanded(_message.Message):
    __slots__ = ()
    MEMBER_ID_FIELD_NUMBER: _ClassVar[int]
    REPO_FIELD_NUMBER: _ClassVar[int]
    BRANCH_FIELD_NUMBER: _ClassVar[int]
    SUMMARY_FIELD_NUMBER: _ClassVar[int]
    member_id: str
    repo: str
    branch: str
    summary: str
    def __init__(self, member_id: _Optional[str] = ..., repo: _Optional[str] = ..., branch: _Optional[str] = ..., summary: _Optional[str] = ...) -> None: ...

class IntegrationConflict(_message.Message):
    __slots__ = ()
    MEMBER_ID_FIELD_NUMBER: _ClassVar[int]
    REPO_FIELD_NUMBER: _ClassVar[int]
    BRANCH_FIELD_NUMBER: _ClassVar[int]
    CONFLICTING_PATHS_FIELD_NUMBER: _ClassVar[int]
    member_id: str
    repo: str
    branch: str
    conflicting_paths: _containers.RepeatedScalarFieldContainer[str]
    def __init__(self, member_id: _Optional[str] = ..., repo: _Optional[str] = ..., branch: _Optional[str] = ..., conflicting_paths: _Optional[_Iterable[str]] = ...) -> None: ...

class CheckerResultEvent(_message.Message):
    __slots__ = ()
    REPO_FIELD_NUMBER: _ClassVar[int]
    RESULT_FIELD_NUMBER: _ClassVar[int]
    repo: str
    result: _turn_pb2.CheckerResult
    def __init__(self, repo: _Optional[str] = ..., result: _Optional[_Union[_turn_pb2.CheckerResult, _Mapping]] = ...) -> None: ...
