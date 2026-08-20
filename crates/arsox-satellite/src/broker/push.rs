// Copyright © 2026 Jalapeno Labs

//! What a push is, and what a thread's policy says about one.
//!
//! Everything here is pure. It takes the lines git writes to a `pre-push`
//! hook's stdin and a thread's push policy, and it answers with a verdict and
//! the commit ranges still to be scanned. No filesystem, no git, no process.
//!
//! # What git hands the hook
//!
//! One line per ref being updated, four whitespace-separated fields:
//!
//! ```text
//! refs/heads/topic <local sha> refs/heads/topic <remote sha>
//! ```
//!
//! The remote sha is all zeros when the remote has never seen the ref, and the
//! local sha is all zeros when the push deletes it. Both are the cases every
//! naive reading of this format gets wrong, so both are named types here rather
//! than a comparison somebody remembers to write.
//!
//! # The order the checks run in
//!
//! Cheapest and broadest first: a thread that denies pushing refuses every ref
//! without looking at any of them, a protected ref refuses without reading a
//! commit, and only a push that survives both is worth scanning. That ordering
//! is also the one that gives the most useful refusal: told that pushing is off
//! for the thread, nobody goes looking for which branch was the problem.

use std::path::PathBuf;

/// One ref update, as one line of the hook's stdin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefUpdate {
    /// The local ref being sent, or empty when the push deletes a remote ref.
    pub local_ref: String,
    pub local_sha: String,

    /// The ref as it will exist on the remote. This is what a protected entry
    /// is matched against: what the push does to the remote is the whole
    /// question, and the local name it came from is not.
    pub remote_ref: String,
    pub remote_sha: String,
}

impl RefUpdate {
    /// Whether this push removes the remote ref rather than updating it.
    ///
    /// A delete sends no objects, so there is nothing in it to scan. It is
    /// still refused by [`PushPolicy::protects`], because deleting a protected
    /// branch is the most destructive thing that can be done to one.
    #[must_use]
    pub fn is_delete(&self) -> bool {
        is_zero(&self.local_sha)
    }

    /// Whether the remote has never seen this ref.
    #[must_use]
    pub fn is_new(&self) -> bool {
        is_zero(&self.remote_sha)
    }

    /// The commits this update would send, or `None` when it sends none.
    #[must_use]
    pub fn range(&self) -> Option<ScanRange> {
        if self.is_delete() {
            return None;
        }

        if self.is_new() {
            return Some(ScanRange::Unseen {
                tip: self.local_sha.clone(),
            });
        }

        Some(ScanRange::Update {
            from: self.remote_sha.clone(),
            to: self.local_sha.clone(),
        })
    }
}

/// Whether a sha is git's "this ref does not exist" placeholder.
///
/// Written as "every character is zero" rather than as a comparison against a
/// literal, because a repository using SHA-256 spells the same absence with 64
/// zeros instead of 40, and a hard-coded forty would read a new-branch push as
/// an ordinary update in exactly the repositories nobody tests on.
fn is_zero(sha: &str) -> bool {
    !sha.is_empty() && sha.bytes().all(|byte| byte == b'0')
}

/// The commits one ref update would send.
///
/// **This is the bound on what a scan costs.** A push of ten thousand commits
/// scans ten thousand commits; a push of one scans one. Neither reads the
/// repository's history, which is the difference between a hook that runs in a
/// moment and one that walks a monorepo from its first commit on every push.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanRange {
    /// The remote has the ref, so the new commits are `from..to`.
    Update { from: String, to: String },

    /// The remote has never seen the ref.
    ///
    /// `tip --not --remotes` rather than the whole history behind `tip`:
    /// a branch cut from `main` shares every commit `main` already has, and
    /// scanning those again would make the first push of a topic branch cost a
    /// walk of the entire repository.
    Unseen { tip: String },
}

impl ScanRange {
    /// The revision arguments that select exactly this range.
    ///
    /// Rendered for `git log`, which takes both forms and can emit the commit
    /// messages and the patch in one walk.
    #[must_use]
    pub fn arguments(&self) -> Vec<String> {
        match self {
            Self::Update { from, to } => vec![format!("{from}..{to}")],
            Self::Unseen { tip } => {
                vec![tip.clone(), "--not".to_owned(), "--remotes".to_owned()]
            }
        }
    }
}

