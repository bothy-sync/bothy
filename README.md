# Bothy

**Serverless, local-first sync engine that uses the user's own cloud storage as the transport.**

> ⚠️ **Early release (0.1).** The API and the on-remote format can still change before 1.0. Keep
> a backup of data you care about.

In Scotland and northern England, a *bothy* is an unlocked mountain shelter that belongs to no
one: anyone may use it, and travellers leave notes and supplies for whoever comes next. Bothy
works the same way: there is no server, the data stays in the user's own cloud account, and
devices drop off and pick up files at a shared place.

## What it does

- **Local-first.** Every device keeps a SQLite database as the source of truth and works offline.
- **No server.** Devices exchange immutable files through the user's own Google Drive: the hidden
  app folder, or a visible `Bothy/<name>` folder. A local or shared directory works too.
- **Deterministic merge.** Hybrid logical clocks and per-field last-writer-wins. Every device
  converges to the same state, and a delete is never undone by an older edit.
- **Robust.** Idempotent uploads, crash-safe local transactions, snapshots and compaction,
  repair after long offline periods. Tested by a seeded multi-device fault simulation.
- **Open format.** A versioned on-remote format with [conformance vectors](spec/vectors) so other
  languages can interoperate.

## Quick start (CLI)

```sh
cargo install bothy-cli

# Your own Google Cloud OAuth client; see the setup guide in crates/bothy-cli.
bothy auth setup --client ~/Downloads/client_secret_desktop.json
bothy auth login

bothy init                                   # on the first machine
bothy put notes n1 '{"title": "Hello"}'
bothy sync

bothy join                                   # on the second machine (after auth)
bothy get notes n1
```

Headless boards sign in with `bothy auth login --device` and need a database created with
`--remote folder` (see [why](crates/bothy-cli/README.md#appdata-or-folder)).

## Quick start (library)

```rust
use std::sync::Arc;
use bothy::{Config, Database, remote::drive::DriveRemote};

let remote = Arc::new(DriveRemote::appdata(my_token_provider)?);
let db = Database::open(Config::new("notes.db", "com.example.notes"), remote).await?;
db.init_or_join().await?;

let notes = db.collection("notes")?;
notes.put("n1", serde_json::json!({"title": "Hello"})).await?;
db.sync().await?;
```

See [`crates/bothy`](crates/bothy) for the API, and `bothy::blocking` for callers without an
async runtime.

## Packages

| Registry | Package | Path | Status |
|---|---|---|---|
| crates.io | [`bothy`](https://crates.io/crates/bothy) | [`crates/bothy`](crates/bothy) | 0.1: core engine |
| crates.io | [`bothy-cli`](https://crates.io/crates/bothy-cli) | [`crates/bothy-cli`](crates/bothy-cli) | 0.1: `bothy` command |
| pub.dev | [`bothy`](https://pub.dev/packages/bothy) | [`bindings/flutter`](bindings/flutter) | planned (0.2) |
| PyPI | [`bothy`](https://pypi.org/project/bothy/) | [`bindings/python`](bindings/python) | planned (0.3) |
| npm | [`@bothy-sync/core`](https://www.npmjs.com/package/@bothy-sync/core) | [`bindings/js`](bindings/js) | format types only |

## Roadmap

- **0.2:** client-side encryption (XChaCha20-Poly1305, Argon2id) and the Flutter binding.
- **0.3:** Python binding and an embedded Linux example.
- **0.4:** a faster path between online devices over WebRTC, with Drive as the fallback.

## Minimum supported Rust version

The Rust crates build with Rust **1.88** or newer. Raising the MSRV is not a breaking change
before 1.0, but it is always noted in the [changelog](CHANGELOG.md).

## Security

See [SECURITY.md](SECURITY.md) for how to report a vulnerability.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion
in this project by you, as defined in the Apache-2.0 license, shall be dual licensed as above,
without any additional terms or conditions.
