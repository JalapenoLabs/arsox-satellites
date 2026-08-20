// Copyright © 2026 Jalapeno Labs

//! Scanning an outgoing push for the thread's secrets, without handing them out.
//!
//! # Why this is not a file the hook reads
//!
//! Every other fact a gate needs is written into the thread's policy file,
//! which the agent may read. A thread's secrets cannot go there, and not for a
//! tidy reason: the set includes the repo's own access token, the GitHub and
//! Jira tokens, and every LLM credential, none of which an agent is ever given.
//! Writing them where the agent can read them, in order to check that the agent
//! is not leaking them, would hand over the very thing being protected. A hash
//! of each is no better: it is an offline cracking target for a credential, and
//! it leaks each secret's length.
//!
//! So the secrets stay with the satellite, which runs as root, and the hook,
//! which runs as the agent, asks a question instead of holding an answer.
//!
//! # The protocol, in full
//!
//! One connection per push, over a unix socket in the thread's own broker
//! directory:
//!
//! 1. the hook writes the bytes of `git log --patch` for the outgoing range,
//! 2. the hook shuts down its writing half,
//! 3. the satellite replies with one line, `clean` or `secret`, and closes.
//!
//! There is no header and no framing, because there is nothing to say: the
//! socket is per thread, so the satellite already knows whose secrets to scan
//! against, and everything sent is content to scan. A protocol with nothing
//! optional in it is a protocol with nothing to get wrong.
//!
//! **The satellite reads and never runs.** It does not spawn git, does not open
//! the repository, and never touches a path the agent named. The one thing it
//! does with the bytes is look for secrets in them.
//!
//! # Bounded memory, whatever the push
//!
//! A push of ten thousand commits is a stream, not a buffer. [`Sieve`] holds one
//! chunk plus an overlap of the longest secret, so the memory cost of scanning
//! a gigabyte is the same as scanning a kilobyte, and a secret that straddles a
//! chunk boundary is still found.
//!
//! # What it cannot see
//!
//! Binary content. git renders a changed binary as `Binary files differ` rather
//! than as bytes, so a credential inside one is not in the text this scans. The
//! same is true of anything the push does not carry: the gate is on the commits
//! being sent, which is what makes it affordable.

use crate::redaction::Redactor;
use std::path::Path;

// The protocol's own vocabulary and bounds. Unix only, like the two halves that
// speak it: a satellite on any other host installs no hook to ask and answers no
// scan, and a constant nothing can reach is one somebody will read as reachable.

/// What the satellite says when it found nothing.
#[cfg(unix)]
const CLEAN: &str = "clean";

/// What the satellite says when it found a secret.
#[cfg(unix)]
const SECRET: &str = "secret";

/// How much of the stream is held at once, before the overlap.
///
/// 64 KiB is a comfortable read size for a socket and small enough that the
/// scan's memory is a rounding error beside the satellite's. Raising it buys
/// fewer syscalls on a very large push and nothing else.
#[cfg(unix)]
const CHUNK: usize = 64 * 1024;

/// How long the hook waits for the satellite to answer.
///
/// Generous, because the satellite is scanning as fast as git can produce, and
/// a very large push legitimately takes a while. It exists so a satellite that
/// has stopped answering ends the push with a refusal rather than a hook that
/// waits forever holding git open.
#[cfg(unix)]
const ANSWER_TIMEOUT: std::time::Duration = std::time::Duration::from_mins(2);

/// What a scan found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing in the outgoing commits matched a secret.
    Clean,

    /// A secret appears in the content or a commit message being pushed.
    SecretFound,
}

