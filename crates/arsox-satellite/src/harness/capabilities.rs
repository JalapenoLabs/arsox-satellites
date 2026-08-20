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
//! plan mode or it does not, and no configuration changes that. The two facts
//! that are not static, whether a CLI is installed at all and which version it
//! is, are probed at boot.

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

/// Resolves what this satellite offers, probing each CLI version once at boot.
///
/// **Claude is always listed and Codex is listed when its binary answered.**
/// The asymmetry is deliberate rather than an oversight: the published image
/// installs Claude and the satellite defaults to it, so a probe that failed says
/// something about the probe. Codex is what an operator may or may not have
/// added on top, so a satellite that cannot run one must not advertise it and
/// have a thread discover the truth as `HARNESS_LAUNCH_FAILED` three turns in.
pub async fn resolve() -> GetHarnessResponse {
    let claude_cli = probe_cli(&super::spawn::claude_binary()).await;
    let codex_cli = probe_cli(&super::spawn::codex_binary()).await;

    offered(claude_cli, codex_cli)
}

/// Turns two probes into the list this satellite reports.
///
/// Separate from [`resolve`] so the gating is testable without spawning
/// anything, which is the only way to assert it against a satellite that has no
/// Codex without changing the process's environment out from under every other
/// test in it.
fn offered(claude_cli: Probed, codex_cli: Probed) -> GetHarnessResponse {
    let mut harnesses = vec![claude(claude_cli.version)];

    if codex_cli.present {
        harnesses.push(codex(codex_cli.version));
    }

    GetHarnessResponse {
        harnesses,
        default_harness: Harness::Claude.into(),
    }
}

/// What the Claude CLI harness supports.
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

/// What the Codex CLI harness supports.
///
/// Every field here is what the mapper in `arsox-harness` actually produces
/// from a recorded `codex exec --json` run, checked against the fixtures rather
/// than read off a feature list. The four that differ from Claude each cost a
/// sentence, because a `false` a consumer cannot explain is worse than one it
/// can plan around.
fn codex(cli_version: String) -> HarnessCapabilities {
    HarnessCapabilities {
        harness: Harness::Codex.into(),
        cli_version,

        // `codex exec` has no plan mode. Arsox supplies the skill fallback and
        // normalizes the output, which is exactly what this field being false is
        // for.
        supports_native_plan_mode: false,

        // The exec stream has no sub-agent tier and carries no id one could be
        // derived from, which is why the Codex mapper attributes every event to
        // the agent itself. Claiming otherwise would promise a `member_id` that
        // is never populated.
        supports_subagents: false,

        // A `reasoning` item maps to `agent.thinking`, so
        // `include_agent_thinking` has something to carry.
        supports_thinking_events: true,

        // `codex fork` forks an interactive session, and `codex exec` has no
        // equivalent. A turn runs under `exec`, so the capability a consumer
        // would be told about is one no turn can reach.
        supports_context_fork: false,

        // `mcp_tool_call` items are mapped, and the CLI manages MCP servers.
        supports_mcp: true,

        // True with one asymmetry worth knowing, and stated in
        // `docs/harness.md` rather than only here: Codex reports cache reads and
        // `cache_read_tokens` carries them, while `cache_write_tokens` is always
        // absent because the provider behind it has no cache-write concept. This
        // field is one bool over both, and reporting false would tell a consumer
        // to ignore the cache read count Codex genuinely measures.
        reports_cache_tokens: true,
    }
}

/// What asking a CLI for its version established.
///
/// The two facts are separate on purpose. A binary that is not installed and one
/// that ran and printed something unusable are the same `version`, and only the
/// first is a reason to leave a harness off the list.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Probed {
    /// Whether the binary ran at all.
    present: bool,

    /// The version it reported, empty when it reported nothing plausible.
    version: String,
}

