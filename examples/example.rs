// Copyright © 2026 Jalapeno Labs

//! End to end example of driving an Arsox satellite from Rust.
//!
//! Creates a thread with the full settings surface, runs a turn, consumes the
//! normalized event stream, answers the agent's questions, and collects artifacts.
//!
//! Run with: `cargo run --example example`

use std::collections::HashSet;
use std::time::Duration;

use anyhow::{Context, Result};
use futures::StreamExt;

use arsox::enums::{
    Disposition, ExecAccess, Harness, MergeMethod, PrefetchInjection, RedactionMode,
    ServiceIsolation, StageDisposition, Viewport, WatchTrigger, WebAccess,
};
use arsox::events::{ControlEvent, TurnEvent};
use arsox::questions::QuestionAnswer;
use arsox::settings::{
    AgentsRepo, Budget, Ceiling, CustomEnvVar, GithubSettings, HumanInTheLoop, JiraSettings,
    McpServer, ModelAuth, ModelEndpoint, Money, Permissions, PlanMode, Prefetch, PullRequests,
    Redaction, RepoAuth, RepoSettings, ResourceLimits, RetryPolicy, SelfReview, Service,
    ServiceReadyWhen, StarCount, StreamSettings, Suggestions, TeamMode, ThreadDeps, ThreadSettings,
    Timeouts, VirtualBrowser, WatchPullRequests,
};
use arsox::incidents::IncidentQuery;
use arsox::{Satellite, Thread, ThreadAttached, ThreadCreated, TurnStarted};

/// Creates a tracker issue from a suggestion.
///
/// Stand-in for your own issue tracker integration.
async fn open_issue(title: &str, body: &str) -> Result<()> {
    tracing::info!("would file: {title}\n{body}");
    Ok(())
}

/// How long a thread may sit untouched before the satellite collects it.
///
/// This is an idle TTL, not a wall clock. It resets on every turn, so a thread
/// actively working for three days is never collected, while the same thread
/// sitting idle past this window is. Required, always, as the safety net
/// against forgotten workspaces filling the disk.
const IDLE_TTL: Duration = Duration::from_secs(120 * 60);

