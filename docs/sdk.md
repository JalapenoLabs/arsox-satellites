# SDKs

Three are planned: Rust, Node, and Python. All three exist.

All three are clients of the same protobuf API, and none of them expose a
protobuf type by accident. Protobuf is always on the wire; what a consumer holds
is an ordinary object in their language.

## The client is a feature

`arsox-sdk` ships the generated contract unconditionally and the client behind a
default-on `client` feature. The satellite depends on the same crate with
`default-features = false`, so a server build never links an HTTP stack it is
already serving.

CI builds the crate both ways. Without that, the contract types could quietly
grow a dependency on the client and nobody would notice until a server build
broke.

## A handle is not a thread

A `ThreadHandle` holds an id and a connection. Every piece of state lives on the
satellite, which is what makes handles disposable and threads durable.

Any process with the URL, the secret, and a thread id can `attach` and do
everything the creating process could. There is no handoff, no lease, and no
ownership: the process that created a thread may have exited hours ago. That is
the property a horizontally scaled application depends on, and there is a test
that attaches from a second client and queues a turn on a thread it did not
create.

`attach` reads the thread before returning, so attaching to a typo fails there
rather than at the first operation on the handle.

## Stopping a thread is three verbs, not a loop

`pause`, `resume`, and `drain` are separate calls because they answer separate
questions: stop claiming work, start again, and clear what is already queued.

Collapsing them into one "stop" would force a choice nobody wants to make on the
caller's behalf. Pausing keeps the backlog, which is right when a thread is
misbehaving and you want to look at what it was about to do. Draining discards
it, which is right when the backlog itself is the problem. They compose, so the
caller says which they meant.

`drain` returns the ids it cancelled rather than a count, and the running turn it
left alone. A caller that needs the thread fully stopped can see there is still
something running and cancel it explicitly.

## The version check happens at connect

`Satellite::connect` calls `/v1/version` and refuses a satellite serving a higher
proto major, rather than letting the mismatch surface as a decode failure three
calls later. A higher minor warns once and proceeds, because additive fields the
SDK does not know about are safely ignored.

## Errors answer three questions

One `Error` type rather than a family. A caller handling a failure almost always
wants the same things regardless of where it came from:

- `code()`: the contract code, when a satellite named one. Absent for a failure
  that never reached one.
- `is_retryable()`: read from the satellite's own flag rather than matched
  against a list of codes. That is what makes an older SDK safe against a newer
  satellite, since a code this build has never heard of still gets a usable
  answer.
- `is_not_found()`, `is_incompatible()`: the two cases worth branching on
  directly.

The contents are boxed. A contract error plus a backtrace runs past a hundred
bytes, and an error that size makes every `Result` in the SDK that large whether
or not anything went wrong.

## Streams are pinned for you

`events()` and `events_from()` return a boxed, pinned stream, so a caller writes
an ordinary `while let Some(event) = events.next().await`. The first version
returned an unpinned `impl Stream` and the integration test would not compile
without pinning it at the call site. That is an ergonomic tax the SDK exists to
absorb, and writing the test from a consumer's seat is the only reason it
surfaced.

## What the aspirational example still needs

`examples/example.rs` at the repository root describes the full surface from the
README, most of which no satellite serves yet. It does not compile, and it is not
meant to yet: it is the design target the SDK is being built toward.

Incidents are reachable now, through `incidents(IncidentQuery)` on the satellite
and on a thread handle. Reaching the rest of the example needs artifacts,
suggestions, plan approval, question answering, and the settings those features
carry. The SDK grows to match as the satellite does.

## Listings return one page

`threads().list` and `incidents` return the first page rather than every match,
because a satellite holding thousands of either should not answer one call with
all of them. The page size and the cursor are the caller's to set;
`IncidentQuery` carries both.

Following the cursor automatically is a convenience the SDK does not offer yet,
and it is deliberate that it does not offer it silently: a call that pages a
hundred thousand incidents into memory behind a caller's back is worse than one
that hands back a cursor.

