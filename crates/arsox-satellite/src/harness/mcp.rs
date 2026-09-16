// Copyright © 2026 Jalapeno Labs

//! Handing a thread's MCP servers to the harness that runs its turns.
//!
//! A thread's `mcp_servers` are remote servers, spoken to over streamable HTTP,
//! whose tools the agents may use. Both harnesses can host many of them at once,
//! and this module is the one place that says what a server must look like,
//! how each CLI is told about it, and which of its parts are credentials.
//!
//! # Header values never reach argv
//!
//! A command line is readable by anything on the host that can run `ps`, and a
//! header value is a credential far more often than not. So each value travels
//! in the harness's **environment**, under a name the satellite assigns, and the
//! command line names the variable rather than the value:
//!
//! | | Claude | Codex |
//! |---|---|---|
//! | servers | `--mcp-config '<json>'` | `-c mcp_servers.<name>.url=...` |
//! | a header | `"<header>": "${MCP_HEADER_SECRET_0_0}"` | `-c mcp_servers.<name>.env_http_headers={...}` |
//!
//! Both CLIs expand the reference into the request they send, which was
//! measured against the pinned versions (Claude 2.1.235, Codex 0.147.0) with a
//! stub server recording what arrived rather than read from a schema.
//!
//! **Nothing is written to disk.** Claude reads `--mcp-config` from a string as
//! readily as from a file, and once the configuration carries no secret there is
//! nothing a file would protect. A file would also be a thing to own, to clean
//! up, and to keep an agent from rewriting between the sessions of one turn,
//! since a restart and a checker fix cycle each launch the harness again.
//!
//! **What this does not do** is keep a value from the agent. The harness is the
//! agent's own process, running as the agent's account, and it must hold the
//! value to send it. That is the same standing a declared `env` variable has,
//! and it is why every header value is also indexed by the thread's
//! [redactor](crate::redaction::Redactor): what an agent prints is masked on its
//! way out, and a push carrying one is refused.
//!
//! # Only the servers the thread declared
//!
//! A Claude session also loads servers from a repo's own `.mcp.json` and from
//! the agent's user settings, and under `bypassPermissions` it connects to them
//! without asking. A thread that declared servers is launched with
//! `--strict-mcp-config`, so the set it runs with is the set it named, and a
//! checkout cannot add a server the host application never saw. A thread that
//! declared none is launched without `--mcp-config` or
//! `--strict-mcp-config`.
//!
//! Codex has no equivalent switch: `-c` overrides merge into whatever
//! `config.toml` the CLI reads. That is stated in `docs/harness.md` rather than
//! papered over.
//!
//! # Validation happens twice
//!
//! [`refusal`] runs at thread creation, where the caller is still listening and
//! can be told which server was wrong. The spawn applies the same per-server
//! rule again and skips what fails it, for settings stored before the rule
//! existed. A name reaches a Codex config key and a Claude tool name unescaped,
//! so the second gate is load-bearing rather than tidy.

use crate::harness::spawn::AgentVar;
use arsox_sdk::proto::settings::v1::McpServer;
use std::collections::{BTreeMap, BTreeSet};

/// Most servers one thread may declare.
///
/// Every server lands in one argument on a Claude command line, and Linux caps a
/// single argument at 128 KiB. Sixteen servers at the longest URL and the most
/// headers allowed stay well inside that, alongside the prompt that shares the
/// process's argument space. A limit reached here is a limit worth raising
/// deliberately rather than discovering as a launch failure.
pub const MAX_SERVERS: usize = 16;

/// Most headers one server may carry.
pub const MAX_HEADERS_PER_SERVER: usize = 8;

/// Longest server name.
///
/// A name becomes part of every tool name the harness shows the model, as
/// `mcp__<name>__<tool>`, so a long one spends context on every call.
pub const MAX_NAME: usize = 64;

/// Longest server URL.
pub const MAX_URL: usize = 2048;

/// Longest header name.
pub const MAX_HEADER_NAME: usize = 128;

/// Longest header value.
///
/// Generous for any bearer token or signed credential, and bounded because each
/// value is an environment variable and the environment shares the argument
/// space [`MAX_SERVERS`] budgets.
pub const MAX_HEADER_VALUE: usize = 4096;

