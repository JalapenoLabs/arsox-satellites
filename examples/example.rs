// Copyright © 2026 Jalapeno Labs

//! End to end example of driving an Arsox satellite from Rust.
//!
//! Creates a thread with the full settings surface, runs a turn, consumes the
//! normalized event stream, answers the agent's questions, and collects artifacts.
//!
//! Run with: `cargo run --example example`

use std::time::Duration;

use anyhow::{Context, Result};
use futures::StreamExt;

use arsox::enums::{Harness, MergeMethod, RedactionMode, WebAccess};
use arsox::events::TurnEvent;
use arsox::questions::QuestionAnswer;
use arsox::settings::{
    Budget, Ceiling, CustomEnvVar, GithubSettings, HumanInTheLoop, JiraSettings, McpServer,
    ModelAuth, ModelEndpoint, Permissions, PlanMode, PullRequests, Redaction, RepoAuth,
    RepoSettings, RetryPolicy, SelfReview, StreamSettings, TeamMode, ThreadDeps, ThreadSettings,
};
use arsox::{Satellite, Thread, ThreadAttached, ThreadCreated, TurnStarted};

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
    // unbounded spend is always a decision rather than an oversight.
    let budget = Budget::builder()
        .max_tokens_per_turn(8_000_000)
        .max_cost_per_thread(40.0)
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
    .pull_requests(
        PullRequests::builder()
            .allow_agent_merge(false)
            .allowed_merge_methods([MergeMethod::Squash])
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
        .build()])
    .github(
        GithubSettings::builder(std::env::var("GITHUB_PAT")?)
            .allow_merge(false)
            .build(),
    )
    .jira(
        JiraSettings::builder(std::env::var("JIRA_PAT")?)
            .base_url("https://jalapenolabs.atlassian.net")
            .allow_status_transitions(true)
            .allow_comments(true)
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
            // Six stars regardless of the secret's real length. Set to -1 to
            // mirror the length, which leaks the length and is why it is not
            // the default.
            .star_count(6)
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
            .allow_git_push(true)
            .protected_branches(["main", "develop"])
            // Omit to inherit the preset allowlist. An explicit list replaces it.
            .allowed_commands(["git", "cargo", "rg", "gh"])
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
    // Opt out of the noisy ones. Statistics are off by default because they
    // change on every token.
    .stream(
        StreamSettings::builder()
            .include_statistics(false)
            .include_agent_thinking(true)
            .include_tool_calls(true)
            .include_team_chat(true)
            .build(),
    )
    .build()?;

    Ok(settings)
}

/// Dispatches a single normalized event from the thread stream.
async fn handle_event(thread: &Thread, event: TurnEvent) -> Result<()> {
    match event {
        TurnEvent::AgentMessage { author, text, .. } => {
            tracing::info!(author = %author, "{text}");
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
        TurnEvent::IntegrationConflict { member_id, .. } => {
            tracing::warn!(member = %member_id, "conflict returned for resolution");
        }
        TurnEvent::CheckerResult { command, exit_code, .. } => {
            tracing::info!(command = %command, exit_code, "checker finished");
        }
        TurnEvent::BudgetWarning { percent_used, ceiling, .. } => {
            tracing::warn!(percent_used, ceiling = %ceiling, "budget warning");
        }
        TurnEvent::ArtifactCreated { path, size_bytes, .. } => {
            tracing::info!(path = %path.display(), size_bytes, "artifact created");
        }
        TurnEvent::TurnCompleted { status, .. } => {
            tracing::info!(status = %status, "turn finished");
        }
        // Plans and questions answer back over HTTP, not up the socket, which is
        // unidirectional. Both live on the thread because only one plan and one
        // question set can ever be outstanding at a time.
        TurnEvent::PlanProposed { plan, .. } => {
            tracing::info!("{plan}");
            thread.approve_plan().await?;
        }
        TurnEvent::QuestionAsked { questions, .. } => {
            // The whole set is answered in one call. Individual answers may be
            // an option, freeform text, or a decline, but partial submission is
            // not a thing: all of them go back together.
            let answers: Vec<QuestionAnswer> = questions
                .iter()
                .map(|question| {
                    let recommended = question.options.iter().find(|option| option.is_recommended);
                    match recommended {
                        Some(option) => QuestionAnswer::option(&question.id, &option.id),
                        None => QuestionAnswer::text(&question.id, "Use your best judgement."),
                    }
                })
                .collect();
            thread.answer_questions(answers).await?;
        }
        // Codes and event types are additive within a proto major, so a newer
        // satellite can send something this SDK version has never heard of.
        // Log it, never panic on it.
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

    // Create returns a struct so the shape can grow without breaking callers.
    let ThreadCreated { thread, .. } = satellite.threads().create(build_settings()?).await?;
    tracing::info!(thread = %thread.id(), "thread created");

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
        .await?;

    // Resolves when this turn reaches a terminal state. The pump task keeps
    // handling events the whole time.
    let result = turn.result().await?;
    tracing::info!("{}", result.summary());
    tracing::info!(
        tokens = result.tokens().total(),
        cost = result.cost(),
        "turn accounting"
    );

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

    Ok(())
}
