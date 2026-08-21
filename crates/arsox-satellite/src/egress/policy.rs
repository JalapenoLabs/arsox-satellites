// Copyright © 2026 Jalapeno Labs

//! Which hosts a thread's agents may reach, and what an entry means.
//!
//! The contract's `web` field names a base and `additional_domains` adds to it.
//! This module resolves the two into one set of hosts and answers one question
//! about it: may this host be reached. Nothing here does I/O, so the rule an
//! operator reads in the docs is the rule a unit test asserts on every platform.
//!
//! # The rule an entry follows
//!
//! An entry matches a host **exactly, or one label deeper**. So `github.com`
//! covers `github.com` and `api.github.com`, and covers neither
//! `a.b.github.com` nor `notgithub.com`. A deeper name is named in full.
//!
//! One label rather than a whole subtree, because an entry should grant what an
//! operator can hold in their head. `*.example.com` granting arbitrary depth
//! reads the same and covers a great deal more, and the cost of the narrower
//! rule is one extra line in a list for the rare deep name.
//!
//! A single-label entry matches exactly and grants no subdomain, so `com` is not
//! a wildcard for the internet and `localhost` means the host of that name.
//!
//! There is **no globbing**, which is the same reading the exec allowlist and
//! the protected-branch list already give an entry. An operator who writes
//! `*.example.com` or `.example.com` meant the entry to cover subdomains, so
//! both are read as `example.com` rather than as a literal host no request can
//! ever match. An entry that cannot be a hostname at all, a URL or a path, is
//! dropped with a warning: it would otherwise be a lockout with no explanation.

use arsox_sdk::proto::settings::v1::{Permissions, WebAccess};
use std::collections::BTreeSet;

/// The curated host set `WebAccess::PRESET` names.
///
/// **The curation principle**: a domain earns a place when tooling the image
/// already ships cannot do an ordinary development task without reaching it.
///
/// So this is the forges and the package registries, plus the archives the base
/// image installs from, and nothing else. It is the network half of the same
/// principle [`crate::broker::PRESET_COMMANDS`] applies to the shell.
///
/// **`api.anthropic.com` and every other provider host are deliberately
/// absent.** An agent never authenticates to a model provider: model requests go
/// to the satellite's [LLM proxy](crate::proxy) on loopback, which is exempt
/// from this proxy entirely, and no provider credential ever reaches an agent.
/// An agent reaching a provider directly would be a model request outside every
/// ceiling the satellite enforces, so the preset refuses it.
///
/// **The harness CLIs also contact telemetry and update hosts, and those are
/// absent too.** The image sets `DISABLE_AUTOUPDATER=1`, and a blocked telemetry
/// call is a `blocked` incident rather than a failed turn. If some later CLI
/// version makes one of them load-bearing, that incident is how an operator
/// finds out, which is a better outcome than a preset that quietly allows a
/// vendor's analytics because a turn once needed it to start.
pub const PRESET_DOMAINS: &[&str] = &[
    // The forges, and the separate apex GitHub serves release assets, raw files,
    // and LFS objects from.
    "github.com",
    "githubusercontent.com",
    "gitlab.com",
    "bitbucket.org",
    // Node, which the image ships with corepack, and where a pinned runtime is
    // fetched from.
    "registry.npmjs.org",
    "registry.yarnpkg.com",
    "nodejs.org",
    // Python, which the image ships with pip.
    "pypi.org",
    "files.pythonhosted.org",
    // Rust. Not in the image, and named because a thread that installs a
    // toolchain in its own layer reaches these and a registry discloses
    // nothing about the work. `static.` and `index.` are already covered by the
    // one-label rule and are spelled out so the list reads as what it grants.
    "crates.io",
    "static.crates.io",
    "index.crates.io",
    // The archives the published images install from. `apt` is deliberately
    // absent from the exec preset, and a repo's setup commands are configuration
    // being executed as configured rather than an agent's shell, so they still
    // need somewhere to install from.
    "deb.debian.org",
    "security.debian.org",
    "archive.ubuntu.com",
    "security.ubuntu.com",
];

