"""End to end example of driving an Arsox satellite from Python.

Creates a thread with the full settings surface, runs a turn, consumes the
normalized event stream, answers the agent's questions, and collects artifacts.

Run with: python examples/example.py
"""

from __future__ import annotations

import asyncio
import logging
import os
import sys
from datetime import timedelta

from arsox import Satellite, Thread
from arsox.settings import (
    AgentsRepoSettings,
    BudgetSettings,
    CustomEnvVar,
    GithubSettings,
    HumanInTheLoopSettings,
    JiraSettings,
    McpServer,
    ModelEndpoint,
    ModelAuth,
    Money,
    PermissionSettings,
    PlanModeSettings,
    PrefetchSettings,
    PullRequestSettings,
    RedactionSettings,
    RepoAuth,
    RepoSettings,
    ResourceLimits,
    RetryPolicy,
    SelfReviewSettings,
    ServiceSettings,
    ServiceReadyWhen,
    StarCount,
    StreamSettings,
    SuggestionSettings,
    TeamModeSettings,
    ThreadSettings,
    TimeoutSettings,
    VirtualBrowserSettings,
    WatchPullRequestSettings,
    UNLIMITED,
)
from arsox.enums import (
    ExecAccess,
    Harness,
    MergeMethod,
    PrefetchInjection,
    RedactionMode,
    ServiceIsolation,
    StageDisposition,
    Viewport,
    WatchTrigger,
    WebAccess,
)
from arsox.events import ControlEvent, TurnEvent
from arsox.questions import QuestionAnswer

logger = logging.getLogger(__name__)

# Stand-in for your own issue tracker. `already_filed` holds the suggestion
# fingerprints you have seen before, which is what stops the same finding
# becoming a new ticket on every turn.
already_filed: set[str] = set()


async def open_issue(title: str, body: str) -> None:
    """Create a tracker issue from a suggestion."""
    logger.info(f"would file: {title}\n{body}")


# ####################### #
#        SETTINGS         #
# ####################### #


