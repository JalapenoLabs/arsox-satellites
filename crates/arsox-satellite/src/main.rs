// Copyright © 2026 Jalapeno Labs

//! The satellite binary.
//!
//! Deliberately thin. It installs an allocator, starts tracing, and hands off to
//! the library beside it, which is where everything the satellite actually does
//! lives and where it can be exercised without a running process.
//!
//! # It is also every gate
//!
//! A brokered thread's `PATH` is a directory of shebang scripts naming this
//! binary as their interpreter, and its checkouts point `core.hooksPath` at a
//! `pre-push` that does the same, so a command an agent types and a push it
//! makes both arrive here. Those branches are taken before the runtime is built
//! and before anything is configured: a gate reads one file, decides, and gets
//! out of the way, and paying for a multi-threaded runtime on the way would put
//! that cost on every command an agent runs. See
//! [the enforcement doc](../../../docs/enforcement.md).

use anyhow::{Result, bail};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

/// Notably faster along allocating hot paths, which here means request decoding
/// and event fan-out. Unavailable on MSVC, where the system allocator stands in.
#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// How long the container healthcheck waits before calling the process dead.
const HEALTH_CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

fn main() -> Result<()> {
    // Ahead of the runtime, because this process may not be a satellite at all:
    // a brokered thread's PATH is full of shebang scripts naming this binary,
    // so an agent's `git status` lands here, and so does the `pre-push` hook
    // git runs on its way to a remote. Each reads one file, decides, and never
    // returns.
    let arguments: Vec<String> = std::env::args().collect();
    if let Some(invocation) = arsox_satellite::broker::shim::invoked_as(&arguments) {
        arsox_satellite::broker::shim::run(&invocation);
    }
    if let Some(invocation) = arsox_satellite::broker::hook::invoked_as(&arguments) {
        arsox_satellite::broker::hook::run(&invocation);
    }

    satellite()
}

/// Boots the runtime and runs the satellite.
///
/// Named apart from the library's `serve` it eventually calls, so the two are
/// not one word meaning two things one line apart.
#[tokio::main]
async fn satellite() -> Result<()> {
    // The container healthcheck runs this same binary rather than curl, so the
    // runtime image carries no HTTP client it would otherwise only need in order
    // to ask itself whether it is alive.
    if std::env::args().any(|argument| argument == "--health-check") {
        return health_check().await;
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_ignored| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    arsox_satellite::serve().await
}

/// Asks the local API whether it is serving, and fails if it is not.
///
/// Deliberately probes over the socket rather than checking that a process
/// exists. A satellite whose listener has wedged is exactly the case a
/// healthcheck is for, and a process-liveness check would call it healthy.
///
/// # Errors
///
/// Returns an error when the port refuses a connection, the request times out,
/// or the response is anything other than a success.
async fn health_check() -> Result<()> {
    // Resolved the way the server resolves it, so a satellite told to listen
    // elsewhere with `ARSOX_PORT` is probed where it actually is rather than
    // reported dead on the default port.
    let port = arsox_satellite::ServeOptions::from_environment().port;

    let probe = async {
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?;

        stream
            .write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await?;

        let mut response = Vec::new();
        stream.read_to_end(&mut response).await?;

        anyhow::Ok(response)
    };

    let response = tokio::time::timeout(HEALTH_CHECK_TIMEOUT, probe)
        .await
        .map_err(|_elapsed| {
            anyhow::anyhow!("the satellite did not answer within the probe window")
        })??;

    let head = String::from_utf8_lossy(&response);
    if !head.starts_with("HTTP/1.1 200") {
        bail!(
            "the satellite answered {}",
            head.lines().next().unwrap_or("nothing at all")
        );
    }

    Ok(())
}
