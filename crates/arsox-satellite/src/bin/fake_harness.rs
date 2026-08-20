// Copyright © 2026 Jalapeno Labs

//! A stand-in harness that replays a recorded transcript.
//!
//! Gated behind the `test-util` feature so it cannot end up in a published
//! image. It exists so the turn runner can be exercised end to end without a
//! model, a network, or a token budget: the satellite spawns this exactly as it
//! would spawn a real CLI and cannot tell the difference.
//!
//! # Which harness it is standing in for
//!
//! The satellite spawns it under whichever command line the thread's harness
//! calls for, and this reads that command line back to decide which harness it
//! is being: `codex exec …` or `claude --print …`. Nothing else could decide it,
//! since the satellite hands both forms to the same binary and the two speak
//! different event vocabularies.
//!
//! # Configuration
//!
//! The transcript comes from `ARSOX_FAKE_TRANSCRIPT` for Claude and
//! `ARSOX_FAKE_CODEX_TRANSCRIPT` for Codex. One per harness, because a
//! transcript belongs to a vocabulary and replaying the wrong one produces a
//! turn made entirely of unrecognized events. Each is the same for every caller
//! and so is safe to set once for a process.
//!
//! Per-run behaviour rides on the **prompt** instead, as `[[key=value]]`
//! directives. That is deliberate: process environment is global, and tests run
//! in parallel in one process, so an env-var knob is a race in which one test
//! silently reconfigures another. The prompt is an argument, and arguments
//! belong to one spawn.
//!
//! - `[[exit=N]]` exits with N rather than 0, **after** the transcript. On its
//!   own that is a harness that reported its result and then exited badly, which
//!   the runner honors rather than restarts. Pair it with `[[truncate=N]]` for a
//!   death the restart has to answer.
//! - `[[truncate=N]]` stops after N lines, for a harness that dies mid-run
//!   without reporting a result.
//! - `[[report_env=NAME]]` writes what the child can see of `NAME` to stderr,
//!   so a test can assert on the environment an agent actually receives rather
//!   than on the environment the satellite intended to give it.
//! - `[[record_argv=FILE]]` writes the command line this replay was launched
//!   with, one argument per line, to `FILE` in the working directory. Same
//!   reasoning as `report_env` and the same vantage point: whether a turn
//!   resumed a session is a fact about what the CLI was asked to do, and the CLI
//!   is the only thing that can report it. A test names a different file per
//!   turn so a later spawn does not overwrite the evidence from an earlier one.
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
//! - `[[crash_once=N]]` exits with N **before** the transcript, on the first run
//!   in this working directory only. `exit` is the harness that dies every time
//!   and this is the one that dies once, which is what makes "restarted once,
//!   and then the turn completed" testable for a crash as well as for a hang.
//!   Before the transcript on purpose: a process that died before it said
//!   anything reported no session id either, which is the case a restart has to
//!   open a session rather than resume one.
//! - `[[transcript=PATH]]` replays PATH instead of the transcript this process
//!   was configured with, for a test that needs a recording the process-wide
//!   variable does not carry. The vocabulary still has to match the harness
//!   being stood in for, which is the caller's to get right.
//! - `[[unrecognized=N]]` emits N lines of an event type nothing maps, before
//!   the transcript, which is what a CLI release adding an event type looks like
//!   from the satellite's side. The mapper records a degraded incident and drops
//!   the line, so the turn still finishes with an incident to read back.
//!   `scripts/fake-harness.sh` implements the same directive, so a check written
//!   against one stand-in runs against either.
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

/// Names the run that already died, so the next one does not.
///
/// Its own marker rather than one shared with [`HUNG_ALREADY`], so a test can
/// ask for a hang and then a crash and get one of each.
const CRASHED_ALREADY: &str = "arsox-fake-harness-crashed";

/// Which harness's command line this replay was invoked with.
///
/// Read from argv rather than from the environment, because the satellite hands
/// both forms to this one binary and an environment variable saying which is
/// which would be a global that two threads in one test process could disagree
/// about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Invocation {
    Claude,
    Codex,
}

impl Invocation {
    /// The form these arguments were built in.
    ///
    /// `codex exec` always leads with its subcommand. Everything else is read as
    /// Claude, which is what the satellite's own default harness is and what a
    /// bare `--version` probe arrives as.
    fn of(arguments: &[String]) -> Self {
        if arguments.get(1).map(String::as_str) == Some("exec") {
            Self::Codex
        } else {
            Self::Claude
        }
    }

    /// Where this invocation's transcript path is read from.
    const fn transcript_env(self) -> &'static str {
        match self {
            Self::Claude => "ARSOX_FAKE_TRANSCRIPT",
            Self::Codex => "ARSOX_FAKE_CODEX_TRANSCRIPT",
        }
    }

    /// The prompt, wherever this CLI's command line carries it.
    ///
    /// Claude takes it as the value of `--print`. Codex takes it as the last
    /// positional behind `--`, after the session id when the form is `resume`.
    fn prompt(self, arguments: &[String]) -> String {
        let found = match self {
            Self::Claude => arguments
                .iter()
                .position(|argument| argument == "--print")
                .and_then(|index| arguments.get(index + 1)),
            Self::Codex => arguments
                .iter()
                .position(|argument| argument == "--")
                .and_then(|index| arguments.get(index + 1..))
                .and_then(<[String]>::last),
        };

        found.cloned().unwrap_or_default()
    }
}