/// The prefix a server name may not start with.
///
/// Reserved for the tools Arsox itself offers the agents, such as
/// `request_integration` and `override_redaction`, so a thread cannot declare a
/// server that shadows one.
const RESERVED_PREFIX: &str = "arsox";

/// Why a thread's servers may not be used, when they may not.
///
/// The reason completes the sentence "`settings.mcp_servers`: ...", names the
/// server it is about, and never carries a header value: half of these are
/// credentials by definition, and an error body is a log line somewhere.
///
/// # Errors
///
/// Returns the first reason found, which is enough for a caller to fix and
/// resubmit.
pub fn refusal(servers: &[McpServer]) -> Result<(), String> {
    if servers.len() > MAX_SERVERS {
        return Err(format!(
            "declares {} servers, and a thread may declare at most {MAX_SERVERS}",
            servers.len()
        ));
    }

    let mut seen = BTreeSet::new();

    for server in servers {
        if let Some(reason) = server_refusal(server) {
            return Err(reason);
        }

        // Case-insensitively, because a model reading `mcp__Search__query` and
        // `mcp__search__query` beside each other has no way to tell which one
        // it meant.
        if !seen.insert(server.name.to_ascii_lowercase()) {
            return Err(format!(
                "server {:?} is declared more than once",
                server.name
            ));
        }
    }

    Ok(())
}

/// Why one server may not be used, when it may not.
fn server_refusal(server: &McpServer) -> Option<String> {
    let name = &server.name;

    if name.is_empty() || name.len() > MAX_NAME {
        return Some(format!(
            "server {name:?} needs a name of 1 to {MAX_NAME} characters"
        ));
    }

    // No dot, because Codex reads the name as one segment of a dotted config
    // path and a dot would nest it. No quote or space, because it is a JSON key
    // and part of a tool name. This is the character set both CLIs accept in a
    // tool name, so it is the one that works everywhere a name travels.
    let identifier = name
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'));
    if !identifier {
        return Some(format!(
            "server {name:?} may use only ASCII letters, digits, '_', and '-' in its name"
        ));
    }

    if name.to_ascii_lowercase().starts_with(RESERVED_PREFIX) {
        return Some(format!(
            "server {name:?} starts with {RESERVED_PREFIX:?}, which is reserved for the tools \
             Arsox offers the agents itself"
        ));
    }

    if let Some(reason) = url_refusal(&server.url) {
        return Some(format!("server {name:?} {reason}"));
    }

    if server.headers.len() > MAX_HEADERS_PER_SERVER {
        return Some(format!(
            "server {name:?} carries {} headers, and a server may carry at most \
             {MAX_HEADERS_PER_SERVER}",
            server.headers.len()
        ));
    }

    let mut header_names = BTreeSet::new();

    for (header, value) in &server.headers {
        if let Some(reason) = header_refusal(header, value_of(value)) {
            return Some(format!("server {name:?} header {header:?} {reason}"));
        }

        // HTTP header names are case-insensitive, so two spellings of one name
        // would be one header sent twice with whichever value a CLI kept.
        if !header_names.insert(header.to_ascii_lowercase()) {
            return Some(format!(
                "server {name:?} header {header:?} is declared more than once"
            ));
        }
    }

    None
}

/// Why a server URL may not be used, completing "server `name` ...".
fn url_refusal(url: &str) -> Option<&'static str> {
    if url.len() > MAX_URL {
        return Some("has a URL longer than 2048 characters");
    }

    // Claude expands `${NAME}` in a URL as it does in a header, so a URL
    // carrying one would be filled from the harness's environment, where the
    // other servers' header values live.
    if url.contains("${") {
        return Some("has a URL containing \"${\", which the harness would expand");
    }

    let Ok(parsed) = reqwest::Url::parse(url) else {
        return Some("has a URL that does not parse");
    };

    if !matches!(parsed.scheme(), "http" | "https") {
        return Some("has a URL that is not http or https, which is the only transport served");
    }

    if parsed.host_str().is_none_or(str::is_empty) {
        return Some("has a URL with no host");
    }

    // A credential belongs in a header, where it is kept off the command line
    // and masked. Userinfo in the URL would be neither.
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Some("has credentials in its URL, which belong in a header instead");
    }

    None
}

