# Thread services

A host application sometimes needs a helper running beside the agent rather than
a command the agent runs: a headless Blender bridge and the Blender MCP server
that talks to it, a dev server a browser is pointed at. A thread declares those
helpers as **services**, and every turn on the thread gets its own set.

The code is `src/services.rs` for declarations, names, and ports, and
`src/services/running.rs` for starting, watching, and stopping them.

## Declaring a service

Services are declared on the thread, in `ThreadSettings.services`:

```rust
use arsox_sdk::proto::settings::v1::{
    McpServer, ReadinessProbe, Service, ServiceEndpoint, ThreadSettings,
};

let settings = ThreadSettings {
    services: vec![
        Service {
            name: "blender".to_owned(),
            command: "blender --background --python bridge.py -- --port \"$PORT\"".to_owned(),
            ready_when: Some(ReadinessProbe {
                http_get: Some("/health".to_owned()),
                timeout: None,
            }),
            ..Service::default()
        },
        Service {
            name: "blender-mcp".to_owned(),
            command: "blender-mcp --bridge \"$ARSOX_SERVICE_BLENDER_URL\" --port \"$PORT\"".to_owned(),
            ..Service::default()
        },
    ],
    mcp_servers: vec![McpServer {
        name: "blender".to_owned(),
        service: Some(ServiceEndpoint {
            service: "blender-mcp".to_owned(),
            path: "/mcp".to_owned(),
        }),
        ..McpServer::default()
    }],
    ..ThreadSettings::default()
};
```

A service declared on a repo, in `Repo.services`, is refused at thread creation
with `REQUEST_FIELD_INVALID`. Nothing reads that list, so accepting it would be a
declaration that silently does nothing.

Refused at thread creation with `REQUEST_FIELD_INVALID`, naming
`settings.services` and the service:

