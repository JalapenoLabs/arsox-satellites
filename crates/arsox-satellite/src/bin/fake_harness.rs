// Copyright © 2026 Jalapeno Labs

//! A stand-in harness that replays a recorded transcript.
//!
//! Gated behind the `test-util` feature so it cannot end up in a published
//! image. It exists so the turn runner can be exercised end to end without a
//! model, a network, or a token budget: the satellite spawns this exactly as it
//! would spawn a real CLI and cannot tell the difference.
//!
//! # Configuration
//!
//! The transcript comes from `ARSOX_FAKE_TRANSCRIPT`, which is the same for
//! every caller and so is safe to set once for a process.
//!
//! Per-run behaviour rides on the **prompt** instead, as `[[key=value]]`
//! directives. That is deliberate: process environment is global, and tests run
//! in parallel in one process, so an env-var knob is a race in which one test
//! silently reconfigures another. The prompt is an argument, and arguments
//! belong to one spawn.
//!
//! - `[[exit=N]]` exits with N rather than 0, for the crash path.
//! - `[[truncate=N]]` stops after N lines, for a harness that dies mid-run
//!   without reporting a result.
//! - `[[report_env=NAME]]` writes what the child can see of `NAME` to stderr,
//!   so a test can assert on the environment an agent actually receives rather
//!   than on the environment the satellite intended to give it.
//! - `[[complete=N]]` sends N completion requests through the satellite's own
//!   proxy before the transcript, exactly as a CLI would. Nothing else in a test
//!   can make the proxy route a request, so this is the only way to exercise
//!   what the runner does with what the proxy reports back.
//! - `[[stall=MS]]` holds the process open for MS milliseconds after the
//!   transcript, producing nothing. A real harness stuck in a long shell command
//!   looks exactly like this from the satellite's side, which is what the wall
//!   clock ceiling exists to end.
//! - `[[hang=MS]]` produces nothing for MS milliseconds **before** the
//!   transcript, on every run. A harness that wedged before it said anything at
//!   all is what the idle bound exists to end, and it is a different shape from
//!   `stall`: nothing has been reported yet, so there is nothing to preserve.
//! - `[[hang_once=MS]]` is the same, on the first run in this working directory
//!   only. A restart therefore gets past it and finishes the transcript, which
//!   is what makes "restarted once, and then the turn completed" testable.
//!
//! "First run" is a marker file in the working directory rather than a counter
//! in this process, because a restart is a **new** process. The same trick a
//! checker test uses with `mkdir`, for the same reason.

use std::io::Write as _;

/// Names the run that already hung, so the next one does not.
///
/// In the working directory, which the satellite gives one per thread, so two
/// threads hanging at once cannot see each other's marker.
const HUNG_ALREADY: &str = "arsox-fake-harness-hung";

#[tokio::main]
async fn main() {
    let Ok(path) = std::env::var("ARSOX_FAKE_TRANSCRIPT") else {
        eprintln!("ARSOX_FAKE_TRANSCRIPT is not set");
        std::process::exit(64);
    };

    let transcript = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) => {
            eprintln!("could not read {path}: {error}");
            std::process::exit(66);
        }
    };

    // The satellite passes the prompt as the argument after `--print`, so the
    // directives arrive with it.
    let arguments: Vec<String> = std::env::args().collect();
    let prompt = arguments
        .iter()
        .position(|argument| argument == "--print")
        .and_then(|index| arguments.get(index + 1))
        .cloned()
        .unwrap_or_default();

    let truncate_after = directive(&prompt, "truncate").unwrap_or(usize::MAX);
    let exit_code = directive(&prompt, "exit").unwrap_or(0);

    // Reported from inside the child, because that is the only vantage point
    // that can answer what an agent actually sees. Anything asserted from the
    // satellite's side is asserting on intent.
    if let Some(name) = text_directive(&prompt, "report_env") {
        let seen = std::env::var(&name).unwrap_or_else(|_unset| "(unset)".to_owned());
        eprintln!("report_env {name}={seen}");
    }

    // Before the replay, because a turn that failed to reach a model has nothing
    // to say afterwards and the runner should not have to read a transcript to
    // find that out.
    if let Some(requests) = directive(&prompt, "complete") {
        complete(requests).await;
    }

    // Before the replay rather than after it. A harness that hung before it said
    // anything is the case the idle bound is written for, and it is the case a
    // restart can actually recover.
    if let Some(millis) = directive(&prompt, "hang") {
        hang(millis);
    }

    // `create_new` is the whole test: it succeeds exactly once per working
    // directory, so the first process hangs and every restart after it does not.
    // Asking and then creating would be two steps a second process could run
    // between.
    if let Some(millis) = directive(&prompt, "hang_once")
        && std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(HUNG_ALREADY)
            .is_ok()
    {
        hang(millis);
    }

    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    for line in transcript.lines().take(truncate_after) {
        if line.trim().is_empty() {
            continue;
        }
        // Flushed per line, because the satellite reads this as a stream and a
        // buffered replay would arrive all at once, proving nothing about the
        // streaming path.
        if writeln!(out, "{line}").is_err() || out.flush().is_err() {
            return;
        }
    }

    // After the replay rather than before it, so a stalled run still proves that
    // the work a turn already did survives the ceiling that stops it.
    if let Some(millis) = directive(&prompt, "stall") {
        hang(millis);
    }

    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        reason = "a test directive is small by construction"
    )]
    std::process::exit(exit_code as i32);
}

/// Asks for a completion the way a CLI does, through the satellite's proxy.
///
/// The environment carries where to ask and what to present, and neither is a
/// provider credential: `ANTHROPIC_BASE_URL` is the satellite's own listener and
/// `ANTHROPIC_API_KEY` is the turn's token, which is worth nothing anywhere
/// else.
///
/// Whatever comes back is reported on stderr and otherwise ignored. This exists
/// to make the request happen, not to act on the answer.
async fn complete(requests: usize) {
    let Ok(base_url) = std::env::var("ANTHROPIC_BASE_URL") else {
        eprintln!("ANTHROPIC_BASE_URL is not set, so there is no proxy to ask");
        return;
    };
    let token = std::env::var("ANTHROPIC_API_KEY").unwrap_or_default();

    for _request in 0..requests {
        let answered = reqwest::Client::new()
            .post(format!("{base_url}/v1/messages"))
            .header("x-api-key", &token)
            .header("content-type", "application/json")
            .body(r#"{"model":"claude-opus-5","messages":[]}"#)
            .send()
            .await;

        match answered {
            Ok(response) => eprintln!("complete {}", response.status()),
            Err(error) => eprintln!("complete failed: {error}"),
        }
    }
}

/// Produces nothing at all for `millis`, which is what a wedged harness does.
fn hang(millis: usize) {
    std::thread::sleep(std::time::Duration::from_millis(
        u64::try_from(millis).unwrap_or(u64::MAX),
    ));
}

/// Reads a numeric `[[key=value]]` directive out of the prompt.
fn directive(prompt: &str, key: &str) -> Option<usize> {
    text_directive(prompt, key)?.parse().ok()
}

/// Reads a `[[key=value]]` directive out of the prompt as written.
fn text_directive(prompt: &str, key: &str) -> Option<String> {
    let opener = format!("[[{key}=");
    let start = prompt.find(&opener)? + opener.len();
    let rest = prompt.get(start..)?;
    let end = rest.find("]]")?;

    rest.get(..end).map(str::to_owned)
}
