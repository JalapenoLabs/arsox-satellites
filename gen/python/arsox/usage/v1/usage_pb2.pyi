from arsox.common.v1 import common_pb2 as _common_pb2
from google.protobuf.internal import containers as _containers
from google.protobuf import descriptor as _descriptor
from google.protobuf import message as _message
from collections.abc import Iterable as _Iterable, Mapping as _Mapping
from typing import ClassVar as _ClassVar, Optional as _Optional, Union as _Union

DESCRIPTOR: _descriptor.FileDescriptor

class TokenUsage(_message.Message):
    __slots__ = ()
    INPUT_TOKENS_FIELD_NUMBER: _ClassVar[int]
    OUTPUT_TOKENS_FIELD_NUMBER: _ClassVar[int]
    TOTAL_TOKENS_FIELD_NUMBER: _ClassVar[int]
    CACHE_READ_TOKENS_FIELD_NUMBER: _ClassVar[int]
    CACHE_WRITE_TOKENS_FIELD_NUMBER: _ClassVar[int]
    REASONING_OUTPUT_TOKENS_FIELD_NUMBER: _ClassVar[int]
    input_tokens: int
    output_tokens: int
    total_tokens: int
    cache_read_tokens: int
    cache_write_tokens: int
    reasoning_output_tokens: int
    def __init__(self, input_tokens: _Optional[int] = ..., output_tokens: _Optional[int] = ..., total_tokens: _Optional[int] = ..., cache_read_tokens: _Optional[int] = ..., cache_write_tokens: _Optional[int] = ..., reasoning_output_tokens: _Optional[int] = ...) -> None: ...

class ServerToolUsage(_message.Message):
    __slots__ = ()
    WEB_SEARCH_REQUESTS_FIELD_NUMBER: _ClassVar[int]
    WEB_FETCH_REQUESTS_FIELD_NUMBER: _ClassVar[int]
    web_search_requests: int
    web_fetch_requests: int
    def __init__(self, web_search_requests: _Optional[int] = ..., web_fetch_requests: _Optional[int] = ...) -> None: ...

class RateLimitWindow(_message.Message):
    __slots__ = ()
    WINDOW_FIELD_NUMBER: _ClassVar[int]
    PERCENT_USED_FIELD_NUMBER: _ClassVar[int]
    RESETS_AT_FIELD_NUMBER: _ClassVar[int]
    window: str
    percent_used: int
    resets_at: _common_pb2.Timestamp
    def __init__(self, window: _Optional[str] = ..., percent_used: _Optional[int] = ..., resets_at: _Optional[_Union[_common_pb2.Timestamp, _Mapping]] = ...) -> None: ...

class RateLimitStatus(_message.Message):
    __slots__ = ()
    WINDOWS_FIELD_NUMBER: _ClassVar[int]
    THROTTLED_FIELD_NUMBER: _ClassVar[int]
    windows: _containers.RepeatedCompositeFieldContainer[RateLimitWindow]
    throttled: bool
    def __init__(self, windows: _Optional[_Iterable[_Union[RateLimitWindow, _Mapping]]] = ..., throttled: _Optional[bool] = ...) -> None: ...

class CostEstimate(_message.Message):
    __slots__ = ()
    AMOUNT_FIELD_NUMBER: _ClassVar[int]
    IS_PARTIAL_FIELD_NUMBER: _ClassVar[int]
    amount: _common_pb2.Money
    is_partial: bool
    def __init__(self, amount: _Optional[_Union[_common_pb2.Money, _Mapping]] = ..., is_partial: _Optional[bool] = ...) -> None: ...

class ModelStatistics(_message.Message):
    __slots__ = ()
    MODEL_FIELD_NUMBER: _ClassVar[int]
    TOKENS_FIELD_NUMBER: _ClassVar[int]
    COST_FIELD_NUMBER: _ClassVar[int]
    SERVER_TOOLS_FIELD_NUMBER: _ClassVar[int]
    model: str
    tokens: TokenUsage
    cost: CostEstimate
    server_tools: ServerToolUsage
    def __init__(self, model: _Optional[str] = ..., tokens: _Optional[_Union[TokenUsage, _Mapping]] = ..., cost: _Optional[_Union[CostEstimate, _Mapping]] = ..., server_tools: _Optional[_Union[ServerToolUsage, _Mapping]] = ...) -> None: ...

class ThreadStatistics(_message.Message):
    __slots__ = ()
    THREAD_ID_FIELD_NUMBER: _ClassVar[int]
    TOKENS_FIELD_NUMBER: _ClassVar[int]
    COST_FIELD_NUMBER: _ClassVar[int]
    thread_id: str
    tokens: TokenUsage
    cost: CostEstimate
    def __init__(self, thread_id: _Optional[str] = ..., tokens: _Optional[_Union[TokenUsage, _Mapping]] = ..., cost: _Optional[_Union[CostEstimate, _Mapping]] = ...) -> None: ...

class LifetimeStatistics(_message.Message):
    __slots__ = ()
    BY_MODEL_FIELD_NUMBER: _ClassVar[int]
    BY_THREAD_FIELD_NUMBER: _ClassVar[int]
    TOTAL_FIELD_NUMBER: _ClassVar[int]
    TOTAL_COST_FIELD_NUMBER: _ClassVar[int]
    COLLECTED_AT_FIELD_NUMBER: _ClassVar[int]
    by_model: _containers.RepeatedCompositeFieldContainer[ModelStatistics]
    by_thread: _containers.RepeatedCompositeFieldContainer[ThreadStatistics]
    total: TokenUsage
    total_cost: CostEstimate
    collected_at: _common_pb2.Timestamp
    def __init__(self, by_model: _Optional[_Iterable[_Union[ModelStatistics, _Mapping]]] = ..., by_thread: _Optional[_Iterable[_Union[ThreadStatistics, _Mapping]]] = ..., total: _Optional[_Union[TokenUsage, _Mapping]] = ..., total_cost: _Optional[_Union[CostEstimate, _Mapping]] = ..., collected_at: _Optional[_Union[_common_pb2.Timestamp, _Mapping]] = ...) -> None: ...

class GetStatisticsRequest(_message.Message):
    __slots__ = ()
    THREAD_IDS_FIELD_NUMBER: _ClassVar[int]
    thread_ids: _containers.RepeatedScalarFieldContainer[str]
    def __init__(self, thread_ids: _Optional[_Iterable[str]] = ...) -> None: ...

class GetStatisticsResponse(_message.Message):
    __slots__ = ()
    STATISTICS_FIELD_NUMBER: _ClassVar[int]
    statistics: LifetimeStatistics
    def __init__(self, statistics: _Optional[_Union[LifetimeStatistics, _Mapping]] = ...) -> None: ...
