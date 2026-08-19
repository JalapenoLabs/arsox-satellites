// Copyright © 2026 Jalapeno Labs

//! Cloning one repo into a thread's workspace.
//!
//! # Credentials are lent, never left behind
//!
//! A clone needs a credential for exactly as long as it runs, and everything
//! that makes a credential convenient also makes it persistent. The three
//! obvious places to put one are all wrong:
//!
//! - **In the URL.** `git clone` writes the remote URL into the new repository's
//!   `.git/config`, so a token embedded there is on disk for the life of the
//!   workspace, readable by every agent working in it, and pushed into any
//!   diagnostic that prints a remote.
//! - **In argv.** Every process on the box can read another's command line.
//! - **In config.** `git clone -c key=value` persists into the clone. Only
//!   `git -c key=value clone` is scoped to the one invocation, and the
//!   difference is a single token's position on the command line.
//!
//! So a token travels in the child's environment and is read from there by a
//! credential helper passed with `git -c`, and an SSH key is written to a
//! private temporary directory that is removed when the clone returns, whatever
//! the clone returned. `GIT_CONFIG_PARAMETERS` and the environment are both
//! inherited, so submodule clones get the same credential without it being
//! recorded anywhere.

use super::WorkspaceError;
use arsox_sdk::proto::settings::v1::git_auth::Credential;
use arsox_sdk::proto::settings::v1::{Repo, SshKeyPair};
use std::path::{Path, PathBuf};

/// The variable the credential helper reads a personal access token from.
///
/// Named for Arsox but never seen by an agent: it is set on the `git` process
/// after the environment scrub and on nothing else, and a setup command runs in
/// its own scrubbed process.
const TOKEN_ENV: &str = "ARSOX_GIT_TOKEN";

/// The credential helper git runs when a remote asks for a password.
///
/// A helper beginning with `!` is run as a shell command with the operation
/// appended, so this answers `get` and stays silent for `store` and `erase`,
/// which is what keeps the token out of any on-disk credential cache.
///
/// The username is a placeholder because every forge that authenticates with a
/// token ignores it: GitHub, GitLab, and Bitbucket all read the password field.
const TOKEN_HELPER: &str = "!f() { test \"$1\" = get && \
     printf 'username=x-access-token\\npassword=%s\\n' \"$ARSOX_GIT_TOKEN\"; }; f";

/// Longest directory name accepted under `repos/`.
///
/// Every filesystem the satellite runs on caps a path component at 255 bytes,
/// and a name that long is a mistake rather than a repository.
const MAX_NAME: usize = 255;

/// A clone that did not complete, and the evidence for why.
#[derive(Debug, Clone)]
pub(super) struct CloneFailed {
    pub message: String,

    /// What git said, with any credential already removed.
    pub output: String,
}

/// Clones `repo` into `into`, with submodules.
///
/// The base branch is checked out when the repo declares one, so the clone sits
/// on the reference an integration branch is cut from rather than on whatever
/// the remote's `HEAD` happens to point at.
///
/// # Errors
///
/// Returns [`CloneFailed`] when git exits nonzero or cannot be launched at all.
/// Neither is a satellite error: both are reported as a `REPO_CLONE_FAILED`
/// incident and end the thread's provisioning.
pub(super) async fn clone(repo: &Repo, into: &Path) -> Result<(), CloneFailed> {
    let credential = Credentials::lend(repo).map_err(|error| CloneFailed {
        message: format!(
            "could not stage the credential for {}",
            redact_url(&repo.url)
        ),
        output: error.to_string(),
    })?;

    let mut process = crate::harness::spawn::scrubbed_command("git");

    // Cleared before anything is added, so a helper configured on the host
    // cannot answer for a thread that brought its own credential, or quietly
    // answer for one that brought none.
    process.arg("-c").arg("credential.helper=");
    credential.apply(&mut process);

    // Without this a remote that wants a credential we do not have blocks on a
    // terminal that is not there, and a satellite has no way to notice.
    process.env("GIT_TERMINAL_PROMPT", "0");

    process.arg("clone").arg("--recurse-submodules");
    if let Some(branch) = repo.base_branch.as_deref().filter(|it| !it.is_empty()) {
        process.arg("--branch").arg(branch);
    }
    // `--` so a URL that begins with a dash is a URL rather than an option.
    process.arg("--").arg(&repo.url).arg(into);

    let output = process.output().await.map_err(|error| CloneFailed {
        message: "could not launch git".to_owned(),
        output: error.to_string(),
    })?;

    if output.status.success() {
        return Ok(());
    }

    let said = String::from_utf8_lossy(&output.stderr);

    Err(CloneFailed {
        message: format!(
            "git clone of {} exited with {}",
            redact_url(&repo.url),
            output.status
        ),
        output: credential.redact(&redact_url(said.trim())),
    })
}