/// Which hosts one thread's agents may reach.
///
/// `Only` with an empty set is `WebAccess::NONE`. A second variant meaning the
/// same thing would be a second thing to match on, and every caller that asks
/// "may this host be reached" gets the same answer from either spelling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebPolicy {
    /// Every host, recorded rather than refused.
    ///
    /// `WebAccess::ALL` is visibility and not enforcement, and the proxy says so
    /// by logging what it lets through. A thread that declared it still runs
    /// with the proxy in front of it, which is what makes "all" an answer the
    /// satellite gave rather than a gate that was never there.
    Everything,

    /// Only these hosts, matched by the rule in the module docs.
    Only(BTreeSet<String>),
}

impl WebPolicy {
    /// Resolves a thread's web policy, or `None` when the gate does not engage.
    ///
    /// **Engagement is the whole behaviour change this feature makes.** A thread
    /// that declared no `web` value and no `additional_domains` keeps exactly the
    /// network it has today: no proxy variables in its agents' environment and
    /// nothing in front of its traffic. Everything else engages, `ALL` included,
    /// because an operator who wrote a value asked for the gate that value
    /// describes.
    ///
    /// This is deliberately **not** how the exec broker reads `PRESET`, and the
    /// difference is worth stating. There, `PRESET` and an undeclared `exec`
    /// collapse into the same thing because the preset base is the `PATH` the
    /// image already provides, so brokering it would change the default for
    /// every thread that asked for nothing. Here an explicit `PRESET` is a value
    /// an operator wrote down, and honouring it changes nothing for a thread that
    /// wrote nothing.
    ///
    /// `additional_domains` is **additive on top of whatever base `web` set**,
    /// which is the contract's own rule and the same rule `allowed_commands`
    /// follows. So `PRESET` plus a domain is the preset and that domain, `CUSTOM`
    /// plus domains is those domains alone, and `NONE` plus a domain is a grant
    /// rather than a contradiction to resolve.
    #[must_use]
    pub fn for_thread(permissions: Option<&Permissions>) -> Option<Self> {
        let permissions = permissions?;
        let web = WebAccess::try_from(permissions.web).unwrap_or(WebAccess::Unspecified);

        if web == WebAccess::Unspecified && permissions.additional_domains.is_empty() {
            return None;
        }

        if web == WebAccess::All {
            return Some(Self::Everything);
        }

        // Unspecified means the documented default and the documented default is
        // the preset. It reaches here only for a thread that named domains
        // without naming a base, which is the reading that "never silently drops
        // the preset" asks for.
        let mut hosts: BTreeSet<String> = match web {
            WebAccess::Unspecified | WebAccess::Preset => {
                PRESET_DOMAINS.iter().copied().map(str::to_owned).collect()
            }
            WebAccess::None | WebAccess::Custom | WebAccess::All => BTreeSet::new(),
        };

        for declared in &permissions.additional_domains {
            let Some(usable) = entry(declared) else {
                tracing::warn!(
                    event.name = "egress.entry.unusable",
                    web.entry = declared,
                    "dropping {{web.entry}} from this thread's web allowlist: it is not a \
                     hostname, so no request could ever match it and every request it was \
                     meant to allow would be refused",
                );
                continue;
            };

            hosts.insert(usable);
        }

        Some(Self::Only(hosts))
    }

    /// Whether this policy permits reaching `host`.
    ///
    /// The port is not considered, because the contract's field is a domain list.
    /// A host that is allowed is allowed on every port. Restricting `CONNECT` to
    /// 443 was the alternative and is worse: it refuses an ordinary self-hosted
    /// forge on 8443 with a message about a port the operator never configured.
    #[must_use]
    pub fn permits(&self, host: &str) -> bool {
        match self {
            Self::Everything => true,
            Self::Only(hosts) => {
                let Some(host) = normalized_host(host) else {
                    return false;
                };

                hosts.contains(&host)
                    || parent_domain(&host).is_some_and(|parent| hosts.contains(parent))
            }
        }
    }

