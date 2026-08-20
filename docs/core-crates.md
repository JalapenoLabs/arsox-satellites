# The core crates

The normalization layer already exists and already works. It lives inside
`arsox-satellite`, which means an application that wants the normalization has
to take a job queue, an HTTP surface, a SQLite store and an LLM proxy to get it.

This document is the plan for pulling that layer out so it can be depended on
alone, written from the perspective of the first outside consumer.

## What is already true

Worth stating, because it decides how much work this is:

- `harness/claude.rs` is **already sans-IO**. `map_line(&str) -> Mapping` is a
  pure function over loose JSON, with no `tokio` and no filesystem, and ten
  tests against it.
- `arsox-sdk` already carries the generated contract types.
- The workspace already pins lints, edition and toolchain the way the crates
  below will want them.

So this is an extraction, not a construction. The knowledge is written; it is
only in a crate that costs too much to depend on.

## Why the split is worth doing anyway

An application that already schedules its own containers, supervises its own
processes, and owns its own storage still wants one thing from this project:
the answer to "what did this harness just do", in one shape, per harness.

Today taking that answer means compiling `axum`, `sqlx`, `reqwest` and a job
queue into a binary that needs none of them. For a consumer that runs the
harness inside a locked-down container, those are not just unused dependencies,
they are attack surface next to a model running with tool approval disabled.

## The crates

| Crate | Holds | Depends on |
|---|---|---|
| `arsox-sdk` | Generated contract types. Exists | `prost`, `prost-types` |
| `arsox-harness` | The mappers: native events in, canonical events out. **No I/O** | `arsox-sdk`, `serde_json` |
| `arsox-runner` | Process supervision: spawn, pipes, lifetime | `arsox-harness`, `tokio` |
| `arsox-satellite` | Job queue, HTTP surface, store, proxy, image | the above |

The boundary that matters is between `arsox-harness` and `arsox-runner`.
Everything above the line is knowledge, and everything below it is plumbing that
a consumer may already have.

### What moves where

- `harness/mod.rs` and `harness/claude.rs` move to `arsox-harness` unchanged.
  They already have the right shape.
- `harness/runner.rs` and `harness/spawn.rs` become `arsox-runner`, or stay in
  the satellite until a second consumer wants them. No need to decide now.
- Everything else stays.

## Conformance

`usage/v1/usage.proto` says it plainly:

> This is the file where the normalization claim is most easily broken and
> hardest to notice. Every harness reports usage differently, and a mapper that
> silently drops a field looks correct until somebody reconciles a bill against
> it. The conformance suite exists for exactly this contract.

Fixtures are recordings per harness **per pinned CLI version**, because an
output shape belongs to a version and to nothing else:

```
crates/arsox-harness/fixtures/
  claude/2.1.221/<scenario>.stdout.jsonl   +   <scenario>.events.json
  codex/0.147.0/...
  kimi/0.34.0/...
```

A version bump then becomes: record new fixtures, and let the suite say exactly
what changed. That turns the worst failure mode, a shape that shifts silently on
upgrade, into a red test.

Two harnesses are covered today and every fixture is a recording. Each one still
states its provenance in `fixtures/README.md`, so a fixture ever built from a
published schema rather than captured has to say so out loud.

Material already measured against real runs rather than read from documentation:

- **Claude 2.1.226** reports usage per assistant message, with
  `cache_creation_input_tokens` and `cache_read_input_tokens` broken out.
- **Codex 0.147.0** stamps a `client_metadata` object carrying `session_id`,
  `thread_id`, `turn_id`, and a `turn_started_at_unix_ms` millisecond timestamp,
  and keeps session state in versioned SQLite (`state_5.sqlite`), not a rollout
  file. Anything reading that store is reading an undocumented schema whose name
  carries its version.
- **Codex's exec stream announces work but not prose.** A tool call arrives as
  an `item.started` and an `item.completed`; an agent message arrives only as an
  `item.completed`, and `item.updated` never appears at all. A turn also emits
  several agent messages, a preamble and then the answer, which is why the
  runner takes its summary from the last rather than the first.
- **The two vendors disagree about `input_tokens`.** Anthropic excludes what
  came from cache; OpenAI includes it, with the cached count broken out beneath.
  A mapper that copies the field across overstates a cached run by most of its
  prompt.

That last one is exactly the defect `usage.proto` warns about, observed in the
wild. `claude.rs` already handles it correctly and says so in a comment; the
point of the fixtures is that it keeps doing so.

## Publishing

**Git dependencies pinned to a tag, not GitHub Packages.** GitHub Packages has
no Cargo registry: npm, Maven, NuGet, RubyGems and containers, nothing else. A
tagged git dependency gives the two properties that actually mattered, ownership
under this org and an exact pin:

```toml
arsox-harness = { git = "https://github.com/JalapenoLabs/arsox-satellites", tag = "harness-v0.1.0" }
```

crates.io stays available later and needs no restructuring to adopt.

Generated code stays checked in and the crates build with plain `cargo build`.
Requiring a consumer to install `buf` and `protoc` would make them un-buildable
out of the box; `buf generate` stays a maintainer step with CI failing on a
diff.

## Known wart

`prost` types appear in the public API. Leaking a third-party type is normally
worth avoiding, and here it is worth accepting: interoperating with a protobuf
contract is the entire purpose, and hiding prost behind a facade would be a
translation layer whose only job is to undo the choice of protobuf.

## Order

`arsox-harness` exists, carries both mappers, and holds the versioned fixtures.
What is left:

1. **A spawn path for Codex**, so a satellite can drive the harness its mapper
   already reads.
2. **`arsox-runner`**, if and when a second consumer wants the supervision.

A consumer can stop after any of them.