/// Scans a stream for a thread's secrets in bounded memory.
///
/// Fed whatever arrives, in whatever sizes it arrives in. It carries the last
/// bytes of each chunk into the next so a secret split across a read is still
/// found, which is the one thing a chunked scan gets wrong when nobody thinks
/// about it.
///
/// The carry is bytes rather than text, so a multi-byte character split by a
/// read is reassembled rather than lost to a replacement character.
///
/// # Examples
///
/// ```
/// use arsox_satellite::broker::scan::Sieve;
/// use arsox_satellite::redaction::Redactor;
///
/// let redactor = Redactor::for_values(vec!["ghp_the_real_token".to_owned()], None);
/// let mut sieve = Sieve::new(&redactor);
///
/// assert!(!sieve.feed(b"+ token = ghp_the_"));
/// assert!(sieve.feed(b"real_token\n"));
/// ```
#[derive(Debug)]
pub struct Sieve<'a> {
    redactor: &'a Redactor,

    /// Bytes held back from the previous chunk, one short of the longest
    /// secret. Any less and a secret spanning the boundary is missed; any more
    /// is text scanned twice for nothing.
    overlap: usize,

    carried: Vec<u8>,
}

impl<'a> Sieve<'a> {
    /// A sieve looking for whatever `redactor` masks.
    #[must_use]
    pub fn new(redactor: &'a Redactor) -> Self {
        Self {
            redactor,
            overlap: redactor.longest_secret().saturating_sub(1),
            carried: Vec::new(),
        }
    }

    /// Feeds the next bytes, and reports whether a secret has appeared.
    ///
    /// Once it answers true the answer does not change, and the caller stops:
    /// the push is refused, and reading the rest of a stream whose verdict is
    /// already decided is work with nowhere to go.
    pub fn feed(&mut self, bytes: &[u8]) -> bool {
        self.carried.extend_from_slice(bytes);

        // Lossy because a patch carries whatever the repository does, and a
        // byte sequence that is not text cannot be a secret this thread
        // declared. What matters is that the conversion never drops a byte
        // silently from the middle of one.
        if self
            .redactor
            .contains_secret(&String::from_utf8_lossy(&self.carried))
        {
            return true;
        }

        let keep = self.carried.len().min(self.overlap);
        self.carried.drain(..self.carried.len() - keep);

        false
    }
}

// The client half: the `pre-push` hook, running as the agent.

/// Sends the outgoing commits to the satellite and returns its verdict.
///
/// `git` is an absolute path rather than a name, because the hook runs with the
/// agent's `PATH`, which on a brokered thread is the shim directory: resolving
/// `git` by name there would put the scan behind the very allowlist the thread
/// may have refused it in.
///
/// git writes straight into the socket, so the content never passes through
/// this process. The satellite is the only thing that reads it, and it reads it
/// once.
///
/// # Errors
///
/// Returns an I/O error when the satellite cannot be reached, git will not run,
/// or the answer is not one this understands. **Every one of them refuses the
/// push**: a scan that did not happen is not a scan that found nothing.
#[cfg(unix)]
pub fn ask(
    socket: &Path,
    git: &Path,
    ranges: &[super::push::ScanRange],
) -> std::io::Result<Outcome> {
    use std::io::Read as _;
    use std::os::fd::OwnedFd;

    let stream = std::os::unix::net::UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(ANSWER_TIMEOUT))?;

    for range in ranges {
        let status = std::process::Command::new(git)
            .args(LOG_ARGUMENTS)
            .args(range.arguments())
            .stdout(std::process::Stdio::from(OwnedFd::from(
                stream.try_clone()?,
            )))
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()?;

        if !status.success() {
            return Err(std::io::Error::other(format!(
                "git could not read the commits this push would send: {status}"
            )));
        }
    }

    // The satellite answers at end of input, so it has to be told there is one.
    stream.shutdown(std::net::Shutdown::Write)?;

    let mut answer = String::new();
    (&stream).read_to_string(&mut answer)?;

    match answer.trim() {
        CLEAN => Ok(Outcome::Clean),
        SECRET => Ok(Outcome::SecretFound),
        unrecognized => Err(std::io::Error::other(format!(
            "the satellite answered `{unrecognized}`, which is not a verdict"
        ))),
    }
}

