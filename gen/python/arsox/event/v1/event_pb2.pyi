from arsox.common.v1 import common_pb2 as _common_pb2
from arsox.event.v1 import agent_pb2 as _agent_pb2
from arsox.event.v1 import integration_pb2 as _integration_pb2
from arsox.event.v1 import lifecycle_pb2 as _lifecycle_pb2
from arsox.event.v1 import service_pb2 as _service_pb2
from arsox.event.v1 import team_pb2 as _team_pb2
from arsox.incident.v1 import incident_pb2 as _incident_pb2
from google.protobuf import descriptor as _descriptor
from google.protobuf import message as _message
from collections.abc import Mapping as _Mapping
from typing import ClassVar as _ClassVar, Optional as _Optional, Union as _Union

DESCRIPTOR: _descriptor.FileDescriptor

class ThreadEvent(_message.Message):
    __slots__ = ()
    SEQUENCE_FIELD_NUMBER: _ClassVar[int]
    THREAD_ID_FIELD_NUMBER: _ClassVar[int]
    TURN_ID_FIELD_NUMBER: _ClassVar[int]
    OCCURRED_AT_FIELD_NUMBER: _ClassVar[int]
    TYPE_FIELD_NUMBER: _ClassVar[int]
    MEMBER_ID_FIELD_NUMBER: _ClassVar[int]
    AGENT_MESSAGE_FIELD_NUMBER: _ClassVar[int]
    AGENT_THINKING_FIELD_NUMBER: _ClassVar[int]
    TOOL_STARTED_FIELD_NUMBER: _ClassVar[int]
    TOOL_COMPLETED_FIELD_NUMBER: _ClassVar[int]
    TEAM_MEMBER_SPAWNED_FIELD_NUMBER: _ClassVar[int]
    TEAM_MEMBER_DESPAWNED_FIELD_NUMBER: _ClassVar[int]
    TEAM_CHAT_FIELD_NUMBER: _ClassVar[int]
    TEAM_DIRECT_MESSAGE_FIELD_NUMBER: _ClassVar[int]
    INTEGRATION_REQUESTED_FIELD_NUMBER: _ClassVar[int]
    INTEGRATION_LANDED_FIELD_NUMBER: _ClassVar[int]
    INTEGRATION_CONFLICT_FIELD_NUMBER: _ClassVar[int]
    CHECKER_RESULT_FIELD_NUMBER: _ClassVar[int]
    SERVICE_STARTED_FIELD_NUMBER: _ClassVar[int]
    SERVICE_LOG_FIELD_NUMBER: _ClassVar[int]
    BUDGET_WARNING_FIELD_NUMBER: _ClassVar[int]
    PLAN_PROPOSED_FIELD_NUMBER: _ClassVar[int]
    PLAN_DECIDED_FIELD_NUMBER: _ClassVar[int]
    QUESTION_ASKED_FIELD_NUMBER: _ClassVar[int]
    QUESTION_ANSWERED_FIELD_NUMBER: _ClassVar[int]
    ARTIFACT_CREATED_FIELD_NUMBER: _ClassVar[int]
    REDACTION_OVERRIDDEN_FIELD_NUMBER: _ClassVar[int]
    TURN_STARTED_FIELD_NUMBER: _ClassVar[int]
    TURN_COMPLETED_FIELD_NUMBER: _ClassVar[int]
    STATISTICS_UPDATED_FIELD_NUMBER: _ClassVar[int]
    RATE_LIMIT_REPORTED_FIELD_NUMBER: _ClassVar[int]
    INCIDENT_FIELD_NUMBER: _ClassVar[int]
    sequence: int
    thread_id: str
    turn_id: str
    occurred_at: _common_pb2.Timestamp
    type: str
    member_id: str
    agent_message: _agent_pb2.AgentMessage
    agent_thinking: _agent_pb2.AgentThinking
    tool_started: _agent_pb2.ToolStarted
    tool_completed: _agent_pb2.ToolCompleted
    team_member_spawned: _team_pb2.TeamMemberSpawned
    team_member_despawned: _team_pb2.TeamMemberDespawned
    team_chat: _team_pb2.TeamChat
    team_direct_message: _team_pb2.TeamDirectMessage
    integration_requested: _integration_pb2.IntegrationRequested
    integration_landed: _integration_pb2.IntegrationLanded
    integration_conflict: _integration_pb2.IntegrationConflict
    checker_result: _integration_pb2.CheckerResultEvent
    service_started: _service_pb2.ServiceStarted
    service_log: _service_pb2.ServiceLog
    budget_warning: _lifecycle_pb2.BudgetWarning
    plan_proposed: _lifecycle_pb2.PlanProposed
    plan_decided: _lifecycle_pb2.PlanDecided
    question_asked: _lifecycle_pb2.QuestionAsked
    question_answered: _lifecycle_pb2.QuestionAnswered
    artifact_created: _lifecycle_pb2.ArtifactCreated
    redaction_overridden: _lifecycle_pb2.RedactionOverridden
    turn_started: _lifecycle_pb2.TurnStarted
    turn_completed: _lifecycle_pb2.TurnCompleted
    statistics_updated: _lifecycle_pb2.StatisticsUpdated
    rate_limit_reported: _lifecycle_pb2.RateLimitReported
    incident: _incident_pb2.Incident
    def __init__(self, sequence: _Optional[int] = ..., thread_id: _Optional[str] = ..., turn_id: _Optional[str] = ..., occurred_at: _Optional[_Union[_common_pb2.Timestamp, _Mapping]] = ..., type: _Optional[str] = ..., member_id: _Optional[str] = ..., agent_message: _Optional[_Union[_agent_pb2.AgentMessage, _Mapping]] = ..., agent_thinking: _Optional[_Union[_agent_pb2.AgentThinking, _Mapping]] = ..., tool_started: _Optional[_Union[_agent_pb2.ToolStarted, _Mapping]] = ..., tool_completed: _Optional[_Union[_agent_pb2.ToolCompleted, _Mapping]] = ..., team_member_spawned: _Optional[_Union[_team_pb2.TeamMemberSpawned, _Mapping]] = ..., team_member_despawned: _Optional[_Union[_team_pb2.TeamMemberDespawned, _Mapping]] = ..., team_chat: _Optional[_Union[_team_pb2.TeamChat, _Mapping]] = ..., team_direct_message: _Optional[_Union[_team_pb2.TeamDirectMessage, _Mapping]] = ..., integration_requested: _Optional[_Union[_integration_pb2.IntegrationRequested, _Mapping]] = ..., integration_landed: _Optional[_Union[_integration_pb2.IntegrationLanded, _Mapping]] = ..., integration_conflict: _Optional[_Union[_integration_pb2.IntegrationConflict, _Mapping]] = ..., checker_result: _Optional[_Union[_integration_pb2.CheckerResultEvent, _Mapping]] = ..., service_started: _Optional[_Union[_service_pb2.ServiceStarted, _Mapping]] = ..., service_log: _Optional[_Union[_service_pb2.ServiceLog, _Mapping]] = ..., budget_warning: _Optional[_Union[_lifecycle_pb2.BudgetWarning, _Mapping]] = ..., plan_proposed: _Optional[_Union[_lifecycle_pb2.PlanProposed, _Mapping]] = ..., plan_decided: _Optional[_Union[_lifecycle_pb2.PlanDecided, _Mapping]] = ..., question_asked: _Optional[_Union[_lifecycle_pb2.QuestionAsked, _Mapping]] = ..., question_answered: _Optional[_Union[_lifecycle_pb2.QuestionAnswered, _Mapping]] = ..., artifact_created: _Optional[_Union[_lifecycle_pb2.ArtifactCreated, _Mapping]] = ..., redaction_overridden: _Optional[_Union[_lifecycle_pb2.RedactionOverridden, _Mapping]] = ..., turn_started: _Optional[_Union[_lifecycle_pb2.TurnStarted, _Mapping]] = ..., turn_completed: _Optional[_Union[_lifecycle_pb2.TurnCompleted, _Mapping]] = ..., statistics_updated: _Optional[_Union[_lifecycle_pb2.StatisticsUpdated, _Mapping]] = ..., rate_limit_reported: _Optional[_Union[_lifecycle_pb2.RateLimitReported, _Mapping]] = ..., incident: _Optional[_Union[_incident_pb2.Incident, _Mapping]] = ...) -> None: ...
