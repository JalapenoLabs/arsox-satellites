from arsox.common.v1 import common_pb2 as _common_pb2
from google.protobuf.internal import containers as _containers
from google.protobuf.internal import enum_type_wrapper as _enum_type_wrapper
from google.protobuf import descriptor as _descriptor
from google.protobuf import message as _message
from collections.abc import Iterable as _Iterable, Mapping as _Mapping
from typing import ClassVar as _ClassVar, Optional as _Optional, Union as _Union

DESCRIPTOR: _descriptor.FileDescriptor

class MergeMethod(int, metaclass=_enum_type_wrapper.EnumTypeWrapper):
    __slots__ = ()
    MERGE_METHOD_UNSPECIFIED: _ClassVar[MergeMethod]
    MERGE_METHOD_MERGE: _ClassVar[MergeMethod]
    MERGE_METHOD_SQUASH: _ClassVar[MergeMethod]
    MERGE_METHOD_REBASE: _ClassVar[MergeMethod]

class WatchTrigger(int, metaclass=_enum_type_wrapper.EnumTypeWrapper):
    __slots__ = ()
    WATCH_TRIGGER_UNSPECIFIED: _ClassVar[WatchTrigger]
    WATCH_TRIGGER_SATELLITE_COMMITS: _ClassVar[WatchTrigger]
    WATCH_TRIGGER_ANY: _ClassVar[WatchTrigger]
MERGE_METHOD_UNSPECIFIED: MergeMethod
MERGE_METHOD_MERGE: MergeMethod
MERGE_METHOD_SQUASH: MergeMethod
MERGE_METHOD_REBASE: MergeMethod
WATCH_TRIGGER_UNSPECIFIED: WatchTrigger
WATCH_TRIGGER_SATELLITE_COMMITS: WatchTrigger
WATCH_TRIGGER_ANY: WatchTrigger

class PullRequestPolicy(_message.Message):
    __slots__ = ()
    ALLOW_AGENT_MERGE_FIELD_NUMBER: _ClassVar[int]
    ALLOWED_MERGE_METHODS_FIELD_NUMBER: _ClassVar[int]
    allow_agent_merge: bool
    allowed_merge_methods: _containers.RepeatedScalarFieldContainer[MergeMethod]
    def __init__(self, allow_agent_merge: _Optional[bool] = ..., allowed_merge_methods: _Optional[_Iterable[_Union[MergeMethod, str]]] = ...) -> None: ...

class WatchPullRequests(_message.Message):
    __slots__ = ()
    ENABLED_FIELD_NUMBER: _ClassVar[int]
    MAX_ATTEMPTS_FIELD_NUMBER: _ClassVar[int]
    WATCH_WINDOW_FIELD_NUMBER: _ClassVar[int]
    POLL_INTERVAL_FIELD_NUMBER: _ClassVar[int]
    REACT_TO_FIELD_NUMBER: _ClassVar[int]
    enabled: bool
    max_attempts: int
    watch_window: _common_pb2.Duration
    poll_interval: _common_pb2.Duration
    react_to: WatchTrigger
    def __init__(self, enabled: _Optional[bool] = ..., max_attempts: _Optional[int] = ..., watch_window: _Optional[_Union[_common_pb2.Duration, _Mapping]] = ..., poll_interval: _Optional[_Union[_common_pb2.Duration, _Mapping]] = ..., react_to: _Optional[_Union[WatchTrigger, str]] = ...) -> None: ...
