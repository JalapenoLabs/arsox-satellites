// Copyright © 2026 Jalapeno Labs

//! The satellite binary.
//!
//! Deliberately thin. It installs an allocator, starts tracing, and hands off to
//! the library beside it, which is where everything the satellite actually does
//! lives and where it can be exercised without a running process.

use anyhow::Result;

/// Notably faster along allocating hot paths, which here means request decoding
/// and event fan-out. Unavailable on MSVC, where the system allocator stands in.
#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_ignored| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    arsox_satellite::serve().await
}
