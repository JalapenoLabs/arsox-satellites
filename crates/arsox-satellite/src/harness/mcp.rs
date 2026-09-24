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
//! # Relayed servers ride the same launch
//!
//! A thread's `relayed_mcp_servers` are served by the satellite itself, at
//! `{grant}/mcp/{name}` on the proxy the harness already reaches its model
//! through, and answered by the host application over the relay. See
//! [`crate::relay`]. To a CLI each is one more streamable HTTP server with no
//! headers, so it is rendered beside the declared ones, counts toward
//! `--strict-mcp-config`, and is allowed by name under the narrow posture. The
//! two lists share one namespace. Codex is also told to wait on a relayed call
//! for longer than the relay's own deadline, because its default is a minute.
//!
//! # Servers run by the thread's own services
//!
//! A server may name one of the thread's [services](crate::services) instead of
//! a URL, for an address that only exists once a turn has started the service.
//! It is rendered as `http://127.0.0.1:<port><path>` with the port the service
//! was given, and is otherwise a declared server like any other: it counts
//! toward `--strict-mcp-config`, is allowed by name under the narrow posture,
//! and may carry headers. It opens no host on the egress allowlist, because
//! loopback is reached directly.
//!
//! # Validation happens twice
//!
//! [`refusal`] runs at thread creation, where the caller is still listening and
//! can be told which server was wrong. The spawn applies the same per-server
//! rule again and skips what fails it, for settings stored before the rule
//! existed. A name reaches a Codex config key and a Claude tool name unescaped,
//! so the second gate is load-bearing rather than tidy.

use crate::harness::spawn::AgentVar;
use crate::services::Addresses;
use arsox_sdk::proto::settings::v1::{
    McpServer, RelayedMcpServer, RelayedTool, Service, ServiceEndpoint,
};
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

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

/// Most tools one relayed server may offer.
///
/// None of them reaches a command line, since the harness lists them over HTTP,
/// so this bounds the settings a thread carries rather than an argument. Every
/// tool's description and schema is shown to the model on every request, and a
/// server past this many is spending an agent's context on a catalogue.
pub const MAX_RELAYED_TOOLS: usize = 64;

/// Longest tool name, the same rule a server name follows.
///
/// A tool reaches the model as `mcp__<server>__<tool>`, and both CLIs cap a
/// whole tool name, so the two halves are bounded alike.
pub const MAX_TOOL_NAME: usize = 64;

/// Longest tool description, in bytes.
pub const MAX_TOOL_DESCRIPTION: usize = 4096;

/// Longest input schema, in bytes of JSON.
///
/// Room for a detailed schema with nested objects and enumerations, and bounded
/// because it is sent to the model on every request of every turn.
pub const MAX_INPUT_SCHEMA: usize = 64 * 1024;

/// Longest relayed server instructions, in bytes.
pub const MAX_INSTRUCTIONS: usize = 8192;

/// How long Codex waits on one relayed tool call before giving up itself.
///
/// Codex 0.147.0 abandons an MCP call after 60 seconds unless told otherwise,
/// which would cut a file transfer off long before the relay's own deadline. A
/// minute past that deadline, so the satellite's answer, which says what
/// happened, is what the agent reads rather than the CLI's generic timeout.
/// Claude waits far longer than the deadline by default and needs no setting.
const CODEX_RELAYED_TOOL_TIMEOUT: Duration =
    Duration::from_secs(crate::relay::CALL_DEADLINE.as_secs() + 60);