def build_settings() -> ThreadSettings:
    """Build the full thread settings for this run."""
    return ThreadSettings(
        # The satellite collects the workspace after this much inactivity. The
        # clock resets on every turn, so a thread working for three days is
        # never collected. Required, always, as the safety net against
        # forgotten workspaces.
        #
        # Every time span in the contract is a Duration, so the SDK takes a
        # timedelta and converts. No setting is ever a bare integer of unstated
        # units.
        idle_ttl=timedelta(minutes=120),
        delete_on_complete=False,
        # Required. UNLIMITED is accepted but has to be typed out, so an
        # unbounded spend is always a decision rather than an oversight. There
        # is no sentinel: 0 does not mean unlimited, it means zero.
        budget=BudgetSettings(
            max_tokens_per_turn=8_000_000,
            # Money on the wire, never a float. A single request can cost a
            # fraction of a cent, and accumulating those in a float is how a
            # ceiling drifts away from the invoice it was meant to predict.
            max_cost_per_thread=Money.usd(40),
            max_wall_clock_per_turn=UNLIMITED,
        ),
        harness=Harness.CLAUDE,
        # Your own agent configuration: CLAUDE.md, docs, custom skills. Cloned
        # once into .agents, then materialized into .claude and .codex. Pin
        # `ref` to a tag or a commit. A floating branch here changes agent
        # behavior between two threads you believed were identical.
        agents_repo=AgentsRepoSettings(
            url="git@github.com:navarrotech/agents.git",
            ref="v2.4.0",
            auth=RepoAuth(ssh_private_key=os.environ.get("AGENTS_DEPLOY_KEY")),
        ),
        # Ordered failover. The satellite walks this list top to bottom on
        # failure, so put the cheapest and most reliable endpoint first: moving
        # to the next endpoint discards the cached prompt prefix and the next
        # request pays full price for the whole history.
        models=[
            ModelEndpoint(
                name="primary-subscription",
                model="claude-opus-5[1m]",
                auth=ModelAuth(subscription_token=os.environ.get("ANTHROPIC_OAUTH_TOKEN")),
                retry=RetryPolicy(
                    max_attempts=10,
                    initial_backoff=timedelta(seconds=5),
                    max_backoff=timedelta(seconds=60),
                    retry_on_status=[429, 529],
                ),
            ),
            ModelEndpoint(
                name="fallback-api-key",
                model="claude-sonnet-5",
                auth=ModelAuth(api_key=os.environ.get("ANTHROPIC_API_KEY")),
                retry=RetryPolicy(max_attempts=3),
            ),
            ModelEndpoint(
                name="self-hosted-azure",
                model="claude-opus-5",
                base_url="https://arsox-models.openai.azure.com/anthropic/v1",
                auth=ModelAuth(api_key=os.environ.get("AZURE_ANTHROPIC_KEY")),
            ),
        ],
        team_mode=TeamModeSettings(
            enabled=True,
            max_members=6,
            # Added to the commander's suggestion list, not a fixed roster. The
            # commander still picks who it actually needs.
            suggested_roles=["Backend", "Frontend", "Unit test", "Doc writer"],
        ),
        plan_mode=PlanModeSettings(enabled=True, auto_approve=False),
        human_in_the_loop=HumanInTheLoopSettings(
            enabled=True,
            # Past this, the turn ends with the questions recorded in the report
            # rather than hanging forever.
            question_timeout=timedelta(minutes=30),
        ),
        self_review=SelfReviewSettings(enabled=True),
        suggestions=SuggestionSettings(
            enabled=True,
            # Any real codebase yields fifty findings. Fifty per turn is noise
            # that teaches you to ignore the feature, so the cap forces ranking.
            max_suggestions_per_category=5,
        ),
        # The single place merging is decided. The `gh` broker enforces it, and
        # the commander still chooses whether to merge even when permitted.
        pull_requests=PullRequestSettings(
            allow_agent_merge=False,
            allowed_merge_methods=[MergeMethod.SQUASH],
        ),
        # The satellite starts turns on its own here, which nothing else in
        # Arsox does. max_attempts is the control that matters: fix, fail, fix,
        # fail has no floor without it.
        watch_pull_requests=WatchPullRequestSettings(
            enabled=True,
            max_attempts=3,
            watch_window=timedelta(minutes=240),
            poll_interval=timedelta(seconds=20),
            # Only react to check runs on commits the satellite itself pushed.
            # ANY also reacts to human pushes, which is usually two parties
            # editing the same branch at cross purposes.
            react_to=WatchTrigger.SATELLITE_COMMITS,
        ),
        repos=[
            RepoSettings(
                name="api",
                url="git@github.com:JalapenoLabs/arsox-satellites.git",
                base_branch="develop",
                auth=RepoAuth(
                    ssh_private_key=os.environ.get("DEPLOY_KEY"),
                    ssh_public_key=os.environ.get("DEPLOY_KEY_PUB"),
                ),
                setup_commands="yarn install --immutable",
                # Semicolons are barriers, newlines run in parallel without
                # failing fast.
                checker=(
                    "yarn install;\n"
                    "yarn lint\n"
                    "yarn typecheck\n"
                    "yarn generate && yarn build\n"
                    "; yarn deploy --dry-run"
                ),
                # Started once per thread, not once per member, and lazily on
                # first use. Every member gets ARSOX_SERVICE_WEB_URL rather
                # than assuming a port, which is what stops three agents racing
                # to bind 3000.
                services=[
                    ServiceSettings(
                        name="web",
                        command="yarn dev",
                        port=3000,
                        ready_when=ServiceReadyWhen(
                            http_get="/health",
                            timeout=timedelta(seconds=120),
                        ),
                        isolation=ServiceIsolation.SHARED,
                    ),
                ],
            ),
        ],
        # Merge permission is not here. It lives in pull_requests above, so
        # exactly one setting decides whether a merge may happen.
        github=GithubSettings(token=os.environ.get("GITHUB_PAT")),
        jira=JiraSettings(
            token=os.environ.get("JIRA_PAT"),
            base_url="https://jalapenolabs.atlassian.net",
            # Atlassian Cloud authenticates with an email plus an API token.
            # Omit it for Data Center, which accepts the token alone.
            email="automation@jalapenolabs.io",
            allow_status_transitions=True,
            allow_comments=True,
        ),
        # Fetched deterministically before the turn, off the model's clock.
        # Jira comes back raw so custom fields survive; a PR brings its diff,
        # reviews, conversation, and check status. Attachments from the item
        # and from every comment land alongside it.
        prefetch=PrefetchSettings(
            jira=["BUG-123", "PLAT-456"],
            github=[11, 12, 13],
            # One line per item in AGENTS.md pointing at issues/<id>/. SUMMARY
            # inlines every rendered issue, which is the token cost this
            # feature exists to avoid. Let the agent read what it needs.
            injection=PrefetchInjection.INDEX,
        ),
        # is_secret defaults to True when omitted, because defaulting to secret
        # fails safe. Spelling it out here for clarity.
        env=[
            CustomEnvVar(key="DEPLOY_TARGET", value="staging", is_secret=False),
            CustomEnvVar(
                key="DATABASE_URL",
                value=os.environ.get("STAGING_DATABASE_URL"),
                is_secret=True,
            ),
        ],
        redaction=RedactionSettings(
            mode=RedactionMode.POSTFIX_SHOWN,
            # Six stars regardless of the secret's real length.
            # StarCount.mirror() instead mirrors the length, which leaks the
            # length and is why it is not the default. A case rather than a
            # magic -1, so -2 is unrepresentable.
            star_count=StarCount.fixed(6),
            # The kill switch. False unregisters the override_redaction tool
            # entirely, so no agent in this thread can reach it no matter what
            # it is told.
            allow_redaction_override=False,
        ),
        permissions=PermissionSettings(
            # `web` picks the base list. PRESET is the curated set the harnesses
            # already reach for (npmjs.org, pypi.org, crates.io, and friends).
            # CUSTOM starts from nothing.
            web=WebAccess.PRESET,
            # Always additive on top of `web`, so this never silently drops
            # the preset.
            additional_domains=["docs.anthropic.com", "jalapenolabs.atlassian.net"],
            # Symmetric with `web`. PRESET inherits the curated command list,
            # CUSTOM starts from nothing, NONE allows no commands at all, which
            # an empty list could never say on its own.
            exec=ExecAccess.PRESET,
            allowed_commands=["git", "yarn", "python3", "rg", "gh"],
            allow_git_push=True,
            protected_branches=["main", "develop"],
        ),
        # Written to /workspace/<thread-id>/AGENTS.md, below the Arsox header.
        # Advisory: it shapes behavior but never constrains it. Anything that
        # must hold belongs in `permissions` above.
        prompt=(
            "This repo is public and open source. The develop branch is the working branch.\n"
            "Never use em dashes in user-facing text.\n"
            "Update docs/ in the same change as the code."
        ),
        mcp_servers=[
            McpServer(
                name="internal-search",
                url="https://mcp.internal.jalapenolabs.io/sse",
                headers={"Authorization": f"Bearer {os.environ.get('INTERNAL_MCP_TOKEN')}"},
            ),
        ],
        # Headless Chrome for members that need to see what they built. Each
        # member gets its own browser context, not its own process, and points
        # at the ARSOX_SERVICE_* addresses above. Traffic still goes through
        # the egress proxy: navigating to a URL is a network request wearing
        # a hat.
        virtual_browser=VirtualBrowserSettings(
            enabled=True,
            allowed_roles=["Frontend", "QA"],
            viewports=[Viewport.MOBILE, Viewport.TABLET, Viewport.DESKTOP],
        ),
        # An agent with a shell can fill a disk. These are enforced, not
        # suggested.
        resource_limits=ResourceLimits(
            workspace_quota_bytes=10 * 1024**3,
            artifact_cap_bytes=100 * 1024**2,
        ),
        # Bounds on the operations that can otherwise hang forever. The turn
        # wall clock bound lives in `budget`, because exceeding it is a budget
        # outcome rather than a hung operation.
        timeouts=TimeoutSettings(
            exec_command=timedelta(minutes=30),
            llm_request=timedelta(minutes=10),
            harness_idle=timedelta(minutes=15),
        ),
        # Opt out of the noisy ones. Statistics are off by default because they
        # change on every token. Incidents are absent from this list and cannot
        # be switched off: a stream you can configure to hide failures is worse
        # than no stream.
        stream=StreamSettings(
            include_statistics=False,
            include_agent_thinking=True,
            include_tool_calls=True,
            include_team_chat=True,
            include_service_logs=True,
        ),
    )