/// Builds the full thread settings for this run.
fn build_settings() -> Result<ThreadSettings> {
    // Required. `Unlimited` is accepted but has to be typed out, so an
    // unbounded spend is always a decision rather than an oversight. There is
    // no sentinel: 0 does not mean unlimited, it means zero.
    let budget = Budget::builder()
        .max_tokens_per_turn(8_000_000)
        // Money on the wire, never a float. A single request can cost a
        // fraction of a cent, and accumulating those in a float is how a
        // ceiling drifts away from the invoice it was meant to predict.
        .max_cost_per_thread(Money::usd(40))
        .max_wall_clock_per_turn(Ceiling::Unlimited)
        .build();

    // Required parameters are passed when the builder is created, optional ones
    // are chained. `ThreadSettings::builder((IDLE_TTL, budget))` also works.
    let settings = ThreadSettings::builder(ThreadDeps {
        idle_ttl: IDLE_TTL,
        budget,
    })
    .delete_on_complete(false)
    .harness(Harness::Claude)
    // Your own agent configuration: CLAUDE.md, docs, custom skills. Cloned once
    // into .agents, then materialized into .claude and .codex. Pin `ref` to a
    // tag or a commit. A floating branch here changes agent behavior between
    // two threads you believed were identical.
    .agents_repo(
        AgentsRepo::builder("git@github.com:navarrotech/agents.git")
            .git_ref("v2.4.0")
            .auth(RepoAuth::ssh_private_key(std::env::var("AGENTS_DEPLOY_KEY")?))
            .build(),
    )
    // Ordered failover. The satellite walks this list top to bottom on failure,
    // so put the cheapest and most reliable endpoint first: moving to the next
    // endpoint discards the cached prompt prefix and the next request pays full
    // price for the whole history.
    .models([
        ModelEndpoint::builder("primary-subscription", "claude-opus-5[1m]")
            .auth(ModelAuth::subscription_token(std::env::var("ANTHROPIC_OAUTH_TOKEN")?))
            .retry(
                RetryPolicy::builder()
                    .max_attempts(10)
                    .initial_backoff(Duration::from_secs(5))
                    .max_backoff(Duration::from_secs(60))
                    .retry_on_status([429, 529])
                    .build(),
            )
            .build(),
        ModelEndpoint::builder("fallback-api-key", "claude-sonnet-5")
            .auth(ModelAuth::api_key(std::env::var("ANTHROPIC_API_KEY")?))
            .retry(RetryPolicy::builder().max_attempts(3).build())
            .build(),
        ModelEndpoint::builder("self-hosted-azure", "claude-opus-5")
            .base_url("https://arsox-models.openai.azure.com/anthropic/v1")
            .auth(ModelAuth::api_key(std::env::var("AZURE_ANTHROPIC_KEY")?))
            .build(),
    ])
    .team_mode(
        TeamMode::builder()
            .enabled(true)
            .max_members(6)
            // Added to the commander's suggestion list, not a fixed roster. The
            // commander still picks who it actually needs.
            .suggested_roles(["Backend", "Frontend", "Unit test", "Doc writer"])
            .build(),
    )
    .plan_mode(PlanMode::builder().enabled(true).auto_approve(false).build())
    .human_in_the_loop(
        HumanInTheLoop::builder()
            .enabled(true)
            // Past this, the turn ends with the questions recorded in the
            // report rather than hanging forever.
            .question_timeout(Duration::from_secs(30 * 60))
            .build(),
    )
    .self_review(SelfReview::builder().enabled(true).build())
    .suggestions(
        Suggestions::builder()
            .enabled(true)
            // Any real codebase yields fifty findings. Fifty per turn is noise
            // that teaches you to ignore the feature, so the cap forces ranking.
            .max_suggestions_per_category(5)
            .build(),
    )
    // The single place merging is decided. The `gh` broker enforces it, and the
    // commander still chooses whether to merge even when permitted.
    .pull_requests(
        PullRequests::builder()
            .allow_agent_merge(false)
            .allowed_merge_methods([MergeMethod::Squash])
            .build(),
    )
    // The satellite starts turns on its own here, which nothing else in Arsox
    // does. `max_attempts` is the control that matters: fix, fail, fix, fail has
    // no floor without it.
    .watch_pull_requests(
        WatchPullRequests::builder()
            .enabled(true)
            .max_attempts(3)
            .watch_window(Duration::from_secs(240 * 60))
            .poll_interval(Duration::from_secs(20))
            // Only react to check runs on commits the satellite itself pushed.
            // `Any` also reacts to human pushes, which is usually two parties
            // editing the same branch at cross purposes.
            .react_to(WatchTrigger::SatelliteCommits)
            .build(),
    )
    .repos([RepoSettings::builder("api", "git@github.com:JalapenoLabs/arsox-satellites.git")
        .base_branch("develop")
        .auth(RepoAuth::ssh_key(
            std::env::var("DEPLOY_KEY")?,
            std::env::var("DEPLOY_KEY_PUB")?,
        ))
        .setup_commands("yarn install --immutable")
        // Semicolons are barriers, newlines run in parallel without failing fast.
        .checker(
            "yarn install;\n\
             yarn lint\n\
             yarn typecheck\n\
             yarn generate && yarn build\n\
             ; yarn deploy --dry-run",
        )
        // Started once per thread, not once per member, and lazily on first
        // use. Every member gets ARSOX_SERVICE_WEB_URL rather than assuming a
        // port, which is what stops three agents racing to bind 3000.
        .services([Service::builder("web", "yarn dev")
            .port(3000)
            .ready_when(
                ServiceReadyWhen::builder()
                    .http_get("/health")
                    .timeout(Duration::from_secs(120))
                    .build(),
            )
            .isolation(ServiceIsolation::Shared)
            .build()])
        .build()])
    // Merge permission is not here. It lives in `pull_requests` above, so
    // exactly one setting decides whether a merge may happen.
    .github(GithubSettings::builder(std::env::var("GITHUB_PAT")?).build())
    .jira(
        JiraSettings::builder(std::env::var("JIRA_PAT")?)
            .base_url("https://jalapenolabs.atlassian.net")
            // Atlassian Cloud authenticates with an email plus an API token.
            // Omit it for Data Center, which accepts the token alone.
            .email("automation@jalapenolabs.io")
            .allow_status_transitions(true)
            .allow_comments(true)
            .build(),
    )
    // Fetched deterministically before the turn, off the model's clock. Jira
    // comes back raw so custom fields survive; a PR brings its diff, reviews,
    // conversation, and check status. Attachments from the item and from every
    // comment land alongside it.
    .prefetch(
        Prefetch::builder()
            .jira(["BUG-123", "PLAT-456"])
            .github([11, 12, 13])
            // One line per item in AGENTS.md pointing at issues/<id>/. `Summary`
            // inlines every rendered issue, which is the token cost this feature
            // exists to avoid. Let the agent read what it needs.
            .injection(PrefetchInjection::Index)
            .build(),
    )
    // `is_secret` defaults to true, because defaulting to secret fails safe.
    // `CustomEnvVar::public` is the explicit opt out.
    .env([
        CustomEnvVar::public("DEPLOY_TARGET", "staging"),
        CustomEnvVar::secret("DATABASE_URL", std::env::var("STAGING_DATABASE_URL")?),
    ])
    .redaction(
        Redaction::builder()
            .mode(RedactionMode::PostfixShown)
            // Six stars regardless of the secret's real length.
            // `StarCount::mirror()` instead mirrors the length, which leaks the
            // length and is why it is not the default. A case rather than a
            // magic -1, so -2 is unrepresentable.
            .star_count(StarCount::fixed(6))
            // The kill switch. False unregisters the override_redaction tool
            // entirely, so no agent in this thread can reach it no matter what
            // it is told.
            .allow_redaction_override(false)
            .build(),
    )
    .permissions(
        Permissions::builder()
            // `web` picks the base list. Preset is the curated set the harnesses
            // already reach for (npmjs.org, pypi.org, crates.io, and friends).
            // Custom starts from nothing.
            .web(WebAccess::Preset)
            // Always additive on top of `web`, so this never silently drops the preset.
            .additional_domains(["docs.anthropic.com", "jalapenolabs.atlassian.net"])
            // Symmetric with `web`. Preset inherits the curated command list,
            // Custom starts from nothing, None allows no commands at all, which
            // an empty list could never say on its own.
            .exec(ExecAccess::Preset)
            .allowed_commands(["git", "cargo", "rg", "gh"])
            .allow_git_push(true)
            .protected_branches(["main", "develop"])
            .build(),
    )
    // Written to /workspace/<thread-id>/AGENTS.md, below the Arsox header.
    // Advisory: it shapes behavior but never constrains it. Anything that must
    // hold belongs in `permissions` above.
    .prompt(
        "This repo is public and open source. The develop branch is the working branch.\n\
         Never use em dashes in user-facing text.\n\
         Update docs/ in the same change as the code.",
    )
    .mcp_servers([McpServer::builder("internal-search", "https://mcp.internal.jalapenolabs.io/sse")
        .header(
            "Authorization",
            format!("Bearer {}", std::env::var("INTERNAL_MCP_TOKEN")?),
        )
        .build()])
    // Headless Chrome for members that need to see what they built. Each member
    // gets its own browser context, not its own process, and points at the
    // ARSOX_SERVICE_* addresses above. Traffic still goes through the egress
    // proxy: navigating to a URL is a network request wearing a hat.
    .virtual_browser(
        VirtualBrowser::builder()
            .enabled(true)
            .allowed_roles(["Frontend", "QA"])
            .viewports([Viewport::Mobile, Viewport::Tablet, Viewport::Desktop])
            .build(),
    )
    // An agent with a shell can fill a disk. These are enforced, not suggested.
    .resource_limits(
        ResourceLimits::builder()
            .workspace_quota_bytes(10 * 1024 * 1024 * 1024)
            .artifact_cap_bytes(100 * 1024 * 1024)
            .build(),
    )
    // Bounds on the operations that can otherwise hang forever. The turn wall
    // clock bound lives in `budget`, because exceeding it is a budget outcome
    // rather than a hung operation.
    .timeouts(
        Timeouts::builder()
            .exec_command(Duration::from_secs(30 * 60))
            .llm_request(Duration::from_secs(10 * 60))
            .harness_idle(Duration::from_secs(15 * 60))
            .build(),
    )
    // Opt out of the noisy ones. Statistics are off by default because they
    // change on every token. Incidents are absent from this list and cannot be
    // switched off: a stream you can configure to hide failures is worse than
    // no stream.
    .stream(
        StreamSettings::builder()
            .include_statistics(false)
            .include_agent_thinking(true)
            .include_tool_calls(true)
            .include_team_chat(true)
            .include_service_logs(true)
            .build(),
    )
    .build()?;

    Ok(settings)
}