/// Why a thread's servers may not be used, when they may not.
///
/// The reason names the settings field it is about, `settings.mcp_servers` or
/// `settings.relayed_mcp_servers`, and the server, and never carries a header
/// value: half of those are credentials by definition, and an error body is a
/// log line somewhere.
///
/// The two lists share one namespace, because both reach the agent as
/// `mcp__<name>` and a CLI holds one server per name.
///
/// A server reached through a service must name one of `services`, the
/// thread's own declarations, exactly as it was declared.
///
/// # Errors
///
/// Returns the first reason found, which is enough for a caller to fix and
/// resubmit.
pub fn refusal(
    servers: &[McpServer],
    relayed: &[RelayedMcpServer],
    services: &[Service],
) -> Result<(), String> {
    const DECLARED: &str = "settings.mcp_servers";
    const RELAYED: &str = "settings.relayed_mcp_servers";

    for (field, count) in [(DECLARED, servers.len()), (RELAYED, relayed.len())] {
        if count > MAX_SERVERS {
            return Err(format!(
                "{field}: declares {count} servers, and a thread may declare at most \
                 {MAX_SERVERS}"
            ));
        }
    }

    for server in servers {
        if let Some(endpoint) = server.service.as_ref()
            && !services
                .iter()
                .any(|service| service.name == endpoint.service)
        {
            return Err(format!(
                "{DECLARED}: server {:?} is reached through service {:?}, which \
                 settings.services does not declare",
                server.name, endpoint.service
            ));
        }
    }

    let mut seen = BTreeSet::new();

    let named = servers
        .iter()
        .map(|server| (DECLARED, &server.name, server_refusal(server)))
        .chain(
            relayed
                .iter()
                .map(|server| (RELAYED, &server.name, relayed_refusal(server))),
        );

    for (field, name, refused) in named {
        if let Some(reason) = refused {
            return Err(format!("{field}: {reason}"));
        }

        // Case-insensitively, because a model reading `mcp__Search__query` and
        // `mcp__search__query` beside each other has no way to tell which one
        // it meant.
        if !seen.insert(name.to_ascii_lowercase()) {
            return Err(format!(
                "{field}: server {name:?} is declared more than once across \
                 {DECLARED} and {RELAYED}"
            ));
        }
    }

    Ok(())
}