# ####################### #
#        PREFLIGHT        #
# ####################### #


async def preflight(satellite: Satellite, harness: Harness) -> None:
    """Refuse a satellite this SDK cannot speak to, and report harness support.

    Both checks are cheap and both fail loudly here rather than three turns into
    a run. An SDK refuses a higher proto major outright rather than failing
    later with a confusing decode error; a higher minor warns once and proceeds,
    ignoring additive fields it does not know about.

    Args:
        satellite: the satellite to interrogate.
        harness: the harness this run intends to use.
    """
    version = await satellite.version()
    logger.info(
        f"satellite {version.satellite_version}, "
        f"proto v{version.proto_major}.{version.proto_minor}"
    )

    # Shapes are not the whole contract. A harness might have no plan mode and
    # no sub-agents, and discovering that by absence three turns in is exactly
    # what asking up front avoids.
    reported = await satellite.harness()
    logger.info(f"default harness: {reported.default_harness}")

    for capabilities in reported.harnesses:
        if capabilities.harness is not harness:
            continue
        if not capabilities.supports_native_plan_mode:
            logger.debug("harness has no native plan mode, Arsox will use its skill fallback")
        # Absent is not zero. A harness that reports no cache accounting leaves
        # the cache token fields as None rather than 0, so a cost reconciliation
        # can tell "not reported" from "read nothing from cache".
        if not capabilities.reports_cache_tokens:
            logger.debug("harness reports no cache accounting, cache token fields will be None")


