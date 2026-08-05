// Copyright © 2026 Jalapeno Labs

//! The satellite binary.
//!
//! Deliberately thin. It installs an allocator, starts tracing, and hands off to
//! the library beside it, which is where everything the satellite actually does
//! lives and where it can be exercised without a running process.

use anyhow::{Result, bail};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

/// Notably faster along allocating hot paths, which here means request decoding
/// and event fan-out. Unavailable on MSVC, where the system allocator stands in.
#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// How long the container healthcheck waits before calling the process dead.
const HEALTH_CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

#[tokio::main]
async fn main() -> Result<()> {
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
    let probe = async {
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", 8080)).await?;

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
