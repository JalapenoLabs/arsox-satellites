// Copyright © 2026 Jalapeno Labs

//! The deny spool: how a refusal reaches the satellite from the shim.
//!
//! A shim runs as the agent and the satellite runs as root, so a refusal has to
//! cross a privilege boundary to become an incident. A root-owned unix socket
//! would be the lower-latency answer and it was the other candidate. **The spool
//! is what is built, and the reason is attribution.** A socket message carries no
//! turn id, so the satellite would have to guess which turn a refusal belonged to
//! from its timing. The runner drains this between sessions and knows exactly
//! which turn was running, so every refusal lands on its own turn.
//!
//! The cost is that a refusal reaches the stream when its session ends rather
//! than the instant it happens. For a record whose consumer is an operator
//! reading incidents afterwards, that is not worth a listener, a framing, a
//! connect timeout in the shim, and a decision about what a shim does when the
//! socket is gone.
//!
//! # This is untrusted input
//!
//! The spool is writable by the agent, which is the entire point of it. So an
//! agent can drop junk in it, or fabricate a refusal that never happened. That
//! is a nuisance rather than an escalation: everything it can fabricate is an
//! incident about itself. A record that does not parse is discarded with a
//! warning rather than failing a drain.
//!
//! What it cannot do is remove one. The spool is `0733` with the sticky bit, so
//! the agent may create a file and may neither list the directory nor unlink a
//! record it did not write. See [`super::install`].

use arsox_sdk::proto::common::v1::Timestamp;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// One command a shim refused, as it travels from the agent to the satellite.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Denial {
    pub thread_id: String,

    /// The command as it was invoked, without its arguments.
    pub name: String,

    /// The whole invocation, the command first.
    ///
    /// This is what reaches `details.argv` on the incident, which is the field
    /// the README promises and the one an operator widening an allowlist reads.
    pub argv: Vec<String>,

    /// Completes "the command was refused because it ...".
    pub reason: String,

    /// When the shim refused, split the way a `Timestamp` splits one.
    pub occurred_at_seconds: i64,
    pub occurred_at_nanos: u32,
}

impl Denial {
    /// Records a refusal as having happened now.
    #[must_use]
    pub fn now(thread_id: &str, name: &str, argv: Vec<String>, reason: &str) -> Self {
        let since_epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO);

        Self {
            thread_id: thread_id.to_owned(),
            name: name.to_owned(),
            argv,
            reason: reason.to_owned(),
            occurred_at_seconds: i64::try_from(since_epoch.as_secs()).unwrap_or(i64::MAX),
            occurred_at_nanos: since_epoch.subsec_nanos(),
        }
    }

    /// When the refusal happened, in the shape an incident carries.
    ///
    /// The shim's clock rather than the drain's. The two are the same clock, and
    /// stamping at drain time would report every refusal in a session as having
    /// happened when the session ended.
    #[must_use]
    pub fn occurred_at(&self) -> Timestamp {
        let seconds = u64::try_from(self.occurred_at_seconds).unwrap_or(0);

        Timestamp::from_system_time(
            UNIX_EPOCH + Duration::new(seconds, self.occurred_at_nanos),
            arsox_sdk::helpers::DEFAULT_TIMEZONE,
        )
    }

    /// The invocation as one readable line, for a message or a log.
    #[must_use]
    pub fn invocation(&self) -> String {
        self.argv.join(" ")
    }
}

/// Writes one refusal where the satellite will find it.
///
/// One file per refusal, created exclusively and named by a `UUIDv7`, so two shims
/// refused at the same instant cannot overwrite each other and a drain reads
/// them in the order they happened.
///
/// # Errors
///
/// Returns the underlying I/O error when the record cannot be written. The shim
/// refuses the command either way: a spool that will not take a record is a
/// refusal nobody hears about, not a command that gets to run.
pub fn record(spool: &Path, denial: &Denial) -> std::io::Result<()> {
    let path = spool.join(format!("{}.json", uuid::Uuid::now_v7()));
    let body = serde_json::to_vec(denial).map_err(std::io::Error::other)?;

    // `create_new` rather than `write`, so a name that somehow already exists
    // fails rather than replacing a refusal that has not been drained yet.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;

    std::io::Write::write_all(&mut file, &body)
}

