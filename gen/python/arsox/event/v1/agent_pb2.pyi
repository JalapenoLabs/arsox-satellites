from arsox.common.v1 import common_pb2 as _common_pb2
from arsox.event.v1 import author_pb2 as _author_pb2
from google.protobuf import struct_pb2 as _struct_pb2
from google.protobuf import descriptor as _descriptor
from google.protobuf import message as _message
from collections.abc import Mapping as _Mapping
from typing import ClassVar as _ClassVar, Optional as _Optional, Union as _Union

DESCRIPTOR: _descriptor.FileDescriptor

class AgentMessage(_message.Message):
    __slots__ = ()
    AUTHOR_FIELD_NUMBER: _ClassVar[int]
    TEXT_FIELD_NUMBER: _ClassVar[int]
    author: _author_pb2.Author
    text: str
    def __init__(self, author: _Optional[_Union[_author_pb2.Author, _Mapping]] = ..., text: _Optional[str] = ...) -> None: ...

class AgentThinking(_message.Message):
    __slots__ = ()
    AUTHOR_FIELD_NUMBER: _ClassVar[int]
    TEXT_FIELD_NUMBER: _ClassVar[int]
    author: _author_pb2.Author
    text: str
    def __init__(self, author: _Optional[_Union[_author_pb2.Author, _Mapping]] = ..., text: _Optional[str] = ...) -> None: ...

class ToolStarted(_message.Message):
    __slots__ = ()
    AUTHOR_FIELD_NUMBER: _ClassVar[int]
    TOOL_CALL_ID_FIELD_NUMBER: _ClassVar[int]
    TOOL_NAME_FIELD_NUMBER: _ClassVar[int]
    INPUT_FIELD_NUMBER: _ClassVar[int]
    author: _author_pb2.Author
    tool_call_id: str
    tool_name: str
    input: _struct_pb2.Struct
    def __init__(self, author: _Optional[_Union[_author_pb2.Author, _Mapping]] = ..., tool_call_id: _Optional[str] = ..., tool_name: _Optional[str] = ..., input: _Optional[_Union[_struct_pb2.Struct, _Mapping]] = ...) -> None: ...

class ToolCompleted(_message.Message):
    __slots__ = ()
    AUTHOR_FIELD_NUMBER: _ClassVar[int]
    TOOL_CALL_ID_FIELD_NUMBER: _ClassVar[int]
    TOOL_NAME_FIELD_NUMBER: _ClassVar[int]
    OK_FIELD_NUMBER: _ClassVar[int]
    OUTPUT_PREVIEW_FIELD_NUMBER: _ClassVar[int]
    ELAPSED_FIELD_NUMBER: _ClassVar[int]
    author: _author_pb2.Author
    tool_call_id: str
    tool_name: str
    ok: bool
    output_preview: str
    elapsed: _common_pb2.Duration
    def __init__(self, author: _Optional[_Union[_author_pb2.Author, _Mapping]] = ..., tool_call_id: _Optional[str] = ..., tool_name: _Optional[str] = ..., ok: _Optional[bool] = ..., output_preview: _Optional[str] = ..., elapsed: _Optional[_Union[_common_pb2.Duration, _Mapping]] = ...) -> None: ...