/// Why a header may not be sent, completing "server `name` header `h` ...".
///
/// Never names the value.
fn header_refusal(header: &str, value: &str) -> Option<&'static str> {
    // An HTTP token, per RFC 9110. It is also a JSON key and a quoted TOML key
    // on the way to the CLI, and a token needs escaping in neither.
    let token = !header.is_empty()
        && header.len() <= MAX_HEADER_NAME
        && header.chars().all(|character| {
            character.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(character)
        });
    if !token {
        return Some("is not a valid HTTP header name of at most 128 characters");
    }

    if value.len() > MAX_HEADER_VALUE {
        return Some("has a value longer than 4096 bytes");
    }

    // A line break would split one header into two, and an environment variable
    // cannot carry a NUL at all.
    if value.contains(['\r', '\n', '\0']) {
        return Some("has a value containing a line break or a NUL byte");
    }

    None
}

/// A header's plaintext, empty when the caller sent none.
///
/// The same reading a declared variable gets: the caller named the header
/// deliberately, and "send it empty" is a likelier meaning than "do not send it",
/// which is spelled by leaving the entry out.
fn value_of(secret: &arsox_sdk::proto::common::v1::Secret) -> &str {
    secret.value.as_deref().unwrap_or_default()
}

/// One header as it is handed to a CLI: its name, and the variable holding it.
#[derive(Debug)]
struct HeaderBinding<'a> {
    header: &'a str,
    variable: String,
    value: &'a str,
}

/// One server a turn launches with, and its headers in a stable order.
#[derive(Debug)]
struct ServerBinding<'a> {
    name: &'a str,
    url: &'a str,
    headers: Vec<HeaderBinding<'a>>,
}

/// The servers a turn actually launches with.
///
/// Headers are ordered by name, because the contract carries them as a map and a
/// map iterates in no particular order. The variable names are derived from
/// position, so an unstable order would still be correct and would make two
/// launches of the same thread differ for no reason anybody could read.
///
/// A server that fails [`server_refusal`], or repeats a name already bound, is
/// skipped with a warning naming it. The API refuses both at creation, so this
/// only fires for settings stored before it did.
fn bindings(servers: &[McpServer]) -> Vec<ServerBinding<'_>> {
    let mut seen = BTreeSet::new();
    let mut bound = Vec::new();

    if servers.len() > MAX_SERVERS {
        tracing::warn!(
            event.name = "harness.mcp.truncated",
            mcp.declared = servers.len(),
            mcp.limit = MAX_SERVERS,
            "only the first {{mcp.limit}} of {{mcp.declared}} MCP servers reach the agent",
        );
    }

    for server in servers.iter().take(MAX_SERVERS) {
        let refused = server_refusal(server).or_else(|| {
            (!seen.insert(server.name.to_ascii_lowercase()))
                .then(|| "is declared more than once".to_owned())
        });

        if let Some(reason) = refused {
            tracing::warn!(
                event.name = "harness.mcp.refused",
                mcp.server = server.name,
                mcp.refusal = reason,
                "an MCP server was withheld from the agent: {{mcp.refusal}}",
            );
            continue;
        }

        let server_index = bound.len();
        let ordered: BTreeMap<&str, &str> = server
            .headers
            .iter()
            .map(|(header, value)| (header.as_str(), value_of(value)))
            .collect();

        let headers = ordered
            .into_iter()
            .enumerate()
            .map(|(header_index, (header, value))| HeaderBinding {
                header,
                variable: format!("MCP_HEADER_SECRET_{server_index}_{header_index}"),
                value,
            })
            .collect();

        bound.push(ServerBinding {
            name: &server.name,
            url: &server.url,
            headers,
        });
    }

    bound
}

/// The variables carrying every header value, for the satellite's own layer.
///
/// Every one is a secret, whatever the header is called: the contract gives
/// header values the credential type, and a value that happens to be public
/// costs a masked log line.
#[must_use]
pub fn environment(servers: &[McpServer]) -> Vec<AgentVar> {
    bindings(servers)
        .into_iter()
        .flat_map(|server| server.headers)
        .map(|binding| AgentVar {
            key: binding.variable,
            value: binding.value.to_owned(),
            secret: true,
        })
        .collect()
}

/// The names of the servers a turn launches with.
///
/// For the Claude permission rules, which allow a declared server's tools by
/// name under a posture that would otherwise refuse them.
#[must_use]
pub fn names(servers: &[McpServer]) -> Vec<String> {
    bindings(servers)
        .into_iter()
        .map(|server| server.name.to_owned())
        .collect()
}

