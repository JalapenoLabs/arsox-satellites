# Satellite setup

The image cannot ship every tool every host application needs. So a host
supplies one install script, the satellite runs it as root, and the tooling it
installs is there for every thread after it.

The script belongs to the satellite rather than to any thread. It is not a
repo's `setup_commands`, which run as the agent inside one thread's checkout;
it is the host preparing the machine every thread runs on.

## The API

| Endpoint | Auth | Purpose |
|---|---|---|
| `PUT /v1/setup` | bearer | set, replace, or clear the script |
| `GET /v1/status` | bearer | `setup` carries the script's status |

`PUT /v1/setup` takes `SetSetupScriptRequest { script }` and answers with
`SetSetupScriptResponse { setup }` as soon as the script is stored, not when it
finishes. Progress is on `GET /v1/status` and on the control socket.

| Sent | What happens | Answered with |
|---|---|---|
| a new script | stored and started at once | `RUNNING` |
| the script already held | nothing | the status its last run left |
| a different script | the running one is stopped, the new one starts | `RUNNING` |
| empty, or only whitespace | a running script is stopped and the script is forgotten | `NONE` |

Sending the script the satellite already holds is deliberately a no-op, whatever
its last run did, so a host can send its script every time it starts without
restarting an install every time. `script_sha256` is the lowercase hex SHA-256 of
the script, so a host can compare it against its own copy without resending.

A script that failed is run again by sending a changed one, or by a container
restart. Clearing it and setting it again also works.

Every SDK carries the call: `Satellite::set_setup_script` in Rust,
`setSetupScript` in Node, and `set_setup_script` in Python. Each returns the
`SetupStatus`, and each client's `status` call carries it in `setup`.

### `SetupStatus`

| Field | Meaning |
|---|---|
| `state` | `NONE`, `RUNNING`, `SUCCEEDED`, or `FAILED` |
| `script_sha256` | hex SHA-256 of the script, empty under `NONE` |
| `exit_code` | the script's exit code; absent while it runs and when it never exited on its own |
| `output_tail` | stdout and stderr as they arrived, oldest dropped first past 16 KiB |
| `started_at` | when the current run started; absent under `NONE` |
| `finished_at` | when it finished; absent while it runs and under `NONE` |

### Control events

| Type | When |
|---|---|
| `setup.started` | a run starts, carrying the script's hash |
| `setup.finished` | a run stops, carrying the status as it stands now |

`setup.finished` after a clear carries `NONE`. A script replaced mid-run gets no
`setup.finished` of its own: the next `setup.started` is its replacement.

## Lifecycle

The script runs:

- **when it is set or changes**, at once;
- **on every container start**, because a replaced container has lost
  everything outside `/var/arsox` and `/workspace`, and installed tooling lives
  outside both.

The row in the database survives a container replacement, which is how a new
container knows to run it again. Whatever the last run did, including a run a
crash left marked `RUNNING`, the new container starts over.

**Idempotency is the script's job.** It runs again on every start, so write it
to check before it downloads. A script that does is a few milliseconds on every
start after the first, and a script that does not is a full install on every
restart.

## New work waits while it runs

While the script is `RUNNING`:

- **no turn is claimed.** The rule is in `claim_next_turn`, beside `PAUSED` and
  `PROVISIONING`, so it holds for whatever does the claiming. Turns can still
  be queued; the queue just does not move.
- **no thread starts provisioning.** A thread with repos created now waits
  before its clones and its repos' setup commands, because those are exactly
  what the host's tooling is installed for. A thread with no repos has nothing
  to provision and opens `IDLE` as always; its turns wait in the claim.
- **turns already running carry on**, and so does a provisioning that had
  already started. Stopping work in flight to install a tool it did not ask for
  would throw away what it had done.

When the script stops, the runner is nudged and the queue moves at once.

## Failure does not stop work

A script that exits nonzero, runs past its bound, or cannot start is `FAILED`,
and work proceeds anyway.

A failed install leaves a satellite missing some tooling. Most turns will not
touch what is missing, and the host can fix the script and send it again. Holding
every queue on the satellite until it did would turn one bad line into an outage,
which is a worse outcome than a turn that meets a missing binary and says so.

The failure is recorded as a `SETUP_FAILED` incident, `degraded`, not retryable,
with no thread: it belongs to the satellite. `details` carries `script_sha256`,
`exit_code` when there is one, and `output`, the same tail the status carries.
It is listed by `GET /v1/incidents` and reaches no thread stream; the control
socket's `setup.finished` carries the same status live.

## How it runs

