# CI

GitHub Actions, on self-hosted runners.

## Runners

Every job targets the org's self-hosted pool:

```yaml
runs-on: [ linux, fedora, flagship, earthly, docker ]
```

Labels are matched as a set, so a runner must carry all five to pick up a job.

**Self-hosted is why there is no caching anywhere in these workflows.** On
GitHub-hosted runners a cold toolchain download is the dominant cost and
`actions/cache` earns its complexity. On a persistent runner it is a cache of a
cache: the work is fast enough that a stale-key bug would cost more debugging
time than caching ever saves. Tools are downloaded fresh, pinned, and
checksum-verified each run.

## Workflows

| Workflow | File | Runs on |
|---|---|---|
| Proto | `.github/workflows/proto.yml` | pushes and PRs touching `proto/` or either generated output root |
| Rust | `.github/workflows/rust.yml` | pushes and PRs touching `crates/`, the workspace manifest, or the toolchain pin |
| Docker | `.github/workflows/docker.yml` | pushes and PRs touching the `Dockerfile`, `crates/`, the workspace manifest, or the toolchain pin |

Each workflow is path-filtered, so editing a README never queues a proto build.
`workflow_dispatch` is enabled on all of them for manual runs, and
`cancel-in-progress` concurrency means a second push supersedes the first rather
than racing it.

## Proto

The protobuf contract is the one thing every other output depends on, so it is
the first thing CI defends. The satellite, three SDKs, and the CLI are all
downstream of these files.

Five checks, in the order a failure is most useful:

1. **Build.** Compiles every file and resolves every import. Nothing below is
   meaningful if the module does not build.
2. **Lint.** `buf lint` under the `STANDARD` category, with
   `disallow_comment_ignores`, so a lint failure cannot be waved through with a
   comment in the file that caused it.
3. **Format.** `buf format --diff --exit-code`, so formatting is settled by a
   tool rather than in review.
4. **Breaking changes.** `buf breaking` at the `FILE` category against the PR's
   base branch. This is the step that turns "the contract only ever grows within
   a major version" from an intention into a guarantee. It runs on pull requests
   only, because "breaking against what" has no useful answer on a direct push.
5. **Codegen is current.** Regenerates into `gen/` and fails if anything moved.

That last check is the one that earns its keep. Generated code is committed, per
[protobuf.md](./protobuf.md), which is only safe if it is provably the output of
the `.proto` files sitting next to it. Without this step a contract change can
land with stale dists, and every downstream consumer then builds against a
contract that no longer exists.

### Pinning

`buf` is pinned by exact version in the workflow's `env` block, downloaded from
its GitHub release, and verified against the `sha256.txt` manifest published
with that release. The installed binary's `--version` is then asserted against
the pin, so a redirect or a partial download fails loudly instead of quietly
linting with a different ruleset.

Codegen plugin versions are pinned separately, in `proto/buf.gen.yaml`.

### Both output roots

The staleness check covers `gen/` **and** `crates/arsox-sdk/src/generated/`.
Rust generates into the SDK crate because `cargo publish` only packages files
beneath the crate directory, and a check that watched one root would silently
stop covering the other the moment a target moved.

### Network dependency

`buf generate` resolves remote plugins from `buf.build`, so the codegen check
needs egress to that host. If the runner pool loses it, the fix is vendoring the
plugins locally rather than dropping the check.

## Rust

`rustup` reads `rust-toolchain.toml` and installs the exact pinned compiler, so
the version lives in one place rather than being repeated in the workflow.

Formatting runs first because it is the cheapest check and the least interesting
failure to discover after a full compile. Clippy runs with `--all-targets`, since
tests and benches are exactly where lint debt accumulates when only the library
is checked, and with `-D warnings` because a warning nobody is forced to read is
a warning nobody reads.

Three checks then cover what a plain `cargo test` misses:

- **Release build.** The satellite ships as a release binary. Different lints
  fire there, and `lto` plus `codegen-units = 1` exercise code paths a debug
  build never links.
- **`--no-default-features` on the SDK.** The satellite consumes the crate with
  its `client` feature off. Without this, the contract types could quietly grow a
  dependency on the client and nobody would notice until a server build broke.
- **`cargo package`.** Catches a crate that cannot be published: files reached
  outside the package directory, missing metadata, a path dependency with no
  version. It caught a declared-but-absent README the first time it ran.

## Docker

The image workflow builds the Ubuntu satellite image and then boots it. The two
steps prove different things. The build proves the builder stage: every `COPY`
path resolves and the workspace compiles. The boot proves the runtime stage: the
binary starts as the unprivileged user, finds every library it links against,
and answers `/healthz`. A missing shared object or a broken entrypoint passes
the build and fails only at boot, which is why the workflow does both.

The Dockerfile names build inputs by path, and nothing else in CI compiles it,
so this workflow is the only thing standing between a crate moving directory and
an image that silently stops building.

The image is built and thrown away. Publishing to docker.io is a release step,
not a CI step, and lands with the release automation.

## Roadmap

- **SDK builds** for TypeScript and Python, each consuming `gen/` rather than
  regenerating it.
- **Conformance suite** once harness mappers exist. That is the job that proves
  the normalization claim, so it belongs in CI from the day the first mapper
  lands.
- **Fedora and Rocky image variants**, and publishing every variant to docker.io
  with each package pinned and the manifest published alongside the image.
- **`buf breaking` against the last release tag** in addition to the base branch,
  so a sequence of individually non-breaking PRs cannot add up to a break across
  a release.
