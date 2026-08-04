# arsox-sdk

Rust client and protobuf contract for [Arsox](https://github.com/JalapenoLabs/arsox-satellites)
satellites.

Arsox runs a fleet of Docker-based, self-hosted agent workers. Claude CLI and
Codex CLI emit different events with different shapes; a satellite normalizes
both into the single contract exposed by this crate, so an application is
written once and never rewritten when the harness, model, or provider changes.

That normalization is the point of the project. Everything else exists to make
it usable.

## Layout

`arsox_sdk::proto` holds the generated contract, mirroring the package tree in
the repository's `proto/` directory:

```rust
use arsox_sdk::proto::event::v1::ThreadEvent;
use arsox_sdk::proto::thread::v1::Thread;
use arsox_sdk::proto::usage::v1::TokenUsage;
```

`arsox_sdk::helpers` carries the conversions that would otherwise be rewritten
at every call site: building a `Timestamp` from a `SystemTime`, rendering
`Money` without going through a float.

The generated code is committed rather than produced by a build script, so
building this crate never requires `protoc`.

## Features

| Feature | Default | What it adds |
|---|---|---|
| `client` | yes | The HTTP and WebSocket client |

A satellite depends on this crate with `default-features = false` to reach the
contract types without linking a client it will never call. Features are
additive: enabling `client` adds items and removes none.

## Versioning

SDK major versions track proto major versions. An SDK 1.x talks to any satellite
serving `arsox.*.v1`, refuses a higher major with a clear message rather than a
confusing decode error, and warns once on a higher minor before proceeding.

Pin your image tag and your SDK version together.

## License

Apache-2.0.
