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
| `claude`, `codex` | every brokered thread | the satellite launches the harness by name, and a policy that refused it would refuse the turn |
| `sh`, `bash` | `CUSTOM` only | the harness runs an allowed command *through* a shell, so a policy that took the shell away would refuse every command it had just permitted |

Under `NONE` the shell is deliberately not on the floor. It is a deny shim like
anything else, which is the better outcome rather than the stricter one: a shell
reached by its absolute path still starts, and every name it then looks up is a
shim that records a denial. A shell that simply did not exist would produce
silence.

The floor is written into the policy file, so what a thread is actually running
under is readable rather than inferred.

### When the directory is built

**At the start of every turn**, in the runner, and nowhere else.

Building it once at thread creation would be cheaper and is wrong in a way that
fails quietly. The shim directories live under `/opt/arsox/threads`, which is
deliberately not a volume, so a container replacement leaves nothing there. A
thread resumed on the new container would come up unbrokered, look perfectly
healthy, and run with the full `PATH` its policy said it should not have. Every
turn re-asserting the policy closes that, and it means a thread runs under the
allowlist it has now rather than the one it was created with.

The rebuild is skipped when the policy already written matches the policy the
settings resolve to, so an unchanged thread pays for one directory read per turn
rather than a file per command name in the image. The deny spool survives a
rebuild either way: a refusal recorded a moment ago is evidence, not stale state.

`ARSOX_BROKER_ROOT` moves the root, which is what a bare-metal run or a test
does. It defaults to `/opt/arsox/threads`.

### Exit codes a shim uses

| Code | Meaning |
|---|---|
| 126 | the policy refused it, which is the shell's own "found and could not be run" |
| 127 | it was allowed and this image does not carry it |
| 125 | the shim could not read its own policy, which is a satellite fault rather than a permission decision |

Distinct on purpose: an agent reading its own tool output can tell a refusal from
a missing command without parsing a message, and an operator seeing 125 should
look at the shim directory rather than at the allowlist.

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

## The push gate

### What it is

A thread that **denies pushing, protects a ref, or declares a secret** gets a
**hooks directory** beside its shim directory, holding one file:

```
#!/usr/local/bin/arsox-satellite --pre-push
```

`core.hooksPath` points every checkout at that directory, so `git push`, typed
by an agent, execs the satellite. It is the same multi-call shim pattern the
exec broker uses, and it learns its thread the same way: from the path of the
file the kernel ran it by, with the policy sitting beside the directory it came
out of.

A thread with none of those three has nothing to enforce about pushing and gets
no hooks directory. Nothing about its pushes changes, and its repos keep
whatever hooks they installed themselves.

### Where the hooks path is written, and why twice

`core.hooksPath` is ordinary repository config, and the interesting question is
which config.

- **At clone time**, as `git clone --config core.hooksPath=...`, which is
  written into the new repository's config and outlives the clone. This is
  deliberately the *opposite* of how the clone's credential is passed: a
  credential goes in with `git -c`, which is scoped to the one invocation and
  persists nowhere. Getting the two the wrong way round would persist a token
  and lose a gate in the same line.
- **At the start of every turn**, over every checkout under the thread's
  `repos/`, for the same reason the shim directory is rebuilt there. Anything
  that writes config can replace `core.hooksPath`, and a repo whose setup
  installs husky does exactly that. Re-asserting also picks up a thread whose
  settings grew a protected branch after it was cloned.

Coverage, stated rather than assumed:

| | Covered | How |
|---|---|---|
| the clone | yes | its own config |
| a worktree of the clone | yes | worktrees share the clone's config, and Arsox does not enable `extensions.worktreeConfig` |
| a submodule | yes | its own config, written in a pass after the clone |
| a repo the agent cloned itself | no | it has a config Arsox never wrote |

The hooks path is set and never unset. A thread that stops protecting a branch
keeps pointing at a hooks directory the installer has emptied, which is a repo
with no hooks; unsetting instead would clobber whatever the repo's own tooling
had put there.

### What the hook decides

git hands a `pre-push` hook the remote's name and URL on argv, and one line per
ref on stdin: the local ref, the local sha, the remote ref, and the remote sha.
Two of those shas are placeholders rather than commits. A remote sha of all
zeros means the remote has never seen the ref, and a local sha of all zeros
means the push deletes it. Both are read as the cases they are rather than
compared against a forty-character literal, because a SHA-256 repository spells
the same absence with sixty-four zeros.

The checks run cheapest and broadest first:

