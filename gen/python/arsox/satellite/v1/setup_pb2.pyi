from arsox.common.v1 import common_pb2 as _common_pb2
from google.protobuf.internal import enum_type_wrapper as _enum_type_wrapper
from google.protobuf import descriptor as _descriptor
from google.protobuf import message as _message
from collections.abc import Mapping as _Mapping
from typing import ClassVar as _ClassVar, Optional as _Optional, Union as _Union

DESCRIPTOR: _descriptor.FileDescriptor

class SetupState(int, metaclass=_enum_type_wrapper.EnumTypeWrapper):
    __slots__ = ()
    SETUP_STATE_UNSPECIFIED: _ClassVar[SetupState]
    SETUP_STATE_NONE: _ClassVar[SetupState]
    SETUP_STATE_RUNNING: _ClassVar[SetupState]
    SETUP_STATE_SUCCEEDED: _ClassVar[SetupState]
    SETUP_STATE_FAILED: _ClassVar[SetupState]
SETUP_STATE_UNSPECIFIED: SetupState
SETUP_STATE_NONE: SetupState
SETUP_STATE_RUNNING: SetupState
SETUP_STATE_SUCCEEDED: SetupState
SETUP_STATE_FAILED: SetupState

class SetupStatus(_message.Message):
    __slots__ = ()
    STATE_FIELD_NUMBER: _ClassVar[int]
    SCRIPT_SHA256_FIELD_NUMBER: _ClassVar[int]
    EXIT_CODE_FIELD_NUMBER: _ClassVar[int]
    OUTPUT_TAIL_FIELD_NUMBER: _ClassVar[int]
    STARTED_AT_FIELD_NUMBER: _ClassVar[int]
    FINISHED_AT_FIELD_NUMBER: _ClassVar[int]
    state: SetupState
    script_sha256: str
    exit_code: int
    output_tail: str
    started_at: _common_pb2.Timestamp
    finished_at: _common_pb2.Timestamp
    def __init__(self, state: _Optional[_Union[SetupState, str]] = ..., script_sha256: _Optional[str] = ..., exit_code: _Optional[int] = ..., output_tail: _Optional[str] = ..., started_at: _Optional[_Union[_common_pb2.Timestamp, _Mapping]] = ..., finished_at: _Optional[_Union[_common_pb2.Timestamp, _Mapping]] = ...) -> None: ...

class SetSetupScriptRequest(_message.Message):
    __slots__ = ()
    SCRIPT_FIELD_NUMBER: _ClassVar[int]
    script: str
    def __init__(self, script: _Optional[str] = ...) -> None: ...

class SetSetupScriptResponse(_message.Message):
    __slots__ = ()
    SETUP_FIELD_NUMBER: _ClassVar[int]
    setup: SetupStatus
    def __init__(self, setup: _Optional[_Union[SetupStatus, _Mapping]] = ...) -> None: ...