    /// How many hosts this policy names, or `None` when it names all of them.
    ///
    /// For a log line rather than for a decision: an operator reading a turn's
    /// boot output wants to know which of the four answers a thread resolved to.
    #[must_use]
    pub fn named(&self) -> Option<usize> {
        match self {
            Self::Everything => None,
            Self::Only(hosts) => Some(hosts.len()),
        }
    }
}

/// One allowlist entry, normalized, or `None` when it cannot be a hostname.
///
/// A leading `*.` or `.` is taken off rather than treated as part of the name.
/// An operator who wrote either meant the entry to cover subdomains, which is
/// what an entry already does, and reading it literally would produce a line in
/// the allowlist that no request can ever match.
fn entry(declared: &str) -> Option<String> {
    let trimmed = declared
        .trim()
        .trim_start_matches("*.")
        .trim_start_matches('.');

    normalized_host(trimmed)
}

/// One hostname, lowercased and stripped of the trailing dot that spells a
/// fully qualified name, or `None` when the text is not a hostname at all.
///
/// Applied to both halves of every comparison, so an entry and a request that
/// differ only in case or in a trailing dot are the same host. Case folding is
/// ASCII: DNS is case insensitive over ASCII and an internationalized name
/// arrives already punycoded.
fn normalized_host(host: &str) -> Option<String> {
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();

    if host.is_empty() || host.len() > MAX_HOSTNAME {
        return None;
    }

    // Anything a hostname cannot hold. A URL, a path, a port, a glob, or an
    // IPv6 literal all land here and are refused rather than being compared as
    // text that will never match. An IPv6 destination therefore cannot be
    // allowed at all, which is a stated limit rather than an oversight: nothing
    // in the contract can name one.
    let usable = host
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || character == '.' || character == '-');

    // A leading or doubled dot would make an empty label, and `parent_domain`
    // would then hand back a name with a dot at the front.
    let labelled = !host.starts_with('.') && !host.contains("..");

    (usable && labelled).then_some(host)
}

/// The longest a DNS name can be, so a pathological entry is refused rather than
/// compared.
const MAX_HOSTNAME: usize = 253;

