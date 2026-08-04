from arsox.common.v1 import common_pb2 as _common_pb2
from google.protobuf import descriptor as _descriptor
from google.protobuf import message as _message
from collections.abc import Mapping as _Mapping
from typing import ClassVar as _ClassVar, Optional as _Optional, Union as _Union

DESCRIPTOR: _descriptor.FileDescriptor

class Budget(_message.Message):
    __slots__ = ()
    MAX_TOKENS_PER_TURN_FIELD_NUMBER: _ClassVar[int]
    MAX_COST_PER_THREAD_FIELD_NUMBER: _ClassVar[int]
    MAX_WALL_CLOCK_PER_TURN_FIELD_NUMBER: _ClassVar[int]
    max_tokens_per_turn: _common_pb2.TokenCeiling
    max_cost_per_thread: _common_pb2.CostCeiling
    max_wall_clock_per_turn: _common_pb2.DurationCeiling
    def __init__(self, max_tokens_per_turn: _Optional[_Union[_common_pb2.TokenCeiling, _Mapping]] = ..., max_cost_per_thread: _Optional[_Union[_common_pb2.CostCeiling, _Mapping]] = ..., max_wall_clock_per_turn: _Optional[_Union[_common_pb2.DurationCeiling, _Mapping]] = ...) -> None: ...