/// Where a repo is cloned under `repos/`.
///
/// The declared name when there is one, and otherwise the last segment of the
/// URL with any `.git` suffix removed, so a caller who only filled in a URL
/// still gets the directory a human would have named.
///
/// # Errors
///
/// Returns [`WorkspaceError::UnsafeRepoName`] for a name that could reach
/// outside `repos/`. Unlike a thread id, this one does come from the client, so
/// the check is the only thing standing between a settings field and the volume.
pub fn directory_name(repo: &Repo) -> Result<String, WorkspaceError> {
    let declared = repo.name.trim();
    let name = if declared.is_empty() {
        derive_name(&repo.url)
    } else {
        declared
    };

    if !is_safe_name(name) {
        return Err(WorkspaceError::UnsafeRepoName(name.to_owned()));
    }

    Ok(name.to_owned())
}

/// The directory a URL names, before it is checked.
fn derive_name(url: &str) -> &str {
    let trimmed = url.trim().trim_end_matches('/');

    // Split on both separators because an SSH remote writes the path after a
    // colon: `git@github.com:JalapenoLabs/arsox-satellites.git`.
    let last = trimmed.rsplit(['/', ':']).next().unwrap_or(trimmed);

    last.strip_suffix(".git").unwrap_or(last)
}

/// Whether a name is a single path component that cannot escape its parent.
fn is_safe_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME
        && name != "."
        && name != ".."
        && name.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_')
        })
}

/// Removes a credential somebody wrote into a URL themselves.
///
/// Arsox never puts one there, but a caller may hand over
/// `https://user:token@host/repo.git`, and that URL reaches tracing and incident
/// messages. Redacting on the way out costs a scan and removes a whole class of
/// accidental disclosure.
pub(super) fn redact_url(text: &str) -> String {
    let mut redacted = String::with_capacity(text.len());

    for (index, part) in text.split("://").enumerate() {
        if index > 0 {
            redacted.push_str("://");

            // Userinfo ends at the first `@` and cannot span a path separator,
            // so anything before a `/` and after an `@` is a credential.
            if let Some(at) = part.find('@')
                && !part[..at].contains('/')
            {
                redacted.push_str("******@");
                redacted.push_str(&part[at + 1..]);
                continue;
            }
        }
        redacted.push_str(part);
    }

    redacted
}

/// A credential made available to one git invocation and to nothing else.
enum Credentials {
    /// The repo is public, or the remote needs no credential.
    None,

    /// An identity file git points ssh at, removed when this drops.
    SshKey(TempKey),

    /// A token the credential helper reads from the environment.
    Token(String),
}

impl Credentials {
    /// Stages whatever `repo` declared, ready to hand to a git process.
    fn lend(repo: &Repo) -> std::io::Result<Self> {
        match repo.auth.as_ref().and_then(|auth| auth.credential.as_ref()) {
            Some(Credential::SshKey(pair)) => Ok(Self::SshKey(TempKey::write(pair)?)),
            Some(Credential::PersonalAccessToken(secret)) => Ok(secret
                .value
                .as_deref()
                .filter(|token| !token.is_empty())
                .map_or(Self::None, |token| Self::Token(token.to_owned()))),
            None => Ok(Self::None),
        }
    }

    /// Attaches the credential to a git process without recording it anywhere.
    fn apply(&self, process: &mut tokio::process::Command) {
        match self {
            Self::None => {}
            Self::SshKey(key) => {
                process.env("GIT_SSH_COMMAND", key.ssh_command());
            }
            Self::Token(token) => {
                // `git -c`, never `git clone -c`. The first is scoped to this
                // invocation and propagates to submodule clones through
                // `GIT_CONFIG_PARAMETERS`; the second is written into the new
                // repository's own config and outlives the clone entirely.
                process
                    .arg("-c")
                    .arg(format!("credential.helper={TOKEN_HELPER}"));
                process.env(TOKEN_ENV, token);
            }
        }
    }

    /// Masks the credential wherever it appears in captured output.
    ///
    /// git does not print a token it was handed, but a remote's error body and a
    /// setup command's log are not ours to predict, and this text is on its way
    /// to an incident row that outlives the thread.
    fn redact(&self, text: &str) -> String {
        match self {
            Self::Token(token) if !token.is_empty() => text.replace(token, "******"),
            _nothing_to_hide => text.to_owned(),
        }
    }
}