/// The same, on a platform with no unix sockets.
///
/// Unreachable in practice: a satellite installs a `pre-push` hook only where
/// it can separate privilege, and Windows is not being asked to. The arm exists
/// so the module compiles and is read on the machine it is developed on.
///
/// # Errors
///
/// Always, which refuses the push.
#[cfg(not(unix))]
pub fn ask(
    _socket: &Path,
    _git: &Path,
    _ranges: &[super::push::ScanRange],
) -> std::io::Result<Outcome> {
    Err(std::io::Error::other(
        "scanning a push needs a Unix host, so this push was not scanned",
    ))
}

/// How the outgoing commits are rendered for scanning.
///
/// Commit messages and patch in one walk, because both are channels the README
/// promises to cover and reading them separately would double the cost.
/// Everything configurable about the rendering is turned off: a textconv or
/// external diff driver configured in the repository would decide what the scan
/// gets to see, and the repository's config belongs to the agent.
#[cfg(unix)]
const LOG_ARGUMENTS: [&str; 7] = [
    "--no-pager",
    "log",
    "--format=%B",
    "--patch",
    "--no-color",
    "--no-textconv",
    "--no-ext-diff",
];

// The server half: the satellite, running as root, holding the secrets.

/// The satellite answering one thread's push scans.
///
/// **Lives for one turn.** An agent pushes during a turn and at no other time,
/// and the secrets it is scanned against are the turn's redactor, so binding it
/// with the turn is what keeps one object rather than two that can disagree.
/// A push attempted with no scanner listening is refused, which is the same
/// fail-closed posture every other gate takes.
#[derive(Debug)]
pub struct Scanner {
    socket: std::path::PathBuf,
    listening: tokio::task::JoinHandle<()>,
}

impl Scanner {
    /// Starts answering scans at `socket`, against `redactor`'s secrets.
    ///
    /// A socket file left by a previous turn is removed first: a stale one
    /// refuses the bind, and a scanner that would not start is every push
    /// refused for the rest of the thread's life.
    ///
    /// # Errors
    ///
    /// Returns the underlying I/O error when the socket cannot be bound.
    #[cfg(unix)]
    pub fn bind(socket: std::path::PathBuf, redactor: Redactor) -> std::io::Result<Self> {
        use std::os::unix::fs::PermissionsExt as _;

        drop(std::fs::remove_file(&socket));

        let listener = tokio::net::UnixListener::bind(&socket)?;

        // Connectable by the agent, which is the whole point of it. It carries
        // no secret in either direction: what goes up is content the agent
        // already has, and what comes back is one word.
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o666))?;

        let listening = tokio::spawn(accept(listener, redactor));

        Ok(Self { socket, listening })
    }

    /// The same, on a platform with no unix sockets.
    ///
    /// # Errors
    ///
    /// Always. A satellite there never installs a hook to ask.
    #[cfg(not(unix))]
    pub fn bind(_socket: std::path::PathBuf, _redactor: Redactor) -> std::io::Result<Self> {
        Err(std::io::Error::other(
            "answering push scans needs a Unix host",
        ))
    }
}

impl Drop for Scanner {
    /// Stops listening and takes the socket with it.
    ///
    /// A socket left behind would be connected to by the next turn's hook and
    /// answered by nobody, which is a push that hangs until its timeout rather
    /// than one that is refused immediately.
    fn drop(&mut self) {
        self.listening.abort();
        drop(std::fs::remove_file(&self.socket));
    }
}

/// Answers every connection until the task is dropped.
#[cfg(unix)]
async fn accept(listener: tokio::net::UnixListener, redactor: Redactor) {
    loop {
        match listener.accept().await {
            Ok((stream, _unnamed)) => {
                let redactor = redactor.clone();
                tokio::spawn(async move { answer(stream, &redactor).await });
            }
            Err(error) => {
                tracing::warn!(
                    event.name = "push.scan.accept_failed",
                    "the push scanner stopped accepting connections, so pushes on this \
                     thread will be refused until the next turn rebinds it: {error}",
                );
                return;
            }
        }
    }
}