/// The Claude CLI arguments that load a thread's servers, and only those.
///
/// Empty when the thread declared none, which leaves its launch exactly as it
/// was. See the module docs for `--strict-mcp-config`.
#[must_use]
pub fn claude_args(servers: &[McpServer]) -> Vec<String> {
    let bound = bindings(servers);
    if bound.is_empty() {
        return Vec::new();
    }

    let mut config = serde_json::Map::new();

    for server in bound {
        let headers: serde_json::Map<String, serde_json::Value> = server
            .headers
            .into_iter()
            .map(|binding| {
                (
                    binding.header.to_owned(),
                    serde_json::Value::String(format!("${{{}}}", binding.variable)),
                )
            })
            .collect();

        config.insert(
            server.name.to_owned(),
            serde_json::json!({
                "type": "http",
                "url": server.url,
                "headers": headers,
            }),
        );
    }

    let document = serde_json::json!({ "mcpServers": config });

    vec![
        "--mcp-config".to_owned(),
        document.to_string(),
        "--strict-mcp-config".to_owned(),
    ]
}

/// The Codex CLI overrides that declare a thread's servers.
///
/// Values are written as quoted TOML rather than bare, unlike the provider
/// overrides in `spawn`. A URL happens to fail TOML parsing and fall back to a
/// literal, but a value that only works because it failed to parse is one
/// character away from parsing as something else. A JSON string is a valid TOML
/// basic string for every character a validated URL and header name can hold.
#[must_use]
pub fn codex_args(servers: &[McpServer]) -> Vec<String> {
    let mut args = Vec::new();

    for server in bindings(servers) {
        let prefix = format!("mcp_servers.{}", server.name);

        args.push("-c".to_owned());
        args.push(format!("{prefix}.url={}", toml_string(server.url)));

        if server.headers.is_empty() {
            continue;
        }

        let headers: Vec<String> = server
            .headers
            .iter()
            .map(|binding| {
                format!(
                    "{}={}",
                    toml_string(binding.header),
                    toml_string(&binding.variable)
                )
            })
            .collect();

        args.push("-c".to_owned());
        args.push(format!(
            "{prefix}.env_http_headers={{{}}}",
            headers.join(",")
        ));
    }

    args
}

/// A string as a quoted TOML basic string.
fn toml_string(text: &str) -> String {
    serde_json::Value::String(text.to_owned()).to_string()
}