## The Node client

`sdks/node`, published as `@jalapenolabs/arsox-sdk`. It mirrors the Rust client's
shape and its semantics rather than inventing a second vocabulary: `connect`
checks the contract version, a handle holds an id and a connection, `pause`,
`resume`, and `drain` stay three verbs, and one error class answers which code,
whether to retry, and what happened.

What it reaches today is the surface above: version, status, harness, threads,
turns, the thread event stream, and incidents. Streaming settings toggles, text
and markdown reports, artifacts, plan approval, question answering, and the
control socket are not there yet, and `examples/example.ts` remains a design
target rather than a description of the package.

Mirroring Rust is also why `create` and `createWith` are two methods where the
example shows one call with an options argument. The pair is the reference
client's shape, and collapsing it is a decision to make once, for all three SDKs,
rather than in whichever one was written most recently.

### The contract is copied in, not committed twice

`gen/ts` is the one checked-in TypeScript output. The package syncs it into
`src/proto` on every build, typecheck, and test, and git ignores the copy.

npm can only publish files beneath a package directory, which is the same
constraint that puts the Rust output inside `arsox-sdk` rather than under `gen/`.
Committing a second copy would satisfy the packager and introduce the failure
worth avoiding: a contract that is current in one tree and stale in the other,
with nothing to say which is which.

The generated files carry `.js` import extensions, which is exactly what this
package's ESM and NodeNext resolution wants, so they compile in place with no
rewriting. Nothing reads a `.proto` at runtime.

### `node:http` rather than `fetch`

The listing endpoints carry their filters in a protobuf request message, on GET,
like every other request in the contract. The WHATWG fetch specification forbids
a body on GET outright, so `fetch` throws before a byte leaves the process. This
is not nostalgia for the older API; it is the only one that can express the
contract.

`ws` rather than Node's global `WebSocket` is the same kind of decision. The
thread socket sits behind the same bearer check as every other authenticated
route, and the WHATWG WebSocket API cannot set a header on the handshake.

### Every satellite the suite starts gets its own port

The Rust suite starts a satellite with `assemble` and binds an ephemeral port
in-process. A Node client cannot, because it drives the binary rather than the
router, so it uses `ARSOX_PORT`: the harness binds port 0 to learn a free number,
releases it, and hands it to the satellite it spawns.

There is a race in that gap, and it is worth stating rather than hiding.
Something else could take the port between the release and the satellite's bind.
The window is milliseconds, and the failure is loud: the satellite exits with a
bind error and the harness reports its stderr. A fixed port collides every time
two satellites run rather than almost never.

So the suite needs no particular port free, runs its files in parallel, and never
skips. `ARSOX_PORT` is for exactly this kind of run, one with no port mapping in
front of it. A container still exposes 8080 and is still reached through
`docker run -p`.

### Ceilings and durations are typed out

A consumer writes an ordinary object literal, never a protobuf constructor: the
client accepts an init shape and encodes it. The contract's shapes still show
through where they carry meaning, and that is deliberate. `maxTokensPerTurn` is
`{ ceiling: { case: 'tokens', value: 8_000_000n } }` because a ceiling is a case
rather than a number that might be a sentinel, and `idleTtl` is
`{ seconds: 7200n }` because every time span in the contract is a Duration.

Ergonomic sugar over both is a later decision, and it belongs next to the helper
wrappers `protobuf.md` already plans rather than in one SDK on its own.

## The Python client

`sdks/python`, published as `arsox-sdk` and imported as `arsox_sdk`. It mirrors
the Rust and Node clients rather than inventing a third vocabulary: `connect`
checks the contract version, a handle holds an id and a connection, `pause`,
`resume`, and `drain` stay three verbs, and one `ArsoxError` answers which code,
whether to retry, and what happened.

It reaches the same surface as Node today: version, status, harness, threads,
turns, the thread event stream, and incidents. Streaming settings toggles, text
and markdown reports, artifacts, plan approval, question answering, and the
control socket are not there yet.