/// Reads one push and answers with a verdict.
///
/// A stream that ends early gets no answer at all, deliberately. The hook reads
/// a verdict or refuses the push, so silence is the correct thing to say about
/// content that was not fully scanned.
#[cfg(unix)]
async fn answer(mut stream: tokio::net::UnixStream, redactor: &Redactor) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let mut sieve = Sieve::new(redactor);
    let mut chunk = vec![0_u8; CHUNK];
    let mut found = false;

    while !found {
        match stream.read(&mut chunk).await {
            Ok(0) => break,
            Ok(read) => found = sieve.feed(&chunk[..read]),
            Err(error) => {
                tracing::warn!(
                    event.name = "push.scan.read_failed",
                    "could not read the commits a push was sending, so it goes unanswered \
                     and the hook refuses it: {error}",
                );
                return;
            }
        }
    }

    let verdict = if found { SECRET } else { CLEAN };

    if let Err(error) = stream.write_all(format!("{verdict}\n").as_bytes()).await {
        tracing::warn!(
            event.name = "push.scan.answer_failed",
            "scanned a push and could not tell the hook: {error}",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A redactor over exactly these secrets.
    fn over(values: &[&str]) -> Redactor {
        Redactor::for_values(
            values.iter().map(|value| (*value).to_owned()).collect(),
            None,
        )
    }

    #[test]
    fn a_secret_in_one_chunk_is_found() {
        let redactor = over(&["ghp_the_real_token"]);

        assert!(Sieve::new(&redactor).feed(b"+const token = 'ghp_the_real_token'\n"));
    }

    #[test]
    fn a_push_with_nothing_in_it_is_clean() {
        let redactor = over(&["ghp_the_real_token"]);
        let mut sieve = Sieve::new(&redactor);

        assert!(!sieve.feed(b""));
        assert!(!sieve.feed(b"+fn main() {}\n"));
    }

    #[test]
    fn a_secret_split_across_two_reads_is_still_found() {
        // The one thing a chunked scan gets wrong when nobody thinks about it,
        // and the reason the sieve carries an overlap at all.
        let redactor = over(&["ghp_the_real_token"]);
        let mut sieve = Sieve::new(&redactor);

        assert!(!sieve.feed(b"password = ghp_the"));
        assert!(sieve.feed(b"_real_token"));
    }

    #[test]
    fn a_secret_split_one_byte_at_a_time_is_still_found() {
        // The pathological case of the same thing: an overlap one byte short
        // would let a secret through whenever the stream arrived slowly.
        let redactor = over(&["ghp_the_real_token"]);
        let mut sieve = Sieve::new(&redactor);
        let mut found = false;

        for byte in b"leaked ghp_the_real_token here" {
            found = sieve.feed(std::slice::from_ref(byte));
            if found {
                break;
            }
        }

        assert!(found);
    }

    #[test]
    fn the_scan_holds_one_chunk_and_an_overlap_however_much_is_pushed() {
        // The bounded-memory claim, asserted rather than only described. A
        // megabyte of patch must not become a megabyte of buffer.
        let redactor = over(&["ghp_the_real_token"]);
        let mut sieve = Sieve::new(&redactor);

        for _chunk in 0..64 {
            assert!(!sieve.feed(&vec![b'x'; 16 * 1024]));
            assert!(
                sieve.carried.len() < "ghp_the_real_token".len(),
                "the sieve is holding the whole stream"
            );
        }
    }

    #[test]
    fn a_multi_byte_character_split_by_a_read_does_not_hide_what_follows() {
        // The carry is bytes rather than text precisely so this reassembles.
        let redactor = over(&["ghp_the_real_token"]);
        let mut sieve = Sieve::new(&redactor);

        let text = "café ghp_the_real_token".as_bytes();
        let split = 4; // Between the two bytes of `é`.

        assert!(!sieve.feed(&text[..split]));
        assert!(sieve.feed(&text[split..]));
    }

    #[test]
    fn a_thread_with_no_secrets_finds_nothing_and_carries_nothing() {
        // Such a thread is never given a scan socket at all. The sieve still
        // has to be sane, because it is the thing that decides that.
        let redactor = Redactor::none();
        let mut sieve = Sieve::new(&redactor);

        assert!(!sieve.feed(b"anything at all"));
        assert!(sieve.carried.is_empty());
    }
}
