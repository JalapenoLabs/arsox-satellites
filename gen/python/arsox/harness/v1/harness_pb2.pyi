from google.protobuf.internal import containers as _containers
from google.protobuf.internal import enum_type_wrapper as _enum_type_wrapper
from google.protobuf import descriptor as _descriptor
from google.protobuf import message as _message
from collections.abc import Iterable as _Iterable, Mapping as _Mapping
from typing import ClassVar as _ClassVar, Optional as _Optional, Union as _Union

DESCRIPTOR: _descriptor.FileDescriptor

class Harness(int, metaclass=_enum_type_wrapper.EnumTypeWrapper):
    __slots__ = ()
    HARNESS_UNSPECIFIED: _ClassVar[Harness]
    HARNESS_CLAUDE: _ClassVar[Harness]
    HARNESS_CODEX: _ClassVar[Harness]
HARNESS_UNSPECIFIED: Harness
HARNESS_CLAUDE: Harness
HARNESS_CODEX: Harness

class HarnessCapabilities(_message.Message):
    __slots__ = ()
    HARNESS_FIELD_NUMBER: _ClassVar[int]
    CLI_VERSION_FIELD_NUMBER: _ClassVar[int]
    SUPPORTS_NATIVE_PLAN_MODE_FIELD_NUMBER: _ClassVar[int]
    SUPPORTS_SUBAGENTS_FIELD_NUMBER: _ClassVar[int]
    SUPPORTS_THINKING_EVENTS_FIELD_NUMBER: _ClassVar[int]
    SUPPORTS_CONTEXT_FORK_FIELD_NUMBER: _ClassVar[int]
    SUPPORTS_MCP_FIELD_NUMBER: _ClassVar[int]
    REPORTS_CACHE_TOKENS_FIELD_NUMBER: _ClassVar[int]
    harness: Harness
    cli_version: str
    supports_native_plan_mode: bool
    supports_subagents: bool
    supports_thinking_events: bool
    supports_context_fork: bool
    supports_mcp: bool
    reports_cache_tokens: bool
    def __init__(self, harness: _Optional[_Union[Harness, str]] = ..., cli_version: _Optional[str] = ..., supports_native_plan_mode: _Optional[bool] = ..., supports_subagents: _Optional[bool] = ..., supports_thinking_events: _Optional[bool] = ..., supports_context_fork: _Optional[bool] = ..., supports_mcp: _Optional[bool] = ..., reports_cache_tokens: _Optional[bool] = ...) -> None: ...

class GetHarnessResponse(_message.Message):
    __slots__ = ()
    HARNESSES_FIELD_NUMBER: _ClassVar[int]
    DEFAULT_HARNESS_FIELD_NUMBER: _ClassVar[int]
    harnesses: _containers.RepeatedCompositeFieldContainer[HarnessCapabilities]
    default_harness: Harness
    def __init__(self, harnesses: _Optional[_Iterable[_Union[HarnessCapabilities, _Mapping]]] = ..., default_harness: _Optional[_Union[Harness, str]] = ...) -> None: ...
