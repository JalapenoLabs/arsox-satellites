from google.protobuf.internal import enum_type_wrapper as _enum_type_wrapper
from google.protobuf import descriptor as _descriptor
from google.protobuf import message as _message
from typing import ClassVar as _ClassVar, Optional as _Optional, Union as _Union

DESCRIPTOR: _descriptor.FileDescriptor

class Effort(int, metaclass=_enum_type_wrapper.EnumTypeWrapper):
    __slots__ = ()
    EFFORT_UNSPECIFIED: _ClassVar[Effort]
    EFFORT_LOW: _ClassVar[Effort]
    EFFORT_MEDIUM: _ClassVar[Effort]
    EFFORT_HIGH: _ClassVar[Effort]
    EFFORT_XHIGH: _ClassVar[Effort]
    EFFORT_MAX: _ClassVar[Effort]
EFFORT_UNSPECIFIED: Effort
EFFORT_LOW: Effort
EFFORT_MEDIUM: Effort
EFFORT_HIGH: Effort
EFFORT_XHIGH: Effort
EFFORT_MAX: Effort

class TurnOverrides(_message.Message):
    __slots__ = ()
    MODEL_FIELD_NUMBER: _ClassVar[int]
    EFFORT_FIELD_NUMBER: _ClassVar[int]
    model: str
    effort: Effort
    def __init__(self, model: _Optional[str] = ..., effort: _Optional[_Union[Effort, str]] = ...) -> None: ...
