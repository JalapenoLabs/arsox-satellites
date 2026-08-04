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
| Proto | `.github/workflows/proto.yml` | pushes and PRs touching `proto/`, `gen/`, or the workflow itself |

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

### Network dependency

`buf generate` resolves remote plugins from `buf.build`, so the codegen check
needs egress to that host. If the runner pool loses it, the fix is vendoring the
plugins locally rather than dropping the check.

## Roadmap

- **Rust satellite**: `cargo build`, `cargo clippy -D warnings`, `cargo fmt
  --check`, and `cargo test`, with the toolchain pinned in `rust-toolchain.toml`.
- **SDK builds** for all three languages, each consuming `gen/` rather than
  regenerating it.
- **Conformance suite** once harness mappers exist. That is the job that proves
  the normalization claim, so it belongs in CI from the day the first mapper
  lands.
- **Image builds** for the Ubuntu, Fedora, and Rocky satellite variants, with
  every package pinned and the manifest published alongside the image.
- **`buf breaking` against the last release tag** in addition to the base branch,
  so a sequence of individually non-breaking PRs cannot add up to a break across
  a release.