/// Why a server name may not be used, when it may not.
///
/// One rule for both kinds of server, because both names travel the same
/// places: a Codex config key, a Claude JSON key, and every tool name.
fn name_refusal(name: &str) -> Option<String> {
    if name.is_empty() || name.len() > MAX_NAME {
        return Some(format!(
            "server {name:?} needs a name of 1 to {MAX_NAME} characters"
        ));
    }

    // No dot, because Codex reads the name as one segment of a dotted config
    // path and a dot would nest it. No quote or space, because it is a JSON key
    // and part of a tool name. This is the character set both CLIs accept in a
    // tool name, so it is the one that works everywhere a name travels.
    if !is_identifier(name) {
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

    None
}

/// Whether a name uses only the characters both CLIs accept in a tool name.
///
/// Also the rule a service name follows, since a service name becomes part of a
/// variable name and nothing wider would survive the trip.
pub(crate) fn is_identifier(name: &str) -> bool {
    name.chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
}

/// Why one relayed server may not be served, when it may not.
fn relayed_refusal(server: &RelayedMcpServer) -> Option<String> {
    let name = &server.name;

    if let Some(reason) = name_refusal(name) {
        return Some(reason);
    }

    if server.instructions.len() > MAX_INSTRUCTIONS {
        return Some(format!(
            "server {name:?} has instructions longer than {MAX_INSTRUCTIONS} bytes"
        ));
    }

    if server.tools.len() > MAX_RELAYED_TOOLS {
        return Some(format!(
            "server {name:?} offers {} tools, and a server may offer at most {MAX_RELAYED_TOOLS}",
            server.tools.len()
        ));
    }

    let mut tool_names = BTreeSet::new();

    for tool in &server.tools {
        if let Some(reason) = tool_refusal(tool) {
            return Some(format!("server {name:?} tool {:?} {reason}", tool.name));
        }

        // Exactly, not ignoring case: a tool name is matched exactly by both
        // CLIs and by the call that names it, and unlike a server it is never
        // a config key that would fold.
        if !tool_names.insert(tool.name.as_str()) {
            return Some(format!(
                "server {name:?} tool {:?} is declared more than once",
                tool.name
            ));
        }
    }

    None
}

/// Why one relayed tool may not be offered, completing "tool `t` ...".
fn tool_refusal(tool: &RelayedTool) -> Option<String> {
    if tool.name.is_empty() || tool.name.len() > MAX_TOOL_NAME || !is_identifier(&tool.name) {
        return Some(format!(
            "needs a name of 1 to {MAX_TOOL_NAME} ASCII letters, digits, '_', and '-'"
        ));
    }

    if tool.description.len() > MAX_TOOL_DESCRIPTION {
        return Some(format!(
            "has a description longer than {MAX_TOOL_DESCRIPTION} bytes"
        ));
    }

    if tool.input_schema_json.len() > MAX_INPUT_SCHEMA {
        return Some(format!(
            "has an input schema longer than {MAX_INPUT_SCHEMA} bytes"
        ));
    }

    // An object whose `type` is `object`, because that is the one shape MCP
    // allows a tool's input schema to take, and a CLI handed anything else
    // refuses the whole server rather than the one tool.
    let schema = serde_json::from_str::<serde_json::Value>(&tool.input_schema_json).ok();
    let is_object_schema = schema
        .as_ref()
        .and_then(serde_json::Value::as_object)
        .is_some_and(|object| {
            object.get("type").and_then(serde_json::Value::as_str) == Some("object")
        });

    if !is_object_schema {
        return Some(
            "needs an input schema that is a JSON object with \"type\": \"object\"".to_owned(),
        );
    }

    None
}

/// Why one server may not be used, when it may not.
fn server_refusal(server: &McpServer) -> Option<String> {
    let name = &server.name;

    if let Some(reason) = name_refusal(name) {
        return Some(reason);
    }

    let reached = match server.service.as_ref() {
        None => url_refusal(&server.url),
        Some(endpoint) => endpoint_refusal(&server.url, endpoint),
    };
    if let Some(reason) = reached {
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

/// Why a server reached through a service may not be used, completing "server
/// `name` ...".
///
/// Exactly one of the two addresses, because a server with both would be
/// rendered as one of them and the other would be a declaration that silently
/// does nothing. Whether the service exists is the thread's question rather than
/// the server's, and [`refusal`] asks it.
fn endpoint_refusal(url: &str, endpoint: &ServiceEndpoint) -> Option<String> {
    if !url.is_empty() {
        return Some("sets both a url and a service, and must set exactly one".to_owned());
    }

    if endpoint.service.is_empty() {
        return Some("is reached through a service but names none".to_owned());
    }

    crate::services::path_refusal(&endpoint.path)
        .map(|reason| format!("is reached through a service at a path that {reason}"))
}

/// Why a server URL may not be used, completing "server `name` ...".
fn url_refusal(url: &str) -> Option<String> {
    if url.is_empty() {
        return Some("needs a url, or a service to be reached through".to_owned());
    }

    literal_url_refusal(url).map(str::to_owned)
}

/// The rule a literal server URL follows.
fn literal_url_refusal(url: &str) -> Option<&'static str> {
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

/// How the harness reaches one server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reached {
    /// At the URL the thread declared.
    Url,

    /// On loopback, at a port one of the thread's services was given.
    Service,

    /// Served by the satellite for the host application to answer.
    Relay,
}

/// One server a turn launches with, and its headers in a stable order.
#[derive(Debug)]
struct ServerBinding<'a> {
    name: &'a str,
    url: String,
    headers: Vec<HeaderBinding<'a>>,
    reached: Reached,
}

/// The servers one launch carries, decided once and rendered for each consumer.
///
/// Built once per launch and read by every piece that needs the servers: the
/// permission rules, the CLI arguments, the environment, and the egress
/// allowlist. Rendering each from the raw settings instead would re-decide which
/// servers survive the rule once per consumer, and repeat every warning with it.
#[derive(Debug)]
pub struct Launch<'a> {
    servers: Vec<ServerBinding<'a>>,
}

