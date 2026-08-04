from google.protobuf.internal import enum_type_wrapper as _enum_type_wrapper
from google.protobuf import descriptor as _descriptor
from google.protobuf import message as _message
from typing import ClassVar as _ClassVar, Optional as _Optional, Union as _Union

DESCRIPTOR: _descriptor.FileDescriptor

class LogChannel(int, metaclass=_enum_type_wrapper.EnumTypeWrapper):
    __slots__ = ()
    LOG_CHANNEL_UNSPECIFIED: _ClassVar[LogChannel]
    LOG_CHANNEL_STDOUT: _ClassVar[LogChannel]
    LOG_CHANNEL_STDERR: _ClassVar[LogChannel]
LOG_CHANNEL_UNSPECIFIED: LogChannel
LOG_CHANNEL_STDOUT: LogChannel
LOG_CHANNEL_STDERR: LogChannel

class ServiceStarted(_message.Message):
    __slots__ = ()
    REPO_FIELD_NUMBER: _ClassVar[int]
    SERVICE_NAME_FIELD_NUMBER: _ClassVar[int]
    URL_FIELD_NUMBER: _ClassVar[int]
    AUTO_PROMOTED_FIELD_NUMBER: _ClassVar[int]
    MEMBER_ID_FIELD_NUMBER: _ClassVar[int]
    repo: str
    service_name: str
    url: str
    auto_promoted: bool
    member_id: str
    def __init__(self, repo: _Optional[str] = ..., service_name: _Optional[str] = ..., url: _Optional[str] = ..., auto_promoted: _Optional[bool] = ..., member_id: _Optional[str] = ...) -> None: ...

class ServiceLog(_message.Message):
    __slots__ = ()
    SERVICE_NAME_FIELD_NUMBER: _ClassVar[int]
    CHANNEL_FIELD_NUMBER: _ClassVar[int]
    LINE_FIELD_NUMBER: _ClassVar[int]
    service_name: str
    channel: LogChannel
    line: str
    def __init__(self, service_name: _Optional[str] = ..., channel: _Optional[_Union[LogChannel, str]] = ..., line: _Optional[str] = ...) -> None: ...
