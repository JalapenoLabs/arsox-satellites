// Copyright © 2026 Jalapeno Labs

//! What the harnesses on this satellite can do, reported rather than inferred.
//!
//! Shapes are not the whole contract. A harness might have no plan mode or no
//! sub-agents, and a consumer that swapped harnesses would otherwise discover
//! that by absence, three turns into a run. `GET /v1/harness` serves what this
//! module resolves, so the SDK can check up front instead of inferring from
//! silence.
//!
//! Capability facts are stated per harness, in code, because they are facts
//! about a CLI rather than about a satellite: the Claude CLI either has a native
//! plan mode or it does not, and no configuration changes that. The one fact
//! that is not static, the CLI's version, is probed at boot.

use arsox_sdk::proto::harness::v1::{GetHarnessResponse, Harness, HarnessCapabilities};
use std::time::Duration;

/// How long the boot-time `--version` probe may take.
///
/// Generous for a command that prints one line and exits, but a hung probe must
/// not hold the whole boot hostage: past this bound the satellite comes up with
/// the version unresolved rather than not coming up at all.
const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// The longest string the probe accepts as a version.
///
/// A real CLI version line is a few dozen characters. Anything longer is some
/// other program's output arriving where a version was expected, and repeating
/// it into the contract would hand junk to every consumer of the endpoint.
const VERSION_MAX_LENGTH: usize = 64;

/// Resolves what this satellite offers, probing the CLI version once at boot.
pub async fn resolve() -> GetHarnessResponse {
    let cli_version = probe_cli_version(&super::spawn::claude_binary())
        .await
        .unwrap_or_default();

    GetHarnessResponse {
        harnesses: vec![claude(cli_version)],
        default_harness: Harness::Claude.into(),
    }
}

/// What the Claude CLI harness supports.
///
/// Claude is the harness implemented today, and this is the honest place to say
/// so: one entry, rather than a list that implies a suite already spanning
/// several. Codex joins this list the moment its mapper exists.
fn claude(cli_version: String) -> HarnessCapabilities {
    HarnessCapabilities {
        harness: Harness::Claude.into(),
        // Empty when the probe could not resolve one. No CLI reports an empty
        // version, so absence stays distinguishable without a contract change.
        cli_version,
        supports_native_plan_mode: true,
        supports_subagents: true,
        supports_thinking_events: true,
        supports_context_fork: true,
        supports_mcp: true,
        reports_cache_tokens: true,
    }
}

/// Asks a CLI for its version, tolerating every way that can fail.
///
/// A missing binary, a hang, garbage output: each leaves the version unresolved
/// and the satellite booting anyway, because "what version is the CLI" is worth
/// a warning and never worth refusing to start.
async fn probe_cli_version(program: &str) -> Option<String> {
    let mut command = tokio::process::Command::new(program);
    command.arg("--version").kill_on_drop(true);

    let probed = tokio::time::timeout(VERSION_PROBE_TIMEOUT, command.output()).await;

    let output = match probed {
        Ok(Ok(output)) if output.status.success() => output,
        Ok(Ok(output)) => {
            tracing::warn!(
                event.name = "harness.version_probe.nonzero",
                process.command = program,
                process.exit_code = output.status.code(),
                "the version probe exited nonzero, capabilities report no CLI version",
            );
            return None;
        }
        Ok(Err(error)) => {
            tracing::warn!(
                event.name = "harness.version_probe.failed",
                process.command = program,
                "the version probe could not run, capabilities report no CLI version: {error}",
            );
            return None;
        }
        Err(_elapsed) => {
            tracing::warn!(
                event.name = "harness.version_probe.timeout",
                process.command = program,
                "the version probe hung, capabilities report no CLI version",
            );
            return None;
        }
    };

    parse_version_line(&String::from_utf8_lossy(&output.stdout))
}

/// Extracts a plausible version from probe output, or nothing.
///
/// Kept separate from the probe so the judgment is testable without spawning
/// anything. The gate matters because tests and smoke runs substitute a
/// stand-in harness that replays a transcript: asked for a version, it prints
/// JSON, and repeating that into the contract would be worse than admitting the
/// version is unknown.
fn parse_version_line(stdout: &str) -> Option<String> {
    let line = stdout.lines().next()?.trim();

    let plausible =
        line.starts_with(|first: char| first.is_ascii_digit()) && line.len() <= VERSION_MAX_LENGTH;

    plausible.then(|| line.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_real_version_line_is_accepted_whole() {
        // The whole line, not the first token: "(Claude Code)" is part of how
        // the CLI identifies itself and costs a consumer nothing to carry.
        assert_eq!(
            parse_version_line("2.1.226 (Claude Code)\n"),
            Some("2.1.226 (Claude Code)".to_owned())
        );
    }

    #[test]
    fn only_the_first_line_is_considered() {
        assert_eq!(
            parse_version_line("1.0.3\nsome trailing diagnostics\n"),
            Some("1.0.3".to_owned())
        );
    }

    #[test]
    fn transcript_replay_is_not_mistaken_for_a_version() {
        // The stand-in harness answers every invocation, `--version` included,
        // by replaying its transcript. That output must not become the version.
        assert_eq!(
            parse_version_line(r#"{"type":"system","subtype":"init"}"#),
            None
        );
    }

    #[test]
    fn empty_output_resolves_no_version() {
        assert_eq!(parse_version_line(""), None);
        assert_eq!(parse_version_line("\n"), None);
    }

    #[test]
    fn an_implausibly_long_line_is_rejected() {
        let long = format!("1.{}", "2".repeat(VERSION_MAX_LENGTH));
        assert_eq!(parse_version_line(&long), None);
    }

    #[tokio::test]
    async fn a_missing_binary_leaves_the_version_unresolved_rather_than_failing() {
        // The probe must never stop a boot. A satellite with no CLI installed
        // still comes up, still serves its API, and says so here.
        let version = probe_cli_version("arsox-no-such-binary-anywhere").await;
        assert_eq!(version, None);
    }

    #[test]
    fn the_default_harness_is_claude_and_it_is_the_only_entry() {
        let response = GetHarnessResponse {
            harnesses: vec![claude(String::new())],
            default_harness: Harness::Claude.into(),
        };

        assert_eq!(response.harnesses.len(), 1);
        assert_eq!(response.default_harness, i32::from(Harness::Claude));
    }
}