1. **`allow_git_push` is false.** Every ref is refused, with
   `PERMISSION_PUSH_DENIED`, without reading a commit.
2. **A ref matches `protected_branches`.** Refused with
   `PERMISSION_BRANCH_PROTECTED` and `details.ref`. The match is against the ref
   *as it will exist on the remote*, so a delete is refused exactly like a
   write, and it accepts either spelling: `main` and `refs/heads/main` name the
   same branch. There is no globbing, which is the same reading the exec
   allowlist gives an entry.
3. **Otherwise the outgoing commits are scanned** for the thread's secrets. A
   hit refuses the push with `SECRET_IN_PUSH_BLOCKED`.

Every refusal is written into the same deny spool the exec broker uses, which
now carries the code for the gate that closed, and the runner drains it into a
`blocked` incident attributed to the turn that was running.

### The scan, and why the hook holds no secrets

The hook runs as the agent, because the push does. The thread's secret set
includes the repo's own access token, the GitHub and Jira tokens, and every LLM
credential, none of which an agent is ever given. **Writing them somewhere the
agent can read them, in order to check that the agent is not leaking them,
would hand over the thing being protected.** A salted hash of each is no better:
it is an offline cracking target for a credential and it leaks the length.

So the secrets stay with the satellite and the hook asks a question instead:

1. the hook runs `git log --format=%B --patch` over the outgoing range and
   writes the bytes straight into a unix socket in the thread's broker
   directory,
2. it shuts down its writing half,
3. the satellite, which is root and holds the secrets, answers `clean` or
   `secret` and closes.

The satellite **reads and never runs**: it does not spawn git, does not open the
repository, and never touches a path the agent named.

The socket is bound per turn, carrying the turn's own redactor, because an agent
pushes during a turn and at no other time. A push made when nothing is listening
is refused rather than allowed.

### Scanning cost

Two bounds, and they are different things.

**The range is bounded.** An update scans `remote..local`. A branch the remote
has never seen scans `local --not --remotes`, which excludes everything already
on a remote-tracking ref, so the first push of a topic branch cut from `main`
costs the topic branch rather than the repository's history. A delete sends
nothing and scans nothing.

**The memory is bounded.** The satellite reads the stream in 64 KiB chunks and
carries one secret's length between them, so a push of ten thousand commits
costs the same memory as a push of one, and a secret straddling a chunk boundary
is still found. The carry is bytes rather than text, so a multi-byte character
split by a read reassembles rather than being lost to a replacement character.

### What git's ordering actually guarantees

A credential helper that refuses to release credentials for a denied push is the
obvious second half of this control, and it is not buildable. Two facts decide
that, and both are worth stating rather than leaving somebody to rediscover.

**git's credential protocol has no field naming the operation.** A helper is
told the protocol, the host, the path, and sometimes the `WWW-Authenticate`
challenge. It is never told whether the fetch it is answering is a fetch or the
ref discovery half of a push, because over HTTP those are the same request shape
to the same host. **A helper cannot tell them apart**, so a helper that refuses
"for a push" cannot be written.

**The ordering makes the hook the gate anyway.** A push is: ref discovery, which
is where credentials are acquired; then the `pre-push` hook, whose stdin needs
the remote shas discovery produced; then the upload that sends objects. So the
hook runs *after* authentication and *before* any content leaves. It is a
complete gate on content leaving and is not a gate on a credential being
released.

**And there is no credential to gate.** Arsox lends a token to its own `git`
process for the length of one invocation and writes nothing into the checkout,
so an agent's own `git push` has no Arsox credential to release at all. That is
a stronger guarantee than the one the helper was supposed to give. If Arsox ever
grows a persistent credential for agents, the holdable version of the promise is
that a thread which denies pushing has no helper installed rather than one that
argues.

### Exit codes the hook uses

| Code | Meaning |
|---|---|
| 0 | the policy permits it |
| 1 | the policy refused it; git aborts on any nonzero code and prints nothing of its own, so the line above it is what the agent reads |
| 125 | the hook could not decide, which is a satellite fault rather than a permission decision |

125 is the exec shim's code for the same condition, and it covers every way the
gate can fail to run: an unreadable policy, stdin that is not the format git
documents, no `git` to read the push with, and a satellite that did not answer a
scan. **All of them refuse.** A scan that did not happen is not a scan that
found nothing.

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

**A hook runs as the agent, so an agent can decline to run it.** `git push
--no-verify` skips every hook, `git -c core.hooksPath=/dev/null push` points at
another directory for one command, and a repository cloned somewhere Arsox never
configured has no hooks path at all. The gate is at the point of invocation,
exactly as the exec broker's is: it holds every push that does not set out to
disable it, which is every push an agent makes in the ordinary course of its
work. Closing it needs the egress proxy, so that the network refuses what the
hook declined to send.