/// What a thread allows a push to do, as the hook reads it.
///
/// Written into the thread's policy file beside the shim directory, which the
/// agent may read and may not write. **Nothing secret is in it.** Whether a
/// thread may push, and which refs it may not push to, are facts about the
/// thread's configuration rather than credentials, and an agent learning them
/// is an agent learning what it is already going to be told the moment it tries.
/// The secrets a push is scanned for are deliberately elsewhere: see
/// [`super::scan`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PushPolicy {
    /// Whether this thread may push at all.
    pub allowed: bool,

    /// Refs no push may touch, as the thread declared them.
    pub protected: Vec<String>,

    /// Where the satellite answers whether a push carries a secret.
    ///
    /// Absent when the thread declared no secret worth scanning for, which is
    /// what keeps the common thread's push free of a round trip that could only
    /// ever answer "nothing found".
    pub scan: Option<PathBuf>,
}

impl PushPolicy {
    /// Resolves the push half of a thread's policy, or `None` when there is
    /// nothing to gate.
    ///
    /// A thread that may push, protects no ref, and declared no secret has
    /// asked for no gate, and installing one would put a process start on every
    /// push to enforce nothing. `scans` is whether the thread has any secret to
    /// scan against, which is a question for the redactor rather than for the
    /// permissions.
    #[must_use]
    pub fn for_thread(
        permissions: Option<&arsox_sdk::proto::settings::v1::Permissions>,
        scan: Option<PathBuf>,
        scans: bool,
    ) -> Option<Self> {
        // Absent means allowed, per the contract. Read as proto3's zero value
        // this would deny every push on every thread that said nothing, which
        // is the failure mode `optional` exists to prevent.
        let allowed = permissions.and_then(|it| it.allow_git_push).unwrap_or(true);

        let protected: Vec<String> = permissions
            .map(|it| it.protected_branches.as_slice())
            .unwrap_or_default()
            .iter()
            .map(|entry| entry.trim())
            .filter(|entry| !entry.is_empty())
            .map(str::to_owned)
            .collect();

        if allowed && protected.is_empty() && !scans {
            return None;
        }

        Some(Self {
            allowed,
            protected,
            scan: scans.then_some(scan).flatten(),
        })
    }

    /// Whether this policy protects `reference`.
    ///
    /// An entry matches a ref written either way round: `main` and
    /// `refs/heads/main` name the same branch and an operator should not have
    /// to know which spelling the hook sees. There is **no globbing**, which is
    /// the same reading [`super::entry_permits`] gives an exec entry: a control
    /// whose matching is a pattern language is one whose refusals have to be
    /// reasoned about rather than read.
    #[must_use]
    pub fn protects(&self, reference: &str) -> bool {
        let short = short_name(reference);

        self.protected
            .iter()
            .any(|entry| entry == reference || entry == short || short_name(entry) == reference)
    }
}

/// A ref without its `refs/heads/` or `refs/tags/` prefix.
fn short_name(reference: &str) -> &str {
    reference
        .strip_prefix("refs/heads/")
        .or_else(|| reference.strip_prefix("refs/tags/"))
        .unwrap_or(reference)
}

/// What the policy says about one push, before anything has been read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing in the policy refuses it. These are the commits still to scan,
    /// which is empty for a push that only deletes refs.
    Permitted(Vec<ScanRange>),

    /// Pushing is off for this thread, whatever the ref.
    PushDenied,

    /// The named ref is protected. Carries the ref rather than the branch, so
    /// `details.ref` on the incident says exactly what the hook matched.
    BranchProtected(String),
}

/// Decides one push against a thread's policy.
///
/// Separated from the hook that runs it so the decision can be asserted without
/// a repository, a remote, or a hook, which is the same split
/// [`super::shim::verdict_for`] makes for the exec broker.
#[must_use]
pub fn verdict_for(policy: &PushPolicy, updates: &[RefUpdate]) -> Verdict {
    if !policy.allowed {
        return Verdict::PushDenied;
    }

    // Any protected ref refuses the whole push rather than the one ref. git
    // gives the hook one yes or no for the invocation, so letting the other
    // refs through is not available, and a partial push nobody asked for would
    // be a worse outcome than a refusal that names what to drop.
    if let Some(protected) = updates
        .iter()
        .find(|update| policy.protects(&update.remote_ref))
    {
        return Verdict::BranchProtected(protected.remote_ref.clone());
    }

    Verdict::Permitted(updates.iter().filter_map(RefUpdate::range).collect())
}