/// A private key on disk for the life of one clone.
///
/// The whole directory is removed on drop rather than the key file alone, so the
/// `known_hosts` ssh writes beside it goes with it and nothing survives the
/// operation that needed it.
struct TempKey {
    directory: PathBuf,
}

impl TempKey {
    /// Writes `pair`'s private key where only this process can read it.
    fn write(pair: &SshKeyPair) -> std::io::Result<Self> {
        let directory = std::env::temp_dir().join(format!("arsox-git-{}", uuid::Uuid::now_v7()));
        create_private_directory(&directory)?;

        let key = Self { directory };

        let mut material = pair
            .private_key
            .as_ref()
            .and_then(|secret| secret.value.clone())
            .unwrap_or_default();

        // OpenSSH rejects a key whose final line is unterminated, and a key
        // pasted into a JSON field routinely loses its trailing newline.
        if !material.ends_with('\n') {
            material.push('\n');
        }

        write_private_file(&key.identity(), material.as_bytes())?;

        Ok(key)
    }

    fn identity(&self) -> PathBuf {
        self.directory.join("identity")
    }

    /// What ssh is told to do with the key.
    ///
    /// `IdentitiesOnly` stops ssh offering an agent key ahead of this one, which
    /// on a host with several loaded keys is how a clone ends up authenticating
    /// as somebody else. `BatchMode` turns every prompt into a failure, because
    /// a satellite has no terminal to answer one.
    ///
    /// Host keys are accepted on first use and pinned in a `known_hosts` that
    /// lives and dies with this directory. The contract carries no host key
    /// field to pin against instead, and refusing every unknown host would mean
    /// no SSH remote ever clones.
    fn ssh_command(&self) -> String {
        format!(
            "ssh -i \"{}\" -o IdentitiesOnly=yes -o BatchMode=yes \
             -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile=\"{}\"",
            self.identity().display(),
            self.directory.join("known_hosts").display()
        )
    }
}

impl Drop for TempKey {
    fn drop(&mut self) {
        // Synchronous because `Drop` cannot await, and deliberately best effort:
        // a key that will not delete is worth neither a panic nor a failed
        // clone, and the directory is unreachable to anything but this process.
        drop(std::fs::remove_dir_all(&self.directory));
    }
}

/// Creates a directory only its owner can enter.
#[cfg(unix)]
fn create_private_directory(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;

    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
}

/// Creates a directory only its owner can enter.
///
/// Windows inherits the temporary directory's ACL, which is already per user.
/// The satellite runs on Linux; this arm exists so the same code path is
/// exercised on a developer machine rather than first meeting CI.
#[cfg(not(unix))]
fn create_private_directory(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)
}

/// Writes a file only its owner can read.
fn write_private_file(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    restrict_to_owner(&mut options);

    options.open(path)?.write_all(contents)
}

/// Asks for a file only its owner can read.
///
/// The mode is set as the file is created rather than afterwards, because ssh
/// refuses to use a key whose permissions are loose and a chmod that follows the
/// write leaves a window where they are.
#[cfg(unix)]
fn restrict_to_owner(options: &mut std::fs::OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt as _;

    options.mode(0o600);
}

/// The same, where permissions are an ACL the temporary directory already
/// carries.
#[cfg(not(unix))]
fn restrict_to_owner(_options: &mut std::fs::OpenOptions) {}

#[cfg(test)]
mod tests {
    use super::*;
    use arsox_sdk::proto::common::v1::Secret;
    use arsox_sdk::proto::settings::v1::GitAuth;

    fn repo(name: &str, url: &str) -> Repo {
        Repo {
            name: name.to_owned(),
            url: url.to_owned(),
            ..Repo::default()
        }
    }

    /// Asserts nobody but the owner can read a staged key.
    ///
    /// ssh refuses to use a key with looser permissions than this, so the
    /// assertion is about whether an SSH remote clones at all, not only about
    /// who could have read the key.
    #[cfg(unix)]
    fn assert_owner_only(path: &Path) {
        use std::os::unix::fs::PermissionsExt as _;

        let mode = std::fs::metadata(path)
            .expect("should stat")
            .permissions()
            .mode();

        assert_eq!(mode & 0o777, 0o600, "ssh refuses a key others can read");
    }

    /// The same, where permissions are an ACL rather than a mode.
    #[cfg(not(unix))]
    fn assert_owner_only(path: &Path) {
        assert!(path.is_file());
    }

    #[test]
    fn a_declared_name_is_the_directory() {
        let named = directory_name(&repo("api", "https://github.com/acme/service.git"))
            .expect("a plain name is safe");

        assert_eq!(named, "api");
    }