Async throughout, on `asyncio`. Events are an async iterator, so a consumer writes
`async for event in stream`, which is the Python spelling of the `while let` the
Rust client hands back and the `for await` the Node one does.

`examples/example.py` is a design target rather than a description of the package,
and it diverges in two visible ways worth naming: it imports the client as
`arsox` and reaches threads through a property rather than `threads()`. The
import name is settled here, because `arsox_sdk` leaves the contract's own
top-level name free. The property is not: `threads()` matches Rust and Node, and
collapsing the pair is a decision to make once, for all three SDKs.

### What a consumer holds is the generated message

Python's conventions say domain data is a pydantic model. This package hands back
the generated protobuf message instead, typed by its `.pyi` stub, and the
divergence is deliberate.

The contract is defined once in `proto/` and compiled. A parallel pydantic model
per message would be a second hand-written copy of a contract whose whole promise
is that it only ever grows, so it drifts the day a field is added and nothing
fails to say so. Worse, proto3 explicit presence would have to be mirrored by
hand on every `optional` field: an `int | None` that somebody types as `int`
turns "this harness reports no cache accounting" into "this run read nothing from
cache", which is the billing-adjacent defect the contract's presence rules exist
to prevent.

It is also the parallel the other two clients already set. Rust hands out prost
structs and Node hands out protobuf-es plain objects, so a consumer of any of the
three holds the contract rather than a rendering of it. Message constructors take
nested dictionaries, so settings still read as ordinary literals rather than a
tree of constructors.

Incident filters are the contract's own `ListIncidentsRequest` for the same
reason. Rust carries an `IncidentQuery` struct and Node a query object, and in
Python either would be a third shape to keep in step with a request message that
already says exactly this.

One wart is worth stating: the generated stubs declare `__slots__ = ()`, which
mypy reads literally and rejects `message.field = value` under. Building through
constructor keywords, repeated-field methods, and `CopyFrom` is what the package
does and what its README tells a consumer to do.

### The contract is copied in, and one import is rewritten

`gen/python` is the one checked-in Python output. The package syncs it into
`src/arsox_sdk/proto` before every build, typecheck, and test, and git ignores the
copy, exactly as `sdks/node` does with `gen/ts`.

The Python copy needs one thing the TypeScript copy did not. The generated
modules import each other absolutely, as `arsox.common.v1`, which resolves only
when `gen/python` is the import root. A plain copy would import a top-level
`arsox` package this distribution does not ship. So the sync rewrites the import
prefix, mechanically, on every run.

The alternative was shipping the generated tree as a top-level `arsox` package,
which needs no rewriting and costs the most useful import name in the ecosystem,
handing it to machine-written modules and leaving nowhere sensible for the client
to live.

### aiohttp for both transports

One dependency covers HTTP and the WebSocket, natively async.

The listing endpoints carry their filters in a protobuf request message on GET,
like every other request in the contract, so the client has to put a body on a
GET. The thread socket sits behind the same bearer check as every other
authenticated route, so it has to set a header on a handshake. `aiohttp` does
both. The alternative was `websockets` for the socket plus blocking `http.client`
in a thread pool for everything else: two dependencies and a thread hop to reach
the same place.

It does mean the client owns a connection pool where the Node one owns nothing,
so `Satellite` closes: `async with`, or `await satellite.close()`.

### Pins

`protobuf` is pinned to `6.33.1`, the runtime the checked-in gencode was
generated by. The gencode validates only that the runtime is not older than
itself, so a 7.x runtime is in policy too; matching the gencode exactly is the
version the contract was compiled and tested against. `types-protobuf` tracks the
same major, because the protobuf runtime ships no inline types and the stub
series follows the runtime it describes.

## Roadmap

- **Retry and failover** in the client, so a `TURN_QUEUE_FULL` or a restarting
  satellite is handled rather than surfaced.
- **`arsox-testkit`**, a fake satellite so a consumer can test their integration
  without running a container.