| | |
|---|---|
| Program | `sh /var/arsox/setup.sh`, the script whole rather than line by line |
| Identity | root, the satellite's own; a satellite that is not root runs it as itself. See [enforcement](./enforcement.md#the-setup-script-runs-as-root) |
| File | beside the database, created `0700` as it is opened, and replaced by rename |
| Environment | empty, then `PATH`, `HOME=/root`, `LANG=C.UTF-8`, `DEBIAN_FRONTEND=noninteractive` |
| Working directory | `/` |
| Standard input | none |
| Bound | 30 minutes |
| Stopping | `SIGTERM` to its process group, 10 seconds, then `SIGKILL` to the group |

**It runs as `sh <file>`, not through the command parser setup commands and
checkers use.** There a newline means "run in parallel"; in an install script
line two depends on line one. The script means exactly what a shell says it
means, `set -e` and heredocs included.

**The script is its own process group**, so stopping it stops everything it
started: `apt-get`, the `dpkg` under it, the maintainer scripts under that.
Stopping the shell alone would orphan an installer that holds its lock until the
container stops. A script is stopped when it runs past its bound, when it is
replaced, and when it is cleared.

The image runs the satellite as PID 1 with no init, and PID 1 is where the
children of a stopped script are reparented once its shell is gone. The satellite
does not reap processes it did not spawn, so each one killed with its group
remains as a zombie entry, holding a PID and nothing else, until the container
stops. Running the container with `docker run --init` puts a reaper in front of
the satellite and clears them.

A script that exits on its own is not chased: a daemon it started deliberately
keeps running. Its output stops being read two seconds after it exits, because a
background process holding the pipe open would otherwise keep the run from ever
finishing, so send a daemon's output to a file.

**The bound is thirty minutes**, the same a repo's setup command gets, because a
toolchain over a slow link is minutes of work. Every turn on the satellite waits
while it runs, so a script that hangs holds the whole satellite until then. That
is the argument against anything longer.

The file lives beside the database, which in the image is `/var/arsox`:
root-owned, `0700`, and a volume, so it is somewhere only root reads and nothing
an agent can reach. It is written to a staged file and renamed into place, so a
shell still stopping a replaced script never reads its successor's lines.

## Security

A host holding `ARSOX_SECRET` can run code as root in the container through this
endpoint. That is new standing, and [the enforcement doc](./enforcement.md#the-setup-script-runs-as-root)
states what it does and does not change.

**Do not write a credential into the script.** The script is stored in the
database and its output is returned by `GET /v1/status` and recorded in an
incident, and no thread's redactor applies to it, because it belongs to no
thread. A script that needs a token to download something should fetch from a
location that does not need one, or the host should run that step some other
way.

## An example

Installing a pinned CLI, checked before it downloads and verified after:

```sh
set -eu

VERSION=4.44.3
SHA256=a2c097180dd884a8d50c956ee16a9cec070f30a7947cf4ebf87d5f36213e9ed7
TARGET=/opt/yq/$VERSION/yq

# Idempotency is the script's job: a restart finds it already there.
if [ -x "$TARGET" ]; then
  exit 0
fi

mkdir -p "$(dirname "$TARGET")"
curl -fsSL -o "$TARGET.download" \
  "https://github.com/mikefarah/yq/releases/download/v$VERSION/yq_linux_amd64"
echo "$SHA256  $TARGET.download" | sha256sum --check --strict
chmod 0755 "$TARGET.download"
mv "$TARGET.download" "$TARGET"
ln -sf "$TARGET" /usr/local/bin/yq
```

The checksum is verified before the binary is moved into place, so a download
that was cut short or tampered with fails the script rather than installing. An
`apt-get` install needs `apt-get update` first, because the image removes the
package lists it was built with:

```sh
set -eu
command -v ffmpeg >/dev/null && exit 0
apt-get update
apt-get install --yes --no-install-recommends ffmpeg
rm -rf /var/lib/apt/lists/*
```

## Where the code is

| Piece | File |
|---|---|
| the manager: set, replace, clear, resume at boot, the route | `src/setup.rs` |
| one run: the file, the spawn, the bound, the group teardown | `src/setup/process.rs` |
| the root spawn path | `src/privilege.rs`, `root_command_for_host` |
| the row and the claim gate | `src/store/setup.rs`, `src/store/claims.rs` |
| provisioning waiting on it | `src/workspace.rs` |
| the schema | `migrations/0005_setup.sql` |

## Roadmap

- **A satellite-level redactor**, so a credential a host writes into its script
  is masked from the status and the incident rather than documented as a thing
  not to do.
- **An init in the image**, so a stopped script's children are reaped without
  `--init` on the run.
- **A way to run a failed script again without changing it**, if resending a
  changed script or restarting the container proves to be the wrong ergonomics.