async def on_control_event(event: ControlEvent) -> None:
    """Handle one event from the satellite's control stream.

    One socket per satellite, carrying lifecycle only: threads created and
    destroyed, queue depth, health transitions, budget warnings. It never
    carries thread content, which is why it is a separate message type rather
    than the thread stream with a filter applied.
    """
    match event.type:
        case "thread.state_changed":
            logger.debug(f"{event.thread_id}: {event.previous} -> {event.current}")

        case "health.changed":
            if event.ready:
                logger.info(f"satellite ready again ({event.check_name})")
            else:
                logger.error(f"satellite not ready: {event.check_name} {event.detail or ''}")

        case _:
            logger.debug(f"control event {event.type}")


# ############################# #
#   STYLE 1: THE EVENT EMITTER  #
# ############################# #
#
# Events belong to the thread, not to a turn: one socket per thread, carrying
# every turn that runs on it. Each event carries turn_id if you need to
# attribute it.
#
# Handlers fire concurrently and never block delivery, which is what makes this
# the right default. A slow handler (a human answering a question, a database
# write) holds up nothing behind it.


async def on_agent_message(event: TurnEvent) -> None:
    """Log an agent's message.

    `author` is a struct, not a display string: kind, member_id, role, and the
    owning member for a sub-agent. That is what lets a client group a stream by
    member without parsing names, which is the whole reason it is not a string.
    """
    logger.info(f"[{event.author.role or event.author.kind}] {event.text}")


async def on_tool_started(event: TurnEvent) -> None:
    """Log the start of a tool call."""
    logger.debug(f"[{event.author.role or event.author.kind}] {event.tool_name}")


async def on_member_spawned(event: TurnEvent) -> None:
    """Log a newly spawned team member."""
    logger.info(f"+ {event.role} ({event.member_id})")


async def on_team_chat(event: TurnEvent) -> None:
    """Log a message on the team channel."""
    logger.info(f"[team] {event.author.role or event.author.kind}: {event.text}")