/// The exact host of every server a turn launches with.
///
/// For the egress allowlist, which admits these hosts and nothing beneath them.
#[must_use]
pub fn hosts(servers: &[McpServer]) -> Vec<String> {
    bindings(servers)
        .into_iter()
        .filter_map(|server| {
            let parsed = reqwest::Url::parse(server.url).ok()?;
            let host = parsed.host_str()?;

            // A bracketed IPv6 literal keeps its brackets in `host_str`, and the
            // proxy compares hostnames. Such a host cannot be allowed at all,
            // which the egress policy states, so it is dropped here rather than
            // compared as text that never matches.
            (!host.starts_with('[')).then(|| host.to_owned())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use arsox_sdk::proto::common::v1::Secret;
    use std::collections::HashMap;

    /// A server as the SDK would send one.
    fn server(name: &str, url: &str, headers: &[(&str, &str)]) -> McpServer {
        McpServer {
            name: name.to_owned(),
            url: url.to_owned(),
            headers: headers
                .iter()
                .map(|(header, value)| {
                    (
                        (*header).to_owned(),
                        Secret {
                            value: Some((*value).to_owned()),
                            display: None,
                        },
                    )
                })
                .collect::<HashMap<String, Secret>>(),
        }
    }

    /// The storage toolset a host application would declare.
    fn storage() -> McpServer {
        server(
            "storage",
            "https://elysium.example.com/mcp/storage",
            &[
                ("X-Workspace", "workspace-7"),
                ("Authorization", "Bearer the-storage-token"),
            ],
        )
    }

    #[test]
    fn an_ordinary_set_of_servers_is_accepted() {
        let servers = [
            storage(),
            server("search_2", "http://127.0.0.1:9000/mcp", &[]),
            server("docs", "https://docs.example.com/mcp", &[("X-Key", "")]),
        ];

        assert_eq!(refusal(&servers), Ok(()));
        assert_eq!(refusal(&[]), Ok(()));
    }

    #[test]
    fn a_name_is_an_identifier_both_clis_can_carry() {
        // A dot would nest the server inside Codex's dotted config path, and a
        // quote or space would break the tool name the model is shown.
        for name in [
            "",
            "has.dot",
            "has space",
            "quote\"d",
            "slash/ed",
            "ünïcode",
        ] {
            let error = refusal(&[server(name, "https://example.com/mcp", &[])])
                .expect_err("should be refused");
            assert!(error.contains("name"), "{name:?}: {error}");
        }

        let long = "a".repeat(MAX_NAME + 1);
        assert!(refusal(&[server(&long, "https://example.com/mcp", &[])]).is_err());
    }

    #[test]
    fn a_name_arsox_reserves_for_its_own_tools_is_refused() {
        for name in ["arsox", "Arsox-tools", "arsox_integration"] {
            let error = refusal(&[server(name, "https://example.com/mcp", &[])])
                .expect_err("should be refused");
            assert!(error.contains("reserved"), "{error}");
        }
    }

    #[test]
    fn a_name_declared_twice_is_refused_whatever_its_case() {
        let error = refusal(&[
            server("search", "https://a.example.com/mcp", &[]),
            server("Search", "https://b.example.com/mcp", &[]),
        ])
        .expect_err("should be refused");

        assert!(error.contains("more than once"), "{error}");
    }

    #[test]
    fn a_url_must_be_http_and_name_a_host() {
        for url in [
            "",
            "not a url",
            "ftp://example.com/mcp",
            "file:///etc/passwd",
            "stdio:npx",
            "https://user:pass@example.com/mcp",
            "https://example.com/${HOME}",
        ] {
            assert!(
                refusal(&[server("search", url, &[])]).is_err(),
                "{url:?} should be refused"
            );
        }

        let long = format!("https://example.com/{}", "a".repeat(MAX_URL));
        assert!(refusal(&[server("search", &long, &[])]).is_err());
    }

    #[test]
    fn a_header_must_be_a_valid_name_with_a_single_line_value() {
        for (header, value) in [
            ("", "value"),
            ("Has Space", "value"),
            ("Colon:", "value"),
            ("X-Split", "one\r\nX-Injected: two"),
            ("X-Nul", "a\0b"),
        ] {
            assert!(
                refusal(&[server(
                    "search",
                    "https://example.com/mcp",
                    &[(header, value)]
                )])
                .is_err(),
                "{header:?} should be refused"
            );
        }

        let long = "v".repeat(MAX_HEADER_VALUE + 1);
        assert!(
            refusal(&[server(
                "search",
                "https://example.com/mcp",
                &[("X-Long", &long)]
            )])
            .is_err()
        );
    }

    #[test]
    fn a_header_declared_twice_is_refused_whatever_its_case() {
        let error = refusal(&[server(
            "search",
            "https://example.com/mcp",
            &[
                ("Authorization", "Bearer one"),
                ("authorization", "Bearer two"),
            ],
        )])
        .expect_err("should be refused");

        assert!(error.contains("more than once"), "{error}");
    }

    #[test]
    fn a_refusal_never_carries_a_header_value() {
        // An error body is a log line somewhere, and a header value is a
        // credential far more often than not.
        let error = refusal(&[server(
            "search",
            "https://example.com/mcp",
            &[("X-Split", "the-secret-value\nX-More: yes")],
        )])
        .expect_err("should be refused");

        assert!(!error.contains("the-secret-value"), "{error}");
        assert!(
            error.contains("search") && error.contains("X-Split"),
            "{error}"
        );
    }

    #[test]
    fn the_limits_are_counted() {
        let many: Vec<McpServer> = (0..=MAX_SERVERS)
            .map(|index| server(&format!("server-{index}"), "https://example.com/mcp", &[]))
            .collect();
        assert!(refusal(&many).is_err());

        let headers: Vec<(String, &str)> = (0..=MAX_HEADERS_PER_SERVER)
            .map(|index| (format!("X-Header-{index}"), "value"))
            .collect();
        let borrowed: Vec<(&str, &str)> = headers
            .iter()
            .map(|(header, value)| (header.as_str(), *value))
            .collect();
        assert!(refusal(&[server("search", "https://example.com/mcp", &borrowed)]).is_err());
    }

    #[test]
    fn claude_is_handed_references_rather_than_values() {
        // argv is readable by anything on the host that can run `ps`. The value
        // travels in the environment and the command line names the variable.
        let args = claude_args(&[storage()]);

        assert_eq!(args[0], "--mcp-config");
        assert_eq!(args[2], "--strict-mcp-config");
        assert!(!args.concat().contains("the-storage-token"), "{args:?}");

        let config: serde_json::Value =
            serde_json::from_str(&args[1]).expect("the config should be JSON");
        let storage = &config["mcpServers"]["storage"];

        assert_eq!(storage["type"], "http");
        assert_eq!(storage["url"], "https://elysium.example.com/mcp/storage");
        // Ordered by header name, so the variables are stable across launches.
        assert_eq!(
            storage["headers"]["Authorization"],
            "${MCP_HEADER_SECRET_0_0}"
        );
        assert_eq!(
            storage["headers"]["X-Workspace"],
            "${MCP_HEADER_SECRET_0_1}"
        );
    }

    #[test]
    fn the_environment_carries_every_value_under_the_referenced_name() {
        let env = environment(&[
            storage(),
            server(
                "docs",
                "https://docs.example.com/mcp",
                &[("X-Key", "docs-key")],
            ),
        ]);

        let pairs: Vec<(&str, &str)> = env
            .iter()
            .map(|variable| (variable.key.as_str(), variable.value.as_str()))
            .collect();

        assert_eq!(
            pairs,
            [
                ("MCP_HEADER_SECRET_0_0", "Bearer the-storage-token"),
                ("MCP_HEADER_SECRET_0_1", "workspace-7"),
                ("MCP_HEADER_SECRET_1_0", "docs-key"),
            ]
        );
        assert!(
            env.iter().all(|variable| variable.secret),
            "a header value is never rendered in a log"
        );
        // Neither an `ARSOX_` variable, which no agent is ever given, nor one
        // shaped like a provider credential, which the scrub would refuse.
        assert!(
            env.iter().all(
                |variable| crate::harness::spawn::declared_key_refusal(&variable.key).is_none()
            )
        );
    }

    #[test]
    fn codex_is_handed_the_same_references_as_config_overrides() {
        let args = codex_args(&[storage(), server("plain", "http://127.0.0.1:9000/mcp", &[])]);

        assert_eq!(
            args,
            [
                "-c",
                "mcp_servers.storage.url=\"https://elysium.example.com/mcp/storage\"",
                "-c",
                "mcp_servers.storage.env_http_headers={\"Authorization\"=\"MCP_HEADER_SECRET_0_0\",\
                 \"X-Workspace\"=\"MCP_HEADER_SECRET_0_1\"}",
                "-c",
                "mcp_servers.plain.url=\"http://127.0.0.1:9000/mcp\"",
            ]
        );
        assert!(!args.concat().contains("the-storage-token"));
    }

    #[test]
    fn a_thread_with_no_servers_changes_nothing_about_its_launch() {
        assert!(claude_args(&[]).is_empty());
        assert!(codex_args(&[]).is_empty());
        assert!(environment(&[]).is_empty());
    }

    #[test]
    fn a_stored_server_that_fails_the_rule_is_skipped_at_launch() {
        // The API refuses these at creation. This is the second gate, for
        // settings stored before that check existed: a name with a dot would
        // nest inside Codex's config path, so it must not reach the command line.
        let servers = [
            server(
                "bad.name",
                "https://example.com/mcp",
                &[("X-Key", "bad-value")],
            ),
            storage(),
            server("storage", "https://duplicate.example.com/mcp", &[]),
        ];

        assert_eq!(names(&servers), ["storage"]);
        assert!(!codex_args(&servers).concat().contains("bad.name"));
        assert!(!codex_args(&servers).concat().contains("duplicate"));

        // Numbered after the skip, so the variables stay dense.
        assert_eq!(environment(&servers)[0].key, "MCP_HEADER_SECRET_0_0");
    }

    #[test]
    fn the_hosts_are_exact_and_carry_no_port() {
        let servers = [
            storage(),
            server("local", "http://127.0.0.1:9000/mcp", &[]),
            server("single", "http://elysium-api:8080/mcp", &[]),
            server("six", "http://[::1]:9000/mcp", &[]),
        ];

        assert_eq!(
            hosts(&servers),
            ["elysium.example.com", "127.0.0.1", "elysium-api"]
        );
    }
}
