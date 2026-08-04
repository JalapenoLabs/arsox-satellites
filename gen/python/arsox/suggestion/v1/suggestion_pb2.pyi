from google.protobuf.internal import containers as _containers
from google.protobuf.internal import enum_type_wrapper as _enum_type_wrapper
from google.protobuf import descriptor as _descriptor
from google.protobuf import message as _message
from collections.abc import Iterable as _Iterable, Mapping as _Mapping
from typing import ClassVar as _ClassVar, Optional as _Optional, Union as _Union

DESCRIPTOR: _descriptor.FileDescriptor

class SuggestionCategory(int, metaclass=_enum_type_wrapper.EnumTypeWrapper):
    __slots__ = ()
    SUGGESTION_CATEGORY_UNSPECIFIED: _ClassVar[SuggestionCategory]
    SUGGESTION_CATEGORY_TECH_DEBT: _ClassVar[SuggestionCategory]
    SUGGESTION_CATEGORY_IMPROVEMENT: _ClassVar[SuggestionCategory]
    SUGGESTION_CATEGORY_SETUP_SCRIPT: _ClassVar[SuggestionCategory]

class Severity(int, metaclass=_enum_type_wrapper.EnumTypeWrapper):
    __slots__ = ()
    SEVERITY_UNSPECIFIED: _ClassVar[Severity]
    SEVERITY_LOW: _ClassVar[Severity]
    SEVERITY_MEDIUM: _ClassVar[Severity]
    SEVERITY_HIGH: _ClassVar[Severity]
    SEVERITY_CRITICAL: _ClassVar[Severity]
SUGGESTION_CATEGORY_UNSPECIFIED: SuggestionCategory
SUGGESTION_CATEGORY_TECH_DEBT: SuggestionCategory
SUGGESTION_CATEGORY_IMPROVEMENT: SuggestionCategory
SUGGESTION_CATEGORY_SETUP_SCRIPT: SuggestionCategory
SEVERITY_UNSPECIFIED: Severity
SEVERITY_LOW: Severity
SEVERITY_MEDIUM: Severity
SEVERITY_HIGH: Severity
SEVERITY_CRITICAL: Severity

class SourceLocation(_message.Message):
    __slots__ = ()
    PATH_FIELD_NUMBER: _ClassVar[int]
    LINE_FIELD_NUMBER: _ClassVar[int]
    path: str
    line: int
    def __init__(self, path: _Optional[str] = ..., line: _Optional[int] = ...) -> None: ...

class Suggestion(_message.Message):
    __slots__ = ()
    FINGERPRINT_FIELD_NUMBER: _ClassVar[int]
    CATEGORY_FIELD_NUMBER: _ClassVar[int]
    SEVERITY_FIELD_NUMBER: _ClassVar[int]
    TITLE_FIELD_NUMBER: _ClassVar[int]
    BODY_FIELD_NUMBER: _ClassVar[int]
    LOCATIONS_FIELD_NUMBER: _ClassVar[int]
    fingerprint: str
    category: SuggestionCategory
    severity: Severity
    title: str
    body: str
    locations: _containers.RepeatedCompositeFieldContainer[SourceLocation]
    def __init__(self, fingerprint: _Optional[str] = ..., category: _Optional[_Union[SuggestionCategory, str]] = ..., severity: _Optional[_Union[Severity, str]] = ..., title: _Optional[str] = ..., body: _Optional[str] = ..., locations: _Optional[_Iterable[_Union[SourceLocation, _Mapping]]] = ...) -> None: ...

class CommandEvidence(_message.Message):
    __slots__ = ()
    COMMAND_FIELD_NUMBER: _ClassVar[int]
    EXIT_CODE_FIELD_NUMBER: _ClassVar[int]
    OUTPUT_FIELD_NUMBER: _ClassVar[int]
    command: str
    exit_code: int
    output: str
    def __init__(self, command: _Optional[str] = ..., exit_code: _Optional[int] = ..., output: _Optional[str] = ...) -> None: ...

class SetupScriptSuggestion(_message.Message):
    __slots__ = ()
    TITLE_FIELD_NUMBER: _ClassVar[int]
    BODY_FIELD_NUMBER: _ClassVar[int]
    EVIDENCE_FIELD_NUMBER: _ClassVar[int]
    PROPOSED_SETUP_COMMANDS_FIELD_NUMBER: _ClassVar[int]
    title: str
    body: str
    evidence: _containers.RepeatedCompositeFieldContainer[CommandEvidence]
    proposed_setup_commands: str
    def __init__(self, title: _Optional[str] = ..., body: _Optional[str] = ..., evidence: _Optional[_Iterable[_Union[CommandEvidence, _Mapping]]] = ..., proposed_setup_commands: _Optional[str] = ...) -> None: ...

class SuggestionReport(_message.Message):
    __slots__ = ()
    TECH_DEBT_FIELD_NUMBER: _ClassVar[int]
    IMPROVEMENTS_FIELD_NUMBER: _ClassVar[int]
    SETUP_SCRIPT_FIELD_NUMBER: _ClassVar[int]
    tech_debt: _containers.RepeatedCompositeFieldContainer[Suggestion]
    improvements: _containers.RepeatedCompositeFieldContainer[Suggestion]
    setup_script: _containers.RepeatedCompositeFieldContainer[SetupScriptSuggestion]
    def __init__(self, tech_debt: _Optional[_Iterable[_Union[Suggestion, _Mapping]]] = ..., improvements: _Optional[_Iterable[_Union[Suggestion, _Mapping]]] = ..., setup_script: _Optional[_Iterable[_Union[SetupScriptSuggestion, _Mapping]]] = ...) -> None: ...
