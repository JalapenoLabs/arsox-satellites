# Project Arsox

Run a fleet of docker based, self-hosted Claude/Codex satellite workers that can ephemerally work through a job queue of given tasks and be managed through a controlled SDK channel.

**One typed event stream, whichever harness runs underneath.** Claude CLI and Codex CLI emit different events with different shapes. Arsox normalizes both into a single protobuf contract, so your application is written once and never rewritten when you switch harnesses, models, or providers. That normalization is the point of the project. Everything else exists to make it usable.

Using polymorphism we support Claude, Codex, and other LLMs as the workers, and they can work through managed tasks from a controlling application.

We provide the SDK and the satellites, your application manages what they do. Your application sends job requests to work on tasks, settings and LLM keys. The satellite handles the job, streams the results back in realtime through a standardized message shape, and provides a robust API and SDK to make it easy to get and manage the statuses.

Satellites come with a job queue, and support threading multiple contexts of conversations together to resume conversation threads. It is like using Claude with a terminal as a human, but fully as an SDK and supporting fully remote architecture.

## How you use it

These are hosted for free on docker.io. The images are:

> `jalapenolabs/arsox-satellite:ubuntu-1.0.0`

```Dockerfile
FROM jalapenolabs/arsox-satellite:ubuntu-1.0.0
```

There are also variants for:
- `jalapenolabs/arsox-satellite:fedora-1.0.0`
- `jalapenolabs/arsox-satellite:rocky-1.0.0`

Every image is published with an exact version tag. Floating tags (`ubuntu-latest`, `latest`) exist for convenience but are not recommended for anything you care about: pin the exact version so local, CI, and production never drift.

<!-- TODO: Show a docker compose example -->

Optionally, you can build them yourself too.

Importantly, you must configure a security key via ENV.

```conf
ARSOX_SECRET=<a long random string>
```

This authenticates your host application against the satellite's API, so the API can be exposed onto the internet but calls will not be responded to without it.

**The satellite fails to start if `ARSOX_SECRET` is unset.** There is no unauthenticated mode by accident. If you genuinely want an open satellite on a trusted private network, you must say so explicitly with `ARSOX_ALLOW_INSECURE=true`, which logs a loud warning on every boot. Failing closed is deliberate: an accidentally unauthenticated satellite is a remote shell with your credentials in it.

Serve the API over TLS whenever it is reachable outside a trusted network. The secret is a bearer token, so it is only as private as the transport carrying it.

<!-- TODO: Enter code details about how to configure it -->
<!-- TODO: Show SDK examples of how to use it -->

The SDK is available in three programming languages:
- Rust (via Cargo) <!-- TODO: Put link here when it's available -->
- Node/Web (via NPM) <!-- TODO: Put link here when it's available -->
- Python (via PyPi) <!-- TODO: Put link here when it's available -->

For setting up the satellite's workspace, you are typically expected to extend the image in your own Dockerfile and install your own tooling on top of it. For example, if you need Go, pull this image with `FROM` and use `RUN` to install it yourself.

## Architecture

The satellite spawns with a Rust API running on it.
The SDK already has each API route registered into it, so you just call the SDK at the given endpoint and it makes the requests for you.

The API speaks protobuf as its primary wire format. This enforces a strong request/response shape and gives you a universal shape bus. This is one of the major benefits of using satellites: whether you prefer the Codex CLI harness or the Claude CLI harness, you get the exact same strongly typed shape out of each.