**A repo's own tooling can take the hooks path.** `core.hooksPath` is repository
config and husky sets it to `.husky` during an ordinary `yarn install`. The
runner re-asserts it at the start of every turn, so the window is one turn wide
rather than permanent, and inside that turn the gate is gone.

**The scan reads what git renders as text.** A changed binary is `Binary files
differ` in a patch, so a credential inside one is not in the text being scanned.
Neither is anything the push does not carry, which is the same fact that makes
the scan affordable.

**The egress proxy is still unbuilt.** Until it lands, a brokered thread reaches
the network through whatever it is allowed to run.

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

Push is the one control with no advisory half worth speaking of. A push can be
spelled a dozen ways in argv, so the CLI flags never tried to match one, and the
hook is the whole of the enforcement rather than the deterministic layer beneath
a suggestion.

## Where the code is

| Piece | File |
|---|---|
| who drops to whom, and whether anything can | `src/privilege.rs` |
| the policy, both halves of it, and which gates engage | `src/broker/mod.rs` |
| the preset, argv matching, the shim set | `src/broker/mod.rs` |
| installing and removing a thread's gates | `src/broker/install.rs` |
| the shim entrypoint the shebang reaches | `src/broker/shim.rs` |
| what a push is, and what a policy says about one | `src/broker/push.rs` |
| the `pre-push` entrypoint the shebang reaches | `src/broker/hook.rs` |
| the scan: the socket, the protocol, the bounded sieve | `src/broker/scan.rs` |
| the deny spool, written by a gate and drained by the runner | `src/broker/spool.rs` |
| pointing a checkout at the hooks it must run | `src/workspace/repos.rs` |

## What is proven where

The uid drop and the `exec` are Unix system calls, and the satellite is developed
on Windows. So the layers are split deliberately:

- **The decision logic is pure and unit-tested on every platform.** Who drops to
  whom given a uid and an `/etc/passwd`, which gates a thread engages, what the
  shim set computes to for a policy, whether an argv is permitted, what the
  preset contains, how a push's refspecs parse, which ref a protected entry
  matches, what range a push scans, and the spool record round trip.
- **Installation and draining are tested on Unix**, against a temporary
  directory, without root. Installation sets no ownership at all: every path in a
  shim directory is owned by whoever built it, which in the image is the
  satellite and therefore root. What it sets is the three modes, and those are
  the same three whether a test builds the directory or a satellite does, so the
  whole mechanism runs on an ordinary Linux CI runner.
- **The `pre-push` hook is driven end to end on every platform**, in
  `tests/push_gate.rs`. git runs a hook as a program with the remote on argv and
  the refspecs on stdin, so the tests run the satellite binary exactly that way,
  against a broker directory in a temporary directory: a denied push, a
  protected ref and its `details.ref`, a delete of a protected branch, a
  permitted push that records nothing, and both fail-closed paths. What the
  temporary directory stands in for is ownership, which is not what the hook
  reads.
- **The scan and the hooks path are driven on Unix**, in the same file: a real
  repository, a real socket, a real `git log` streamed to a real satellite. A
  commit carrying a secret is refused with `SECRET_IN_PUSH_BLOCKED` and a clean
  one is not, a push with nothing listening is refused rather than allowed, and
  a checkout comes out of `point_hooks_at` with `core.hooksPath` set.
- **The drop itself and the shebang exec need Linux and root.** The Docker
  workflow boots the image and asserts that PID 1 runs as uid 0 and that the
  satellite reported `satellite.boot.enforcement_ready`, which is the posture
  every spawn then applies.

  **Not asserted in CI**, and worth saying rather than implying: the uid a
  spawned child actually lands on, a real command going through a real shim, and
  a real `git push` finding the hook through `core.hooksPath` rather than the
  hook being invoked the way the kernel would invoke it. Observing any of them
  means driving a turn inside the container, which is a longer smoke than the
  build workflow is. A turn-driving container smoke is the test that would close
  all three.

## Roadmap

- The egress proxy, and a container with no default route around it. It is also
  what would close the `--no-verify` route around the push gate, by refusing at
  the network what the hook declined to send.
- `PRESET` under the broker, which is a change to the default for every thread
  and needs its own decision.
- A mount namespace per thread, which is what would close the absolute-path
  route rather than documenting it.
- Filesystem scope per team member, once `members/` exists.
