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
COPY crates/arsox-harness/Cargo.toml crates/arsox-harness/
COPY crates/arsox-satellite/Cargo.toml crates/arsox-satellite/

# `cargo package` reads this, and a missing file named in the manifest fails the
# build rather than being ignored.
COPY crates/arsox-sdk/README.md crates/arsox-sdk/

# Stub sources so the dependency graph compiles without the real code. The
# stubs are overwritten below; only the compiled dependencies survive into the
# next layer.
RUN mkdir -p crates/arsox-sdk/src crates/arsox-harness/src crates/arsox-satellite/src \
    && echo "" > crates/arsox-sdk/src/lib.rs \
    && echo "" > crates/arsox-harness/src/lib.rs \
    && echo "" > crates/arsox-satellite/src/lib.rs \
    && echo "fn main() {}" > crates/arsox-satellite/src/main.rs \
    && cargo build --release --workspace \
    && rm -rf crates/arsox-sdk/src crates/arsox-harness/src crates/arsox-satellite/src

# Migrations are embedded by `sqlx::migrate!` and the conformance fixtures by
# `include_str!`, so both are build input rather than runtime data and must be
# present before the real build.
COPY crates/arsox-satellite/migrations/ crates/arsox-satellite/migrations/
COPY crates/arsox-harness/fixtures/ crates/arsox-harness/fixtures/
COPY crates/arsox-sdk/src/ crates/arsox-sdk/src/
COPY crates/arsox-harness/src/ crates/arsox-harness/src/
COPY crates/arsox-satellite/src/ crates/arsox-satellite/src/

# Cargo skips a rebuild when only mtimes moved, and COPY resets them in a way it
# does not always notice. Touching the roots makes the rebuild unambiguous.
RUN touch crates/arsox-sdk/src/lib.rs crates/arsox-harness/src/lib.rs crates/arsox-satellite/src/lib.rs \
    && cargo build --release --bin arsox-satellite


FROM ubuntu:24.04 AS runtime

# The agent's working environment, per the README's preinstalled tooling list,
# so agents are not spending their first ten minutes installing basics. Every
# package is pinned exactly; CI rebuilds this image on every relevant push, so
# a version the archive has dropped surfaces as a red build rather than drift.
RUN apt-get update \
    && apt-get install --no-install-recommends --yes \
        ca-certificates=20240203 \
        git=1:2.43.0-1ubuntu7.3 \
        git-lfs=3.4.1-1ubuntu0.4 \
        openssh-client=1:9.6p1-3ubuntu13.18 \
        build-essential=12.10ubuntu1 \
        pkg-config=1.8.1-2build1 \
        curl=8.5.0-2ubuntu10.12 \
        wget=1.21.4-1ubuntu4.4 \
        jq=1.7.1-3ubuntu0.24.04.2 \
        zip=3.0-13ubuntu0.2 \
        unzip=6.0-28ubuntu4.1 \
        ripgrep=14.1.0-1 \
        fd-find=9.0.0-1 \
        less=590-2ubuntu2.1 \
        xz-utils=5.6.1+really5.4.5-1ubuntu0.3 \
        python3=3.12.3-0ubuntu2.1 \
        python3-pip=24.0+dfsg-1ubuntu1.3 \
        python3-venv=3.12.3-0ubuntu2.1 \
    && rm -rf /var/lib/apt/lists/* \
    # Ubuntu ships the binary as `fdfind`; agents and docs expect `fd`.
    && ln -s "$(command -v fdfind)" /usr/local/bin/fd

# Node, from the official tarball rather than a distro or vendor repo: exact
# version, exact checksum, no third-party archive that can rot or redirect.
# corepack rides along, which is how yarn and pnpm are provided.
ARG NODE_VERSION=22.23.2
ARG NODE_SHA256=d60acfe00a2932254bb0ad20e01b0d74397a0875595de719654b214f4b03f307
RUN curl -fsSLo /tmp/node.tar.xz "https://nodejs.org/dist/v${NODE_VERSION}/node-v${NODE_VERSION}-linux-x64.tar.xz" \
    && echo "${NODE_SHA256}  /tmp/node.tar.xz" | sha256sum --check --quiet \
    && tar -xJf /tmp/node.tar.xz -C /usr/local --strip-components=1 --no-same-owner \
    && rm /tmp/node.tar.xz \
    && corepack enable \
    && node --version

# The GitHub CLI, from its release tarball for the same reason as Node.
ARG GH_VERSION=2.97.0
ARG GH_SHA256=a2c9b8497e1f85b1ad0dfcb78b5a622e098801b8e461e459e88e1ee12f018112
RUN curl -fsSLo /tmp/gh.tar.gz "https://github.com/cli/cli/releases/download/v${GH_VERSION}/gh_${GH_VERSION}_linux_amd64.tar.gz" \
    && echo "${GH_SHA256}  /tmp/gh.tar.gz" | sha256sum --check --quiet \
    && tar -xzf /tmp/gh.tar.gz -C /usr/local --strip-components=1 --no-same-owner \
    && rm /tmp/gh.tar.gz \
    && gh --version

# The Jira CLI (ankitpokhrel/jira-cli), which is what the README's `jira`
# entry names. Same release-tarball pattern as gh.
ARG JIRA_VERSION=1.7.0
ARG JIRA_SHA256=b5e0ba4804f3f11f92c483d9a6ea9ebccec1c735cd2e12b0440cab9d7afd626a
RUN curl -fsSLo /tmp/jira.tar.gz "https://github.com/ankitpokhrel/jira-cli/releases/download/v${JIRA_VERSION}/jira_${JIRA_VERSION}_linux_x86_64.tar.gz" \
    && echo "${JIRA_SHA256}  /tmp/jira.tar.gz" | sha256sum --check --quiet \
    && tar -xzf /tmp/jira.tar.gz -C /usr/local --strip-components=1 --no-same-owner \
    && rm /tmp/jira.tar.gz \
    && jira version

# The Claude CLI, the harness the satellite spawns. Pinned exactly, and the
# autoupdater is disabled below because a harness that upgrades itself under a
# running satellite is the version drift everything else here exists to prevent.
ARG CLAUDE_VERSION=2.1.235
RUN npm install --global "@anthropic-ai/claude-code@${CLAUDE_VERSION}" \
    && npm cache clean --force \
    && claude --version

ENV DISABLE_AUTOUPDATER=1

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

# Where a container serves, and the only port worth exposing. `ARSOX_PORT` exists
# for runs with no port mapping in front of them; `docker run -p` is how a
# container's reachable address is decided.
EXPOSE 8080

# Liveness only, and deliberately unauthenticated: an orchestrator needs to know
# whether the process is alive before it holds a credential.
HEALTHCHECK --interval=10s --timeout=3s --start-period=5s --retries=3 \
    CMD [ "/usr/local/bin/arsox-satellite", "--health-check" ]

ENTRYPOINT [ "/usr/local/bin/arsox-satellite" ]