async def on_integration_landed(event: TurnEvent) -> None:
    """Log a member branch merging cleanly into the integration branch."""
    logger.info(f"merged {event.member_id} into {event.branch}")


async def on_integration_conflict(event: TurnEvent) -> None:
    """Log a conflict handed back to the member that caused it."""
    logger.warning(f"conflict from {event.member_id}, returned for resolution")


async def on_checker_result(event: TurnEvent) -> None:
    """Log the exit code of a finished checker command."""
    logger.info(f"checker {event.result.command} exited {event.result.exit_code}")


async def on_service_started(event: TurnEvent) -> None:
    """Log a service the exec broker promoted without the thread declaring it.

    A member ran something long-lived that bound a port. Every later member
    running the same command gets this URL rather than a second process.
    """
    if event.auto_promoted:
        logger.info(f"auto-promoted {event.service_name} to a service at {event.url}")


async def on_budget_warning(event: TurnEvent) -> None:
    """Warn as a budget ceiling approaches."""
    logger.warning(f"budget at {event.percent_used}% of {event.ceiling}")


async def on_artifact_created(event: TurnEvent) -> None:
    """Log a newly produced artifact."""
    logger.info(f"artifact {event.artifact.path} ({event.artifact.size_bytes} bytes)")


async def on_redaction_overridden(event: TurnEvent) -> None:
    """Log an agent overriding redaction for one secret and one operation.

    High priority by design: the guardrail can move on an explicit human
    instruction, but nothing moves quietly.
    """
    logger.warning(
        f"redaction overridden for {event.secret_key} on {event.operation}: "
        f"{event.justification}"
    )


async def on_incident(event: TurnEvent) -> None:
    """Log a failure at any severity.

    This is the one event type that cannot be switched off. RECOVERED and
    BLOCKED are the ones worth watching: a failover that keeps working looks
    like success, and a permission denial the agent quietly routed around
    looks like nothing at all.
    """
    logger.warning(f"[{event.disposition}] {event.code}: {event.message}")


async def on_any_event(event: TurnEvent) -> None:
    """Mirror every event to a durable sink.

    Fires for every event that reaches the client, in sequence order, in
    addition to any typed handler. Both run.

    Its real job is forward compatibility: event types are additive within a
    proto major, so a newer satellite sends types this SDK version has no name
    for. A typed handler cannot subscribe to a type it has never heard of, and
    an unknown payload decodes to nothing. The envelope carries `type` as a
    plain string for exactly this reason, so an event can still be named,
    logged, and forwarded even when its body cannot be read.
    """
    logger.debug(f"{event.sequence} {event.type}")


def build_plan_handler(thread: Thread):
    """Build a plan handler bound to the given thread."""

    async def on_plan_proposed(event: TurnEvent) -> None:
        logger.info(event.plan.body)
        await thread.approve_plan(event.plan.plan_id)

    return on_plan_proposed


def build_question_handler(thread: Thread):
    """Build a question handler bound to the given thread."""

    async def on_question_asked(event: TurnEvent) -> None:
        # The whole set is answered in one call. Individual answers may be an
        # option, freeform text, or a decline, but partial submission is not a
        # thing: all of them go back together.
        answers: list[QuestionAnswer] = []
        for question in event.question_set.questions:
            recommended = next(
                (option for option in question.options if option.is_recommended),
                None,
            )
            if recommended:
                answers.append(
                    QuestionAnswer(
                        question_id=question.question_id,
                        option_id=recommended.option_id,
                    )
                )
            else:
                answers.append(
                    QuestionAnswer(
                        question_id=question.question_id,
                        text="Use your best judgement.",
                    )
                )
        await thread.answer_questions(event.question_set.question_set_id, answers)

    return on_question_asked


# ############################## #
#   STYLE 2: THE ASYNC ITERATOR  #
# ############################## #