#[tokio::main]
async fn main() {
    let arguments: Vec<String> = std::env::args().collect();
    let invocation = Invocation::of(&arguments);

    // The directives arrive with the prompt, which is an argument and so belongs
    // to one spawn.
    let prompt = invocation.prompt(&arguments);

    // A named transcript outranks the configured one, so a test needing a
    // recording this process was not configured with says so per spawn rather
    // than reconfiguring every other test in the process.
    let path = text_directive(&prompt, "transcript").unwrap_or_else(|| {
        let variable = invocation.transcript_env();

        std::env::var(variable).unwrap_or_else(|_unset| {
            eprintln!("{variable} is not set");
            std::process::exit(64);
        })
    });

    let transcript = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) => {
            eprintln!("could not read {path}: {error}");
            std::process::exit(66);
        }
    };

    let truncate_after = directive(&prompt, "truncate").unwrap_or(usize::MAX);
    let exit_code = directive(&prompt, "exit").unwrap_or(0);

    // Reported from inside the child, because that is the only vantage point
    // that can answer what an agent actually sees. Anything asserted from the
    // satellite's side is asserting on intent.
    if let Some(name) = text_directive(&prompt, "report_env") {
        let seen = std::env::var(&name).unwrap_or_else(|_unset| "(unset)".to_owned());
        eprintln!("report_env {name}={seen}");
    }

    // Written to a file rather than to stderr, which the runner reads for
    // liveness and then discards. A test that has to know what the CLI was asked
    // to do needs it to survive the turn.
    if let Some(file) = text_directive(&prompt, "record_argv")
        && let Err(error) = std::fs::write(&file, arguments.join("\n"))
    {
        eprintln!("could not record the command line to {file}: {error}");
    }

    // Before the replay, because a turn that failed to reach a model has nothing
    // to say afterwards and the runner should not have to read a transcript to
    // find that out.
    if let Some(requests) = directive(&prompt, "complete") {
        complete(invocation, requests).await;
    }

    // Before the replay rather than after it. A harness that hung before it said
    // anything is the case the idle bound is written for, and it is the case a
    // restart can actually recover.
    if let Some(millis) = directive(&prompt, "hang") {
        hang(millis);
    }

    if let Some(millis) = directive(&prompt, "hang_once")
        && first_run(HUNG_ALREADY)
    {
        hang(millis);
    }

    // Before the transcript, and before the session id it would have announced.
    // A restart after this one has nothing to resume, which is exactly the shape
    // a harness that died on startup leaves behind.
    if let Some(code) = directive(&prompt, "crash_once")
        && first_run(CRASHED_ALREADY)
    {
        eprintln!("the fake harness is dying with {code} before it says anything");
        exit_with(code);
    }

    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    // Before the transcript, so the turn it degrades still runs to completion
    // and reports what it did. An incident that ended the turn would be a
    // different case entirely.
    for _line in 0..directive(&prompt, "unrecognized").unwrap_or(0) {
        if writeln!(out, r#"{{"type":"an_event_type_from_a_later_cli"}}"#).is_err() {
            return;
        }
    }

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

    exit_with(exit_code);
}

/// Ends this replay with the code a directive asked for.
fn exit_with(code: usize) -> ! {
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        reason = "a test directive is small by construction"
    )]
    std::process::exit(code as i32);
}

/// Whether this is the first run in this working directory to claim `marker`.
///
/// `create_new` is the whole trick: it succeeds exactly once per directory, so
/// the first process takes the branch and every restart after it does not.
/// Asking and then creating would be two steps a second process could run
/// between.
fn first_run(marker: &str) -> bool {
    std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(marker)
        .is_ok()
}

/// Asks for a completion the way this CLI does, through the satellite's proxy.
///
/// The environment carries where to ask and what to present, and neither is a
/// provider credential: the base URL is the satellite's own listener and the key
/// is the turn's token, which is worth nothing anywhere else.
///
/// Each form asks the way its real CLI asks, path and credential header
/// included, because a stand-in that all asked one way would prove nothing about
/// the other's route through the proxy.
///
/// Whatever comes back is reported on stderr and otherwise ignored. This exists
/// to make the request happen, not to act on the answer.
async fn complete(invocation: Invocation, requests: usize) {
    let (base, key, path, header) = match invocation {
        Invocation::Claude => (
            "ANTHROPIC_BASE_URL",
            "ANTHROPIC_API_KEY",
            "v1/messages",
            "x-api-key",
        ),
        // Codex is pointed at a base URL that already ends in `/v1`, and
        // presents its key as a bearer.
        Invocation::Codex => (
            "OPENAI_BASE_URL",
            "OPENAI_API_KEY",
            "responses",
            "authorization",
        ),
    };

    let Ok(base_url) = std::env::var(base) else {
        eprintln!("{base} is not set, so there is no proxy to ask");
        return;
    };

    let presented = match (invocation, std::env::var(key).unwrap_or_default()) {
        (Invocation::Claude, token) => token,
        (Invocation::Codex, token) => format!("Bearer {token}"),
    };

    for _request in 0..requests {
        let answered = reqwest::Client::new()
            .post(format!("{}/{path}", base_url.trim_end_matches('/')))
            .header(header, &presented)
            .header("content-type", "application/json")
            .body(r#"{"model":"a-model","messages":[]}"#)
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
