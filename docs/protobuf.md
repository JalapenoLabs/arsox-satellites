# Protobuf

The wire contract for every Arsox surface. The satellite serves it, the three
SDKs consume it, and the CLI reaches it through the Rust SDK. Nothing in Arsox
defines a message anywhere else.

## Layout

One root `proto/` directory. One package per bounded domain, and within a
package, one file per group of messages that are read together. Directories match
package names, which is what `buf lint` checks and what keeps a file's location
predictable from its import path.

Files are kept small on purpose. A package is the namespace and the unit of
generated Rust output; a file is a unit of reading. Nothing about the wire format
depends on which file a message lives in, so file boundaries are free to follow
what a person needs on screen at once.

```
proto/
  buf.yaml                            module, lint, and breaking config
  buf.gen.yaml                        codegen targets for all three languages
  arsox/
    common/v1/     common              Timestamp, Duration, Money, ceilings, paging
    error/v1/      error               ErrorCode, Error
    incident/v1/   incident            Disposition, Incident, incident queries
    usage/v1/      usage               TokenUsage, CostEstimate, lifetime statistics
    harness/v1/    harness             Harness, HarnessCapabilities
    settings/v1/   settings            ThreadSettings, the index into the rest
                   budget              Budget
                   model               ModelEndpoint, LlmAuth, RetryPolicy
                   repo                GitAuth, AgentsRepo, Repo
                   service             Service, ReadinessProbe, ServiceIsolation
                   team                TeamMode
                   stages              PlanMode, HumanInTheLoop, SelfReview, Suggestions
                   pull_request        PullRequestPolicy, WatchPullRequests
                   integration         GithubIntegration, JiraIntegration
                   prefetch            Prefetch, PrefetchInjection
                   secret              EnvVar, Redaction, RedactionMode, StarCount
                   permission          Permissions, WebAccess, ExecAccess
                   tool                McpServer, VirtualBrowser, Viewport
                   limits              StreamSettings, ResourceLimits, Timeouts
    thread/v1/     thread              Thread, ThreadState, thread endpoints
    turn/v1/       turn                TurnStatus, Turn, report vocabulary, endpoints
                   result              Stage, StageOutcome, TurnResult
                   brief               TurnBrief
    interaction/v1/ question           Question, QuestionSet, QuestionAnswer
                   plan                Plan, PlanDecision
    suggestion/v1/ suggestion          Suggestion, SetupScriptSuggestion
    artifact/v1/   artifact            Artifact, WorkspaceFile
    event/v1/      author              AuthorKind, Author
                   agent               agent messages and tool calls
                   team                spawn, despawn, chat, direct messages
                   integration         integration requests and outcomes, checkers
                   service             service start and logs
                   lifecycle           budget, plan, question, artifact, turn, stats
                   event               ThreadEvent envelope
                   control             ControlEvent and satellite lifecycle
    satellite/v1/  satellite           version, readiness, status
```

Dependencies point one way. `common` imports nothing. `suggestion` imports
nothing, so a suggestion never drags a turn behind it. `event` sits at the bottom
and imports almost everything, because an event stream is by definition a view
onto every other domain.

Within a package the same rule applies, and it is a hard constraint rather than a
preference: protobuf forbids circular imports between files even when they share
a package. This is why `Author` has its own file rather than living in
`event.proto` beside the envelope that references it, and why the turn
vocabulary sits in `turn.proto` while `TurnResult` sits in `result.proto`.

**Split before publishing, not after.** `buf breaking` runs at the `FILE`
category, so moving a message between files once v1 is out is a breaking change.
Reorganizing is free now and costs a major version later.

## Conventions

- **proto3 only.** Packages are `arsox.<domain>.v1`. The trailing version lets a
  `v2` coexist rather than replace.
- **Field numbers are permanent.** A deleted field keeps its number and name
  reserved. `buf breaking` runs with the `FILE` category, the strictest one, so
  the contract only ever grows within a major version.
- **Enums carry a zero `*_UNSPECIFIED`** that means "not set" and never a real
  case, because proto3 cannot tell an unset enum from its zero value.
- **`ErrorCode` values are numbered in blocks of 100 per domain**, so a family
  can be matched on a range and a new code never has to squeeze between two
  existing ones.
- **No `service` blocks.** Arsox serves protobuf over ordinary HTTP rather than
  gRPC, so endpoints are Request and Response message pairs and the Rust
  toolchain is prost without tonic.

### Absent is not zero

Multi-harness normalization guarantees that some harnesses report fields others
do not. Any field a harness might not report is `optional`, and its doc comment
says what absence means. Fields every harness always reports stay bare.

### Booleans encode their documented default

A flag whose documented default is off is a bare `bool`, because proto3's zero
value already means off. A flag whose documented default is on is an
`optional bool`, because a bare one silently flips the default for any client
that leaves it unset. Absent means "use the documented default", never "false".

This is why `allow_git_push` and `allow_redaction_override` are `optional bool`
while `enabled` on every experimental feature is not.

### Every time span is a Duration

No `*_minutes` or `*_seconds` integers anywhere in the contract. A bare integer
of unstated units is what produces a poll interval a thousand times too long.
The SDKs are free to accept ergonomic sugar and convert at the boundary.

### Ceilings are explicit, never sentinels

