from arsox.turn.v1 import turn_pb2 as _turn_pb2
from google.protobuf.internal import containers as _containers
from google.protobuf import descriptor as _descriptor
from google.protobuf import message as _message
from collections.abc import Iterable as _Iterable, Mapping as _Mapping
from typing import ClassVar as _ClassVar, Optional as _Optional, Union as _Union

DESCRIPTOR: _descriptor.FileDescriptor

class TurnBrief(_message.Message):
    __slots__ = ()
    TASK_FIELD_NUMBER: _ClassVar[int]
    PLAN_FIELD_NUMBER: _ClassVar[int]
    SUMMARY_FIELD_NUMBER: _ClassVar[int]
    MEMBERS_FIELD_NUMBER: _ClassVar[int]
    CHANGED_FILES_FIELD_NUMBER: _ClassVar[int]
    INTEGRATIONS_FIELD_NUMBER: _ClassVar[int]
    CHECKER_RESULTS_FIELD_NUMBER: _ClassVar[int]
    FRICTION_FIELD_NUMBER: _ClassVar[int]
    task: str
    plan: str
    summary: str
    members: _containers.RepeatedCompositeFieldContainer[_turn_pb2.TeamMember]
    changed_files: _containers.RepeatedCompositeFieldContainer[_turn_pb2.ChangedFile]
    integrations: _containers.RepeatedCompositeFieldContainer[_turn_pb2.IntegrationRecord]
    checker_results: _containers.RepeatedCompositeFieldContainer[_turn_pb2.CheckerResult]
    friction: _containers.RepeatedScalarFieldContainer[str]
    def __init__(self, task: _Optional[str] = ..., plan: _Optional[str] = ..., summary: _Optional[str] = ..., members: _Optional[_Iterable[_Union[_turn_pb2.TeamMember, _Mapping]]] = ..., changed_files: _Optional[_Iterable[_Union[_turn_pb2.ChangedFile, _Mapping]]] = ..., integrations: _Optional[_Iterable[_Union[_turn_pb2.IntegrationRecord, _Mapping]]] = ..., checker_results: _Optional[_Iterable[_Union[_turn_pb2.CheckerResult, _Mapping]]] = ..., friction: _Optional[_Iterable[str]] = ...) -> None: ...
