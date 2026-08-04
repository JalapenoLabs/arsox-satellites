from google.protobuf.internal import containers as _containers
from google.protobuf.internal import enum_type_wrapper as _enum_type_wrapper
from google.protobuf import descriptor as _descriptor
from google.protobuf import message as _message
from collections.abc import Iterable as _Iterable
from typing import ClassVar as _ClassVar, Optional as _Optional, Union as _Union

DESCRIPTOR: _descriptor.FileDescriptor

class PrefetchInjection(int, metaclass=_enum_type_wrapper.EnumTypeWrapper):
    __slots__ = ()
    PREFETCH_INJECTION_UNSPECIFIED: _ClassVar[PrefetchInjection]
    PREFETCH_INJECTION_INDEX: _ClassVar[PrefetchInjection]
    PREFETCH_INJECTION_SUMMARY: _ClassVar[PrefetchInjection]
    PREFETCH_INJECTION_NONE: _ClassVar[PrefetchInjection]
PREFETCH_INJECTION_UNSPECIFIED: PrefetchInjection
PREFETCH_INJECTION_INDEX: PrefetchInjection
PREFETCH_INJECTION_SUMMARY: PrefetchInjection
PREFETCH_INJECTION_NONE: PrefetchInjection

class Prefetch(_message.Message):
    __slots__ = ()
    JIRA_FIELD_NUMBER: _ClassVar[int]
    GITHUB_FIELD_NUMBER: _ClassVar[int]
    INJECTION_FIELD_NUMBER: _ClassVar[int]
    MAX_ATTACHMENT_BYTES_PER_ITEM_FIELD_NUMBER: _ClassVar[int]
    jira: _containers.RepeatedScalarFieldContainer[str]
    github: _containers.RepeatedScalarFieldContainer[int]
    injection: PrefetchInjection
    max_attachment_bytes_per_item: int
    def __init__(self, jira: _Optional[_Iterable[str]] = ..., github: _Optional[_Iterable[int]] = ..., injection: _Optional[_Union[PrefetchInjection, str]] = ..., max_attachment_bytes_per_item: _Optional[int] = ...) -> None: ...
