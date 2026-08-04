from arsox.common.v1 import common_pb2 as _common_pb2
from google.protobuf import descriptor as _descriptor
from google.protobuf import message as _message
from collections.abc import Mapping as _Mapping
from typing import ClassVar as _ClassVar, Optional as _Optional, Union as _Union

DESCRIPTOR: _descriptor.FileDescriptor

class GithubIntegration(_message.Message):
    __slots__ = ()
    TOKEN_FIELD_NUMBER: _ClassVar[int]
    token: _common_pb2.Secret
    def __init__(self, token: _Optional[_Union[_common_pb2.Secret, _Mapping]] = ...) -> None: ...

class JiraIntegration(_message.Message):
    __slots__ = ()
    TOKEN_FIELD_NUMBER: _ClassVar[int]
    BASE_URL_FIELD_NUMBER: _ClassVar[int]
    EMAIL_FIELD_NUMBER: _ClassVar[int]
    ALLOW_STATUS_TRANSITIONS_FIELD_NUMBER: _ClassVar[int]
    ALLOW_COMMENTS_FIELD_NUMBER: _ClassVar[int]
    token: _common_pb2.Secret
    base_url: str
    email: str
    allow_status_transitions: bool
    allow_comments: bool
    def __init__(self, token: _Optional[_Union[_common_pb2.Secret, _Mapping]] = ..., base_url: _Optional[str] = ..., email: _Optional[str] = ..., allow_status_transitions: _Optional[bool] = ..., allow_comments: _Optional[bool] = ...) -> None: ...
