from arsox.common.v1 import common_pb2 as _common_pb2
from arsox.settings.v1 import service_pb2 as _service_pb2
from google.protobuf.internal import containers as _containers
from google.protobuf import descriptor as _descriptor
from google.protobuf import message as _message
from collections.abc import Iterable as _Iterable, Mapping as _Mapping
from typing import ClassVar as _ClassVar, Optional as _Optional, Union as _Union

DESCRIPTOR: _descriptor.FileDescriptor

class GitAuth(_message.Message):
    __slots__ = ()
    SSH_KEY_FIELD_NUMBER: _ClassVar[int]
    PERSONAL_ACCESS_TOKEN_FIELD_NUMBER: _ClassVar[int]
    ssh_key: SshKeyPair
    personal_access_token: _common_pb2.Secret
    def __init__(self, ssh_key: _Optional[_Union[SshKeyPair, _Mapping]] = ..., personal_access_token: _Optional[_Union[_common_pb2.Secret, _Mapping]] = ...) -> None: ...

class SshKeyPair(_message.Message):
    __slots__ = ()
    PRIVATE_KEY_FIELD_NUMBER: _ClassVar[int]
    PUBLIC_KEY_FIELD_NUMBER: _ClassVar[int]
    private_key: _common_pb2.Secret
    public_key: str
    def __init__(self, private_key: _Optional[_Union[_common_pb2.Secret, _Mapping]] = ..., public_key: _Optional[str] = ...) -> None: ...

class AgentsRepo(_message.Message):
    __slots__ = ()
    URL_FIELD_NUMBER: _ClassVar[int]
    REF_FIELD_NUMBER: _ClassVar[int]
    AUTH_FIELD_NUMBER: _ClassVar[int]
    url: str
    ref: str
    auth: GitAuth
    def __init__(self, url: _Optional[str] = ..., ref: _Optional[str] = ..., auth: _Optional[_Union[GitAuth, _Mapping]] = ...) -> None: ...

class Repo(_message.Message):
    __slots__ = ()
    NAME_FIELD_NUMBER: _ClassVar[int]
    URL_FIELD_NUMBER: _ClassVar[int]
    BASE_BRANCH_FIELD_NUMBER: _ClassVar[int]
    AUTH_FIELD_NUMBER: _ClassVar[int]
    SETUP_COMMANDS_FIELD_NUMBER: _ClassVar[int]
    CHECKER_FIELD_NUMBER: _ClassVar[int]
    SERVICES_FIELD_NUMBER: _ClassVar[int]
    name: str
    url: str
    base_branch: str
    auth: GitAuth
    setup_commands: str
    checker: str
    services: _containers.RepeatedCompositeFieldContainer[_service_pb2.Service]
    def __init__(self, name: _Optional[str] = ..., url: _Optional[str] = ..., base_branch: _Optional[str] = ..., auth: _Optional[_Union[GitAuth, _Mapping]] = ..., setup_commands: _Optional[str] = ..., checker: _Optional[str] = ..., services: _Optional[_Iterable[_Union[_service_pb2.Service, _Mapping]]] = ...) -> None: ...