/// Refuses a satellite this SDK cannot speak to, and reports harness support.
///
/// Both checks are cheap and both fail loudly here rather than three turns into
/// a run. An SDK refuses a higher proto major outright rather than failing later
/// with a confusing decode error; a higher minor warns once and proceeds,
/// ignoring additive fields it does not know about.
///
/// # Errors
///
/// Returns `PROTO_VERSION_UNSUPPORTED` when the satellite serves a proto major
/// this SDK does not.
async fn preflight(satellite: &Satellite, harness: Harness) -> Result<()> {
    let version = satellite.version().await?;
    tracing::info!(
        satellite.version = %version.satellite_version(),
        proto.major = version.proto_major(),
        proto.minor = version.proto_minor(),
        "connected to satellite",
    );

    // Shapes are not the whole contract. A harness might have no plan mode and
    // no sub-agents, and discovering that by absence three turns in is exactly
    // what asking up front avoids.
    let reported = satellite.harness().await?;
    tracing::info!(harness.default = ?reported.default_harness(), "harnesses reported");

    for capabilities in reported.harnesses() {
        if capabilities.harness() != harness {
            continue;
        }
        if !capabilities.supports_native_plan_mode() {
            tracing::debug!("harness has no native plan mode, Arsox will use its skill fallback");
        }
        // Absent is not zero. A harness that reports no cache accounting leaves
        // the cache token fields as `None` rather than `Some(0)`, so a cost
        // reconciliation can tell "not reported" from "read nothing from cache".
        if !capabilities.reports_cache_tokens() {
            tracing::debug!("harness reports no cache accounting, cache token fields will be None");
        }
    }

    Ok(())
}