/// The host one label shallower, when naming it would name a real domain.
///
/// `api.github.com` yields `github.com`. `github.com` yields nothing, because
/// the remainder would be a bare `com` and an entry of `com` must not be a
/// wildcard for the internet.
fn parent_domain(host: &str) -> Option<&str> {
    let (_label, parent) = host.split_once('.')?;

    parent.contains('.').then_some(parent)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A thread's permissions, with only the web policy varying.
    fn permissions(web: WebAccess, domains: &[&str]) -> Permissions {
        Permissions {
            web: web.into(),
            additional_domains: domains.iter().copied().map(str::to_owned).collect(),
            ..Permissions::default()
        }
    }

    /// The policy a thread with these permissions runs under.
    fn policy(web: WebAccess, domains: &[&str]) -> Option<WebPolicy> {
        WebPolicy::for_thread(Some(&permissions(web, domains)))
    }

    #[test]
    fn a_thread_that_declared_nothing_is_not_gated() {
        // The whole opt-in claim. A thread that asked for no policy reaches the
        // network exactly as it does today, with no proxy in front of it.
        assert_eq!(WebPolicy::for_thread(None), None);
        assert_eq!(policy(WebAccess::Unspecified, &[]), None);
    }

    #[test]
    fn naming_domains_alone_engages_the_gate_on_top_of_the_preset() {
        // "Always additive on top of `web`, so setting this never silently drops
        // the preset." A thread that named one host and no base meant the preset
        // and that host, not that host alone.
        let Some(WebPolicy::Only(hosts)) =
            policy(WebAccess::Unspecified, &["internal.example.com"])
        else {
            panic!("naming a domain should engage the gate");
        };

        assert!(hosts.contains("internal.example.com"));
        assert!(hosts.contains("github.com"));
    }

    #[test]
    fn declaring_the_preset_engages_the_gate_with_the_curated_set() {
        // Deliberately unlike the exec broker's reading of PRESET: an explicit
        // value is a value the operator wrote down, and honouring it changes
        // nothing for a thread that wrote nothing.
        let Some(WebPolicy::Only(hosts)) = policy(WebAccess::Preset, &[]) else {
            panic!("an explicit preset should engage the gate");
        };

        assert_eq!(hosts.len(), PRESET_DOMAINS.len());
    }

    #[test]
    fn all_permits_everything_and_still_engages() {
        // `ALL` is visibility rather than absence. The proxy stays in front of
        // the traffic so what a thread reached is a fact the satellite recorded.
        let resolved = policy(WebAccess::All, &["ignored.example.com"]).expect("should engage");

        assert_eq!(resolved, WebPolicy::Everything);
        assert!(resolved.permits("anything.example.invalid"));
        assert_eq!(resolved.named(), None);
    }

    #[test]
    fn none_refuses_every_host() {
        let resolved = policy(WebAccess::None, &[]).expect("should engage");

        assert_eq!(resolved.named(), Some(0));
        assert!(!resolved.permits("github.com"));
        assert!(!resolved.permits("registry.npmjs.org"));
    }

    #[test]
    fn custom_names_its_hosts_and_nothing_else() {
        // "Start from nothing and use `additional_domains` alone", per the
        // contract's own doc comment on the enum value.
        let resolved = policy(WebAccess::Custom, &["internal.example.com"]).expect("should engage");

        assert!(resolved.permits("internal.example.com"));
        assert!(!resolved.permits("github.com"));
    }

    #[test]
    fn domains_are_additive_on_top_of_whatever_the_web_policy_set() {
        // The contract's own rule, and the same rule `allowed_commands` follows
        // for the exec half. Naming a host beside a base of nothing is a grant
        // rather than a contradiction to resolve.
        let resolved = policy(WebAccess::None, &["internal.example.com"]).expect("should engage");

        assert!(resolved.permits("internal.example.com"));
        assert!(!resolved.permits("github.com"));
    }

    #[test]
    fn an_entry_matches_exactly_and_one_label_deeper() {
        let resolved = policy(WebAccess::Custom, &["example.com"]).expect("should engage");

        assert!(resolved.permits("example.com"));
        assert!(resolved.permits("api.example.com"));

        // Two labels deep is named in full or not at all, which is what keeps an
        // entry something an operator can hold in their head.
        assert!(!resolved.permits("a.b.example.com"));
    }

    #[test]
    fn matching_is_label_for_label_rather_than_textual() {
        // The failure this rules out is a suffix match, where `example.com`
        // would permit `notexample.com` and `evil-example.com`.
        let resolved = policy(WebAccess::Custom, &["example.com"]).expect("should engage");

        assert!(!resolved.permits("notexample.com"));
        assert!(!resolved.permits("example.com.evil.invalid"));
        assert!(!resolved.permits("example.co"));
    }

    #[test]
    fn a_single_label_entry_is_not_a_wildcard_for_the_internet() {
        // `com` naming every domain under it would turn one careless entry into
        // an open satellite.
        let resolved = policy(WebAccess::Custom, &["com", "localhost"]).expect("should engage");

        assert!(!resolved.permits("github.com"));
        assert!(resolved.permits("com"));
        assert!(resolved.permits("localhost"));
        assert!(!resolved.permits("api.localhost"));
    }

    #[test]
    fn a_host_and_an_entry_agree_about_case_and_the_trailing_dot() {
        // A resolver is handed `GitHub.com.` by plenty of clients, and it is the
        // same host as `github.com`.
        let resolved = policy(WebAccess::Custom, &["  Example.COM. "]).expect("should engage");

        assert!(resolved.permits("example.com"));
        assert!(resolved.permits("EXAMPLE.com"));
        assert!(resolved.permits("api.example.com."));
    }

    #[test]
    fn an_entry_written_as_a_wildcard_means_what_the_operator_meant() {
        // Read literally, `*.example.com` is a line no request can ever match,
        // which is a lockout with no explanation rather than a policy.
        for spelling in ["*.example.com", ".example.com", "example.com"] {
            let resolved = policy(WebAccess::Custom, &[spelling]).expect("should engage");

            assert!(resolved.permits("api.example.com"), "{spelling}");
            assert!(resolved.permits("example.com"), "{spelling}");
        }
    }

    #[test]
    fn an_entry_that_cannot_be_a_hostname_is_dropped_rather_than_kept() {
        // Kept, it would be a line that never matches. Dropped, it is a warning
        // an operator can act on, and the policy still fails closed.
        let Some(WebPolicy::Only(hosts)) = policy(
            WebAccess::Custom,
            &[
                "https://example.com/path",
                "example.com:8443",
                "",
                "   ",
                "..",
                "good.example.com",
            ],
        ) else {
            panic!("should engage");
        };

        assert_eq!(
            hosts.iter().map(String::as_str).collect::<Vec<&str>>(),
            ["good.example.com"]
        );
    }

    #[test]
    fn an_ipv4_literal_is_an_ordinary_entry() {
        // A self-hosted forge on an address rather than a name is ordinary, and
        // the matcher compares host strings, so it needs no special case.
        let resolved = policy(WebAccess::Custom, &["10.1.2.3"]).expect("should engage");

        assert!(resolved.permits("10.1.2.3"));
        assert!(!resolved.permits("10.1.2.4"));
    }

    #[test]
    fn the_preset_names_the_forges_and_the_registries() {
        // The curation principle, asserted rather than only written down: the
        // image's own tooling cannot do an ordinary task without these.
        for expected in [
            "github.com",
            "gitlab.com",
            "bitbucket.org",
            "registry.npmjs.org",
            "registry.yarnpkg.com",
            "pypi.org",
            "files.pythonhosted.org",
            "crates.io",
            "deb.debian.org",
            "archive.ubuntu.com",
        ] {
            assert!(
                PRESET_DOMAINS.contains(&expected),
                "{expected} should be in the preset"
            );
        }
    }

    #[test]
    fn the_preset_refuses_every_model_provider() {
        // An agent reaching a provider directly is a model request outside every
        // ceiling the satellite enforces. Model traffic goes to the LLM proxy on
        // loopback, which this proxy never sees.
        let preset = policy(WebAccess::Preset, &[]).expect("should engage");

        for provider in [
            "api.anthropic.com",
            "api.openai.com",
            "generativelanguage.googleapis.com",
            "bedrock-runtime.us-east-1.amazonaws.com",
        ] {
            assert!(!preset.permits(provider), "{provider}");
        }
    }

    #[test]
    fn the_preset_names_each_host_once() {
        // A duplicate is harmless and is also a sign nobody read the list before
        // adding to it.
        let unique: BTreeSet<&&str> = PRESET_DOMAINS.iter().collect();

        assert_eq!(unique.len(), PRESET_DOMAINS.len());
    }

    #[test]
    fn every_preset_entry_is_a_usable_hostname() {
        // A preset entry that could not be normalized would be a curated list
        // with a hole in it that no test of the policy would ever reach.
        for host in PRESET_DOMAINS {
            assert_eq!(entry(host).as_deref(), Some(*host), "{host}");
        }
    }

    #[test]
    fn the_preset_covers_what_the_forges_serve_from_their_own_subdomains() {
        // The one-label rule is what makes a short list a working one: `gh` talks
        // to `api.github.com` and git fetches packs from `codeload.github.com`.
        let preset = policy(WebAccess::Preset, &[]).expect("should engage");

        for reached in [
            "api.github.com",
            "codeload.github.com",
            "objects.githubusercontent.com",
            "raw.githubusercontent.com",
            "static.crates.io",
        ] {
            assert!(preset.permits(reached), "{reached}");
        }
    }
}
