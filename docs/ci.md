# CI

GitHub Actions, on self-hosted runners.

## Runners

Every job targets the org's self-hosted pool:

```yaml
runs-on: [ linux, rocky9, flagship, earthly, docker ]
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
| Node | `.github/workflows/node.yml` | pushes and PRs touching `sdks/node/`, `gen/ts/`, `crates/`, the workspace manifest, or the toolchain pin |
| Python | `.github/workflows/python.yml` | pushes and PRs touching `sdks/python/`, `gen/python/`, `crates/`, the workspace manifest, or the toolchain pin |
| Docker | `.github/workflows/docker.yml` | pushes and PRs touching the `Dockerfile`, `crates/`, the workspace manifest, or the toolchain pin |
| Release | `.github/workflows/release.yml` | `v*` tags, and `workflow_dispatch` with a version |

Each workflow is path-filtered, so editing a README never queues a proto build.
`workflow_dispatch` is enabled on all of them for manual runs, and
`cancel-in-progress` concurrency means a second push supersedes the first rather
than racing it. Release is the exception on both counts: it is triggered by a
tag rather than a path, and it is never cancelled.

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

## Node

The Node workflow proves the published package builds and that it still drives a
satellite. Those are two claims, and the second is the one worth paying for: the
integration tests spawn the real satellite binary with its `test-util` fake
harness and exercise the SDK from a consumer's seat, so anything the SDK needs
and cannot reach shows up here rather than in somebody's application.

That is why the job builds `arsox-satellite` with `--features test-util` before
it installs a package, and why its path filter watches `crates/` as well as
`sdks/node/` and `gen/ts/`. A satellite change can break the SDK without touching
a line of TypeScript. The build is a debug build, because debug is the path the
suite spawns; a release build would be a second compile of a binary nothing here
runs.

`corepack prepare` pins yarn to the exact version in `packageManager`, and the
step asserts it, so a runner carrying a different yarn fails loudly rather than
resolving the lockfile with a different resolver. `yarn install --immutable`
then fails on a lockfile the install would have changed.

Each satellite the suite starts takes its own port through `ARSOX_PORT`, picked
by binding port 0 and releasing it. Two jobs on one runner cannot collide, and
the suite never skips itself for a busy port, so a green run means the tests ran
rather than stood aside.

## Python

The Python job proves the same two claims the Node one does: the package
installs from its own pins, and it still drives a satellite. So it builds
`arsox-satellite` with `--features test-util` before it installs anything, and
watches `crates/` alongside `sdks/python/` and `gen/python/`. Both suites run:
the unit tests need nothing, and the integration tests spawn the real binary
with the fake harness and exercise the client from a consumer's seat.

**The runner must carry `python3.12`, and the workflow names it explicitly.**
Rocky 9 ships 3.9 as `python3`, the package requires 3.10 or newer, and a
workflow that said `python3` would fail somewhere inside pip rather than at the
top with a reason. The venv step runs the interpreter rather than only resolving
its name, so an absent one, or a shim pointing at an interpreter that has moved,
fails with a sentence naming what to install.

Nothing after that step activates the environment. Each step is its own shell,
so an activation would not survive to the next one, and every command names
`.venv/bin/python` instead. Every version it installs is pinned exactly in
`pyproject.toml`, dev tools included, which is what makes a ruff or mypy finding
on a runner the same finding a laptop reports.

`scripts/sync_proto.py` runs before lint, typecheck, and test. It copies the
committed contract from `gen/python` into the package, where the copy is git
ignored and rebuilt on every run, so a stale one cannot exist to be tested
against. Nothing imports without it.

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
not a CI step, and lives in the Release workflow below.

## Release

One workflow publishes the satellite image and the Node SDK, because they are
one release. The README tells you to pin your image tag and your SDK version
together, and a pipeline that shipped them separately would make that advice
something you have to arrange by hand.

### Trigger

A `v*` tag. `workflow_dispatch` takes a version and reruns the same release,
which is what a half-finished run needs.

Both paths check out the tag rather than a branch, so a rerun cannot quietly
publish a different commit under a version number that is already spent. The
dispatch input accepts `v0.1.0` or `0.1.0` and normalizes; anything that is not
`major.minor.patch` with an optional prerelease suffix fails immediately, before
a runner spends fifteen minutes on it.

**A prerelease claims its exact tag and nothing else.** `v1.0.0-rc.1` pushes
`ubuntu-1.0.0-rc.1` and leaves `ubuntu-latest` and `latest` where they are. A
release candidate that becomes `latest` is how a fleet ends up running one
without deciding to.

### Secrets

Three, and a release without all three does not start.

| Secret | For |
|---|---|
| `DOCKERHUB_USERNAME` | the docker.io account the image is pushed as |
| `DOCKERHUB_TOKEN` | that account's access token, not its password |
| `NPM_TOKEN` | an npm automation token with publish rights to the `@jalapenolabs` scope |

The first step checks all three and names every one that is missing, rather than
failing on the first and making you learn about the second on the next run. This
matters more than it sounds: the alternative is discovering an absent `NPM_TOKEN`
after the image is already on docker.io, and the version number is gone.

`GITHUB_TOKEN` is the one credential nobody configures. It is minted per run, and
the workflow grants it `contents: write` to create the release and upload the
manifest.

### Gates, then publish

Everything verifiable runs before anything leaves the runner:

1. The secrets are present.
2. The tag matches the `[workspace.package]` version in `Cargo.toml`. The
   satellite embeds `CARGO_PKG_VERSION` at compile time and serves it from
   `/v1/version`, so agreement in the manifest is agreement in the binary the
   image ships. An image tagged with a version its binary does not report is the
   drift the rest of this repository works to prevent.
3. The image builds, with its final tags rather than a scratch tag, so the thing
   verified is the thing pushed.
4. The image boots and answers `/healthz`, exactly as the Docker workflow does.
5. The Node package builds, typechecks, and its full suite runs against the
   commit being released, including the integration tests that drive a real
   satellite.
6. `npm pack --dry-run` is inspected: nothing outside `dist` beyond the README
   and `package.json`, and the generated contract present under `dist/proto`.

Then, in order: push the image, attach the manifest to the GitHub release, publish
to npm. The image is the slower and likelier failure, so putting it first means a
broken release usually stops before anything is published at all. The manifest
follows the push because a manifest with no image is useless. npm is last because
it is the only step that can never be redone under the same version.

The ordering does not make a half-release impossible. It makes the worst case one
artifact short rather than a pair that disagree, and it puts the irreversible step
where the fewest things can still go wrong after it.

### The image manifest

The README promises the exact package set is published in the image's manifest.
`.github/scripts/image-manifest.sh` produces it by running inside the image it
just built, with the entrypoint overridden, and the output is attached to the
GitHub release as `arsox-satellite-ubuntu-<version>-manifest.txt`.

It reads the running image rather than the Dockerfile on purpose. A Dockerfile
says what was asked for; the image says what is installed, transitive packages
included, and that is the only version a consumer can act on.

### The npm package

`@jalapenolabs/arsox-sdk`, published public. The scope is not decoration: an
unscoped `arsox-sdk` is a name anyone can take, and a scoped one cannot be
confused for a package Jalapeno Labs did not publish. Scoped packages default to
restricted, so `publishConfig.access` says `public` in `package.json` rather than
in a flag somebody can forget.

The version is set from the tag before the build, so the compiled package and its
manifest agree whatever the committed `package.json` says. A committed version
that disagrees with the tag is a warning rather than a failure: it means the
repository disagrees with itself and wants a bump, not that the release is wrong.

**Provenance is on, and whether it attests here is unproven.** npm signs the
package against an OIDC token minted for the workflow run, which is why the job
grants `id-token: write`. Self-hosted runners request that token from the same
Actions service a hosted runner does, so it should work, and nothing in this
repository has yet proven it does. The honest part is the failure mode:
`--provenance` fails the publish outright when the token is unavailable, so a
release can never ship silently unattested. The `provenance` dispatch input turns
it off for a release that has to go out while that is being sorted, and logs a
warning when it does.

The token never lands on disk. The `.npmrc` the publish uses holds the literal
string `${NPM_TOKEN}`, which npm expands out of the environment, and it is written
under `RUNNER_TEMP` so a persistent runner is not left holding a registry
credential. Docker is logged out for the same reason.

## Roadmap

- **Conformance suite** once harness mappers exist. That is the job that proves
  the normalization claim, so it belongs in CI from the day the first mapper
  lands.
- **Fedora and Rocky image variants**, published by the same release workflow
  with each package pinned and a manifest of its own alongside the image.
- **The Rust SDK to crates.io and the Python SDK to PyPi**, on the same tag as
  everything else. Until then a release ships two of the four outputs, and the
  ship table in the README says which.
- **`buf breaking` against the last release tag** in addition to the base branch,
  so a sequence of individually non-breaking PRs cannot add up to a break across
  a release.
