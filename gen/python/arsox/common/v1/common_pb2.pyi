from google.protobuf import descriptor as _descriptor
from google.protobuf import message as _message
from collections.abc import Mapping as _Mapping
from typing import ClassVar as _ClassVar, Optional as _Optional, Union as _Union

DESCRIPTOR: _descriptor.FileDescriptor

class Timestamp(_message.Message):
    __slots__ = ()
    EPOCH_SECONDS_FIELD_NUMBER: _ClassVar[int]
    NANOS_FIELD_NUMBER: _ClassVar[int]
    TIMEZONE_FIELD_NUMBER: _ClassVar[int]
    epoch_seconds: int
    nanos: int
    timezone: str
    def __init__(self, epoch_seconds: _Optional[int] = ..., nanos: _Optional[int] = ..., timezone: _Optional[str] = ...) -> None: ...

class Duration(_message.Message):
    __slots__ = ()
    SECONDS_FIELD_NUMBER: _ClassVar[int]
    NANOS_FIELD_NUMBER: _ClassVar[int]
    seconds: int
    nanos: int
    def __init__(self, seconds: _Optional[int] = ..., nanos: _Optional[int] = ...) -> None: ...

class Money(_message.Message):
    __slots__ = ()
    CURRENCY_CODE_FIELD_NUMBER: _ClassVar[int]
    UNITS_FIELD_NUMBER: _ClassVar[int]
    NANOS_FIELD_NUMBER: _ClassVar[int]
    currency_code: str
    units: int
    nanos: int
    def __init__(self, currency_code: _Optional[str] = ..., units: _Optional[int] = ..., nanos: _Optional[int] = ...) -> None: ...

class Secret(_message.Message):
    __slots__ = ()
    VALUE_FIELD_NUMBER: _ClassVar[int]
    DISPLAY_FIELD_NUMBER: _ClassVar[int]
    value: str
    display: str
    def __init__(self, value: _Optional[str] = ..., display: _Optional[str] = ...) -> None: ...

class Unlimited(_message.Message):
    __slots__ = ()
    def __init__(self) -> None: ...

class TokenCeiling(_message.Message):
    __slots__ = ()
    TOKENS_FIELD_NUMBER: _ClassVar[int]
    UNLIMITED_FIELD_NUMBER: _ClassVar[int]
    tokens: int
    unlimited: Unlimited
    def __init__(self, tokens: _Optional[int] = ..., unlimited: _Optional[_Union[Unlimited, _Mapping]] = ...) -> None: ...

class CostCeiling(_message.Message):
    __slots__ = ()
    COST_FIELD_NUMBER: _ClassVar[int]
    UNLIMITED_FIELD_NUMBER: _ClassVar[int]
    cost: Money
    unlimited: Unlimited
    def __init__(self, cost: _Optional[_Union[Money, _Mapping]] = ..., unlimited: _Optional[_Union[Unlimited, _Mapping]] = ...) -> None: ...

class DurationCeiling(_message.Message):
    __slots__ = ()
    DURATION_FIELD_NUMBER: _ClassVar[int]
    UNLIMITED_FIELD_NUMBER: _ClassVar[int]
    duration: Duration
    unlimited: Unlimited
    def __init__(self, duration: _Optional[_Union[Duration, _Mapping]] = ..., unlimited: _Optional[_Union[Unlimited, _Mapping]] = ...) -> None: ...

class PageRequest(_message.Message):
    __slots__ = ()
    LIMIT_FIELD_NUMBER: _ClassVar[int]
    CURSOR_FIELD_NUMBER: _ClassVar[int]
    limit: int
    cursor: str
    def __init__(self, limit: _Optional[int] = ..., cursor: _Optional[str] = ...) -> None: ...

class PageResponse(_message.Message):
    __slots__ = ()
    NEXT_CURSOR_FIELD_NUMBER: _ClassVar[int]
    TOTAL_FIELD_NUMBER: _ClassVar[int]
    next_cursor: str
    total: int
    def __init__(self, next_cursor: _Optional[str] = ..., total: _Optional[int] = ...) -> None: ...