/// Reads the lines git writes to a `pre-push` hook's stdin.
///
/// Returns `None` for input that is not the four-field format, which the hook
/// treats as a satellite fault and fails closed on. Guessing at a line whose
/// shape is unexpected is the one thing this must not do: a misread ref is a
/// protected branch that looks unprotected.
///
/// Blank lines are skipped. git does not write them, and a trailing newline is
/// otherwise a parse failure that refuses every push on the satellite.
#[must_use]
pub fn updates_from(stdin: &str) -> Option<Vec<RefUpdate>> {
    let mut updates = Vec::new();

    for line in stdin.lines() {
        if line.trim().is_empty() {
            continue;
        }

        let mut fields = line.split_whitespace();
        let (Some(local_ref), Some(local_sha), Some(remote_ref), Some(remote_sha)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            return None;
        };

        updates.push(RefUpdate {
            local_ref: local_ref.to_owned(),
            local_sha: local_sha.to_owned(),
            remote_ref: remote_ref.to_owned(),
            remote_sha: remote_sha.to_owned(),
        });
    }

    Some(updates)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arsox_sdk::proto::settings::v1::Permissions;

    /// The forty zeros git writes for a ref that does not exist.
    const ABSENT: &str = "0000000000000000000000000000000000000000";
    const LOCAL: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f9012345678";
    const REMOTE: &str = "9876543210fedcba9876543210fedcba98765432";

    /// A thread's push policy, with only the permissions varying.
    fn policy(allow: Option<bool>, protected: &[&str]) -> Option<PushPolicy> {
        PushPolicy::for_thread(
            Some(&Permissions {
                allow_git_push: allow,
                protected_branches: protected.iter().copied().map(str::to_owned).collect(),
                ..Permissions::default()
            }),
            None,
            false,
        )
    }

    /// One update of `refs/heads/topic`.
    fn update(remote_ref: &str, local_sha: &str, remote_sha: &str) -> RefUpdate {
        RefUpdate {
            local_ref: remote_ref.to_owned(),
            local_sha: local_sha.to_owned(),
            remote_ref: remote_ref.to_owned(),
            remote_sha: remote_sha.to_owned(),
        }
    }

    #[test]
    fn a_thread_with_nothing_to_enforce_gets_no_push_gate() {
        // The opt-in claim. A thread that may push, protects nothing, and
        // declared no secret would pay a process start per push to enforce
        // nothing at all.
        assert!(policy(None, &[]).is_none());
        assert!(policy(Some(true), &[]).is_none());
        assert!(PushPolicy::for_thread(None, None, false).is_none());
    }

    #[test]
    fn any_one_of_the_three_reasons_earns_the_gate() {
        assert!(policy(Some(false), &[]).is_some());
        assert!(policy(None, &["main"]).is_some());
        assert!(PushPolicy::for_thread(None, None, true).is_some());
    }

    #[test]
    fn an_absent_permission_means_pushing_is_allowed() {
        // Read as proto3's zero value this would deny every push on every
        // thread that said nothing about pushing.
        let permitted = PushPolicy::for_thread(
            Some(&Permissions {
                allow_git_push: None,
                protected_branches: vec!["main".to_owned()],
                ..Permissions::default()
            }),
            None,
            false,
        )
        .expect("a protected branch earns the gate");

        assert!(permitted.allowed);
    }

    #[test]
    fn a_thread_that_denies_pushing_refuses_before_it_reads_a_ref() {
        let denied = policy(Some(false), &[]).expect("denying pushing earns the gate");

        assert_eq!(
            verdict_for(&denied, &[update("refs/heads/topic", LOCAL, REMOTE)]),
            Verdict::PushDenied
        );
        // Including a push that does nothing at all, because the answer does
        // not depend on what the push was.
        assert_eq!(verdict_for(&denied, &[]), Verdict::PushDenied);
    }

    #[test]
    fn a_protected_ref_is_matched_written_either_way_round() {
        // `main` and `refs/heads/main` name the same branch, and an operator
        // should not have to know which spelling the hook is handed.
        for declared in ["main", "refs/heads/main"] {
            let guarded = policy(None, &[declared]).expect("a protected branch earns the gate");

            assert!(guarded.protects("refs/heads/main"), "{declared}");
            assert!(guarded.protects("main"), "{declared}");
        }
    }

    #[test]
    fn protection_is_exact_rather_than_a_pattern() {
        // The same reading the exec allowlist gives an entry. A control whose
        // matching is a pattern language is one whose refusals have to be
        // reasoned about rather than read.
        let guarded = policy(None, &["main"]).expect("gated");

        assert!(!guarded.protects("refs/heads/maintenance"));
        assert!(!guarded.protects("refs/heads/feature/main"));
        assert!(!guarded.protects("refs/heads/Main"));
    }

    #[test]
    fn a_protected_ref_refuses_the_push_and_names_itself() {
        // The ref rather than the branch, because `details.ref` on the incident
        // is what an operator reads to learn what the hook matched.
        let guarded = policy(None, &["main"]).expect("gated");

        assert_eq!(
            verdict_for(
                &guarded,
                &[
                    update("refs/heads/topic", LOCAL, REMOTE),
                    update("refs/heads/main", LOCAL, REMOTE),
                ]
            ),
            Verdict::BranchProtected("refs/heads/main".to_owned())
        );
    }

    #[test]
    fn deleting_a_protected_branch_is_refused_like_writing_to_one() {
        // A delete sends no objects and is the most destructive thing that can
        // happen to a branch, so a gate that only watched content would miss
        // exactly the case it exists for.
        let guarded = policy(None, &["main"]).expect("gated");

        assert_eq!(
            verdict_for(&guarded, &[update("refs/heads/main", ABSENT, REMOTE)]),
            Verdict::BranchProtected("refs/heads/main".to_owned())
        );
    }

    #[test]
    fn an_ordinary_push_scans_only_what_it_sends() {
        let guarded = policy(None, &["main"]).expect("gated");

        assert_eq!(
            verdict_for(&guarded, &[update("refs/heads/topic", LOCAL, REMOTE)]),
            Verdict::Permitted(vec![ScanRange::Update {
                from: REMOTE.to_owned(),
                to: LOCAL.to_owned(),
            }])
        );
    }

    #[test]
    fn a_branch_the_remote_has_never_seen_excludes_what_it_already_has() {
        // Otherwise the first push of a topic branch cut from `main` scans
        // every commit in the repository's history.
        let guarded = policy(None, &["main"]).expect("gated");

        let Verdict::Permitted(ranges) =
            verdict_for(&guarded, &[update("refs/heads/topic", LOCAL, ABSENT)])
        else {
            panic!("an unprotected new branch is permitted");
        };

        assert_eq!(
            ranges,
            vec![ScanRange::Unseen {
                tip: LOCAL.to_owned()
            }]
        );
        assert_eq!(
            ranges[0].arguments(),
            vec![LOCAL.to_owned(), "--not".to_owned(), "--remotes".to_owned()]
        );
    }

    #[test]
    fn a_delete_carries_nothing_to_scan() {
        let guarded = policy(None, &["main"]).expect("gated");

        assert_eq!(
            verdict_for(&guarded, &[update("refs/heads/topic", ABSENT, REMOTE)]),
            Verdict::Permitted(Vec::new())
        );
    }

    #[test]
    fn a_sha_256_repository_spells_absence_with_sixty_four_zeros() {
        // A hard-coded forty would read a new-branch push as an ordinary
        // update, in exactly the repositories nobody tests on.
        assert!(is_zero(&"0".repeat(64)));
        assert!(is_zero(ABSENT));
        assert!(!is_zero(LOCAL));
        assert!(!is_zero(""));
    }

    #[test]
    fn the_hooks_stdin_format_is_read_field_for_field() {
        let updates = updates_from(&format!(
            "refs/heads/topic {LOCAL} refs/heads/topic {REMOTE}\n\
             refs/heads/other {LOCAL} refs/heads/other {ABSENT}\n"
        ))
        .expect("git writes four fields per line");

        assert_eq!(updates.len(), 2);
        assert_eq!(updates[0].local_sha, LOCAL);
        assert_eq!(updates[0].remote_ref, "refs/heads/topic");
        assert!(updates[1].is_new());
    }

    #[test]
    fn a_trailing_newline_is_not_a_malformed_push() {
        // git writes one, and reading it as a parse failure would refuse every
        // push on the satellite.
        assert_eq!(
            updates_from("").expect("nothing to push is not a failure"),
            Vec::new()
        );
        assert_eq!(
            updates_from(&format!("refs/heads/a {LOCAL} refs/heads/a {REMOTE}\n\n"))
                .expect("should parse")
                .len(),
            1
        );
    }

    #[test]
    fn a_line_that_is_not_four_fields_is_refused_rather_than_guessed() {
        // A misread ref is a protected branch that looks unprotected, so the
        // hook fails closed on input it does not recognize.
        assert_eq!(updates_from("refs/heads/a"), None);
        assert_eq!(
            updates_from(&format!("refs/heads/a {LOCAL} refs/heads/a")),
            None
        );
    }
}
