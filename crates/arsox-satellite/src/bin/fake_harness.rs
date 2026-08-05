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

use std::io::Write as _;

fn main() {
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

    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        reason = "a test directive is small by construction"
    )]
    std::process::exit(exit_code as i32);
}

/// Reads a `[[key=value]]` directive out of the prompt.
fn directive(prompt: &str, key: &str) -> Option<usize> {
    let opener = format!("[[{key}=");
    let start = prompt.find(&opener)? + opener.len();
    let rest = prompt.get(start..)?;
    let end = rest.find("]]")?;

    rest.get(..end)?.parse().ok()
}