There is a secondary JSON representation of the same contract for manual and browser use. See [Wire protocol](#wire-protocol).

The satellite can be queried for its current state or it can be commanded upon.
Realtime events arrive over a unidirectional WebSocket. See [Event streaming](#event-streaming).

The satellite handles:
- LLM polymorphism (converting Claude request/response shapes to be used by Codex, for example)
- Job queues
- Settings and config
- Workspace management, including saving or clearing workspaces between jobs
- Deterministic enforcement of permissions, budgets, and secret redaction

### Preinstalled tooling

The docker container ships a working development environment out of the gate so the agents are not spending their first ten minutes installing basics. Every package is pinned to an exact version in the image build, and the exact set is published in the image's manifest.

Core:
- `git`, `git-lfs`, `openssh-client`, `ca-certificates`
- `build-essential` (gcc, g++, make), `pkg-config`
- `curl`, `wget`, `jq`, `zip`, `unzip`
- `ripgrep`, `fd`, `less`

Languages and package managers:
- Node with `corepack` (yarn and pnpm available through corepack)
- Python 3 with `pip` and `venv`

CLIs:
- `gh` (GitHub)
- `jira` (Atlassian)

<!-- TODO: Finalize and publish the exact pinned version manifest per image variant -->

Anything else is yours to add with a `RUN` layer in your own Dockerfile.

### The workspace

On the satellite's volume, the workspace is mounted at:

> `/workspace`

Mount it as a named docker volume. Nothing under `/workspace` survives a container replacement otherwise, which silently breaks thread resumption.

Inside, it looks like this:

```
/workspace
  |-- <thread-id>/
    AGENTS.md                        the real instruction file
    CLAUDE.md                        pointer to AGENTS.md
    CODEX.md                         pointer to AGENTS.md
    |-- .claude/
    |-- .codex/
    |-- repos/                       integration checkouts, owned by the commander
      |-- repo-name-1/
      |-- repo-name-2/
    |-- members/                     one directory per team member (team mode only)
      |-- <member-id>/
        |-- repos/
          |-- repo-name-1/           git worktree of ../../../repos/repo-name-1
          |-- repo-name-2/
        |-- .claude/
        |-- .codex/
    |-- artifacts/
  |-- <thread-id-2>/
    ...
```

Thread IDs are **always generated by the satellite** as UUIDv7 and never accepted from the client. They become filesystem paths, so a client-supplied ID is a path traversal waiting to happen. UUIDv7 also sorts by creation time, which makes listing threads naturally chronological.

The agent files (`CLAUDE.md`, `CODEX.md`) are pointers to `AGENTS.md`. They reference it with `@/workspace/<thread-id>/AGENTS.md`, which is how global custom instructions reach the runner regardless of harness.

When team mode is off, there is no `members/` directory. The single agent works directly in `repos/`.

Repos are not required to perform any work.

### Parallel checkouts

Team mode runs several agents at once. If they shared one checkout they would overwrite each other's files and fight over `.git/index.lock`. They do not share one checkout.

Each repo is cloned exactly once into `repos/<repo-name>`. Every team member gets a **git worktree** of that clone:

```bash
git -C /workspace/<thread-id>/repos/api \
  worktree add /workspace/<thread-id>/members/<member-id>/repos/api \
  -b arsox/<thread-id>/<member-id>
```

A worktree shares the object store with the primary clone but has its own working directory, its own index, and its own `HEAD`. Disk cost is one working copy per member, not one full clone. Git refuses to check out the same branch in two worktrees, which is exactly the guarantee we want.

Branch layout:

| Branch | Owner | Purpose |
|---|---|---|
| the repo's configured base branch | nobody | untouched reference |
| `arsox/<thread-id>` | commander | the integration branch, the only branch ever pushed |
| `arsox/<thread-id>/<member-id>` | one team member | that member's private work |

#### Integration protocol

Members never merge into the integration branch themselves. They request it, and Arsox serializes the requests so exactly one merge runs at a time.

1. A member commits to its own branch in its own worktree.
2. The member calls the `request_integration` MCP tool with the repo and a summary of the change.
3. Arsox queues the request and, when its turn comes, runs `git merge --no-ff` of the member branch into `arsox/<thread-id>` inside the commander's checkout.
4. **Clean merge**: the member is notified, and every other member receives an `integration_landed` event telling it to merge the integration branch back into its own worktree to pick up the new work.
5. **Conflict**: the merge is aborted, and the conflicting hunks are returned to the requesting member. That member merges the integration branch into its own branch, resolves the conflict in its own worktree, and requests integration again. The integration branch is never left in a conflicted state.

Because merges are serialized and conflicts are pushed back to the member that caused them, the integration branch is always buildable and the commander never has to arbitrate a three-way conflict it did not create.

Member branches are local only and are deleted at teardown. Only `arsox/<thread-id>` is ever pushed, and only if push permissions allow it.

### Wire protocol

The SDK handles all of this for you. **Protobuf is always on the wire, in both directions, and the SDK converts it into ordinary native objects.** You never touch a protobuf type: in the Node SDK you get plain TypeScript objects, in Python you get dataclasses, in Rust you get structs. JSON is never the wire format for the SDK, but what you hold in your hands is an ordinary object in your language.

For text and markdown reports, the SDK exposes dedicated methods.

The rest of this section applies only if you are making HTTP requests by hand, from Postman or curl.

**Authentication.** Every request carries:

```
Authorization: Bearer <ARSOX_SECRET>
```

The satellite compares in constant time, so the secret cannot be recovered by timing the comparison.

**Request bodies** declare their own encoding with `Content-Type`:

| Value | Meaning |
|---|---|
| `application/protobuf` | default, and what the SDK always sends |
| `application/json` | the same contract rendered as JSON |

**Responses** are negotiated with `Accept`:

| Value | Meaning |
|---|---|
| `application/protobuf` | default, and what the SDK always requests |
| `application/json` | the same contract rendered as JSON |
| `text/plain` | UTF-8 human-readable report |
| `text/markdown` | markdown report |

`Content-Type` describes the body you are sending. `Accept` describes what you want back. They are independent, so a JSON request may ask for a markdown response.

Text and markdown responses are generated reports meant for human eyes. They are not a parseable API and their layout may change between minor versions.

### Event streaming

Realtime events travel over **WebSocket**

There are two kinds of socket:

**Thread socket**, one per active thread:
```
wss://<satellite>/v1/threads/<thread-id>/stream
```
Carries everything that happens inside that thread: agent messages, tool calls, team chat, checker output, integration events, artifacts.

**Control socket**, one per satellite, optional:
```
wss://<satellite>/v1/stream
```
Carries satellite-level lifecycle only: thread created, thread destroyed, queue depth, health transitions, budget warnings. It never carries thread content.

**Many consumers per socket are allowed.** A horizontally scaled host application can have several replicas subscribed to the same thread, and each receives every event. This matters: a single-subscriber design would force you to designate one special replica and fan out internally.

**Direction.** The socket is unidirectional, server to client. Nothing is ever sent up it. Every command, starting a turn, answering questions, approving a plan, cancelling, is an ordinary HTTP request. The socket only tells you what happened.

**The SDK is protobuf only.** Frames are binary protobuf, and no setting in any SDK changes that. JSON frames exist here for the same reason JSON exists on the HTTP API: so you can point Postman or a browser console at a satellite and read what is happening without a protobuf decoder. Ask for them with the `arsox.json.v1` subprotocol at handshake. That is a debugging affordance for hand-driven clients, never a mode the SDK runs in.

**Resumption.** Every event carries a `sequence` number, monotonic per thread, and is persisted before it is sent. Reconnect with `?from_sequence=<n>` to replay everything you missed, so a network blip costs you nothing. Retained history is bounded by the thread's lifetime, so an expired thread cannot be replayed.

**Backpressure.** The satellite buffers a bounded number of undelivered events per consumer. A consumer that falls too far behind is closed with `STREAM_CONSUMER_LAGGED` rather than being silently starved or allowed to exhaust satellite memory. Reconnect with `from_sequence` and you lose nothing. Slow consumers degrade loudly, never quietly.

### Attaching to an existing thread

A thread lives entirely on the satellite. The SDK client holds no thread state, only a handle, which means **any process holding the satellite URL, the secret, and the thread ID can attach to a running thread**:

```typescript
const { thread } = await satellite.threads.attach(threadId)
```

There is no handoff, no lease, and no ownership. The process that created the thread has no privileged claim on it, and it may have exited hours ago. An attached handle can do everything a creating handle can: read the event stream from any sequence, read current state, queue turns, answer questions, approve plans, download artifacts, and destroy the thread.

This is the intended pattern for a horizontally scaled host application. Persist the thread ID in your own database when you create the thread, and any replica can pick the work back up. A replica that dies mid-turn costs you nothing: the satellite keeps working, and whichever replica attaches next replays from the last sequence it recorded.

Two rules still hold no matter how many clients attach:

- **One turn at a time per thread.** A turn submitted while another runs is queued, not run in parallel. See [Job queue](#job-queue).
- **One question set at a time, answered once.** If two replicas answer the same question set, the first write wins and the second is rejected with `QUESTION_SET_ALREADY_ANSWERED`. Answering is not idempotent, so coordinate on your side which replica speaks for the human. The same applies to plans, which reject with `PLAN_ALREADY_DECIDED`.

### State management

The API that orchestrates the satellite holds its state in an embedded SQLite database, accessed through `sqlx` in WAL mode. There is no external database to run.

Database files live in `/var/arsox/arsox.db`. **Mount `/var/arsox` as a named volume.** The database holds threads, queued turns, event history, and lifetime statistics, so losing it means losing every thread you intended to resume.

#### Lifetime statistics

Lifetime statistics track totals per LLM model and per thread: total, input, and output tokens, plus estimated cost.

In a human readable format via HTTP request:

```
Model: Opus 4.8 [1m]
1,234,567 total tokens
1,234,567 input tokens
1,234,567 output tokens

<Thread id 1>
1,234,567 total tokens
1,234,567 input tokens
1,234,567 output tokens
```

Statistics events are **not** emitted over the socket by default, because they change very rapidly. Opt in through stream settings if you want them. Otherwise fetch them with a GET request through the SDK.

### Job queue

Each thread processes **exactly one turn at a time**, forever. Turns within a thread are strictly FIFO. This is not tunable, because a thread is a conversation and conversations are sequential.

Threads run concurrently with each other, bounded by `ARSOX_MAX_CONCURRENT_THREADS` (default `4`). Raise it if the satellite has the CPU and memory, remembering that team mode multiplies the real process count.

- **Enqueue while busy.** The SDK may submit a turn while another is running. It joins the thread's queue and returns a turn ID immediately.
- **Cancel.** Any queued or running turn can be cancelled by ID. A running turn is asked to stop cooperatively first, then killed after a grace period. Work already committed to a member branch survives.
- **Depth cap.** Once a thread's queue reaches its cap, further submissions are rejected with `TURN_QUEUE_FULL` rather than accumulating without bound.
- **Persistence.** The queue lives in the embedded database and survives a satellite restart.

### Health and readiness

| Endpoint | Auth | Purpose |
|---|---|---|
| `GET /healthz` | none | liveness, always cheap, never touches the database |
| `GET /readyz` | none | readiness: database open, `/workspace` writable, LLM proxy reachable |
| `GET /v1/version` | none | satellite version and proto contract version |
| `GET /v1/status` | bearer | full satellite state, thread list, queue depths |
| `GET /metrics` | bearer | Prometheus metrics, opt in with `ARSOX_METRICS=true` |

The unauthenticated endpoints expose no thread content and no configuration, only the liveness facts an orchestrator needs before it holds a credential.

### Resource limits

An agent with a shell can fill a disk. These are enforced, not suggested.

- **Per-thread disk quota** for its workspace subtree, default 10 GiB. Exceeding it fails the turn with `DISK_QUOTA_EXCEEDED` rather than taking down the satellite. Configurable per thread.
- **Per-satellite memory and CPU** are the container's, so set them at the docker level with `--memory` and `--cpus`. Team mode runs several harness processes at once and each one is a real memory consumer.
- **Artifact size cap**, default 100 MiB per thread, to keep a runaway log out of your artifact download.

### Versioning and compatibility

Protobuf definitions live in a single root `proto/` directory, package `arsox.<domain>.v1`, generated with `buf`. Field numbers are permanent and deleted fields are reserved, so the contract only ever grows within a major version.

- The satellite's proto major version is reported by `GET /v1/version`.
- SDK major versions track proto major versions. An SDK 1.x talks to any satellite serving `arsox.*.v1`.
- An SDK **refuses** to talk to a satellite with a higher proto major and says so clearly, rather than failing later with a confusing decode error.
- An SDK talking to a satellite with a higher minor version warns once and proceeds. Additive fields it does not know about are ignored.

Pin your image tag and your SDK version together.

### Errors

Every error, on every transport, is the same shape: a stable enum code, a human message, a `retryable` flag, and structured details. Match on the code, never on the message.

**Codes are specific on purpose.** A caller should never have to read a message string, reproduce the failure, or open a support ticket to learn which of six things went wrong. If two failures need different handling, they get different codes. We would rather carry a long enum than make you debug ours.

Codes are named `DOMAIN_CONDITION` and grouped by prefix, so you can match a whole family with a prefix check and still narrow to the exact case when you care.

**Auth**

| Code | Retryable | Meaning |
|---|---|---|
| `AUTH_HEADER_MISSING` | no | no `Authorization` header was sent |
| `AUTH_SCHEME_UNSUPPORTED` | no | the header was present but was not `Bearer` |
| `AUTH_SECRET_INVALID` | no | the bearer token does not match `ARSOX_SECRET` |

**Request**

| Code | Retryable | Meaning |
|---|---|---|
| `REQUEST_BODY_MALFORMED` | no | the body did not decode as the declared content type |
| `REQUEST_CONTENT_TYPE_UNSUPPORTED` | no | unrecognized `Content-Type` |
| `REQUEST_ACCEPT_UNSUPPORTED` | no | unrecognized `Accept` |
| `REQUEST_FIELD_MISSING` | no | a required field was absent; `details.field` names it |
| `REQUEST_FIELD_INVALID` | no | a field failed validation; `details.field` and `details.reason` explain |
| `PROTO_VERSION_UNSUPPORTED` | no | the client speaks a proto major this satellite does not serve |

**Thread lifecycle**

| Code | Retryable | Meaning |
|---|---|---|
| `THREAD_NOT_FOUND` | no | unknown thread ID |
| `THREAD_EXPIRED` | no | the idle TTL elapsed and the workspace was collected |
| `THREAD_DESTROYED` | no | the thread was explicitly destroyed |
| `THREAD_LIMIT_REACHED` | yes | `ARSOX_MAX_CONCURRENT_THREADS` is saturated |

**Turns and the queue**

| Code | Retryable | Meaning |
|---|---|---|
| `TURN_NOT_FOUND` | no | unknown turn ID |
| `TURN_ALREADY_RUNNING` | yes | a turn is in flight; submit to the queue instead of running in parallel |
| `TURN_QUEUE_FULL` | yes | the thread's queue depth cap was reached |
| `TURN_CANCELLED` | no | the turn was cancelled before completing |
| `TURN_INTERRUPTED` | yes | the satellite restarted mid-turn; the thread survived |

**Human in the loop**

| Code | Retryable | Meaning |
|---|---|---|
| `QUESTION_SET_NOT_FOUND` | no | no question set is outstanding on this thread |
| `QUESTION_SET_ALREADY_ANSWERED` | no | another client answered first; read the stream for what it said |
| `QUESTION_ANSWER_INCOMPLETE` | no | the response did not cover every question in the set |
| `QUESTION_ANSWER_UNKNOWN_ID` | no | an answer referenced a question or option that is not in the set |
| `QUESTION_SET_TIMED_OUT` | no | the question timeout elapsed and the turn moved on |
| `PLAN_NOT_AWAITING_REVIEW` | no | no plan is currently up for approval |
| `PLAN_ALREADY_DECIDED` | no | another client approved or rejected this plan first |

**Budgets and resources**

| Code | Retryable | Meaning |
|---|---|---|
| `BUDGET_TOKENS_EXHAUSTED` | no | `maxTokensPerTurn` was reached |
| `BUDGET_COST_EXHAUSTED` | no | `maxCostPerThread` was reached |
| `BUDGET_WALL_CLOCK_EXHAUSTED` | no | `maxWallClockPerTurn` elapsed |
| `DISK_QUOTA_EXCEEDED` | no | the thread exceeded its workspace quota |
| `ARTIFACT_TOO_LARGE` | no | an artifact exceeded the per-thread artifact cap |

**Permissions**

Each control gets its own code, because "denied" without saying which gate closed is exactly the vagueness this list exists to avoid.

| Code | Retryable | Meaning |
|---|---|---|
| `PERMISSION_COMMAND_DENIED` | no | the command is not on the exec allowlist; `details.argv` shows it |
| `PERMISSION_DOMAIN_DENIED` | no | the egress proxy refused the host; `details.host` shows it |
| `PERMISSION_PUSH_DENIED` | no | pushing is disabled for this thread |
| `PERMISSION_BRANCH_PROTECTED` | no | the target ref is blacklisted; `details.ref` shows it |
| `PERMISSION_MERGE_DENIED` | no | PR merging is disabled, or the method is not permitted |
| `PERMISSION_PATH_DENIED` | no | a write landed outside the agent's own directory |
| `REDACTION_OVERRIDE_DISABLED` | no | `allowRedactionOverride` is false, so the tool is not registered |
| `SECRET_IN_PUSH_BLOCKED` | no | the `pre-push` hook found an unredacted secret in the outgoing diff |

**Repos and integration**

| Code | Retryable | Meaning |
|---|---|---|
| `REPO_AUTH_FAILED` | no | the SSH key or PAT was rejected by the remote |
| `REPO_CLONE_FAILED` | yes | the clone did not complete |
| `REPO_SETUP_FAILED` | no | a setup command exited nonzero |
| `WORKTREE_CREATE_FAILED` | yes | a member's worktree could not be created |
| `INTEGRATION_CONFLICT` | yes | a member's branch conflicts with the integration branch |
| `CHECKER_FAILED` | no | checkers failed and the commander declined to skip them |

**LLM endpoints**

| Code | Retryable | Meaning |
|---|---|---|
| `LLM_ENDPOINT_UNAUTHORIZED` | no | credentials for that endpoint were rejected |
| `LLM_ENDPOINT_RATE_LIMITED` | yes | 429 or 529 past the endpoint's retry policy |
| `LLM_ENDPOINT_TIMEOUT` | yes | a single request exceeded the LLM request timeout |
| `LLM_MODEL_UNKNOWN` | no | the endpoint does not serve the requested model |
| `LLM_CONTEXT_EXCEEDED` | no | the conversation no longer fits the model's context window |
| `LLM_ALL_ENDPOINTS_EXHAUSTED` | yes | every configured endpoint failed; `details.attempts` lists why each did |

**Harness**

| Code | Retryable | Meaning |
|---|---|---|
| `HARNESS_LAUNCH_FAILED` | yes | the Claude or Codex CLI could not be started |
| `HARNESS_CRASHED` | yes | the CLI process died and the single restart did not recover it |
| `HARNESS_IDLE_TIMEOUT` | yes | the harness produced no output past the idle bound |

**Streaming**

| Code | Retryable | Meaning |
|---|---|---|
| `STREAM_CONSUMER_LAGGED` | yes | this consumer fell too far behind; reconnect with `from_sequence` |
| `STREAM_SEQUENCE_EXPIRED` | no | the requested `from_sequence` is older than retained history |
| `STREAM_SUBPROTOCOL_UNSUPPORTED` | no | the requested WebSocket subprotocol is not offered |

**Internal**

| Code | Retryable | Meaning |
|---|---|---|
| `INTERNAL` | yes | a satellite bug, please report it with the message and `details.trace_id` |

New codes are additive within a proto major version, so an older SDK will meet codes it has never heard of. Handle the unknown case by falling back to the `retryable` flag, which is always populated, and log the raw code so the specificity is not lost on its way to you.

### Timeouts

Every long-running operation has a bound, and every bound is configurable per thread.

| Operation | Default | On expiry |
|---|---|---|
| single exec command | 30 minutes | the command is killed, output returns to the agent as a failure |
| single LLM request | 10 minutes | counts as an endpoint failure and triggers the retry or failover policy |
| turn wall clock | unset | see [Budgets and cost ceilings](#budgets-and-cost-ceilings) |
| harness idle (no output at all) | 15 minutes | the harness is considered hung and restarted once |

### Failure recovery

**The harness crashes.** Arsox captures the exit code and the last output, then restarts it once with the same context. If it dies again, the turn fails with `HARNESS_CRASHED` and everything already committed survives. In team mode only the crashed member restarts, and the commander is told what happened so it can reassign.

**Every LLM endpoint fails.** The turn ends with `LLM_ALL_ENDPOINTS_EXHAUSTED`, and `details.attempts` records why each endpoint was given up on. The thread and its workspace are preserved, so once you add working credentials a new turn resumes from where the last one stopped.

**The satellite restarts.** Threads and queues restore from the database, and workspaces restore from the volume. A turn that was in flight is marked `INTERRUPTED`. By default the thread waits for you to decide, and you can configure it to resume automatically instead.

**A checker fails.** See [Repo settings](#repo-settings). Checker failure is a normal outcome routed back to the commander, not a crash.

## Settings

There are many settings available. These are passed from the SDK and let you take full control of the satellite.

### Repo settings

If you have a git repo you want to work in, this is how.

First, a repo endpoint:

> `https://github.com/JalapenoLabs/arsox-satellites.git`
> `git@github.com:JalapenoLabs/arsox-satellites.git`

The satellite clones it with submodules:

```bash
git clone --recurse-submodules
```

This works if your repository and submodules are all public. If they are private, provide authentication: either an SSH private/public key pair or a PAT.

<!-- TODO: Revisit if more auth forms are supported -->

You can also provide repo setup commands (`yarn install`, `pip3 install -r requirements.txt`, and so on) as a multi-line string, using semicolons or newlines to separate them.

Multiple repos can be provided.

#### Checkers

Repos can be configured with a checker command, also a multi-line string.

- Commands separated by **newlines** run in parallel and do **not** fail fast.
- Commands separated by **semicolons** fail fast and block everything below them.

The checker runs after the agents decide their work is complete. Use it for verification such as `yarn lint` or `yarn typecheck`. If a command exits nonzero, the agents wake back up to fix it or to justify ignoring it before exiting fully.

This adds real verification to your LLM's work before it returns to your SDK as completed. If a checker is provided, the SDK includes its results in the report.

An example checker:

```
yarn install;
yarn lint
yarn typecheck
yarn generate && yarn build
; yarn deploy --dry-run
```

Here `yarn install` runs first and nothing else starts until it finishes, because the semicolon makes it a barrier. Then lint, typecheck, and generate/build run in parallel. If typecheck fails it does not block the others. `yarn deploy` never fires until everything above it has succeeded, so a failed typecheck means deploy never runs, intentionally. All of these logs are captured and sent to the LLM automatically.

### Github

Pass a PAT into the settings to let the agents use GitHub. This is highly recommended and enables any needed `gh` command.

When creating the PAT, we recommend allowing:
<!-- TODO: Populate for PRs, reading GH actions, reading vars/secrets -->

Possible settings:
- Allow or disallow merging PRs (default: allowed)
<!-- TODO: Populate -->

GitHub is supported first-class today. The integration is built polymorphically so GitLab and Bitbucket can follow.

### Jira

Pass a Jira PAT to let the agent look up items.

Possible settings:
- Allow or disallow moving ticket statuses (default: allowed)
- Allow or disallow commenting on tickets (default: allowed)
<!-- TODO: Populate -->

### Custom remote ENV

Pass an array of custom environment variables for the agents to use:

```json
[
  { "key": "", "value": "", "isSecret": true }
]
```

Only `key` and `value` are required. If `isSecret` is omitted it defaults to `true`, because defaulting to secret fails safe. Every secret is scanned for and hidden before anything leaves the satellite. See [Secret redaction](#secret-redaction).

### Ephemeral and cleanup

Every thread must declare a lifetime when it is created. This is required, always, as a safety net against forgotten workspaces filling a disk.

The lifetime is an **idle TTL measured in minutes**. It resets on every turn, and on any SDK interaction with the thread. A thread with a 60 minute TTL that is actively working for three days is never collected, and the same thread sitting untouched for 61 minutes is. This is what you want: the alternative, wall clock from creation, deletes long-running work mid-flight.

You can also mark a thread to delete itself the moment its turns complete.

When a thread is collected, its entire workspace subtree is removed, including member worktrees and any artifacts you did not download.

### Budgets and cost ceilings

Team mode can run nine LLM contexts at once for days. Without a ceiling, a single prompt loop is a very expensive invoice. Budgets are therefore **required** at thread creation, with an explicit `unlimited` value if that is genuinely what you want. Making the ceiling a conscious decision rather than a default is the whole point.

| Setting | Scope | Default |
|---|---|---|
| `maxTokensPerTurn` | one turn, all agents combined | required |
| `maxCostPerThread` | the thread's entire life, in USD | required |
| `maxWallClockPerTurn` | one turn | optional, unset |

Enforcement is deterministic, not advisory. Every model request passes through the Arsox LLM proxy, so the proxy counts tokens and refuses further completions once the ceiling is reached. An agent cannot talk its way past it.

At 80% of any ceiling, a `budget_warning` event is emitted so your application can react before the wall.

When a ceiling is hit, the turn ends with the code for the ceiling that was actually hit: `BUDGET_TOKENS_EXHAUSTED`, `BUDGET_COST_EXHAUSTED`, or `BUDGET_WALL_CLOCK_EXHAUSTED`. This is a graceful stop, not a kill: the commander is told the budget is gone, work already committed to branches survives, and artifacts already produced remain downloadable.

### LLM to use

Pick the Claude CLI harness or the Codex CLI harness. Claude is the default.

The two are independent of the model, which is the interesting part. You can run an Anthropic model such as `Opus 5 [1m]` under the Codex CLI.

As we expand we plan to support more LLMs, including but not limited to:
- Deepseek
- Bedrock
<!-- TODO: Add others here! -->

We use a LiteLLM sidecar to transform request and response shapes into whatever each service requires. It converts an inbound Deepseek conversation request into a Codex-compatible response, and so on.

LiteLLM as a whole is a huge ecosystem that ships a database, an API, and much more. We use only its lightweight transformation layer, which is the part we actually want, and which gives us a very large model catalog for very little surface area. Anything LiteLLM supports, we can support. The sidecar is pinned to an exact version like everything else.

We support Anthropic auth tokens (subscription access/refresh tokens, `claude setup-token` tokens, and API keys) and OpenAI auth tokens (subscription access/refresh tokens and API keys).

We also support custom LLM endpoints, for example self-hosted Azure Anthropic models.

#### Endpoint failover

You can pass multiple LLM endpoints per job. If one errors, because usage is exhausted or the provider is returning 529s, the satellite moves to the next. Order matters and is followed strictly. This lets you stack subscriptions, stack API keys, or list the same endpoint twice with different models.

Each endpoint carries its own retry policy. By default it retries up to 10 times with increasing backoff, starting at 5 seconds and capping at 60 seconds, on 429 and 529. You can change the count, the backoff, and the exact set of status codes that trigger a retry.

Failover is correct but it is not free, and you should order your endpoints knowing why:

- The conversation history is re-serialized into the new provider's shape, and tool-call IDs are rewritten because providers format them differently.
- The cached prompt prefix at the old provider is gone. The first request to the new endpoint pays full price for the entire history, which on a long thread is a real cost spike rather than a rounding error.

It's recommended to put your cheapest and most reliable endpoint first, and treat later entries as genuine fallbacks rather than a load-balancing pool.

### Permissions

Permissions are **deterministic**. A permission that is merely written into a prompt is a suggestion, and an agent under pressure will route around a suggestion. Every control below is enforced by infrastructure the agent cannot reach: agents run as an unprivileged `arsox` user, and the enforcement points are owned by root.

| Control | Default | How it is enforced |
|---|---|---|
| Network egress | preset allowlist | The container has no default route. All traffic goes through the Arsox egress proxy, which enforces the domain list. Options: all, none, preset, custom list. |
| Exec allowlist | preset allowlist | Harness shell calls are brokered by Arsox. Commands outside the list are rejected by exact argv match, and their binaries are not on the agent's `PATH`. |
| Push at all | allowed | A root-owned `pre-push` hook, installed through `core.hooksPath` outside every worktree, plus a git credential helper that refuses to release credentials for a denied push. |
| Protected branches | none | Same hook and credential helper. Blacklist `main` and no refspec, config edit, or clever remote gets around it. |
| Secrets in pushed content | blocked | The same `pre-push` hook scans the outgoing diff. See [Secret redaction](#secret-redaction). |
| Redaction override | available | The `override_redaction` MCP tool. Setting `allowRedactionOverride: false` unregisters the tool entirely, so no agent in the thread can reach it. |
| PR merging | disallowed | `gh` is brokered by Arsox, which rejects merge calls that policy forbids. |
| Filesystem writes | member scope | Unix ownership. A team member can write its own directory and the shared artifacts directory, nothing else. |
| Token and cost ceilings | required | The Arsox LLM proxy, which every model request traverses. |

Instructions written into `AGENTS.md` are **advisory**, and always will be. They shape behavior, they do not constrain it. Never rely on an `AGENTS.md` line for anything that matters if it is violated.

<!-- TODO: Add more here once added! -->

### Prompt

Provide a global prompt, which is written into `/workspace/<thread-id>/AGENTS.md`. `CLAUDE.md` and `CODEX.md` are created automatically and point at that universal agents file, so it is always loaded regardless of harness. Arsox places its own header at the top of `AGENTS.md`, and your content follows below it.

### Secret redaction

Anything marked `isSecret` in [custom env](#custom-remote-env) is redacted everywhere it could escape the satellite, not just in the log stream. The stream is the obvious channel and the least dangerous one. A secret committed to a repo and pushed to GitHub is the leak that actually hurts.

Redaction applies to:
- stream events and their payloads
- text and markdown reports
- team chat and direct messages between members
- checker output and command logs
- **file contents in commits, and commit messages**
- **PR titles, bodies, and review comments, and Jira comments**
- artifact contents, at upload time

The `pre-push` hook is the hard gate for git. A push containing an unredacted secret is refused outright, so a leak requires a deliberate override rather than an oversight.

#### Redaction modes

For a secret of length `L`, `N` is the number of revealed characters:

| Mode | Rule | `sk_ant_12345` (L=12) |
|---|---|---|
| `Anonymous` | reveal nothing | `******` |
| `PrefixShown` | first `N`, `N = floor(min(8, 0.20 × L))` | `sk******` |
| `PostfixShown` | last `N`, same `N` | `******45` |
| `HybridShown` | first and last `N`, `N = max(1, floor(min(5, 0.10 × L)))` | `s******5` |

Two safety rules override the table:
- A secret shorter than 8 characters is always `Anonymous`. There is no safe prefix of a short secret.
- If the computed reveal would expose more than half the secret, it falls back to `Anonymous`.

`PostfixShown` is usually the most useful when you need to tell two credentials apart in a log, because the tail of a key is distinguishing while the head is often a shared vendor prefix.

#### Star count

The mask is six stars by default, independent of the secret's length. Configure it:

- any positive integer: exactly that many stars
- `0`: clamped to `1`, because a mask with no stars is invisible
- `-1`: mirror the redacted length, one star per hidden character

Be deliberate about `-1`. Mirroring the length leaks the length, which is real information about a credential. The fixed count is the default for that reason.

#### Overrides

Sometimes putting a secret in a repo is the actual task, for example writing a deploy manifest into a private repository. The default is to refuse, and the refusal can be overridden.

**Whether to override is the agent's decision, made in response to an explicit human instruction, not an SDK toggle.** An agent that has been told plainly to commit a specific value calls the `override_redaction` MCP tool naming the exact secret and stating its justification in writing. Every override:

- is scoped to that one secret and that one operation, never blanket and never persistent
- emits a high-priority stream event the moment it happens
- appears in the turn's report with the justification the agent gave

So the guardrail holds by default, an explicit human instruction can move it, and nothing moves quietly.

#### The kill switch

The SDK does not decide any individual override, but it does decide whether the capability exists at all.

Set `allowRedactionOverride: false` on a thread and **the `override_redaction` tool is never registered for that thread.** No agent in it, commander or member, can override redaction for any secret, for any reason, no matter what it is told.

The distinction matters. Removing the tool is strictly stronger than gating its behavior: there is no call to make, no refusal to argue with, and no instruction that can reach it. A gate can be talked around, because a persuasive prompt is exactly the thing a gate has to evaluate. An absent tool cannot be.

The default is `true`, which keeps the agent's judgment in play for the case the feature exists to serve. Set it to `false` when a thread runs prompts you do not fully control, or when the repos it can push to are ones where a single leaked credential is unacceptable. Under `false` the `pre-push` hook is an absolute gate rather than a strong default.

### Streaming settings

By default you receive every event over the socket. You can toggle off specific event types if you want less traffic.

It does not matter whether you are running the Claude CLI or the Codex CLI. If Claude emits a `tool call started` event you get the standardized shape, and if you switch to Codex, which emits the same event with a different shape, it is conformed to that same standardized shape. This is the point of the whole project.

## Team mode

Team mode is **opt in, default off**.

With it off, the satellite behaves like a standard Claude or Codex session: one agent, one context window, role `agent`. This is the right default. A first task should not silently spawn nine LLM contexts and the bill that comes with them.

With it on, a root **commander** owns the task through to completion. Its first act each turn is to decide which team members it needs. These are not the harness's built-in sub-agents, they are peer LLM team members with their own context windows and their own worktrees.

A commander might spawn:
- Architect
- Backend
- Frontend
- CI
- QA
- Unit test
- Doc writer

Team members communicate through a team chat and through direct messages. Arsox provides that infrastructure, so members always know their assignments and can reach each other.

Members can also spawn their own native sub-agents, which makes three tiers: commander, members, sub-agents.

### Why a team

**Maximum parallelism.** Backend, frontend, and unit tests are written at nearly the same time, each in its own worktree, coordinating as they go.

**Maximum context efficiency.** If each agent gets roughly a million tokens of context, having one agent do both frontend and backend wastes the budget. A frontend member dominates frontend files and a backend member dominates backend files, so you have two million tokens total and each lane is far more focused.

**Ownership.** When a member owns an area it can be held to a higher standard in that area. Collaboration produces micro-decisions that a single agent never surfaces: a team debates, works through disagreements, and generates options. A team beats the individual.

### What the commander observes

The commander is a manager, not a surveillance system, and this distinction is what keeps the context efficiency argument true.

The commander **does** see:
- the team chat in full
- direct messages between members
- integration requests and their outcomes
- checker results
- escalations a member deliberately raises

The commander **does not** see:
- individual tool calls
- file reads and writes
- a member's private reasoning
- sub-agent activity inside a member

If the commander ingested every file operation, all context would reconverge into one window and it would exhaust its budget faster than a single agent would have. It watches communication between the team, not the work itself. Members surface what matters by saying it out loud.

The SDK still captures everything, from every member: activity, tool calls, discussions. Full observability is yours. It is the commander's context that is kept narrow, not your visibility.

### Competitive framing

Team members are told they are competing with each other, and the commander is told it is competing with other commanders it cannot see.

> The team members play against each other, and the commander plays against the other commanders.

Be clear about what this is: **a motivational prompting technique with unproven effect, not a scheduling or quality mechanism.** Nothing in Arsox scores, ranks, or routes work based on it. It costs nothing but the words in the prompt, and it may sharpen output. It is framing, and we would rather say so than dress it up as engineering.

The competition is for the highest quality output: the most thoroughly tested, the most thought through, the best documented, the most production-ready. **Quality is encouraged over speed.** A task taking hours or even days is completely fine.

### Team lifecycle

The team spawns together and despawns together. The commander can also spawn or despawn members mid-run through an MCP tool Arsox provides, so it is not locked into the roster it started with.

By default a commander will not spawn more than 8 members, and you can raise or lower that cap. The commander chooses roles itself, guided by a suggestion list rather than restricted to a fixed set. You can add your own roles to that suggestion list.

At the end of a session the commander decides which files are artifacts, and it is typically the commander that responds back through the SDK.

## Human in the loop

The human is in the loop by default. Disable it and the team runs the entire task alone.

Agents can ask for approvals ("do you approve this command, this approach") and for clarification.

**Only the commander asks.** Team members escalate to the commander, and the commander decides whether the question is worth your attention. Without that funnel, eight members would each queue their own questions.

Clarification behaves like the Claude Code CLI. One question set at a time per thread, containing between 1 and 5 questions. There is never a second question set queued behind the first, and never ten questions waiting.

Each question has a title and up to 5 selectable options, each with its own title and sub-description.

**The SDK must answer the whole set at once.** If 5 questions are asked, the response carries all 5. Individual answers may differ in kind: questions 1 through 4 can be answered with a chosen option or with freeform text, and question 5 can be declined. What you cannot do is answer three now and two later. The response shape is all of them, together.

If a question set goes unanswered past the thread's configured question timeout, the turn ends with the questions recorded in the report rather than hanging forever.

All of this can be disabled, leaving the agents to use their own judgment. That is more dangerous, and it is the implementer's call.

## Built in skills

Satellites ship with built-in skills, provided per conversation thread at `/workspace/<thread-id>/.claude/skills` or `/workspace/<thread-id>/.codex/skills`.

## Plan mode

EXPERIMENTAL. Opt in, default off.

A dedicated agent with a clean context window drafts a plan for the request before any work begins, even in team mode. Claude and Codex both have native plan modes, and where the harness lacks one Arsox supplies a skill fallback. The plan output is normalized to the same shape either way.

The plan goes to the SDK for review. Once approved, work proceeds. The planning agent may also decide a plan is unnecessary and skip the step.

If human in the loop is off, or if you enable auto-approval, the plan is treated as approved and passes to the commander (or to the single agent when team mode is off).

## Automated self-review

EXPERIMENTAL. Opt in, default off.

Arsox provides a default skill for reviewing its own work. A dedicated agent with a clean context window scans all changes and reviews them.

Attached to a pull request, it leaves its review as PR comments. Not attached to one, whether the work is a plain commit or an artifact, it writes the full review to a file for the agents to act on. It can suggest changes to an artifact before upload, or follow-up commits to improve something that fell short.

With no file output, there is nothing to review and the step is skipped.

## Pull request merging

EXPERIMENTAL. Opt in, default off.

By default a human merges pull requests. Grant more autonomy and the commander may merge on its own once a task is complete and reviewed.

The commander still **chooses** whether to merge. You can also define which merge methods are permitted, such as squash versus rebase. The policy is enforced by the `gh` broker, not by asking nicely.

## Artifacts

When a job completes, its artifacts are ready for you. A generated text report that was never committed to a repo is the typical case.

Artifacts live until the thread expires. For an ephemeral thread, the SDK provides a method to pull artifacts down before teardown completes.

The agents decide what counts as an artifact. You can also use the SDK to list every file, artifact and non-artifact alike, and to download or upload any file to or from your host application.

Artifacts are moved up from the repo level into the thread's `artifacts/` directory, because repo directories are the most ephemeral part of the workspace and are frequently torn down or recreated, while artifacts should outlive them.

Artifact totals are capped per thread. See [Resource limits](#resource-limits).

## MCP

Satellites support MCP, so you can wire your own servers into the Claude and Codex sessions and let the agents use them for outbound work.

Arsox also provides its own MCP tools to the agents, including team spawn and despawn, `request_integration`, and `override_redaction`.

## Order of operations

Each turn runs through a fixed stack.

**A new turn on a new thread:**
1. Create the thread, defining repos, budgets, TTL, and settings (SDK call)
2. A turn starts (SDK call)
3. If plan mode is enabled, a plan agent works through a plan and awaits review. It may also decide no plan is needed and skip this step.
4. A commander is spawned, ingests the job, and spawns its team. Each member receives its own worktree.
5. The team works, integrating through the commander's queue as they go
6. The team despawns
7. Automated checkers run. Failures return to the commander to reassign. Checkers may be failing for reasons the agents deliberately accept, so the commander can skip them. A skip applies to this turn only and never carries into future turns.
8. Automated self-review runs, if enabled
9. Auto squash or merge runs, if enabled
10. The commander scans for artifacts
11. Artifacts upload to the SDK, if the SDK wants them returned automatically

**A new turn on an existing thread:**
1. A turn starts (SDK call)
2. A plan agent is created with fresh context and analyzes the turn plus history. It decides whether a plan is needed, and creates one if so.
3. The commander is spawned with its previous context intact and spawns its team. The commander chooses per member whether that member starts with a clean slate or with its previous context.
4. The team works, integrating as before
5. The team despawns
6. Automated checkers run, as above
7. Automated self-review runs, if enabled
8. Auto squash or merge runs, if enabled
9. The commander scans for artifacts
10. Artifacts upload to the SDK, if requested

The SDK can destroy threads directly, or let them expire through their idle TTL.

## License

Apache License 2.0. See [LICENSE](./LICENSE).

Apache-2.0 grants patent rights explicitly, which matters for a project published as libraries to Cargo, NPM, and PyPi: downstream users get a clear patent grant from every contributor rather than relying on an implied one.

Contributions are accepted under the same license, per section 5 of the license text. There is no separate CLA.
