from arsox.common.v1 import common_pb2 as _common_pb2
from google.protobuf.internal import enum_type_wrapper as _enum_type_wrapper
from google.protobuf import descriptor as _descriptor
from google.protobuf import message as _message
from collections.abc import Mapping as _Mapping
from typing import ClassVar as _ClassVar, Optional as _Optional, Union as _Union

DESCRIPTOR: _descriptor.FileDescriptor

class RedactionMode(int, metaclass=_enum_type_wrapper.EnumTypeWrapper):
    __slots__ = ()
    REDACTION_MODE_UNSPECIFIED: _ClassVar[RedactionMode]
    REDACTION_MODE_ANONYMOUS: _ClassVar[RedactionMode]
    REDACTION_MODE_PREFIX_SHOWN: _ClassVar[RedactionMode]
    REDACTION_MODE_POSTFIX_SHOWN: _ClassVar[RedactionMode]
    REDACTION_MODE_HYBRID_SHOWN: _ClassVar[RedactionMode]
REDACTION_MODE_UNSPECIFIED: RedactionMode
REDACTION_MODE_ANONYMOUS: RedactionMode
REDACTION_MODE_PREFIX_SHOWN: RedactionMode
REDACTION_MODE_POSTFIX_SHOWN: RedactionMode
REDACTION_MODE_HYBRID_SHOWN: RedactionMode

class EnvVar(_message.Message):
    __slots__ = ()
    KEY_FIELD_NUMBER: _ClassVar[int]
    VALUE_FIELD_NUMBER: _ClassVar[int]
    IS_SECRET_FIELD_NUMBER: _ClassVar[int]
    key: str
    value: _common_pb2.Secret
    is_secret: bool
    def __init__(self, key: _Optional[str] = ..., value: _Optional[_Union[_common_pb2.Secret, _Mapping]] = ..., is_secret: _Optional[bool] = ...) -> None: ...

class MirrorLength(_message.Message):
    __slots__ = ()
    def __init__(self) -> None: ...

class StarCount(_message.Message):
    __slots__ = ()
    FIXED_FIELD_NUMBER: _ClassVar[int]
    MIRROR_FIELD_NUMBER: _ClassVar[int]
    fixed: int
    mirror: MirrorLength
    def __init__(self, fixed: _Optional[int] = ..., mirror: _Optional[_Union[MirrorLength, _Mapping]] = ...) -> None: ...

class Redaction(_message.Message):
    __slots__ = ()
    MODE_FIELD_NUMBER: _ClassVar[int]
    STAR_COUNT_FIELD_NUMBER: _ClassVar[int]
    ALLOW_REDACTION_OVERRIDE_FIELD_NUMBER: _ClassVar[int]
    mode: RedactionMode
    star_count: StarCount
    allow_redaction_override: bool
    def __init__(self, mode: _Optional[_Union[RedactionMode, str]] = ..., star_count: _Optional[_Union[StarCount, _Mapping]] = ..., allow_redaction_override: _Optional[bool] = ...) -> None: ...
