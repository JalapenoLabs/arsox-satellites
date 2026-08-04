from arsox.common.v1 import common_pb2 as _common_pb2
from google.protobuf import descriptor as _descriptor
from google.protobuf import message as _message
from collections.abc import Mapping as _Mapping
from typing import ClassVar as _ClassVar, Optional as _Optional, Union as _Union

DESCRIPTOR: _descriptor.FileDescriptor

class StreamSettings(_message.Message):
    __slots__ = ()
    INCLUDE_STATISTICS_FIELD_NUMBER: _ClassVar[int]
    INCLUDE_AGENT_THINKING_FIELD_NUMBER: _ClassVar[int]
    INCLUDE_TOOL_CALLS_FIELD_NUMBER: _ClassVar[int]
    INCLUDE_TEAM_CHAT_FIELD_NUMBER: _ClassVar[int]
    INCLUDE_SERVICE_LOGS_FIELD_NUMBER: _ClassVar[int]
    include_statistics: bool
    include_agent_thinking: bool
    include_tool_calls: bool
    include_team_chat: bool
    include_service_logs: bool
    def __init__(self, include_statistics: _Optional[bool] = ..., include_agent_thinking: _Optional[bool] = ..., include_tool_calls: _Optional[bool] = ..., include_team_chat: _Optional[bool] = ..., include_service_logs: _Optional[bool] = ...) -> None: ...

class ResourceLimits(_message.Message):
    __slots__ = ()
    WORKSPACE_QUOTA_BYTES_FIELD_NUMBER: _ClassVar[int]
    ARTIFACT_CAP_BYTES_FIELD_NUMBER: _ClassVar[int]
    workspace_quota_bytes: int
    artifact_cap_bytes: int
    def __init__(self, workspace_quota_bytes: _Optional[int] = ..., artifact_cap_bytes: _Optional[int] = ...) -> None: ...

class Timeouts(_message.Message):
    __slots__ = ()
    EXEC_COMMAND_FIELD_NUMBER: _ClassVar[int]
    LLM_REQUEST_FIELD_NUMBER: _ClassVar[int]
    HARNESS_IDLE_FIELD_NUMBER: _ClassVar[int]
    exec_command: _common_pb2.Duration
    llm_request: _common_pb2.Duration
    harness_idle: _common_pb2.Duration
    def __init__(self, exec_command: _Optional[_Union[_common_pb2.Duration, _Mapping]] = ..., llm_request: _Optional[_Union[_common_pb2.Duration, _Mapping]] = ..., harness_idle: _Optional[_Union[_common_pb2.Duration, _Mapping]] = ...) -> None: ...