| Field | Rule | Why |
|---|---|---|
| the list | at most 16 services | each is started and waited on before every turn |
| `name` | 1 to 64 of `A-Z a-z 0-9 _ -`, unique once upper-cased with `-` written as `_` | it becomes variable names, and `web-app` and `web_app` would be one set of variables naming two services |
| `command` | not blank, at most 16 KiB, no NUL | it is a process argument |
| `port` | absent, or 1024 to 65535 and unique within the thread | the agent account cannot bind below 1024 |
| `ready_when.http_get` | starts with `/`, at most 2048 characters, no whitespace, control character, or `${`, and cannot move the host or port | it is appended to `http://127.0.0.1:<port>` |
| `isolation` | unset or `SHARED` | `PER_MEMBER` is not implemented; see [isolation](#isolation-and-its-limits) |

## Ports and environment

A service with no `port` is given a free loopback port for each turn. One that
declares a port gets exactly that port, and is not started when the port is
already taken.

**Every port a turn's services use is leased, satellite wide.** Asking the kernel
for a free port and then releasing it for the service to bind leaves a window,
and the likeliest thing to take the port in it is another turn's service
starting at the same moment. So the satellite holds every port it hands out, and
every port a service declared, until the turn's services have stopped, and no
other turn is handed one it holds. That is also what keeps two threads declaring
the same fixed port honest: the second is refused the port, recorded as
`SERVICE_START_FAILED`, rather than having its readiness probe answered by the
first thread's process. A process outside the satellite can still take an
assigned port in the window; the service then fails its probe and says so.

A port is leased for the whole turn even for a service that failed, so no other
turn is handed an address this turn's agent was already told about.

| Variable | Set on | Value |
|---|---|---|
| `PORT` | the service itself | its own port |
| `ARSOX_SERVICE_<NAME>_PORT` | every later service, and the harness | the port |
| `ARSOX_SERVICE_<NAME>_URL` | every later service, and the harness | `http://127.0.0.1:<port>` |

`<NAME>` is the declared name upper-cased with `-` written as `_`, so
`blender-mcp` is `ARSOX_SERVICE_BLENDER_MCP_URL`. The harness is told about
every service that was given a port, whether or not it became ready: an agent
told nothing would go looking for it, and an agent told the address meets a
refused connection, which says exactly what happened.

These are the one `ARSOX_` family an agent is given. Every other `ARSOX_*`
variable is scrubbed from every spawn and a thread may not declare one, so a
declared variable cannot forge a service address either. The satellite's own
variables are set after the thread's declared ones, so a declared `PORT` cannot
point a service away from the port it was leased.

## An MCP server a service runs

`McpServer.service` names a declared service and a path, instead of a `url`.
Exactly one of the two is set. The satellite renders it as
`http://127.0.0.1:<port><path>` with the port the service was given for the
turn, for both harnesses, and it is otherwise a declared server like any other:
it counts toward Claude's `--strict-mcp-config`, is allowed by name as
`mcp__<name>` under a narrow exec posture, and may carry headers. See
[the harness doc](./harness.md#mcp-servers-reach-the-harness-as-launch-arguments).

It opens no host on the egress allowlist. Loopback is in the `NO_PROXY` every
gated agent is handed, so the harness reaches the service directly.

## Lifecycle: one set per turn

Services are **turn scoped**:

1. The turn opens: its model grant, its egress admission, and its wall clock.
2. The thread's services start, in declaration order. Each is launched, then
   probed until it is ready or gives up, before the next one starts, because a
   later service may depend on an earlier one the way an MCP server depends on
   the editor it drives.
3. The harness runs, with every service's address, and so does every checker fix
   cycle and restart in the turn, all against the same processes on the same
   ports.
4. The turn ends, however it ends: completed, failed, cancelled, out of budget,
   or out of wall clock. Every service is stopped, and the turn's result is
   recorded only once they are gone.

Startup counts against the turn's `maxWallClockPerTurn`, like everything else a
turn waits for.

**What a service holds in memory does not survive into the next turn.** A service
is a new process every turn, on a port that may differ. Anything worth keeping
belongs in the workspace, which is where a service runs and which outlives every
turn. The payoff is that nothing keeps running while a thread idles for a week,
and a turn never shares a process with another thread's turn.

### How a service runs

Each service runs through `sh -c` as the agent account, handed down by the same
`harness::spawn::scrubbed_command` every spawn goes through, with the thread's
workspace root as its working directory. Its environment is the one a repo's
setup commands get, the scrubbed base plus the thread's declared `env`, then the
variables above.

Services are **not brokered** by the exec allowlist, and are not pointed at the
egress proxy. They are host configuration executed as configured, the same
standing a repo's `setup_commands` have, rather than anything an agent chose. See
[the enforcement doc](./enforcement.md#setup-commands-checkers-and-services-are-not-brokered).

### Readiness

`ready_when.http_get` is polled on the service's port until it answers 2xx.
Without it, a TCP connection to the port being accepted is enough. Each attempt
is bounded at two seconds and they repeat every 200 milliseconds, bypassing any
proxy the satellite's own environment names.

The probe gives up after `ready_when.timeout`, or 60 seconds when that is absent,
zero, or negative, which is `timeouts::DEFAULT_SERVICE_READY`. It also gives up
the moment the service exits.

### Stopping

Each service is the leader of its own process group, so stopping it reaches
everything it started, not just the shell. The satellite sends the group
`SIGTERM`, waits up to five seconds for the service to exit, sends the group
`SIGKILL` for whatever is left, and reads the service's pipes to their end so the
last lines it wrote land on the stream before the turn's result. All of a turn's
services are asked at once, so ending a turn costs one grace period rather than
one per service.

A turn future dropped before it could stop its services, by a satellite shutting
down or by a panic, kills every group it still holds as it drops. That backstop
is `SIGKILL` alone, since dropping cannot wait out a grace period. A satellite
killed outright stops nothing, but inside the image it takes its container, and
every process in it, with it.

### Restarts

A service that exits after it became ready is started again on the same port,
after a backoff of half a second that doubles each time, at most three times in
one turn. A restart that brings it back is a `recovered` `SERVICE_EXITED`
incident and a second `service.started` event, because a service crashing every
few minutes and being quietly revived is a pattern nobody sees otherwise. Past
the bound it stays down for the rest of the turn, and a `degraded`
`SERVICE_EXITED` says so. The next turn starts it fresh.

A service that exits before it ever became ready is not restarted. That is a
service failing to start, and starting it again in the same turn would reach the
same place.

## Failure is degraded, not fatal

A service that cannot be given its port, cannot be launched, exits before it is
ready, or never passes its probe is stopped and recorded as a `degraded`
`SERVICE_START_FAILED` incident carrying its last 40 lines of output, and the
next service starts anyway. The turn goes on.

Degraded rather than fatal, because the harness can still do most of what it was
asked without a helper, and failing the turn would throw away its work to report
one missing process. An MCP server pointing at the missing service simply fails
to connect, and both harnesses start without it. That was measured against
Claude 2.1.280 and Codex 0.156.1, the versions at hand rather than the pinned
ones, launched with a declared server on a port nothing listened on: Claude
reported the server as `failed` in its init line and went on to its model
requests, and Codex logged the refused connection on stderr and did the same.
The agent is told the address either way, and the incident is the record of why
nothing answered.

| Code | Disposition | When |
|---|---|---|
| `SERVICE_START_FAILED` | `degraded` | no port, no launch, an exit before ready, or a probe that never passed |
| `SERVICE_EXITED` | `recovered` | a ready service exited and a restart brought it back |
| `SERVICE_EXITED` | `degraded` | a ready service exited with the turn's restarts spent |

Each carries `details.service`, and `details.output_tail` when the service wrote
anything; `SERVICE_EXITED` also carries `details.status`, the exit as the
platform describes it. None is retryable: a service that will not start is a fact
about its command, and the same turn submitted again meets the same command.

## Events

| Event | When |
|---|---|
| `service.started` | a service passed its probe, again after each restart that brings it back |
| `service.log` | each line a service writes, on either pipe |

`ServiceStarted.url` is the address the harness is given as
`ARSOX_SERVICE_<NAME>_URL`. `repo` is empty, because a service is declared on the
thread; `auto_promoted` is false and `member_id` absent, because the exec broker
promotes nothing and every service is shared by its turn today.

`service.log` is emitted unless `StreamSettings.include_service_logs` is `false`,
absent meaning included. A line is cut at 4096 characters. Output is read as bytes
and decoded lossily, so a service printing something that is not UTF-8 never
stops the reader and never blocks on a full pipe. Both events, and every
incident, are masked by the thread's redactor like any other.

## Isolation, and its limits

**Per-turn processes separate state; they are not a security boundary.** Two
threads running at once never share a service process, a port, or anything a
service holds in memory, which is what keeps one thread's Blender scene out of
another's. What they do share is the agent account: every thread's agent runs as
the same unprivileged uid in the same network namespace, so an agent can dial
another concurrently running thread's service on its loopback port, and nothing
at the network layer stops it. The same is true of every loopback listener on a
satellite, and it is why the egress proxy identifies a turn by its credentials
rather than by its port. See
[the enforcement doc](./enforcement.md#a-turn-is-identified-by-its-credentials-never-by-its-port).

A service that must not be reachable from another thread should demand a
credential its own thread holds, such as a token passed in the thread's declared
`env`, rather than rely on its port being unknown.

Process-group teardown is what stops a service; the exec bound on a setup command
or checker still ends only its shell. See
[the timeouts doc](./timeouts.md#the-exec-bound-kills-the-shell-and-only-the-shell).

## Roadmap

- **`PER_MEMBER` isolation.** One instance per member, each in its own network
  namespace, so one instance cannot reach another and hardcoded ports stop
  mattering. That is the real boundary; today the declaration is refused.
- **Services on repos, and promotion of undeclared commands** by the exec
  broker, so the first member running `yarn dev` gets a process and every later
  one gets its URL. Neither exists; `Repo.services` is refused.
- **A bound on `service.log` volume per turn.** Lines are cut at 4096
  characters, and nothing bounds how many a chatty service writes.
- **Checkers told where services listen**, so a checker can run an end-to-end
  test against them. Checkers get the thread's declared `env` today and no
  `ARSOX_SERVICE_*`.