`TokenCeiling`, `CostCeiling`, and `DurationCeiling` are each a `oneof` between a
value and an empty `Unlimited` message. Budgets are required at thread creation
precisely so an unbounded spend is a decision rather than a field somebody
forgot, and a sentinel like `0` or `-1` would hand that decision straight back to
the default. `StarCount` uses the same pattern so that "mirror the secret's
length" is a case rather than a magic `-1`.

### One canonical message per concept

There is one `TokenUsage`. There is deliberately no `ClaudeTokenUsage` and no
`CodexTokenUsage`: a consumer written against a per-harness message has to be
rewritten to switch harnesses, which is the cost Arsox exists to remove.

### Money is never a float

Costs and cost ceilings are `common.v1.Money`: an ISO 4217 code, whole units,
and billionths. A single model request can cost a small fraction of a cent, and
accumulating those in a float is how a ceiling drifts away from the invoice it
was meant to predict.

## Codegen

Generation is deterministic and happens at build time. Nothing reads a `.proto`
at runtime, and the contract is checked by the compiler rather than discovered by
the interpreter.

```bash
cd proto
buf lint
buf format --diff --exit-code
buf generate
```

Output lands in `gen/`, one directory per language, and is committed. A
published Rust crate must carry pre-generated `.rs` rather than force downstream
consumers to install `protoc`, and the same reasoning keeps the TypeScript and
Python artifacts here. `.gitattributes` marks the whole tree generated so a
contract change reads as the `.proto` diff it actually is.

| Language | Plugin | Output |
|---|---|---|
| TypeScript | `bufbuild/es` | `gen/ts` |
| Rust | `community/neoeinstein-prost` plus `neoeinstein-prost-crate` | `gen/rust/src` |
| Python | `protocolbuffers/python` plus `pyi` | `gen/python` |

Plugin versions are pinned in `buf.gen.yaml` for the same reason every other
tool version is pinned: a floating plugin silently changes generated code
between a developer's machine and CI.

### Output granularity

The three languages do not agree on what a generated file is, and that is fine.
Each gets one **entry point**, not one file.

| Language | Files emitted | Entry point |
|---|---|---|
| Rust | one per **package** | `gen/rust/src/mod.rs` |
| TypeScript | one per **proto file** | package `index.ts` barrel |
| Python | one per **proto file** | PEP 420 namespace packages |

`prost` merges every file in a package into a single `.rs`, so the 14 files of
`arsox.settings.v1` still generate one `arsox.settings.v1.rs` holding all 32
messages. Splitting a package into more files costs the Rust output nothing.
`prost` emits no module tree of its own, so `prost-crate` writes the `mod.rs`
wiring that turns those per-package files into a crate the Rust SDK can depend
on.

Collapsing TypeScript into one `proto.ts` is possible with a bundler and is the
wrong move. A single module means importing one message pulls the entire
contract into the consumer's bundle, which defeats tree-shaking on a web SDK.
Many small modules plus a barrel is what lets a browser application pay only for
the messages it touches.

Python generates one `_pb2.py` per proto file because the protobuf runtime keys
its descriptor pool on file paths; there is no supported way to merge them. No
`__init__.py` files are needed, because the tree imports as PEP 420 namespace
packages.

## Identifiers are strings

Thread, turn, member, incident, and artifact IDs are `string` rather than a
`Uuid` wrapper message. A wrapper would make every required ID an `Option` in
prost, so every access becomes an unwrap for no safety gained: a UUID string is
unambiguous in a way that a bare `int64 created` is not. Newtypes belong in the
SDK helper layer, where they cost nothing on the wire.

Thread IDs are UUIDv7, always generated by the satellite and never accepted from
a client. They become filesystem paths, so a client-supplied one is a path
traversal waiting to happen, and UUIDv7 sorts by creation time, which makes
listing threads naturally chronological.

## Forward compatibility on the stream

`ThreadEvent` and `ControlEvent` each carry a `string type` alongside the `oneof
payload`. The duplication is deliberate: event types are additive within a proto
major, so a newer satellite sends arms an older SDK has never heard of, and an
unknown oneof arm decodes to nothing at all. Carrying the name separately is what
lets that SDK name, log, and forward the event anyway, which is what makes a
wildcard subscription a real forward compatibility hook rather than a hole.

`ControlEvent` is a separate message rather than `ThreadEvent` with a narrower
payload set. The control socket carries satellite lifecycle only and never thread
content, and making that a structural property beats making it a promise
somebody has to keep.

## Roadmap

- **Conformance tests.** A captured native transcript per harness and the
  canonical output it must produce. Adding a harness means writing a mapper and
  passing the existing suite. This is the normalization claim expressed as
  tests, and it is what makes "swap the harness, keep your code" a guarantee
  instead of an intention.
- **`buf breaking` in CI**, against the previous release rather than only
  against `main`, so the promise that the contract only grows is enforced rather
  than asserted.
- **Field validation.** Required-in-practice fields such as `Budget` and
  `idle_ttl` are enforced in satellite code today. Moving the constraints into
  the contract with `protovalidate` would let every SDK reject a bad thread
  before it leaves the client.
- **Helper wrappers** under `proto/helpers/<lang>/` for `Timestamp`, `Duration`,
  and `Money`, so call sites work in `chrono`, `moment`, and `datetime` rather
  than in epoch seconds and billionths.
