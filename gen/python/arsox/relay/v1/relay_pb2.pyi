from arsox.common.v1 import common_pb2 as _common_pb2
from google.protobuf.internal import containers as _containers
from google.protobuf import descriptor as _descriptor
from google.protobuf import message as _message
from collections.abc import Iterable as _Iterable, Mapping as _Mapping
from typing import ClassVar as _ClassVar, Optional as _Optional, Union as _Union

DESCRIPTOR: _descriptor.FileDescriptor

class ToolCall(_message.Message):
    __slots__ = ()
    CALL_ID_FIELD_NUMBER: _ClassVar[int]
    SERVER_FIELD_NUMBER: _ClassVar[int]
    TOOL_FIELD_NUMBER: _ClassVar[int]
    ARGUMENTS_JSON_FIELD_NUMBER: _ClassVar[int]
    TURN_ID_FIELD_NUMBER: _ClassVar[int]
    MEMBER_ID_FIELD_NUMBER: _ClassVar[int]
    DEADLINE_FIELD_NUMBER: _ClassVar[int]
    call_id: str
    server: str
    tool: str
    arguments_json: str
    turn_id: str
    member_id: str
    deadline: _common_pb2.Timestamp
    def __init__(self, call_id: _Optional[str] = ..., server: _Optional[str] = ..., tool: _Optional[str] = ..., arguments_json: _Optional[str] = ..., turn_id: _Optional[str] = ..., member_id: _Optional[str] = ..., deadline: _Optional[_Union[_common_pb2.Timestamp, _Mapping]] = ...) -> None: ...

class ToolContent(_message.Message):
    __slots__ = ()
    TEXT_FIELD_NUMBER: _ClassVar[int]
    text: str
    def __init__(self, text: _Optional[str] = ...) -> None: ...

class ToolResult(_message.Message):
    __slots__ = ()
    CALL_ID_FIELD_NUMBER: _ClassVar[int]
    CONTENT_FIELD_NUMBER: _ClassVar[int]
    IS_ERROR_FIELD_NUMBER: _ClassVar[int]
    call_id: str
    content: _containers.RepeatedCompositeFieldContainer[ToolContent]
    is_error: bool
    def __init__(self, call_id: _Optional[str] = ..., content: _Optional[_Iterable[_Union[ToolContent, _Mapping]]] = ..., is_error: _Optional[bool] = ...) -> None: ...

class ToolCallCancelled(_message.Message):
    __slots__ = ()
    CALL_ID_FIELD_NUMBER: _ClassVar[int]
    call_id: str
    def __init__(self, call_id: _Optional[str] = ...) -> None: ...

class SatelliteRelayFrame(_message.Message):
    __slots__ = ()
    CALL_FIELD_NUMBER: _ClassVar[int]
    CANCELLED_FIELD_NUMBER: _ClassVar[int]
    call: ToolCall
    cancelled: ToolCallCancelled
    def __init__(self, call: _Optional[_Union[ToolCall, _Mapping]] = ..., cancelled: _Optional[_Union[ToolCallCancelled, _Mapping]] = ...) -> None: ...

class ClientRelayFrame(_message.Message):
    __slots__ = ()
    RESULT_FIELD_NUMBER: _ClassVar[int]
    result: ToolResult
    def __init__(self, result: _Optional[_Union[ToolResult, _Mapping]] = ...) -> None: ...