async def consume_events(thread: Thread, from_sequence: int | None = None) -> None:
    """Pull events off the same socket with an async iterator and a match.

    The match keeps every case in one place, which reads better than scattered
    handlers when the dispatch itself is the interesting part. In exchange the
    loop body is serial: anything slow inside it stalls every event behind it,
    and the satellite eventually closes the socket with STREAM_CONSUMER_LAGGED.
    Reach for this when you actually want that backpressure, or when you are
    resuming and want to drive the replay yourself.

    Args:
        thread: the thread whose stream to consume.
        from_sequence: resume point, or None to start from the live edge.
    """
    async for event in thread.events(from_sequence=from_sequence):
        match event.type:
            case "agent.message":
                logger.info(f"[{event.author.role or event.author.kind}] {event.text}")

            case "tool.started":
                logger.debug(f"[{event.author.role or event.author.kind}] {event.tool_name}")

            case "team.member_spawned":
                logger.info(f"+ {event.role} ({event.member_id})")

            case "team.chat":
                logger.info(f"[team] {event.author.role or event.author.kind}: {event.text}")

            case "integration.landed":
                logger.info(f"merged {event.member_id} into {event.branch}")

            case "integration.conflict":
                logger.warning(f"conflict from {event.member_id}, returned for resolution")

            case "checker.result":
                logger.info(f"checker {event.result.command} exited {event.result.exit_code}")

            case "budget.warning":
                logger.warning(f"budget at {event.percent_used}% of {event.ceiling}")

            case "plan.proposed":
                logger.info(event.plan.body)
                await thread.approve_plan(event.plan.plan_id)

            case "question.asked":
                await build_question_handler(thread)(event)

            case "artifact.created":
                logger.info(
                    f"artifact {event.artifact.path} ({event.artifact.size_bytes} bytes)"
                )

            case "incident":
                await on_incident(event)

            case "turn.completed":
                logger.info(f"turn finished: {event.result.status}")

            # Codes and event types are additive within a proto major, so a
            # newer satellite can send something this SDK version has never
            # heard of. Log it, never raise on it.
            case _:
                logger.debug(f"unhandled event type {event.type}: {event}")


async def resume(satellite: Satellite, thread_id: str, last_seen_sequence: int) -> None:
    """Pick a thread back up from a different process.

    A thread lives entirely on the satellite, so any process holding the URL,
    the secret, and the thread ID can attach. This is how a horizontally scaled
    host application survives a replica dying mid-turn: persist the thread ID
    and the last sequence you saw, and whichever replica comes up next resumes
    from there without losing an event.
    """
    attached = await satellite.threads.attach(thread_id)
    await consume_events(attached.thread, from_sequence=last_seen_sequence)


# ####################### #
#        EXECUTION        #
# ####################### #


