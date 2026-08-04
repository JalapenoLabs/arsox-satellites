from arsox.common.v1 import common_pb2 as _common_pb2
from google.protobuf import descriptor as _descriptor
from google.protobuf import message as _message
from collections.abc import Mapping as _Mapping
from typing import ClassVar as _ClassVar, Optional as _Optional, Union as _Union

DESCRIPTOR: _descriptor.FileDescriptor

class PlanMode(_message.Message):
    __slots__ = ()
    ENABLED_FIELD_NUMBER: _ClassVar[int]
    AUTO_APPROVE_FIELD_NUMBER: _ClassVar[int]
    enabled: bool
    auto_approve: bool
    def __init__(self, enabled: _Optional[bool] = ..., auto_approve: _Optional[bool] = ...) -> None: ...

class HumanInTheLoop(_message.Message):
    __slots__ = ()
    ENABLED_FIELD_NUMBER: _ClassVar[int]
    QUESTION_TIMEOUT_FIELD_NUMBER: _ClassVar[int]
    enabled: bool
    question_timeout: _common_pb2.Duration
    def __init__(self, enabled: _Optional[bool] = ..., question_timeout: _Optional[_Union[_common_pb2.Duration, _Mapping]] = ...) -> None: ...

class SelfReview(_message.Message):
    __slots__ = ()
    ENABLED_FIELD_NUMBER: _ClassVar[int]
    enabled: bool
    def __init__(self, enabled: _Optional[bool] = ...) -> None: ...

class SuggestionSettings(_message.Message):
    __slots__ = ()
    ENABLED_FIELD_NUMBER: _ClassVar[int]
    MAX_SUGGESTIONS_PER_CATEGORY_FIELD_NUMBER: _ClassVar[int]
    enabled: bool
    max_suggestions_per_category: int
    def __init__(self, enabled: _Optional[bool] = ..., max_suggestions_per_category: _Optional[int] = ...) -> None: ...