/// Pumps the satellite's control stream until it closes.
///
/// One socket per satellite, carrying lifecycle only: threads created and
/// destroyed, queue depth, health transitions, budget warnings. It never carries
/// thread content, which is why it is a separate type rather than the thread
/// stream with a filter applied.
async fn watch_satellite(satellite: Satellite) -> Result<()> {
    let mut events = satellite.events();

    while let Some(event) = events.next().await {
        match event? {
            ControlEvent::ThreadStateChanged { thread_id, previous, current, .. } => {
                tracing::debug!(thread = %thread_id, ?previous, ?current, "thread state changed");
            }
            ControlEvent::HealthChanged { ready: true, check_name, .. } => {
                tracing::info!(check = %check_name, "satellite ready again");
            }
            ControlEvent::HealthChanged { ready: false, check_name, detail } => {
                tracing::error!(check = %check_name, detail = ?detail, "satellite not ready");
            }
            other => {
                tracing::debug!(?other, "control event");
            }
        }
    }

    Ok(())
}

/// Dispatches a single normalized event from the thread stream.
///
/// `author` is a struct rather than a display string: kind, member id, role, and
/// the owning member for a sub-agent. That is what lets a client group a stream
/// by member without parsing names. It implements `Display` for the common case
/// of logging it, and the fields are there when grouping matters.
async fn handle_event(thread: &Thread, event: TurnEvent) -> Result<()> {
    match event {
        TurnEvent::AgentMessage { author, text, .. } => {
            tracing::info!(author = %author, role = ?author.role(), "{text}");
        }
        TurnEvent::ToolStarted { author, tool_name, .. } => {
            tracing::debug!(author = %author, tool = %tool_name, "tool started");
        }
        TurnEvent::MemberSpawned { member_id, role, .. } => {
            tracing::info!(member = %member_id, role = %role, "member spawned");
        }
        TurnEvent::TeamChat { author, text, .. } => {
            tracing::info!(author = %author, "[team] {text}");
        }
        TurnEvent::IntegrationLanded { member_id, branch, .. } => {
            tracing::info!(member = %member_id, branch = %branch, "integration landed");
        }
        TurnEvent::IntegrationConflict { member_id, conflicting_paths, .. } => {
            tracing::warn!(
                member = %member_id,
                paths = ?conflicting_paths,
                "conflict returned for resolution",
            );
        }
        TurnEvent::CheckerResult { result, .. } => {
            tracing::info!(
                command = %result.command(),
                exit_code = result.exit_code(),
                "checker finished",
            );
        }
        // A service the thread never declared, promoted by the exec broker
        // because a member ran something long-lived that bound a port. Every
        // later member running the same command gets this URL rather than a
        // second process.
        TurnEvent::ServiceStarted { service_name, url, auto_promoted: true, .. } => {
            tracing::info!(service = %service_name, url = %url, "auto-promoted to a service");
        }
        TurnEvent::BudgetWarning { percent_used, ceiling, .. } => {
            tracing::warn!(percent_used, ?ceiling, "budget warning");
        }
        TurnEvent::ArtifactCreated { artifact, .. } => {
            tracing::info!(
                path = %artifact.path().display(),
                size_bytes = artifact.size_bytes(),
                "artifact created",
            );
        }
        // An agent overrode redaction for one secret and one operation. High
        // priority by design: the guardrail can move on an explicit human
        // instruction, but nothing moves quietly.
        TurnEvent::RedactionOverridden { secret_key, operation, justification, .. } => {
            tracing::warn!(
                secret = %secret_key,
                operation = %operation,
                "redaction overridden: {justification}",
            );
        }
        TurnEvent::TurnCompleted { result, .. } => {
            tracing::info!(status = ?result.status(), "turn finished");
        }
        // Every failure at every severity lands here, and this is the one event
        // type that cannot be switched off. `Recovered` and `Blocked` are the
        // ones worth watching: a failover that keeps working looks like success,
        // and a permission denial the agent quietly routed around looks like
        // nothing at all.
        TurnEvent::Incident { disposition, code, message, .. } => {
            match disposition {
                Disposition::Fatal | Disposition::Degraded => {
                    tracing::warn!(code = %code, ?disposition, "{message}");
                }
                Disposition::Recovered | Disposition::Blocked => {
                    tracing::info!(code = %code, ?disposition, "{message}");
                }
            }
        }
        // Plans and questions answer back over HTTP, not up the socket, which is
        // unidirectional. Both live on the thread because only one plan and one
        // question set can ever be outstanding at a time.
        TurnEvent::PlanProposed { plan, .. } => {
            tracing::info!("{}", plan.body());
            thread.approve_plan(plan.plan_id()).await?;
        }
        TurnEvent::QuestionAsked { question_set, .. } => {
            // The whole set is answered in one call. Individual answers may be
            // an option, freeform text, or a decline, but partial submission is
            // not a thing: all of them go back together.
            let answers: Vec<QuestionAnswer> = question_set
                .questions()
                .iter()
                .map(|question| {
                    let recommended = question.options().iter().find(|option| option.is_recommended());
                    match recommended {
                        Some(option) => QuestionAnswer::option(question.question_id(), option.option_id()),
                        None => QuestionAnswer::text(question.question_id(), "Use your best judgement."),
                    }
                })
                .collect();
            thread
                .answer_questions(question_set.question_set_id(), answers)
                .await?;
        }
        // Codes and event types are additive within a proto major, so a newer
        // satellite can send something this SDK version has never heard of. The
        // envelope carries `type` as a plain string for exactly this reason, so
        // an event can still be named, logged, and forwarded even when its body
        // cannot be read. Log it, never panic on it.
        other => {
            tracing::debug!(?other, "unhandled event");
        }
    }

    Ok(())
}