/// Asks a CLI for its version, tolerating every way that can fail.
///
/// A missing binary, a hang, garbage output: each leaves the version unresolved
/// and the satellite booting anyway, because "what version is the CLI" is worth
/// a warning and never worth refusing to start.
async fn probe_cli(program: &str) -> Probed {
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
            // It ran, so the binary is there. Only what it said is unusable.
            return Probed {
                present: true,
                version: String::new(),
            };
        }
        Ok(Err(error)) => {
            tracing::warn!(
                event.name = "harness.version_probe.failed",
                process.command = program,
                "the version probe could not run, capabilities report no CLI version: {error}",
            );
            return Probed::default();
        }
        Err(_elapsed) => {
            tracing::warn!(
                event.name = "harness.version_probe.timeout",
                process.command = program,
                "the version probe hung, capabilities report no CLI version",
            );
            // A hung binary is an installed binary, and a satellite that hid a
            // harness because its version probe was slow would be reporting the
            // probe rather than the install.
            return Probed {
                present: true,
                version: String::new(),
            };
        }
    };

    Probed {
        present: true,
        version: parse_version_line(&String::from_utf8_lossy(&output.stdout)).unwrap_or_default(),
    }
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
        let probed = probe_cli("arsox-no-such-binary-anywhere").await;

        assert_eq!(probed, Probed::default());
    }

    /// A probe of a CLI that answered with the version it reported.
    fn installed(version: &str) -> Probed {
        Probed {
            present: true,
            version: version.to_owned(),
        }
    }

    #[test]
    fn a_satellite_without_codex_does_not_offer_it() {
        // A harness a thread cannot actually run must not be advertised. The
        // alternative is a caller that reads the endpoint, picks Codex, and
        // learns the truth as `HARNESS_LAUNCH_FAILED` on its first turn.
        let response = offered(installed("2.1.237 (Claude Code)"), Probed::default());

        assert_eq!(response.harnesses.len(), 1);
        assert_eq!(response.harnesses[0].harness, i32::from(Harness::Claude));
        assert_eq!(response.default_harness, i32::from(Harness::Claude));
    }

    #[test]
    fn a_satellite_with_both_reports_both_and_still_defaults_to_claude() {
        let response = offered(
            installed("2.1.237 (Claude Code)"),
            installed("codex-cli 0.147.0"),
        );

        assert_eq!(response.harnesses.len(), 2);
        assert_eq!(response.harnesses[1].harness, i32::from(Harness::Codex));
        assert_eq!(response.harnesses[1].cli_version, "codex-cli 0.147.0");
        assert_eq!(response.default_harness, i32::from(Harness::Claude));
    }

    #[test]
    fn claude_is_listed_whether_or_not_its_probe_answered() {
        // The image installs it and the satellite defaults to it, so a probe
        // that failed says something about the probe rather than about what this
        // satellite offers.
        let response = offered(Probed::default(), Probed::default());

        assert_eq!(response.harnesses.len(), 1);
        assert_eq!(response.harnesses[0].harness, i32::from(Harness::Claude));
        assert!(response.harnesses[0].cli_version.is_empty());
    }

    #[test]
    fn a_codex_that_ran_but_said_nothing_usable_is_still_offered() {
        // It is installed, which is the fact the listing is about. The version
        // is separately empty, which is what the contract's own absence means.
        let response = offered(Probed::default(), installed(""));

        assert_eq!(response.harnesses.len(), 2);
        assert!(response.harnesses[1].cli_version.is_empty());
    }

    #[test]
    fn the_codex_entry_states_what_its_mapper_can_actually_produce() {
        // Each of these is checked against the recorded fixtures rather than a
        // feature list, because a capability a consumer plans around and then
        // never sees is worse than one it was told about.
        let codex = codex("0.147.0".to_owned());

        assert!(codex.supports_thinking_events, "a reasoning item is mapped");
        assert!(codex.supports_mcp, "mcp_tool_call items are mapped");
        assert!(
            codex.reports_cache_tokens,
            "cache reads are measured, and a false here would tell a consumer to \
             ignore a real count"
        );

        assert!(
            !codex.supports_subagents,
            "the exec stream carries no member id to derive one from"
        );
        assert!(
            !codex.supports_native_plan_mode,
            "codex exec has no plan mode, which is what the skill fallback is for"
        );
        assert!(
            !codex.supports_context_fork,
            "codex fork forks an interactive session, and a turn runs under exec"
        );
    }
}
