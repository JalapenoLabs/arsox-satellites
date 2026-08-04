from arsox.common.v1 import common_pb2 as _common_pb2
from arsox.harness.v1 import harness_pb2 as _harness_pb2
from arsox.settings.v1 import budget_pb2 as _budget_pb2
from arsox.settings.v1 import integration_pb2 as _integration_pb2
from arsox.settings.v1 import limits_pb2 as _limits_pb2
from arsox.settings.v1 import model_pb2 as _model_pb2
from arsox.settings.v1 import permission_pb2 as _permission_pb2
from arsox.settings.v1 import prefetch_pb2 as _prefetch_pb2
from arsox.settings.v1 import pull_request_pb2 as _pull_request_pb2
from arsox.settings.v1 import repo_pb2 as _repo_pb2
from arsox.settings.v1 import secret_pb2 as _secret_pb2
from arsox.settings.v1 import stages_pb2 as _stages_pb2
from arsox.settings.v1 import team_pb2 as _team_pb2
from arsox.settings.v1 import tool_pb2 as _tool_pb2
from google.protobuf.internal import containers as _containers
from google.protobuf import descriptor as _descriptor
from google.protobuf import message as _message
from collections.abc import Iterable as _Iterable, Mapping as _Mapping
from typing import ClassVar as _ClassVar, Optional as _Optional, Union as _Union

DESCRIPTOR: _descriptor.FileDescriptor

