# Bothy

**Serverless, local-first sync engine that uses the user's own cloud storage as the transport.**

> ⚠️ **Early release (0.5).** The API and the on-remote format can still change before 1.0. Keep
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
- **End-to-end encryption (optional).** With a password, everything the database stores on
  Drive is encrypted (XChaCha20-Poly1305, Argon2id). Changing the password does not re-encrypt
  data, and a forgotten password cannot be recovered.
- **Conflicts you can see.** Edits that lose to a newer edit of the same field are kept in a
  conflict log.
- **Fast between online devices (optional).** Devices that are online at the same time open a
  direct WebRTC data channel, found through small files on the remote with no server of
  their own, so writes arrive within about a second instead of at the next poll. Drive stays
  the durable record; without a channel, sync works as before.
- **Open format.** A versioned on-remote format with [conformance vectors](spec/vectors) so other
  languages can interoperate.

## Quick start (CLI)

```sh
cargo install bothy-cli

# Your own Google Cloud OAuth client; see the setup guide in crates/bothy-cli.
bothy auth setup --client ~/Downloads/client_secret_desktop.json
bothy auth login

bothy init --encrypt --remember              # on the first machine; asks for a password
bothy put notes n1 '{"title": "Hello"}'
bothy sync

bothy join --remember                        # on the second machine (after auth)
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

## Quick start (Flutter)

```dart
await Bothy.initialize();
final db = await Bothy.open(
  path: '${dir.path}/notes.db',
  appId: 'com.example.notes',
  remote: BothyRemoteSpec.googleDriveAppData((refresh) => signIn.token(refresh)),
  password: password,
);
await db.initOrJoin();
await db.collection('notes').put('n1', {'title': 'Hello'});
```

See [`bindings/flutter`](bindings/flutter), and the demo app
[`examples/flutter-notes`](examples/flutter-notes) (Android, iOS, macOS, Linux, Windows).

## Quick start (Python)

```sh
pip install bothy
```

```python
import bothy, bothy.auth

grant = bothy.auth.device_login("client_tv.json")      # once, on a board without a browser
tokens = bothy.auth.TokenProvider("client_tv.json", grant)
db = bothy.Database.open("robot.db", app_id="com.example.robot",
                         remote=bothy.DriveRemote.folder("robot", tokens))
db.init_or_join()
db.collection("telemetry").put("t1", {"cpu_temp_c": 48.2})
db.sync()
```

Wheels for Linux (x86_64, aarch64, armv7), macOS and Windows; see
[`bindings/python`](bindings/python). [`examples/pi-agent`](examples/pi-agent) is a robot agent
that takes missions from the Flutter demo and sends telemetry back, with an optional ROS 2
bridge. A C ABI is in [`crates/bothy-ffi`](crates/bothy-ffi) (early, not published).

## Packages

| Registry | Package | Path | Status |
|---|---|---|---|
| crates.io | [`bothy`](https://crates.io/crates/bothy) | [`crates/bothy`](crates/bothy) | 0.4: core engine |
| crates.io | [`bothy-cli`](https://crates.io/crates/bothy-cli) | [`crates/bothy-cli`](crates/bothy-cli) | 0.4: `bothy` command |
| crates.io | [`bothy-oauth`](https://crates.io/crates/bothy-oauth) | [`crates/bothy-oauth`](crates/bothy-oauth) | 0.4: Google sign-in helpers |
| pub.dev | [`bothy`](https://pub.dev/packages/bothy) | [`bindings/flutter`](bindings/flutter) | 0.4: Flutter plugin |
| PyPI | [`bothy`](https://pypi.org/project/bothy/) | [`bindings/python`](bindings/python) | 0.4: Python package |
| npm | [`@bothy-sync/core`](https://www.npmjs.com/package/@bothy-sync/core) | [`bindings/js`](bindings/js) | format types only |

## Roadmap

- **0.2:** client-side encryption, the conflict log, and the Flutter binding.
- **0.3:** the Python binding, an embedded Linux robot agent with a ROS 2 bridge, and the
  start of a C ABI.
- **0.4 (this release):** direct channels between online devices over WebRTC, with Drive as
  the durable record and the fallback.

## Minimum supported Rust version

The Rust crates build with Rust **1.88** or newer. Raising the MSRV is not a breaking change
before 1.0, but it is always noted in the [changelog](CHANGELOG.md).

## Documentation and contributing

Guides and concepts are at **[bothysync.dev](https://bothysync.dev)**. Contributions are
welcome: see [CONTRIBUTING.md](CONTRIBUTING.md) and the
[good first issues](https://github.com/bothy-sync/bothy/labels/good%20first%20issue).

## Security

See [SECURITY.md](SECURITY.md) for how to report a vulnerability.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion
in this project by you, as defined in the Apache-2.0 license, shall be dual licensed as above,
without any additional terms or conditions.
