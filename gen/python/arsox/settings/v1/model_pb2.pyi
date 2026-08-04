from arsox.common.v1 import common_pb2 as _common_pb2
from google.protobuf.internal import containers as _containers
from google.protobuf import descriptor as _descriptor
from google.protobuf import message as _message
from collections.abc import Iterable as _Iterable, Mapping as _Mapping
from typing import ClassVar as _ClassVar, Optional as _Optional, Union as _Union

DESCRIPTOR: _descriptor.FileDescriptor

class ModelEndpoint(_message.Message):
    __slots__ = ()
    NAME_FIELD_NUMBER: _ClassVar[int]
    MODEL_FIELD_NUMBER: _ClassVar[int]
    BASE_URL_FIELD_NUMBER: _ClassVar[int]
    AUTH_FIELD_NUMBER: _ClassVar[int]
    RETRY_FIELD_NUMBER: _ClassVar[int]
    name: str
    model: str
    base_url: str
    auth: LlmAuth
    retry: RetryPolicy
    def __init__(self, name: _Optional[str] = ..., model: _Optional[str] = ..., base_url: _Optional[str] = ..., auth: _Optional[_Union[LlmAuth, _Mapping]] = ..., retry: _Optional[_Union[RetryPolicy, _Mapping]] = ...) -> None: ...

class LlmAuth(_message.Message):
    __slots__ = ()
    API_KEY_FIELD_NUMBER: _ClassVar[int]
    SUBSCRIPTION_TOKEN_FIELD_NUMBER: _ClassVar[int]
    OAUTH_FIELD_NUMBER: _ClassVar[int]
    api_key: _common_pb2.Secret
    subscription_token: _common_pb2.Secret
    oauth: OAuthCredential
    def __init__(self, api_key: _Optional[_Union[_common_pb2.Secret, _Mapping]] = ..., subscription_token: _Optional[_Union[_common_pb2.Secret, _Mapping]] = ..., oauth: _Optional[_Union[OAuthCredential, _Mapping]] = ...) -> None: ...

class OAuthCredential(_message.Message):
    __slots__ = ()
    ACCESS_TOKEN_FIELD_NUMBER: _ClassVar[int]
    REFRESH_TOKEN_FIELD_NUMBER: _ClassVar[int]
    EXPIRES_AT_FIELD_NUMBER: _ClassVar[int]
    access_token: _common_pb2.Secret
    refresh_token: _common_pb2.Secret
    expires_at: _common_pb2.Timestamp
    def __init__(self, access_token: _Optional[_Union[_common_pb2.Secret, _Mapping]] = ..., refresh_token: _Optional[_Union[_common_pb2.Secret, _Mapping]] = ..., expires_at: _Optional[_Union[_common_pb2.Timestamp, _Mapping]] = ...) -> None: ...

class RetryPolicy(_message.Message):
    __slots__ = ()
    MAX_ATTEMPTS_FIELD_NUMBER: _ClassVar[int]
    INITIAL_BACKOFF_FIELD_NUMBER: _ClassVar[int]
    MAX_BACKOFF_FIELD_NUMBER: _ClassVar[int]
    RETRY_ON_STATUS_FIELD_NUMBER: _ClassVar[int]
    max_attempts: int
    initial_backoff: _common_pb2.Duration
    max_backoff: _common_pb2.Duration
    retry_on_status: _containers.RepeatedScalarFieldContainer[int]
    def __init__(self, max_attempts: _Optional[int] = ..., initial_backoff: _Optional[_Union[_common_pb2.Duration, _Mapping]] = ..., max_backoff: _Optional[_Union[_common_pb2.Duration, _Mapping]] = ..., retry_on_status: _Optional[_Iterable[int]] = ...) -> None: ...