/// Reads and clears every refusal a thread has accumulated.
///
/// Returns them oldest first, which `UUIDv7` file names already sort into.
///
/// Never fails. A spool that cannot be read is a thread that was not brokered or
/// one whose directory has been collected, and neither is worth propagating out
/// of a drain that runs at the end of every session.
#[must_use]
pub fn drain(spool: &Path) -> Vec<Denial> {
    let Ok(entries) = std::fs::read_dir(spool) else {
        return Vec::new();
    };

    let mut files: Vec<std::path::PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    files.sort();

    let mut denials = Vec::new();

    for path in files {
        let parsed = std::fs::read(&path)
            .ok()
            .and_then(|body| serde_json::from_slice::<Denial>(&body).ok());

        // Removed whether or not it parsed. A record nothing can read is not
        // going to become readable, and leaving it would have every later drain
        // rediscover the same unparseable file.
        drop(std::fs::remove_file(&path));

        if let Some(denial) = parsed {
            denials.push(denial);
        } else {
            tracing::warn!(
                event.name = "broker.denial.unreadable",
                file.path = %path.display(),
                "discarding a deny record that did not parse: the spool is agent-writable, \
                 so this is a record somebody wrote by hand rather than a shim's",
            );
        }
    }

    denials
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A spool directory of this test's own.
    fn spool() -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("arsox-spool-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&path).expect("should create");
        path
    }

    fn denial(name: &str, argv: &[&str]) -> Denial {
        Denial::now(
            "019fd32f-a25f-7611-a4fe-c93cc2a6d782",
            name,
            argv.iter().copied().map(str::to_owned).collect(),
            "is not on this thread's exec allowlist",
        )
    }

    #[test]
    fn a_refusal_survives_the_trip_from_the_shim_to_the_satellite() {
        // The whole channel: the shim writes as the agent, the satellite reads
        // as root, and the argv the operator needs is on the other side.
        let spool = spool();
        let written = denial("docker", &["docker", "build", "."]);

        record(&spool, &written).expect("should record");
        let drained = drain(&spool);

        assert_eq!(drained, vec![written]);

        drop(std::fs::remove_dir_all(&spool));
    }

    #[test]
    fn draining_a_spool_empties_it() {
        // Otherwise every session after the first would re-report every refusal
        // that came before it.
        let spool = spool();

        record(&spool, &denial("git", &["git", "push"])).expect("should record");

        assert_eq!(drain(&spool).len(), 1);
        assert!(drain(&spool).is_empty());

        drop(std::fs::remove_dir_all(&spool));
    }

    #[test]
    fn refusals_are_drained_in_the_order_they_happened() {
        let spool = spool();

        for command in ["first", "second", "third"] {
            record(&spool, &denial(command, &[command])).expect("should record");
        }

        let names: Vec<String> = drain(&spool)
            .into_iter()
            .map(|refused| refused.name)
            .collect();

        assert_eq!(names, ["first", "second", "third"]);

        drop(std::fs::remove_dir_all(&spool));
    }

    #[test]
    fn a_record_an_agent_wrote_by_hand_is_discarded_rather_than_believed() {
        // The spool is agent-writable by construction, so junk in it is
        // expected. It must not take down a drain, and it must not survive one.
        let spool = spool();
        std::fs::write(spool.join("junk.json"), b"not json at all").expect("should write");
        record(&spool, &denial("git", &["git", "push"])).expect("should record");

        let drained = drain(&spool);

        assert_eq!(drained.len(), 1, "the real record still arrives");
        assert!(drain(&spool).is_empty(), "and the junk did not survive");

        drop(std::fs::remove_dir_all(&spool));
    }

    #[test]
    fn draining_a_spool_that_does_not_exist_is_empty_rather_than_an_error() {
        // An unbrokered thread has no spool, and every session drains anyway.
        assert!(drain(&std::env::temp_dir().join("arsox-no-such-spool-019fd32f")).is_empty());
    }

    #[test]
    fn a_refusal_carries_the_moment_the_shim_refused() {
        // Stamped at the shim rather than at the drain: otherwise every refusal
        // in a session would report the moment the session ended.
        let refused = denial("git", &["git", "push"]);
        let stamped = refused.occurred_at();

        assert!(stamped.epoch_seconds > 1_700_000_000, "{stamped:?}");
        assert_eq!(stamped.epoch_seconds, refused.occurred_at_seconds);
    }

    #[test]
    fn an_invocation_reads_as_the_command_line_that_was_refused() {
        assert_eq!(
            denial("docker", &["docker", "build", "."]).invocation(),
            "docker build ."
        );
    }
}