impl<'a> Launch<'a> {
    /// Decides which of a thread's servers a launch carries.
    ///
    /// Headers are ordered by name, because the contract carries them as a map
    /// and a map iterates in no particular order. The variable names are derived
    /// from position, so an unstable order would still be correct and would make
    /// two launches of the same thread differ for no reason anybody could read.
    ///
    /// A server that fails the creation rule, or repeats a name already bound,
    /// is skipped with a warning naming it. The API refuses both at creation, so
    /// this only fires for settings stored before it did.
    ///
    /// `relayed` servers are served on the turn's own grant, at
    /// `{grant}/mcp/{name}`, so they reach the launch only when `grant`, the
    /// model grant's base URL, is present. Declared servers come first, so a
    /// name both lists claim goes to the declared one.
    ///
    /// A declared server reached through a service is rendered at the port
    /// `services` holds for it, and skipped with a warning when it holds none.
    /// A service keeps its port for the whole turn whether or not it became
    /// ready, so none means it was never leased one: its fixed port was taken,
    /// or the kernel offered none, and a `SERVICE_START_FAILED` incident says
    /// so. Settings stored before the API checked the service is declared are
    /// the other way here.
    #[must_use]
    pub fn of(
        servers: &'a [McpServer],
        relayed: &'a [RelayedMcpServer],
        grant: Option<&str>,
        services: &Addresses,
    ) -> Self {
        let mut seen = BTreeSet::new();
        let mut bound = Vec::new();

        for (list, declared) in [
            ("mcp_servers", servers.len()),
            ("relayed_mcp_servers", relayed.len()),
        ] {
            if declared > MAX_SERVERS {
                tracing::warn!(
                    event.name = "harness.mcp.truncated",
                    mcp.list = list,
                    mcp.declared = declared,
                    mcp.limit = MAX_SERVERS,
                    "only the first {{mcp.limit}} of {{mcp.declared}} {{mcp.list}} reach the agent",
                );
            }
        }

        let mut admit = |name: &str, refused: Option<String>| {
            let refused = refused.or_else(|| {
                (!seen.insert(name.to_ascii_lowercase()))
                    .then(|| "is declared more than once".to_owned())
            });

            let Some(reason) = refused else {
                return true;
            };

            tracing::warn!(
                event.name = "harness.mcp.refused",
                mcp.server = name,
                mcp.refusal = reason,
                "an MCP server was withheld from the agent: {{mcp.refusal}}",
            );
            false
        };

        for server in servers.iter().take(MAX_SERVERS) {
            if !admit(&server.name, server_refusal(server)) {
                continue;
            }

            let Some((url, reached)) = address_of(server, services) else {
                continue;
            };

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
                url,
                headers,
                reached,
            });
        }

        let Some(grant) = grant else {
            // Only a launch with no model grant at all, which no turn the
            // runner drives is. Said rather than skipped silently, because the
            // agent will not find tools its thread declared.
            if !relayed.is_empty() {
                tracing::warn!(
                    event.name = "harness.mcp.relayed_ungranted",
                    mcp.relayed = relayed.len(),
                    "a launch with no model grant cannot serve its {{mcp.relayed}} relayed MCP \
                     servers",
                );
            }

            return Self { servers: bound };
        };

        for server in relayed.iter().take(MAX_SERVERS) {
            if !admit(&server.name, relayed_refusal(server)) {
                continue;
            }

            bound.push(ServerBinding {
                name: &server.name,
                url: format!("{}/mcp/{}", grant.trim_end_matches('/'), server.name),
                headers: Vec::new(),
                reached: Reached::Relay,
            });
        }

        Self { servers: bound }
    }

    /// The variables carrying every header value, for the satellite's own layer.
    ///
    /// Every one is a secret, whatever the header is called: the contract gives
    /// header values the credential type, and a value that happens to be public
    /// costs a masked log line.
    #[must_use]
    pub fn environment(&self) -> Vec<AgentVar> {
        self.servers
            .iter()
            .flat_map(|server| &server.headers)
            .map(|binding| AgentVar {
                key: binding.variable.clone(),
                value: binding.value.to_owned(),
                secret: true,
            })
            .collect()
    }

    /// The names of the servers this launch carries.
    ///
    /// For the Claude permission rules, which allow a declared server's tools by
    /// name under a posture that would otherwise refuse them.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.servers
            .iter()
            .map(|server| server.name.to_owned())
            .collect()
    }

    /// The Claude CLI arguments that load these servers, and only these.
    ///
    /// Empty when there are none, which leaves the launch without either flag.
    /// See the module docs for `--strict-mcp-config`.
    #[must_use]
    pub fn claude_args(&self) -> Vec<String> {
        if self.servers.is_empty() {
            return Vec::new();
        }

        let mut config = serde_json::Map::new();

        for server in &self.servers {
            let headers: serde_json::Map<String, serde_json::Value> = server
                .headers
                .iter()
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

    /// The Codex CLI overrides that declare these servers.
    ///
    /// Values are written as quoted TOML rather than bare, unlike the provider
    /// overrides in `spawn`. A URL happens to fail TOML parsing and fall back to
    /// a literal, but a value that only works because it failed to parse is one
    /// character away from parsing as something else. A JSON string is a valid
    /// TOML basic string for every character a validated URL and header name can
    /// hold.
    #[must_use]
    pub fn codex_args(&self) -> Vec<String> {
        let mut args = Vec::new();

        for server in &self.servers {
            let prefix = format!("mcp_servers.{}", server.name);

            args.push("-c".to_owned());
            args.push(format!("{prefix}.url={}", toml_string(&server.url)));

            if server.reached == Reached::Relay {
                args.push("-c".to_owned());
                args.push(format!(
                    "{prefix}.tool_timeout_sec={}",
                    CODEX_RELAYED_TOOL_TIMEOUT.as_secs()
                ));
            }

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

    /// The exact host of every server this launch carries.
    ///
    /// For the egress allowlist, which admits these hosts and nothing beneath
    /// them. Relayed servers and servers run by a service are left out: both
    /// are on loopback, which an agent reaches directly rather than through the
    /// egress proxy, exactly as it reaches its model.
    #[must_use]
    pub fn hosts(&self) -> Vec<String> {
        self.servers
            .iter()
            .filter(|server| server.reached == Reached::Url)
            .filter_map(|server| {
                let parsed = reqwest::Url::parse(&server.url).ok()?;
                let host = parsed.host_str()?;

                // A bracketed IPv6 literal keeps its brackets in `host_str`, and
                // the proxy compares hostnames. Such a host cannot be allowed at
                // all, which the egress policy states, so it is dropped here
                // rather than compared as text that never matches.
                (!host.starts_with('[')).then(|| host.to_owned())
            })
            .collect()
    }
}

/// Where a declared server is reached, and how, when it can be.
///
/// A server run by a service whose port `services` does not hold is withheld
/// with a warning naming it, rather than rendered at an address nothing serves.
fn address_of(server: &McpServer, services: &Addresses) -> Option<(String, Reached)> {
    let Some(endpoint) = server.service.as_ref() else {
        return Some((server.url.clone(), Reached::Url));
    };

    let Some(port) = services.port_of(&endpoint.service) else {
        tracing::warn!(
            event.name = "harness.mcp.service_unbound",
            mcp.server = server.name,
            mcp.service = endpoint.service,
            "an MCP server was withheld from the agent: service {{mcp.service}} was given no \
             port this turn",
        );
        return None;
    };

    Some((
        format!("http://127.0.0.1:{port}{}", endpoint.path),
        Reached::Service,
    ))
}

/// A string as a quoted TOML basic string.
fn toml_string(text: &str) -> String {
    serde_json::Value::String(text.to_owned()).to_string()
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
            service: None,
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

    /// The refusal for declared servers alone, which most tests here are about.
    fn declared_refusal(servers: &[McpServer]) -> Result<(), String> {
        refusal(servers, &[], &[])
    }

    /// A relayed server offering the named tools, each with a valid schema.
    fn relayed(name: &str, tools: &[&str]) -> RelayedMcpServer {
        RelayedMcpServer {
            name: name.to_owned(),
            instructions: String::new(),
            tools: tools
                .iter()
                .map(|tool| RelayedTool {
                    name: (*tool).to_owned(),
                    description: "Does a thing.".to_owned(),
                    input_schema_json: r#"{"type":"object"}"#.to_owned(),
                })
                .collect(),
        }
    }

    /// The model grant's base URL, as the runner mints one.
    const GRANT: &str = "http://127.0.0.1:41000/t/the-turn-token";

    #[test]
    fn an_ordinary_relayed_server_is_accepted_beside_declared_ones() {
        assert_eq!(
            refusal(
                &[storage()],
                &[relayed("elysium", &["upload", "download"])],
                &[]
            ),
            Ok(())
        );
    }

    #[test]
    fn a_name_is_unique_across_both_lists_whatever_its_case() {
        let error = refusal(&[storage()], &[relayed("Storage", &["upload"])], &[])
            .expect_err("should be refused");

        assert!(
            error.starts_with("settings.relayed_mcp_servers:"),
            "{error}"
        );
        assert!(error.contains("more than once"), "{error}");
    }

    #[test]
    fn a_relayed_server_name_follows_the_declared_rule() {
        for name in ["", "has.dot", "arsox-tools"] {
            let error =
                refusal(&[], &[relayed(name, &["upload"])], &[]).expect_err("should be refused");
            assert!(
                error.starts_with("settings.relayed_mcp_servers:"),
                "{error}"
            );
        }
    }

    #[test]
    fn a_tool_needs_a_unique_identifier_name() {
        for tools in [&["has space"][..], &[""], &["upload", "upload"]] {
            let error =
                refusal(&[], &[relayed("elysium", tools)], &[]).expect_err("should be refused");
            assert!(error.contains("tool"), "{error}");
        }

        let long = "t".repeat(MAX_TOOL_NAME + 1);
        refusal(&[], &[relayed("elysium", &[&long])], &[])
            .expect_err("a long name should be refused");
    }

    #[test]
    fn a_tool_schema_must_be_an_object_schema_within_its_bound() {
        for schema in [
            "",
            "not json",
            "[]",
            r#""object""#,
            r#"{"properties":{}}"#,
            r#"{"type":"string"}"#,
        ] {
            let mut server = relayed("elysium", &["upload"]);
            server.tools[0].input_schema_json = schema.to_owned();

            let error = refusal(&[], &[server], &[]).expect_err("should be refused");
            assert!(error.contains("schema"), "{schema:?}: {error}");
        }

        let mut oversized = relayed("elysium", &["upload"]);
        oversized.tools[0].input_schema_json = format!(
            r#"{{"type":"object","description":"{}"}}"#,
            "d".repeat(MAX_INPUT_SCHEMA)
        );
        refusal(&[], &[oversized], &[]).expect_err("an oversized schema should be refused");
    }

    #[test]
    fn the_relayed_limits_are_counted() {
        let many_servers: Vec<RelayedMcpServer> = (0..=MAX_SERVERS)
            .map(|index| relayed(&format!("server-{index}"), &["upload"]))
            .collect();
        refusal(&[], &many_servers, &[]).expect_err("too many servers should be refused");

        let tool_names: Vec<String> = (0..=MAX_RELAYED_TOOLS)
            .map(|index| format!("tool-{index}"))
            .collect();
        let borrowed: Vec<&str> = tool_names.iter().map(String::as_str).collect();
        refusal(&[], &[relayed("elysium", &borrowed)], &[])
            .expect_err("too many tools should be refused");

        let mut described = relayed("elysium", &["upload"]);
        described.tools[0].description = "d".repeat(MAX_TOOL_DESCRIPTION + 1);
        refusal(&[], &[described], &[]).expect_err("a long description should be refused");

        let mut instructed = relayed("elysium", &["upload"]);
        instructed.instructions = "i".repeat(MAX_INSTRUCTIONS + 1);
        refusal(&[], &[instructed], &[]).expect_err("long instructions should be refused");
    }

    #[test]
    fn a_relayed_server_is_served_on_the_turns_grant_to_both_harnesses() {
        let declared = [storage()];
        let relayed = [relayed("elysium", &["upload"])];
        let launch = Launch::of(&declared, &relayed, Some(GRANT), &Addresses::default());

        assert_eq!(launch.names(), ["storage", "elysium"]);

        let args = launch.claude_args();
        let config: serde_json::Value =
            serde_json::from_str(&args[1]).expect("the config should be JSON");
        assert_eq!(
            config["mcpServers"]["elysium"],
            serde_json::json!({
                "type": "http",
                "url": format!("{GRANT}/mcp/elysium"),
                "headers": {},
            })
        );

        let codex = launch.codex_args();
        assert!(
            codex.contains(&format!("mcp_servers.elysium.url=\"{GRANT}/mcp/elysium\"")),
            "{codex:?}"
        );
        assert!(
            codex.contains(&format!(
                "mcp_servers.elysium.tool_timeout_sec={}",
                CODEX_RELAYED_TOOL_TIMEOUT.as_secs()
            )),
            "{codex:?}"
        );
        // Only the relayed server is given the long wait.
        assert_eq!(
            codex
                .iter()
                .filter(|arg| arg.contains("tool_timeout_sec"))
                .count(),
            1
        );

        // Loopback, reached directly rather than through the egress proxy.
        assert_eq!(launch.hosts(), ["elysium.example.com"]);
    }

    #[test]
    fn relayed_servers_alone_still_hold_claude_to_the_declared_set() {
        let relayed = [relayed("elysium", &["upload"])];
        let args = Launch::of(&[], &relayed, Some(GRANT), &Addresses::default()).claude_args();

        assert!(args.contains(&"--strict-mcp-config".to_owned()), "{args:?}");
    }

    #[test]
    fn a_launch_with_no_grant_serves_no_relayed_server() {
        let relayed = [relayed("elysium", &["upload"])];

        assert!(
            Launch::of(&[], &relayed, None, &Addresses::default())
                .names()
                .is_empty()
        );
    }

    #[test]
    fn a_relayed_name_a_declared_server_already_holds_is_skipped_at_launch() {
        let declared = [storage()];
        let relayed = [relayed("STORAGE", &["upload"])];

        assert_eq!(
            Launch::of(&declared, &relayed, Some(GRANT), &Addresses::default()).names(),
            ["storage"]
        );
    }

    #[test]
    fn an_ordinary_set_of_servers_is_accepted() {
        let servers = [
            storage(),
            server("search_2", "http://127.0.0.1:9000/mcp", &[]),
            server("docs", "https://docs.example.com/mcp", &[("X-Key", "")]),
        ];

        assert_eq!(declared_refusal(&servers), Ok(()));
        assert_eq!(declared_refusal(&[]), Ok(()));
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
            let error = declared_refusal(&[server(name, "https://example.com/mcp", &[])])
                .expect_err("should be refused");
            assert!(error.contains("name"), "{name:?}: {error}");
        }

        let long = "a".repeat(MAX_NAME + 1);
        assert!(declared_refusal(&[server(&long, "https://example.com/mcp", &[])]).is_err());
    }

    #[test]
    fn a_name_arsox_reserves_for_its_own_tools_is_refused() {
        for name in ["arsox", "Arsox-tools", "arsox_integration"] {
            let error = declared_refusal(&[server(name, "https://example.com/mcp", &[])])
                .expect_err("should be refused");
            assert!(error.contains("reserved"), "{error}");
        }
    }

    #[test]
    fn a_name_declared_twice_is_refused_whatever_its_case() {
        let error = declared_refusal(&[
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
                declared_refusal(&[server("search", url, &[])]).is_err(),
                "{url:?} should be refused"
            );
        }

        let long = format!("https://example.com/{}", "a".repeat(MAX_URL));
        assert!(declared_refusal(&[server("search", &long, &[])]).is_err());
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
                declared_refusal(&[server(
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
            declared_refusal(&[server(
                "search",
                "https://example.com/mcp",
                &[("X-Long", &long)]
            )])
            .is_err()
        );
    }

    #[test]
    fn a_header_declared_twice_is_refused_whatever_its_case() {
        let error = declared_refusal(&[server(
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
        let error = declared_refusal(&[server(
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
        assert!(declared_refusal(&many).is_err());

        let headers: Vec<(String, &str)> = (0..=MAX_HEADERS_PER_SERVER)
            .map(|index| (format!("X-Header-{index}"), "value"))
            .collect();
        let borrowed: Vec<(&str, &str)> = headers
            .iter()
            .map(|(header, value)| (header.as_str(), *value))
            .collect();
        assert!(
            declared_refusal(&[server("search", "https://example.com/mcp", &borrowed)]).is_err()
        );
    }

    #[test]
    fn claude_is_handed_references_rather_than_values() {
        // argv is readable by anything on the host that can run `ps`. The value
        // travels in the environment and the command line names the variable.
        let args = Launch::of(&[storage()], &[], None, &Addresses::default()).claude_args();

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
        let env = Launch::of(
            &[
                storage(),
                server(
                    "docs",
                    "https://docs.example.com/mcp",
                    &[("X-Key", "docs-key")],
                ),
            ],
            &[],
            None,
            &Addresses::default(),
        )
        .environment();

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
        let args = Launch::of(
            &[storage(), server("plain", "http://127.0.0.1:9000/mcp", &[])],
            &[],
            None,
            &Addresses::default(),
        )
        .codex_args();

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
        assert!(
            Launch::of(&[], &[], None, &Addresses::default())
                .claude_args()
                .is_empty()
        );
        assert!(
            Launch::of(&[], &[], None, &Addresses::default())
                .codex_args()
                .is_empty()
        );
        assert!(
            Launch::of(&[], &[], None, &Addresses::default())
                .environment()
                .is_empty()
        );
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

        assert_eq!(
            Launch::of(&servers, &[], None, &Addresses::default()).names(),
            ["storage"]
        );
        assert!(
            !Launch::of(&servers, &[], None, &Addresses::default())
                .codex_args()
                .concat()
                .contains("bad.name")
        );
        assert!(
            !Launch::of(&servers, &[], None, &Addresses::default())
                .codex_args()
                .concat()
                .contains("duplicate")
        );

        // Numbered after the skip, so the variables stay dense.
        assert_eq!(
            Launch::of(&servers, &[], None, &Addresses::default()).environment()[0].key,
            "MCP_HEADER_SECRET_0_0"
        );
    }

    /// A server run by the named service, answering on `path`.
    fn served_by(name: &str, service: &str, path: &str) -> McpServer {
        McpServer {
            name: name.to_owned(),
            service: Some(ServiceEndpoint {
                service: service.to_owned(),
                path: path.to_owned(),
            }),
            ..McpServer::default()
        }
    }

    /// A service the thread declares, as far as the MCP rule reads one.
    fn declared_service(name: &str) -> Service {
        Service {
            name: name.to_owned(),
            command: "blender --background --python bridge.py".to_owned(),
            ..Service::default()
        }
    }

    /// Where a turn's services listen, as the runner hands them to a launch.
    fn listening(services: &[(&str, u16)]) -> Addresses {
        services
            .iter()
            .map(|(name, port)| crate::services::Address {
                name: (*name).to_owned(),
                port: *port,
            })
            .collect()
    }

    #[test]
    fn a_server_run_by_a_declared_service_is_accepted() {
        assert_eq!(
            refusal(
                &[served_by("blender", "blender-mcp", "/mcp"), storage()],
                &[],
                &[declared_service("blender-mcp")],
            ),
            Ok(())
        );
    }

    #[test]
    fn a_server_run_by_a_service_the_thread_never_declared_is_refused() {
        let error = refusal(
            &[served_by("blender", "blender-mcp", "/mcp")],
            &[],
            &[declared_service("BLENDER-MCP")],
        )
        .expect_err("should be refused");

        assert!(error.starts_with("settings.mcp_servers:"), "{error}");
        assert!(error.contains("does not declare"), "{error}");
    }

    #[test]
    fn a_server_sets_exactly_one_of_a_url_and_a_service() {
        let services = [declared_service("blender-mcp")];

        let both = McpServer {
            url: "https://elysium.example.com/mcp".to_owned(),
            ..served_by("blender", "blender-mcp", "/mcp")
        };
        let error = refusal(&[both], &[], &services).expect_err("both should be refused");
        assert!(error.contains("exactly one"), "{error}");

        let neither = McpServer {
            name: "blender".to_owned(),
            ..McpServer::default()
        };
        let error = refusal(&[neither], &[], &services).expect_err("neither should be refused");
        assert!(error.contains("needs a url"), "{error}");
    }

    #[test]
    fn a_service_path_starts_with_a_slash_and_stays_on_loopback() {
        let services = [declared_service("blender-mcp")];

        for path in ["", "mcp", "/has space", "/${HOME}"] {
            let error = refusal(&[served_by("blender", "blender-mcp", path)], &[], &services)
                .expect_err("should be refused");
            assert!(error.contains("path"), "{path:?}: {error}");
        }
    }

    #[test]
    fn a_server_run_by_a_service_is_rendered_on_its_port_for_both_harnesses() {
        let servers = [served_by("blender", "blender-mcp", "/mcp"), storage()];
        let launch = Launch::of(
            &servers,
            &[],
            Some(GRANT),
            &listening(&[("blender-mcp", 41_234)]),
        );

        // Named like any declared server, so the narrow posture allows it.
        assert_eq!(launch.names(), ["blender", "storage"]);

        let args = launch.claude_args();
        let config: serde_json::Value =
            serde_json::from_str(&args[1]).expect("the config should be JSON");
        assert_eq!(
            config["mcpServers"]["blender"]["url"],
            "http://127.0.0.1:41234/mcp"
        );
        assert!(args.contains(&"--strict-mcp-config".to_owned()), "{args:?}");

        let codex = launch.codex_args();
        assert!(
            codex.contains(&"mcp_servers.blender.url=\"http://127.0.0.1:41234/mcp\"".to_owned()),
            "{codex:?}"
        );
        // Not relayed, so not given the relay's long wait.
        assert!(
            !codex.iter().any(|arg| arg.contains("tool_timeout_sec")),
            "{codex:?}"
        );

        // Loopback, reached directly, so it opens no host on the allowlist.
        assert_eq!(launch.hosts(), ["elysium.example.com"]);
    }

    #[test]
    fn a_server_whose_service_was_given_no_port_is_withheld_rather_than_misaddressed() {
        let servers = [served_by("blender", "blender-mcp", "/mcp"), storage()];

        assert_eq!(
            Launch::of(&servers, &[], None, &Addresses::default()).names(),
            ["storage"]
        );
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
            Launch::of(&servers, &[], None, &Addresses::default()).hosts(),
            ["elysium.example.com", "127.0.0.1", "elysium-api"]
        );
    }
}