async def main() -> None:
    """Run one turn on a fresh thread and collect its artifacts."""
    # Environment variables cross a runtime boundary, so they get a real check.
    arsox_secret = os.environ.get("ARSOX_SECRET")
    if not arsox_secret:
        logger.error("ARSOX_SECRET is not set, refusing to start")
        sys.exit(1)

    settings = build_settings()

    async with Satellite(
        url="https://satellite-01.internal.jalapenolabs.io",
        secret=arsox_secret,
    ) as satellite:
        await preflight(satellite, settings.harness)
        satellite.on("all", on_control_event)

        # Create returns a response object so the shape can grow without
        # breaking callers.
        #
        # The idempotency key is what makes a timed-out create safe to retry.
        # Without it, a response lost in transit is indistinguishable from a
        # thread that was never created, and the only safe move is to retry and
        # leak a whole workspace. `deduplicated` tells you which happened.
        created = await satellite.threads.create(
            settings,
            idempotency_key="rate-limiting-2026-08-04",
        )
        thread = created.thread
        logger.info(
            f"Thread {thread.id} {'reused' if created.deduplicated else 'created'}"
        )

        # Events belong to the thread, not to a turn: one socket per thread,
        # carrying every turn that runs on it. Each event carries turn_id if you
        # need to attribute it. Handlers fire without blocking the stream.
        thread.on("agent.message", on_agent_message)
        thread.on("tool.started", on_tool_started)
        thread.on("team.member_spawned", on_member_spawned)
        thread.on("team.chat", on_team_chat)
        thread.on("integration.landed", on_integration_landed)
        thread.on("integration.conflict", on_integration_conflict)
        thread.on("checker.result", on_checker_result)
        thread.on("service.started", on_service_started)
        thread.on("budget.warning", on_budget_warning)
        thread.on("artifact.created", on_artifact_created)
        thread.on("redaction.overridden", on_redaction_overridden)
        thread.on("incident", on_incident)
        thread.on("all", on_any_event)

        # Plans and questions answer back over HTTP, not up the socket, which is
        # unidirectional. Both live on the thread because only one plan and one
        # question set can ever be outstanding at a time.
        thread.on("plan.proposed", build_plan_handler(thread))
        thread.on("question.asked", build_question_handler(thread))

        started = await thread.start_turn(
            prompt="Add per-endpoint rate limiting to the public API and open a PR against develop.",
            idempotency_key="rate-limiting-2026-08-04-turn-1",
        )
        turn = started.turn

        # Resolves when this turn reaches a terminal state. Handlers above keep
        # firing the whole time.
        result = await turn.result()
        logger.info(result.summary)

        # Cost comes back as an estimate, not a number. `amount` is None when no
        # endpoint published pricing for its model, and is_partial means some
        # requests could be priced and others could not. A confident zero would
        # be a lie in both cases.
        if result.cost.amount is None:
            logger.info(f"{result.tokens.total_tokens} tokens, cost not priced")
        else:
            dollars = result.cost.amount.units + result.cost.amount.nanos / 1_000_000_000
            qualifier = " (partial, some requests unpriced)" if result.cost.is_partial else ""
            logger.info(f"{result.tokens.total_tokens} tokens, ${dollars:.2f}{qualifier}")

        # Every stage of the stack reports what it did, including the ones that
        # did nothing. This is what stops "the budget ran out before self-review"
        # reading as "self-review found nothing".
        for stage in result.stages:
            if stage.disposition is StageDisposition.SKIPPED:
                logger.warning(f"stage {stage.stage} skipped: {stage.reason}")

        # Counts by disposition ride along on the report, so the common case
        # needs no query at all. Query when you want the detail.
        logger.info(result.incident_counts)

        problems = await thread.incidents.list(
            dispositions=["fatal", "degraded"],
            turn_ids=[turn.id],
        )
        for incident in problems:
            logger.warning(
                f"{incident.code} ({incident.disposition}): {incident.message}"
            )

        # Questions nobody answered before the timeout. The turn ended with them
        # recorded here rather than hanging forever.
        for question_set in result.unanswered_questions:
            logger.warning(f"{len(question_set.questions)} questions went unanswered")

        # fingerprint is the field that makes this an issue pipeline rather
        # than a report. Suppress the ones you have already filed or you will
        # open the same ticket again on every turn.
        for suggestion in result.suggestions.tech_debt:
            if suggestion.fingerprint in already_filed:
                continue
            await open_issue(suggestion.title, suggestion.body)
            already_filed.add(suggestion.fingerprint)

        # The agent's proposed setup scripts are inert data. Arsox never adopts
        # them, never writes them, never runs them. Adopting one is this line,
        # and it is yours: commands that will execute on a later satellite are a
        # permission decision, and permission decisions are never the agent's to
        # make.
        for setup in result.suggestions.setup_script:
            logger.info(f"setup gap: {setup.title}")
            for evidence in setup.evidence:
                logger.info(f"  {evidence.command} exited {evidence.exit_code}")
            if setup.proposed_setup_commands:
                logger.info(f"  proposed:\n{setup.proposed_setup_commands}")

        for artifact in await thread.artifacts.list():
            await thread.artifacts.download(artifact.path, f"./out/{artifact.name}")

        # Or leave it to expire through the idle TTL. Incidents survive either
        # way, on their own retention, because "why did last night go wrong" is
        # asked after the workspace is gone.
        await thread.destroy()


if __name__ == "__main__":
    logging.basicConfig(level=logging.INFO, format="%(message)s")
    asyncio.run(main())