class ThreadSettings(_message.Message):
    __slots__ = ()
    IDLE_TTL_FIELD_NUMBER: _ClassVar[int]
    DELETE_ON_COMPLETE_FIELD_NUMBER: _ClassVar[int]
    BUDGET_FIELD_NUMBER: _ClassVar[int]
    HARNESS_FIELD_NUMBER: _ClassVar[int]
    AGENTS_REPO_FIELD_NUMBER: _ClassVar[int]
    MODELS_FIELD_NUMBER: _ClassVar[int]
    TEAM_MODE_FIELD_NUMBER: _ClassVar[int]
    PLAN_MODE_FIELD_NUMBER: _ClassVar[int]
    HUMAN_IN_THE_LOOP_FIELD_NUMBER: _ClassVar[int]
    SELF_REVIEW_FIELD_NUMBER: _ClassVar[int]
    SUGGESTIONS_FIELD_NUMBER: _ClassVar[int]
    PULL_REQUESTS_FIELD_NUMBER: _ClassVar[int]
    WATCH_PULL_REQUESTS_FIELD_NUMBER: _ClassVar[int]
    REPOS_FIELD_NUMBER: _ClassVar[int]
    GITHUB_FIELD_NUMBER: _ClassVar[int]
    JIRA_FIELD_NUMBER: _ClassVar[int]
    PREFETCH_FIELD_NUMBER: _ClassVar[int]
    ENV_FIELD_NUMBER: _ClassVar[int]
    REDACTION_FIELD_NUMBER: _ClassVar[int]
    PERMISSIONS_FIELD_NUMBER: _ClassVar[int]
    PROMPT_FIELD_NUMBER: _ClassVar[int]
    MCP_SERVERS_FIELD_NUMBER: _ClassVar[int]
    VIRTUAL_BROWSER_FIELD_NUMBER: _ClassVar[int]
    STREAM_FIELD_NUMBER: _ClassVar[int]
    RESOURCE_LIMITS_FIELD_NUMBER: _ClassVar[int]
    TIMEOUTS_FIELD_NUMBER: _ClassVar[int]
    RESUME_INTERRUPTED_TURNS_FIELD_NUMBER: _ClassVar[int]
    idle_ttl: _common_pb2.Duration
    delete_on_complete: bool
    budget: _budget_pb2.Budget
    harness: _harness_pb2.Harness
    agents_repo: _repo_pb2.AgentsRepo
    models: _containers.RepeatedCompositeFieldContainer[_model_pb2.ModelEndpoint]
    team_mode: _team_pb2.TeamMode
    plan_mode: _stages_pb2.PlanMode
    human_in_the_loop: _stages_pb2.HumanInTheLoop
    self_review: _stages_pb2.SelfReview
    suggestions: _stages_pb2.SuggestionSettings
    pull_requests: _pull_request_pb2.PullRequestPolicy
    watch_pull_requests: _pull_request_pb2.WatchPullRequests
    repos: _containers.RepeatedCompositeFieldContainer[_repo_pb2.Repo]
    github: _integration_pb2.GithubIntegration
    jira: _integration_pb2.JiraIntegration
    prefetch: _prefetch_pb2.Prefetch
    env: _containers.RepeatedCompositeFieldContainer[_secret_pb2.EnvVar]
    redaction: _secret_pb2.Redaction
    permissions: _permission_pb2.Permissions
    prompt: str
    mcp_servers: _containers.RepeatedCompositeFieldContainer[_tool_pb2.McpServer]
    virtual_browser: _tool_pb2.VirtualBrowser
    stream: _limits_pb2.StreamSettings
    resource_limits: _limits_pb2.ResourceLimits
    timeouts: _limits_pb2.Timeouts
    resume_interrupted_turns: bool
    def __init__(self, idle_ttl: _Optional[_Union[_common_pb2.Duration, _Mapping]] = ..., delete_on_complete: _Optional[bool] = ..., budget: _Optional[_Union[_budget_pb2.Budget, _Mapping]] = ..., harness: _Optional[_Union[_harness_pb2.Harness, str]] = ..., agents_repo: _Optional[_Union[_repo_pb2.AgentsRepo, _Mapping]] = ..., models: _Optional[_Iterable[_Union[_model_pb2.ModelEndpoint, _Mapping]]] = ..., team_mode: _Optional[_Union[_team_pb2.TeamMode, _Mapping]] = ..., plan_mode: _Optional[_Union[_stages_pb2.PlanMode, _Mapping]] = ..., human_in_the_loop: _Optional[_Union[_stages_pb2.HumanInTheLoop, _Mapping]] = ..., self_review: _Optional[_Union[_stages_pb2.SelfReview, _Mapping]] = ..., suggestions: _Optional[_Union[_stages_pb2.SuggestionSettings, _Mapping]] = ..., pull_requests: _Optional[_Union[_pull_request_pb2.PullRequestPolicy, _Mapping]] = ..., watch_pull_requests: _Optional[_Union[_pull_request_pb2.WatchPullRequests, _Mapping]] = ..., repos: _Optional[_Iterable[_Union[_repo_pb2.Repo, _Mapping]]] = ..., github: _Optional[_Union[_integration_pb2.GithubIntegration, _Mapping]] = ..., jira: _Optional[_Union[_integration_pb2.JiraIntegration, _Mapping]] = ..., prefetch: _Optional[_Union[_prefetch_pb2.Prefetch, _Mapping]] = ..., env: _Optional[_Iterable[_Union[_secret_pb2.EnvVar, _Mapping]]] = ..., redaction: _Optional[_Union[_secret_pb2.Redaction, _Mapping]] = ..., permissions: _Optional[_Union[_permission_pb2.Permissions, _Mapping]] = ..., prompt: _Optional[str] = ..., mcp_servers: _Optional[_Iterable[_Union[_tool_pb2.McpServer, _Mapping]]] = ..., virtual_browser: _Optional[_Union[_tool_pb2.VirtualBrowser, _Mapping]] = ..., stream: _Optional[_Union[_limits_pb2.StreamSettings, _Mapping]] = ..., resource_limits: _Optional[_Union[_limits_pb2.ResourceLimits, _Mapping]] = ..., timeouts: _Optional[_Union[_limits_pb2.Timeouts, _Mapping]] = ..., resume_interrupted_turns: _Optional[bool] = ...) -> None: ...
