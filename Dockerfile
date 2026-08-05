# Copyright © 2026 Jalapeno Labs

# The Ubuntu satellite image.
#
# Two stages: a builder that carries the Rust toolchain, and a runtime that
# carries only the binary and what it links against. The toolchain is several
# hundred megabytes and has no business in a published image.

# Pinned exactly, matching rust-toolchain.toml. A floating tag means the image
# and a developer's machine can compile the same commit differently.
FROM rust:1.96.0-slim-bookworm AS builder

WORKDIR /build

# Manifests first, so the dependency layer is rebuilt only when a dependency
# actually changes. Source edits below this line reuse it.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates/arsox-sdk/Cargo.toml crates/arsox-sdk/
COPY crates/arsox-satellite/Cargo.toml crates/arsox-satellite/

# `cargo package` reads this, and a missing file named in the manifest fails the
# build rather than being ignored.
COPY crates/arsox-sdk/README.md crates/arsox-sdk/

# Stub sources so the dependency graph compiles without the real code. The
# stubs are overwritten below; only the compiled dependencies survive into the
# next layer.
RUN mkdir -p crates/arsox-sdk/src crates/arsox-satellite/src \
    && echo "" > crates/arsox-sdk/src/lib.rs \
    && echo "" > crates/arsox-satellite/src/lib.rs \
    && echo "fn main() {}" > crates/arsox-satellite/src/main.rs \
    && cargo build --release --workspace \
    && rm -rf crates/arsox-sdk/src crates/arsox-satellite/src

# Migrations are embedded by `sqlx::migrate!` at compile time, so they are build
# input rather than runtime data and must be present before the real build.
COPY crates/arsox-satellite/migrations/ crates/arsox-satellite/migrations/
COPY crates/arsox-satellite/fixtures/ crates/arsox-satellite/fixtures/
COPY crates/arsox-sdk/src/ crates/arsox-sdk/src/
COPY crates/arsox-satellite/src/ crates/arsox-satellite/src/

# Cargo skips a rebuild when only mtimes moved, and COPY resets them in a way it
# does not always notice. Touching the roots makes the rebuild unambiguous.
RUN touch crates/arsox-sdk/src/lib.rs crates/arsox-satellite/src/lib.rs \
    && cargo build --release --bin arsox-satellite


FROM ubuntu:24.04 AS runtime

# `git` and the rest of the agent toolchain land here when the workspace manager
# does. For now the satellite needs certificates for outbound TLS and nothing
# else: SQLite is compiled into the binary rather than linked from the system.
RUN apt-get update \
    && apt-get install --no-install-recommends --yes ca-certificates=20240203 \
    && rm -rf /var/lib/apt/lists/*

# Agents run unprivileged. The satellite process runs as the same user for now;
# when the enforcement layer lands, the pieces an agent must not reach stay
# owned by root while the agent keeps this account.
RUN useradd --create-home --shell /usr/sbin/nologin --uid 10001 arsox \
    && mkdir -p /var/arsox /workspace \
    && chown arsox:arsox /var/arsox /workspace

COPY --from=builder /build/target/release/arsox-satellite /usr/local/bin/arsox-satellite

USER arsox
WORKDIR /workspace

# The database and the workspace are the two things that must survive a
# container replacement. Losing either silently breaks thread resumption.
VOLUME [ "/var/arsox", "/workspace" ]

EXPOSE 8080

# Liveness only, and deliberately unauthenticated: an orchestrator needs to know
# whether the process is alive before it holds a credential.
HEALTHCHECK --interval=10s --timeout=3s --start-period=5s --retries=3 \
    CMD [ "/usr/local/bin/arsox-satellite", "--health-check" ]

ENTRYPOINT [ "/usr/local/bin/arsox-satellite" ]