/// Picks a thread back up from a different process.
///
/// A thread lives entirely on the satellite, so any process holding the URL, the
/// secret, and the thread ID can attach. This is how a horizontally scaled host
/// application survives a replica dying mid-turn: persist the thread ID and the
/// last sequence you saw, and whichever replica comes up next resumes from there
/// without losing an event.
///
/// # Errors
///
/// Returns `STREAM_SEQUENCE_EXPIRED` if `last_seen_sequence` is older than the
/// thread's retained history, and `THREAD_EXPIRED` if the idle TTL already
/// collected it.
pub async fn resume(
    satellite: &Satellite,
    thread_id: impl AsRef<str>,
    last_seen_sequence: u64,
) -> Result<()> {
    let ThreadAttached { thread, .. } = satellite.threads().attach(thread_id).await?;

    let mut events = thread.events_from(last_seen_sequence);
    while let Some(event) = events.next().await {
        handle_event(&thread, event?).await?;
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    // Environment variables cross a runtime boundary, so they get a real check.
    let secret = std::env::var("ARSOX_SECRET")
        .context("ARSOX_SECRET is not set, refusing to start")?;

    let satellite = Satellite::builder("https://satellite-01.internal.jalapenolabs.io", secret)
        .build()?;

    let settings = build_settings()?;
    preflight(&satellite, settings.harness()).await?;

    // `Satellite` is Clone with shared-ownership semantics, so this clone is a
    // handle rather than a copy of the connection pool.
    let control = tokio::spawn(watch_satellite(satellite.clone()));

    // Create returns a struct so the shape can grow without breaking callers.
    //
    // The idempotency key is what makes a timed-out create safe to retry.
    // Without it, a response lost in transit is indistinguishable from a thread
    // that was never created, and the only safe move is to retry and leak a
    // whole workspace. `deduplicated` tells you which happened.
    let ThreadCreated { thread, deduplicated } = satellite
        .threads()
        .create(settings)
        .idempotency_key("rate-limiting-2026-08-04")
        .await?;
    tracing::info!(thread = %thread.id(), deduplicated, "thread ready");

    // Suggestion fingerprints already turned into tickets. Without this, the
    // same finding opens a new issue on every turn.
    let mut already_filed: HashSet<String> = HashSet::new();

    // Events belong to the thread, not to a turn: one socket per thread,
    // carrying every turn that runs on it. Each event carries `turn_id` if you
    // need to attribute it.
    //
    // Rust gets both styles from one stream, so it needs no emitter. Consuming
    // `events()` inline is the async-iterator style with its serial loop body
    // and real backpressure. Moving that same loop onto its own task, as below,
    // is the non-blocking shape the Node and Python SDKs get from `on()`.
    // `Thread` is Clone with shared-ownership semantics, so the clone is a
    // handle, not a copy.
    //
    // The stream is also Rust's `on("all")`. It carries every event that
    // reaches the client, including types this SDK version has no variant for,
    // which arrive in the catch-all arm of `handle_event`. That makes this loop
    // the right place to hang an audit log or a bus forwarder.
    let pump_thread = thread.clone();
    let pump = tokio::spawn(async move {
        let mut events = pump_thread.events();
        while let Some(event) = events.next().await {
            handle_event(&pump_thread, event?).await?;
        }
        anyhow::Ok(())
    });

    let TurnStarted { turn, .. } = thread
        .start_turn("Add per-endpoint rate limiting to the public API and open a PR against develop.")
        .idempotency_key("rate-limiting-2026-08-04-turn-1")
        .await?;

    // Resolves when this turn reaches a terminal state. The pump task keeps
    // handling events the whole time.
    let result = turn.result().await?;
    tracing::info!("{}", result.summary());

    // Cost comes back as an estimate, not a number. `amount` is `None` when no
    // endpoint published pricing for its model, and `is_partial` means some
    // requests could be priced and others could not. A confident zero would be a
    // lie in both cases.
    //
    // Money is integer units plus billionths, so the display is assembled rather
    // than divided: no float ever touches a cost in this SDK.
    match result.cost().amount() {
        Some(amount) => {
            tracing::info!(
                tokens = result.tokens().total_tokens(),
                cost = %format!("{}.{:02}", amount.units(), amount.nanos() / 10_000_000),
                partial = result.cost().is_partial(),
                "turn accounting",
            );
        }
        None => {
            tracing::info!(
                tokens = result.tokens().total_tokens(),
                "turn accounting, no endpoint published pricing",
            );
        }
    }

    // Every stage of the stack reports what it did, including the ones that did
    // nothing. This is what stops "the budget ran out before self-review"
    // reading as "self-review found nothing".
    for stage in result.stages() {
        if stage.disposition() == StageDisposition::Skipped {
            tracing::warn!(stage = ?stage.stage(), reason = ?stage.reason(), "stage skipped");
        }
    }

    // Counts by disposition ride along on the report, so the common case needs
    // no query at all. Query when you want the detail.
    tracing::info!(?result.incident_counts(), "incidents");

    let problems = thread
        .incidents()
        .list(
            IncidentQuery::builder()
                .dispositions([Disposition::Fatal, Disposition::Degraded])
                .turn_ids([turn.id()])
                .build(),
        )
        .await?;
    for incident in problems {
        tracing::warn!(
            code = %incident.code(),
            disposition = ?incident.disposition(),
            "{}",
            incident.message()
        );
    }

    // Questions nobody answered before the timeout. The turn ended with them
    // recorded here rather than hanging forever.
    for question_set in result.unanswered_questions() {
        tracing::warn!(count = question_set.questions().len(), "questions went unanswered");
    }

    // `fingerprint` is the field that makes this an issue pipeline rather than a
    // report. Suppress the ones you have already filed or you will open the same
    // ticket again on every turn.
    for suggestion in result.suggestions().tech_debt() {
        if already_filed.contains(suggestion.fingerprint()) {
            continue;
        }
        open_issue(suggestion.title(), suggestion.body()).await?;
        already_filed.insert(suggestion.fingerprint().to_owned());
    }

    // The agent's proposed setup scripts are inert data. Arsox never adopts
    // them, never writes them, never runs them. Adopting one is this line, and
    // it is yours: commands that will execute on a later satellite are a
    // permission decision, and permission decisions are never the agent's to
    // make.
    for setup in result.suggestions().setup_script() {
        tracing::info!("setup gap: {}", setup.title());
        for evidence in setup.evidence() {
            tracing::info!(
                command = %evidence.command(),
                exit_code = evidence.exit_code(),
                "evidence",
            );
        }
        if let Some(commands) = setup.proposed_setup_commands() {
            tracing::info!("proposed setup change:\n{commands}");
        }
    }

    for artifact in thread.artifacts().list().await? {
        thread
            .artifacts()
            .download(artifact.path(), format!("./out/{}", artifact.name()))
            .await?;
    }

    // Or leave it to expire through the idle TTL. Destroying closes the socket,
    // which ends the pump task.
    thread.destroy().await?;
    pump.await??;

    control.abort();

    Ok(())
}
