## 0.5.0

Built on `bothy` 0.5.0. No API changes.

- A database that isn't on the remote, as after signing in to another account, now fails with
  `remoteInconsistent` instead of uploading to that account's Drive. Keep one database file per
  account (#131).
- Writes of a restored copy of a database file no longer stay invisible to other devices (#115).

## 0.4.0

Built on `bothy` 0.4.0.

- `Bothy.open(p2p: true, stunServers: ..., p2pHost: ...)` and `Bothy.p2pStatus()`
  (`BothyP2pStatus`, `BothyPeer`): direct channels to other online devices while
  `startAutoSync` runs.

## 0.3.0

Built on `bothy` 0.3.0.

- New key files use the lighter default Argon2id cost (64 MiB, 2 passes); existing key files
  keep their own parameters.
- `BothyRemoteSpec.googleDriveFolder` is the remote to share a database with a headless
  device; the demo app's Robot screen shows it against the Python robot agent.

## 0.2.0

First release with the sync engine. Requires a Rust toolchain to build (see the README).

- The package is now a Flutter FFI plugin around the Rust core (flutter_rust_bridge 2.13,
  built with cargokit for Android, iOS, macOS, Linux and Windows).
- `Bothy.open`, `initOrJoin`, collections (`put`, `patch`, `get`, `list`, `delete`,
  `watch`), `sync`, `startAutoSync`, `status`, `conflicts`, `clearConflicts`.
- Remotes: Google Drive app folder, Google Drive folder (tokens from an app callback) and a
  local path.
- Encryption: `encrypt`/`password`/`key` on open, `unlock`, `unlockWithKey`, `lock`,
  `exportKey`, `changePassword`.

## 0.0.1

- Initial early-development release: `bothyFormatVersion` and the `Hlc` timestamp type.