    #[test]
    fn an_undeclared_name_comes_from_the_url() {
        // Both remote forms, because an SSH remote writes its path after a colon
        // rather than after a slash.
        for url in [
            "https://github.com/JalapenoLabs/arsox-satellites.git",
            "https://github.com/JalapenoLabs/arsox-satellites",
            "git@github.com:JalapenoLabs/arsox-satellites.git",
            "ssh://git@github.com/JalapenoLabs/arsox-satellites.git",
            "file:///srv/mirrors/arsox-satellites.git/",
        ] {
            assert_eq!(
                directory_name(&repo("", url)).expect("should derive"),
                "arsox-satellites",
                "{url} should name its own directory"
            );
        }
    }

    #[test]
    fn a_name_that_could_escape_the_repos_directory_is_refused() {
        // This one does come from the client, unlike a thread id, so the check
        // is the only thing between a settings field and the whole volume.
        for hostile in ["..", ".", "../../etc", "api/../..", "a\\b", "/etc/passwd"] {
            assert!(
                directory_name(&repo(hostile, "https://example.com/x.git")).is_err(),
                "{hostile} should be refused"
            );
        }
    }

    #[test]
    fn a_credential_written_into_a_url_is_masked_before_it_is_logged() {
        assert_eq!(
            redact_url("https://user:ghp_secret@github.com/acme/api.git"),
            "https://******@github.com/acme/api.git"
        );
        assert_eq!(
            redact_url("fatal: could not read from https://x:tok@host/a.git"),
            "fatal: could not read from https://******@host/a.git"
        );
        // A URL with no userinfo is left exactly as it was, and an `@` further
        // down the path is part of the path rather than a credential.
        assert_eq!(
            redact_url("https://github.com/acme/api.git"),
            "https://github.com/acme/api.git"
        );
        assert_eq!(
            redact_url("https://github.com/acme/@scope.git"),
            "https://github.com/acme/@scope.git"
        );
    }

    #[test]
    fn a_token_is_masked_wherever_it_surfaces_in_captured_output() {
        let staged = Credentials::Token("ghp_the_real_token".to_owned());

        let redacted = staged.redact("remote: rejected ghp_the_real_token");

        assert!(!redacted.contains("ghp_the_real_token"));
        assert!(redacted.contains("******"));
    }

    #[test]
    fn a_token_reaches_git_through_the_environment_rather_than_argv_or_config() {
        // argv is readable by every process on the box, and config written by
        // `git clone -c` persists into the clone. Neither may ever hold this.
        let staged = Credentials::Token("ghp_the_real_token".to_owned());
        let mut process = tokio::process::Command::new("git");
        staged.apply(&mut process);

        let rendered = format!("{process:?}");

        assert!(
            !rendered.contains("ghp_the_real_token"),
            "the token appeared in the command line: {rendered}"
        );
        assert!(
            rendered.contains("credential.helper") || rendered.contains("!f()"),
            "the helper that reads it should be on the command line"
        );
    }

    #[test]
    fn an_ssh_key_lives_in_a_private_directory_and_leaves_with_the_clone() {
        let pair = SshKeyPair {
            private_key: Some(Secret {
                value: Some("-----BEGIN OPENSSH PRIVATE KEY-----".to_owned()),
                display: None,
            }),
            public_key: None,
        };

        let key = TempKey::write(&pair).expect("should stage the key");
        let identity = key.identity();

        let written = std::fs::read_to_string(&identity).expect("should read back");
        // OpenSSH rejects a key whose final line is unterminated.
        assert!(written.ends_with('\n'));

        assert_owner_only(&identity);

        assert!(key.ssh_command().contains("IdentitiesOnly=yes"));
        assert!(key.ssh_command().contains("BatchMode=yes"));

        // Dropped where the clone would have returned, which is the moment the
        // key is supposed to stop existing.
        drop(key);

        assert!(
            !identity.exists(),
            "the key must not outlive the clone that borrowed it"
        );
    }

    #[test]
    fn an_absent_credential_stages_nothing() {
        let staged = Credentials::lend(&repo("api", "https://example.com/x.git"))
            .expect("no auth should stage cleanly");

        assert!(matches!(staged, Credentials::None));
    }

    #[test]
    fn an_empty_token_is_no_credential_rather_than_an_empty_one() {
        // An empty password answers a prompt with nothing and turns a clean
        // "this repo is public" into an authentication failure.
        let staged = Credentials::lend(&Repo {
            auth: Some(GitAuth {
                credential: Some(Credential::PersonalAccessToken(Secret {
                    value: Some(String::new()),
                    display: None,
                })),
            }),
            ..repo("api", "https://example.com/x.git")
        })
        .expect("should stage");

        assert!(matches!(staged, Credentials::None));
    }
}
