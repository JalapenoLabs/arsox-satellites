from arsox.common.v1 import common_pb2 as _common_pb2
from google.protobuf.internal import containers as _containers
from google.protobuf import descriptor as _descriptor
from google.protobuf import message as _message
from collections.abc import Iterable as _Iterable, Mapping as _Mapping
from typing import ClassVar as _ClassVar, Optional as _Optional, Union as _Union

DESCRIPTOR: _descriptor.FileDescriptor

class Artifact(_message.Message):
    __slots__ = ()
    NAME_FIELD_NUMBER: _ClassVar[int]
    PATH_FIELD_NUMBER: _ClassVar[int]
    SIZE_BYTES_FIELD_NUMBER: _ClassVar[int]
    CONTENT_TYPE_FIELD_NUMBER: _ClassVar[int]
    SHA256_FIELD_NUMBER: _ClassVar[int]
    CREATED_AT_FIELD_NUMBER: _ClassVar[int]
    MEMBER_ID_FIELD_NUMBER: _ClassVar[int]
    name: str
    path: str
    size_bytes: int
    content_type: str
    sha256: str
    created_at: _common_pb2.Timestamp
    member_id: str
    def __init__(self, name: _Optional[str] = ..., path: _Optional[str] = ..., size_bytes: _Optional[int] = ..., content_type: _Optional[str] = ..., sha256: _Optional[str] = ..., created_at: _Optional[_Union[_common_pb2.Timestamp, _Mapping]] = ..., member_id: _Optional[str] = ...) -> None: ...

class WorkspaceFile(_message.Message):
    __slots__ = ()
    PATH_FIELD_NUMBER: _ClassVar[int]
    SIZE_BYTES_FIELD_NUMBER: _ClassVar[int]
    IS_ARTIFACT_FIELD_NUMBER: _ClassVar[int]
    MODIFIED_AT_FIELD_NUMBER: _ClassVar[int]
    path: str
    size_bytes: int
    is_artifact: bool
    modified_at: _common_pb2.Timestamp
    def __init__(self, path: _Optional[str] = ..., size_bytes: _Optional[int] = ..., is_artifact: _Optional[bool] = ..., modified_at: _Optional[_Union[_common_pb2.Timestamp, _Mapping]] = ...) -> None: ...

class ListArtifactsRequest(_message.Message):
    __slots__ = ()
    THREAD_ID_FIELD_NUMBER: _ClassVar[int]
    PAGE_FIELD_NUMBER: _ClassVar[int]
    thread_id: str
    page: _common_pb2.PageRequest
    def __init__(self, thread_id: _Optional[str] = ..., page: _Optional[_Union[_common_pb2.PageRequest, _Mapping]] = ...) -> None: ...

class ListArtifactsResponse(_message.Message):
    __slots__ = ()
    ARTIFACTS_FIELD_NUMBER: _ClassVar[int]
    PAGE_FIELD_NUMBER: _ClassVar[int]
    artifacts: _containers.RepeatedCompositeFieldContainer[Artifact]
    page: _common_pb2.PageResponse
    def __init__(self, artifacts: _Optional[_Iterable[_Union[Artifact, _Mapping]]] = ..., page: _Optional[_Union[_common_pb2.PageResponse, _Mapping]] = ...) -> None: ...

class ListWorkspaceFilesRequest(_message.Message):
    __slots__ = ()
    THREAD_ID_FIELD_NUMBER: _ClassVar[int]
    PATH_PREFIX_FIELD_NUMBER: _ClassVar[int]
    PAGE_FIELD_NUMBER: _ClassVar[int]
    thread_id: str
    path_prefix: str
    page: _common_pb2.PageRequest
    def __init__(self, thread_id: _Optional[str] = ..., path_prefix: _Optional[str] = ..., page: _Optional[_Union[_common_pb2.PageRequest, _Mapping]] = ...) -> None: ...

class ListWorkspaceFilesResponse(_message.Message):
    __slots__ = ()
    FILES_FIELD_NUMBER: _ClassVar[int]
    PAGE_FIELD_NUMBER: _ClassVar[int]
    files: _containers.RepeatedCompositeFieldContainer[WorkspaceFile]
    page: _common_pb2.PageResponse
    def __init__(self, files: _Optional[_Iterable[_Union[WorkspaceFile, _Mapping]]] = ..., page: _Optional[_Union[_common_pb2.PageResponse, _Mapping]] = ...) -> None: ...
