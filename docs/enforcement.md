# Deterministic enforcement

Everything in [Permissions](../README.md#permissions) claims the same bar:
enforcement by infrastructure the agent cannot reach. This document is where
that claim is cashed. It states which process runs as which user, what root
owns, what an agent can and cannot modify, and, at least as importantly, what
none of it stops.

The honest summary in one paragraph: a satellite is a set of **deterministic
gates**, not a jail. Agents run as an unprivileged account, the gates are owned
by root, and no prompt talks past them. But an agent that has been granted an
interpreter has been granted everything that interpreter can do, and an agent
that types an absolute path has stepped around a `PATH`. Both are stated below
rather than implied away, because a control whose limits are unwritten is a
control somebody will over-trust.

## The privilege model

Inside the image there are two identities:

| | Runs as | Owns |
|---|---|---|
| the satellite process | `root` | the enforcement points, the database, the shim directories |
| every process it spawns | `arsox`, uid 10001 | its own work under `/workspace` |

**Every child the satellite spawns is handed down to `arsox`.** Harness
processes, `git`, a repo's setup commands, and checkers alike. The drop happens
in `harness::spawn::scrubbed_command`, which is the one function every spawn site
already goes through for the credential scrub, so a new spawn site inherits both
guarantees by construction rather than by somebody remembering.

Handing down means the child's real and effective uid and gid are the agent's
before its first instruction runs, set with `Command::uid`/`Command::gid` so the
kernel applies them between `fork` and `exec`. It is one-way: an unprivileged
process cannot climb back. `HOME`, `USER`, and `LOGNAME` are rewritten to the
agent account at the same moment, because a child left with `HOME=/root` would
spend the turn failing to write files it is not allowed to write.

What root keeps:

| Path | Owner | Mode | Why |
|---|---|---|---|
| `/usr/local/bin/arsox-satellite` | root | `0755` | the binary is also the shim; an agent that could rewrite it would own the gate |
| `/var/arsox` | root | `0700` | threads, queues, event history, and incidents. An agent that could delete the database could delete the record of what it did |
| `/opt/arsox/threads` | root | `0711` | traversable, not listable, so one thread cannot enumerate another's shims |
| `/opt/arsox/threads/<id>/bin` | root | `0755` | the shim directory: readable and executable by the agent, writable by nobody but root |
| `/opt/arsox/threads/<id>/policy.json` | root | `0644` | the allowlist the shim reads on every invocation |
| `/opt/arsox/threads/<id>/denied` | root | `01733` | the deny spool: the agent may drop a record, and may not read or remove one |
| `/workspace/<id>` | `arsox` | `0755` | the agent's own work, which it must be able to write |

The `01733` on the spool is the load-bearing one. Write plus execute lets the
agent create a file; the missing read bit means it cannot list what is there;
the sticky bit means it cannot unlink a record it did not create. So an agent
can be denied a command and cannot then delete the evidence.

### When there is no privilege to separate

The satellite runs on bare metal too, and a developer running it does not run it
as root. There is nothing to drop to and nothing an agent could not overwrite, so
**the deterministic layer does not engage at all** rather than engaging
decoratively.

The decision is made once at boot, from two facts: whether the effective uid is
0, and whether the `arsox` account exists in `/etc/passwd`. Both must hold. When
either does not, the satellite logs `satellite.boot.enforcement_unavailable` at
warn level, naming which one failed, and every thread runs with the advisory
layer alone exactly as it does today.

This is why enforcement is a property of the **container**, and why the README's
recommendation to run the published image is a security recommendation and not
only a convenience one.

## The exec broker

### What it is

A thread that declares `exec: NONE` or `exec: CUSTOM` gets a **shim directory**,
and that directory becomes the entire `PATH` of the harness process. Nothing else
is on it.

Every name in the directory is a small root-owned file whose only content is a
shebang pointing back at the satellite binary:

```
#!/usr/local/bin/arsox-satellite --exec-shim
```

So `git status`, typed by an agent into a shell, execs the satellite in shim
mode. The kernel hands it the path of the file that was invoked, which is how the
shim learns both which command was asked for and which thread's policy applies:
the name is the file's basename and the policy sits beside the directory.

The shim then does one of two things:

- **Allowed.** It resolves the real binary from the policy's recorded search
  path and `exec`s it, replacing itself. The command's own `argv[0]` is set to
  the name it was invoked by, so a program that reads it sees what it expects.
  There is one extra process start on the way in and no wrapper process left
  behind afterwards.
- **Denied.** It writes one record into the deny spool, prints a line to stderr
  naming `PERMISSION_COMMAND_DENIED` and the argv that was refused, and exits
  126. The agent reads that line as the tool's output, so it learns it was
  denied rather than meeting an unexplained "not found".

The name is taken from the file the kernel executed rather than from `argv[0]`,
which a caller can set to anything. Spoofing `argv[0]` therefore cannot make the
shim run a different binary: the target is derived from the trusted name, so the
worst an agent achieves by lying about its own `argv[0]` is running the command
it named.

#### Why a shebang rather than a symlink

A symlink per name would be cheaper. It also loses the one fact the shim needs
most: when a shell resolves `git` through `PATH`, `argv[0]` is the bare word
`git`, and `/proc/self/exe` resolves through the symlink to the satellite binary.
Neither says which directory the shim came out of, and therefore neither says
which thread's policy to apply. A shebang script is passed to its interpreter by
path, so the shim is told exactly where it lives.

### Exact argv semantics

An entry in `allowed_commands` is split on whitespace into tokens. An invocation
is permitted when its leading tokens equal the entry's tokens, one for one:

| Entry | Permits | Refuses |
|---|---|---|
| `gh` | `gh`, `gh pr list`, `gh pr merge` | anything not named `gh` |
| `yarn install` | `yarn install`, `yarn install --frozen-lockfile` | `yarn build`, `yarn` |

There is no globbing, no substring matching, and no shell parsing. `yarn` does
not match `yarnpkg`, and an entry never matches a token it did not name.

This is the same reading the advisory layer already gives an entry: the Claude
arm emits `Bash(yarn install)` and `Bash(yarn install *)` as a pair precisely
because an entry means the bare invocation **and** the invocation with arguments.
The two layers share one meaning deliberately. An operator whose allowlist reads
one way to the CLI and another way to the broker would be debugging a
disagreement rather than a policy.

Whole-argv equality was the other candidate and is worse: `gh` would then permit
only the bare word `gh`, and every useful entry would have to enumerate its own
subcommands and flags.

### The runtime floor

Some names resolve whatever the policy says, because a harness that cannot start
is not a restricted agent:

| Name | On the floor for | Why |
|---|---|---|
| `node` | every brokered thread | the Claude CLI is a Node program; without it no turn runs at all |
| `env` | every brokered thread | its `#!/usr/bin/env node` shebang resolves `node` through `PATH` |
| `sh`, `bash` | `CUSTOM` only | the harness runs an allowed command *through* a shell, so a policy that took the shell away would refuse every command it had just permitted |

Under `NONE` the shell is deliberately not on the floor. It is a deny shim like
anything else, which is the better outcome rather than the stricter one: a shell
reached by its absolute path still starts, and every name it then looks up is a
shim that records a denial. A shell that simply did not exist would produce
silence.

The floor is written into the policy file, so what a thread is actually running
under is readable rather than inferred.

### Denial is reported, not silent

A denied command is worth a record for the reason the README gives `blocked`
incidents: an agent that tried `docker build`, was refused, and quietly worked
around it is almost always telling you that the allowlist or the setup script is
wrong.

That needs a channel from a process running as the agent to the satellite running
as root. Two designs were considered:

- **A root-owned unix socket.** Lower latency, and a denial reaches the stream
  the instant it happens. It also needs a listener, a framing, a connect timeout
  in the shim, and a decision about what a shim does when the socket is gone.
- **A per-thread spool directory the satellite drains.** One `O_EXCL` file per
  denial, drained by the satellite after each harness session and attributed to
  the turn that was running.

**The spool is what is built**, and the reason is attribution rather than
simplicity. A socket message carries no turn id; the satellite would have to
guess which turn a denial belonged to from timing. The runner draining the spool
between sessions knows exactly which turn was running, so every denial lands on
its turn. The cost is that a denial reaches the stream when its session ends
rather than the moment it happens, which for a record whose consumer is an
operator reading incidents afterwards is not a cost worth a listener.

The spool is not trusted input. An agent can write junk into it, or fabricate a
denial that never happened. That is a nuisance and not an escalation: everything
it can fabricate is an incident about itself. Records that do not parse are
discarded with a warning rather than crashing a drain.

### The preset list

`ExecAccess::PRESET` names a curated set of commands. It is defined once, in
`broker::PRESET_COMMANDS`, and the curation principle is one sentence:

> A command earns a place when an ordinary development task fails without it,
> and its effect is confined to the workspace when run by an unprivileged user.

So the list is the image's shipped tooling plus the coreutils that development
actually uses: `git`, `node`, `npm`, `yarn`, `python3`, `pip3`, `gh`, `jira`,
`rg`, `fd`, `jq`, `curl`, the file and text utilities, the build tools, and the
shells.

Deliberately absent, and each for the second half of the principle rather than
the first: `sudo` and `su`, `apt` and `dpkg`, `docker`, `systemctl`, `mount`,
`useradd`, `chown`. Each changes something outside the workspace or changes who
the agent is.

**`PRESET` is not brokered today, and neither is a thread that declares
nothing.** Both keep the satellite's own `PATH`, exactly as they do now. The
contract states that unspecified means the preset, so putting the preset under
the broker would change the default for every existing thread and for every
thread that never asked for a policy at all. That is a decision with its own
blast radius and it gets its own change. The list exists now so that when it
lands there is one definition rather than one invented at the time.

Which leaves the engagement rule, in full:

| `exec` | Broker | Agent `PATH` |
|---|---|---|
| unspecified | no | the satellite's own |
| `PRESET` | no | the satellite's own |
| `NONE` | yes | the shim directory, allowlist empty but for the floor |
| `CUSTOM` | yes | the shim directory, allowlist as declared |

### Setup commands and checkers are not brokered

A repo's `setup_commands` and its `checker` come from the host application, not
from an agent. They are configuration being executed as configured, and they keep
the full `PATH`. They still drop to the agent account, so they cannot write
anything root owns.

Brokering them would mean an operator's own `yarn install` had to appear in an
allowlist written for the agent, which reads as a bug every single time.

## What this does not enforce

Each of these is real, and each is a consequence of the shape of the control
rather than an oversight.

**An allowed interpreter is a general-purpose escape.** A thread that allows
`python3`, `node`, `perl`, or `make` has allowed everything those can execute,
including a subprocess of any name. There is no fix for this that keeps the
interpreter useful. Allowing an interpreter is allowing arbitrary code, and an
allowlist containing one should be read that way.

**An absolute path steps around `PATH`.** `/usr/bin/git` runs whether or not
`git` is on the allowlist, because the real binaries stay readable and executable
by everyone; making them otherwise would break the image for its own tooling.
What the broker deterministically shapes is **name resolution**, which is how
harnesses invoke commands and how nearly every command inside a script is
written. Closing the absolute path needs a mount namespace with a restricted
`/usr/bin` bind-mounted under it, which is future work and is named as such.

**A denied command is refused, not prevented.** The gate is at the point of
invocation. An agent can retry, rename, or route around, and every attempt is
recorded. `blocked` incidents are evidence about intent, not proof of
prevention.

**Nothing here bounds resource use.** Disk quotas, memory, and CPU are separate
controls; a permitted command may still fill a volume.

**The other two deterministic controls are still unbuilt.** The egress proxy and
the root-owned `pre-push` hook are separate work. Until they land, a brokered
thread still reaches the network through whatever it is allowed to run, and a
push is still governed only by the advisory layer.

## The advisory layer stays

The CLI permission flags do not go away and are not superseded. They are what
makes a non-interactive turn able to do work at all, and they shape what an agent
reaches for first. See
[the harness doc](./harness.md#permissions-reach-the-harness-as-flags) for the
setting-to-flag table.

The relationship is that the broker stands **underneath** them. No flag switches
it off, no prompt argues with it, and a harness that decides to run a command
anyway meets the shim. Two layers with one meaning: the flags tell an agent what
it should reach for, the broker decides what it reaches.

## Where the code is

| Piece | File |
|---|---|
| who drops to whom, and whether anything can | `src/privilege.rs` |
| the policy, the preset, argv matching, the shim set | `src/broker/mod.rs` |
| installing and removing a thread's shim directory | `src/broker/install.rs` |
| the shim entrypoint the shebang reaches | `src/broker/shim.rs` |
| the deny spool, written by the shim and drained by the runner | `src/broker/spool.rs` |

## What is proven where

The uid drop and the `exec` are Unix system calls, and the satellite is developed
on Windows. So the layers are split deliberately:

- **The decision logic is pure and unit-tested on every platform.** Who drops to
  whom given a uid and an `/etc/passwd`, which threads engage the broker, what
  the shim set computes to for a policy, whether an argv is permitted, what the
  preset contains, and the spool record round trip.
- **Installation and draining are integration-tested on Unix**, against a
  temporary directory, without root. Ownership is a parameter rather than a
  hard-coded call, so the same code path runs in the test with no account to
  chown to and in production with one.
- **The drop itself and the shebang exec need Linux and root**, so they are
  proven by the container: the Docker workflow boots the image and asserts the
  satellite runs as root while a spawned probe lands as `arsox`.

## Roadmap

- The egress proxy, and a container with no default route around it.
- The root-owned `pre-push` hook, plus the credential helper that refuses to
  release credentials for a denied push.
- `PRESET` under the broker, which is a change to the default for every thread
  and needs its own decision.
- A mount namespace per thread, which is what would close the absolute-path
  route rather than documenting it.
- Filesystem scope per team member, once `members/` exists.
