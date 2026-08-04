from arsox.common.v1 import common_pb2 as _common_pb2
from google.protobuf.internal import containers as _containers
from google.protobuf import descriptor as _descriptor
from google.protobuf import message as _message
from collections.abc import Iterable as _Iterable, Mapping as _Mapping
from typing import ClassVar as _ClassVar, Optional as _Optional, Union as _Union

DESCRIPTOR: _descriptor.FileDescriptor

class QuestionOption(_message.Message):
    __slots__ = ()
    OPTION_ID_FIELD_NUMBER: _ClassVar[int]
    TITLE_FIELD_NUMBER: _ClassVar[int]
    DESCRIPTION_FIELD_NUMBER: _ClassVar[int]
    IS_RECOMMENDED_FIELD_NUMBER: _ClassVar[int]
    option_id: str
    title: str
    description: str
    is_recommended: bool
    def __init__(self, option_id: _Optional[str] = ..., title: _Optional[str] = ..., description: _Optional[str] = ..., is_recommended: _Optional[bool] = ...) -> None: ...

class Question(_message.Message):
    __slots__ = ()
    QUESTION_ID_FIELD_NUMBER: _ClassVar[int]
    TITLE_FIELD_NUMBER: _ClassVar[int]
    DETAIL_FIELD_NUMBER: _ClassVar[int]
    OPTIONS_FIELD_NUMBER: _ClassVar[int]
    ALLOW_FREEFORM_FIELD_NUMBER: _ClassVar[int]
    question_id: str
    title: str
    detail: str
    options: _containers.RepeatedCompositeFieldContainer[QuestionOption]
    allow_freeform: bool
    def __init__(self, question_id: _Optional[str] = ..., title: _Optional[str] = ..., detail: _Optional[str] = ..., options: _Optional[_Iterable[_Union[QuestionOption, _Mapping]]] = ..., allow_freeform: _Optional[bool] = ...) -> None: ...

class QuestionSet(_message.Message):
    __slots__ = ()
    QUESTION_SET_ID_FIELD_NUMBER: _ClassVar[int]
    QUESTIONS_FIELD_NUMBER: _ClassVar[int]
    ASKED_AT_FIELD_NUMBER: _ClassVar[int]
    EXPIRES_AT_FIELD_NUMBER: _ClassVar[int]
    question_set_id: str
    questions: _containers.RepeatedCompositeFieldContainer[Question]
    asked_at: _common_pb2.Timestamp
    expires_at: _common_pb2.Timestamp
    def __init__(self, question_set_id: _Optional[str] = ..., questions: _Optional[_Iterable[_Union[Question, _Mapping]]] = ..., asked_at: _Optional[_Union[_common_pb2.Timestamp, _Mapping]] = ..., expires_at: _Optional[_Union[_common_pb2.Timestamp, _Mapping]] = ...) -> None: ...

class Declined(_message.Message):
    __slots__ = ()
    def __init__(self) -> None: ...

class QuestionAnswer(_message.Message):
    __slots__ = ()
    QUESTION_ID_FIELD_NUMBER: _ClassVar[int]
    OPTION_ID_FIELD_NUMBER: _ClassVar[int]
    TEXT_FIELD_NUMBER: _ClassVar[int]
    DECLINED_FIELD_NUMBER: _ClassVar[int]
    question_id: str
    option_id: str
    text: str
    declined: Declined
    def __init__(self, question_id: _Optional[str] = ..., option_id: _Optional[str] = ..., text: _Optional[str] = ..., declined: _Optional[_Union[Declined, _Mapping]] = ...) -> None: ...

class AnswerQuestionsRequest(_message.Message):
    __slots__ = ()
    THREAD_ID_FIELD_NUMBER: _ClassVar[int]
    QUESTION_SET_ID_FIELD_NUMBER: _ClassVar[int]
    ANSWERS_FIELD_NUMBER: _ClassVar[int]
    thread_id: str
    question_set_id: str
    answers: _containers.RepeatedCompositeFieldContainer[QuestionAnswer]
    def __init__(self, thread_id: _Optional[str] = ..., question_set_id: _Optional[str] = ..., answers: _Optional[_Iterable[_Union[QuestionAnswer, _Mapping]]] = ...) -> None: ...

class AnswerQuestionsResponse(_message.Message):
    __slots__ = ()
    QUESTION_SET_FIELD_NUMBER: _ClassVar[int]
    ANSWERS_FIELD_NUMBER: _ClassVar[int]
    question_set: QuestionSet
    answers: _containers.RepeatedCompositeFieldContainer[QuestionAnswer]
    def __init__(self, question_set: _Optional[_Union[QuestionSet, _Mapping]] = ..., answers: _Optional[_Iterable[_Union[QuestionAnswer, _Mapping]]] = ...) -> None: ...
