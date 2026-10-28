# Changelog

All notable changes to the Rust crates (`bothy`, `bothy-cli`, `bothy-oauth`) are listed here. The on-remote
format has its own version (`bothy_format`), independent of crate versions.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Versions follow
[Semantic Versioning](https://semver.org/); before 1.0 a minor release may break the API.

## [Unreleased]

### Added

- CLI: `bothy setup google`, a guided setup of the Google Cloud project and OAuth clients.
  It opens each console page for the project, checks downloaded client files (Desktop, TV,
  Web or mobile, and the project), signs in, and confirms that Drive answers. With
  `--project`, `--client`, `--device-client`, `--no-browser` and `--no-login` it runs
  without prompts.

- OAuth: Bothy's official clients (`bothy_oauth::official`, Cloud project `bothy-512819`).
  Release builds get their secrets at compile time (`BOTHY_OFFICIAL_DESKTOP_SECRET`,
  `BOTHY_OFFICIAL_DEVICE_SECRET`); builds without them have none. The CLI uses them when no
  client of one's own is stored, so `bothy auth login` in a release binary needs no Cloud
  setup (#13). `bothy auth setup --official` goes back to them.

### Changed

- OAuth: `Grant` records the id of the client that issued it (`client_id`; `None` for grants
  stored by earlier versions). The CLI uses it to say "sign in again" after a switch between
  clients, instead of failing with a refresh error. Breaking for code that builds `Grant`
  with a struct literal.
- Drive: a project without the Drive API enabled now fails with an error that says so
  (`accessNotConfigured`), instead of a generic refusal.
- CLI: `bothy auth status` shows the client id and Cloud project of each stored client (text
  and `--json`), so two machines that see different data can be compared at a glance (#92).

### Fixed

- CLI: every `BOTHY_HOME` now keeps its Google sign-in in its own keyring entry
  (`google:<config dir>`). Before, all homes on a machine shared one entry, and signing in
  from one home replaced the sign-in of the others. The default home keeps the old entry, so
  existing sign-ins keep working (#104).

## [0.4.0]

A fast path between online devices. On-remote format unchanged (`bothy_format = 1`); the new
signaling files are optional and 0.3 readers ignore them. The Python and Flutter packages
have their own changelogs.

### Added

- Direct channels between online devices (`p2p` feature, on by default but inactive until
  `Config::p2p(P2pConfig)`). While background sync runs, devices find each other through
  signaling files on the remote and open a WebRTC data channel
  ([str0m](https://crates.io/crates/str0m); ICE with host and STUN candidates,
  `stun.l.google.com` by default, no TURN).
  - After uploading a segment, a device sends it to its peers (up to 64 KiB, otherwise an
    announcement), and they apply it at once. The remote stays the durable record: nothing
    is sent before it is uploaded, and without a channel sync works as before.
  - Measured p50 from a write to the other device: 1.1 to 1.7 s, against about 30 s by
    polling.
  - `Database::p2p_status()` (also in the blocking API) with `P2pStatus::discovering` and
    per-peer `PeerState`.
  - `AutoSync::p2p_debounce` (100 ms) is the write debounce while a channel is open.
    Background sync polls every 3 s while peers are being found.
  - A failed attempt is retried after 15 s, doubling up to 10 minutes. A device whose
    `online` file could not be written retries after 15 s.
- Signaling files: `codec::keys::{KeyKind::Signal, SignalKind}`
  (`<db_id>/signaling/<device_id>/{online-<hlc>,offer-<peer>-<hlc>,answer-<peer>-<hlc>}.json`)
  and `codec::signal`. They are sealed in encrypted databases, so SDPs and IP addresses never
  reach the remote in plain text. Conformance vectors: `spec/vectors/signaling.json`.
- CLI:
  - `sync --watch --p2p`, with `--stun` or `--no-stun`, prints peer state changes.
  - The hidden `bench propagate` (`--peer` for a second local database, or `bench echo` on
    another device) measures how long a write takes to reach another device.

### Changed

- Drive: file ids are reserved 20 at a time, so most uploads need one request instead of two.
- `codec::keys::KeyKind` is `#[non_exhaustive]`.
- `AutoSync` has a new field; struct literals need `..AutoSync::default()`.
- The `p2p` feature builds `aws-lc-sys` (C) for str0m's `aws-lc-rs` crypto backend.

### Security

- No `crypto-bigint` 0.5 (RUSTSEC-2026-0387): P2P uses str0m's `aws-lc-rs` backend rather than
  its RustCrypto one.

## [0.3.0]

Python and embedded Linux. On-remote format unchanged (`bothy_format = 1`). The Python
package (`bothy` on PyPI) and the Flutter package have their own changelogs.

### Added

- `bothy-oauth`: the Google OAuth helpers (loopback sign-in with PKCE, device flow, token
  refresh, revoke, a keyring/file token store and a refreshing `AccessTokenProvider`), moved
  out of the CLI so other front ends can use them. The CLI uses it; its behaviour is unchanged.
- `crates/bothy-ffi` (not published): the start of a C ABI with a cbindgen header, for
  embedding the engine in C and C++ programs.

### Changed

- The default Argon2id cost for new key files is 64 MiB, 2 passes, 1 lane (was 3 passes),
  `bothy::crypto::DEFAULT_KDF_COST`. Three passes took 1.09 s per unlock on a Cortex-A53
  board. Existing key files keep the parameters stored in them.

### Fixed

- The blocking API's runtime had no IO driver, so Drive requests from it (the Python
  binding) panicked.

### Security

- `ctutils` 0.4.5 in the lockfile (RUSTSEC-2026-0385), which `cargo install --locked
  bothy-cli` uses.

## [0.2.0]

Encryption, the conflict log and the Flutter binding. On-remote format still
`bothy_format = 1`: encrypted files use the encrypted flag that format 1 reserved, and
key files are new files that 0.1 readers ignore.

### Added

- Client-side encryption (`crypto` feature, on by default): XChaCha20-Poly1305 for segment
  bodies, snapshot chunks and `device.json`, with a random data key wrapped under an
  Argon2id-derived key in per-device key files (`keys/<device>/key-<hlc>.json`).
- `Config::encrypt`, `Config::password`, `Config::key` and `Config::kdf_cost`;
  `Database::{is_encrypted, is_locked, unlock, unlock_with_key, lock, export_key,
  change_password}`, mirrored in the blocking API. A password unlocks offline from the cached
  key file.
- Password change re-wraps the data key; the old password stops working on every device and
  `SyncReport::password_changed` reports it. Cached data keys keep working.
- `ErrorKind::KeyRequired` for syncing a locked database.
- Conflict log across the database: `Database::conflicts(since_ms)` and `clear_conflicts()`,
  `Config::conflict_log(limit)` (default 1,000 records, `None` turns logging off), and the
  CLI command `bothy conflicts [--since <duration|date>] [--clear]`. `Conflict` now carries
  the collection, document id and the time it was recorded, and is `#[non_exhaustive]`.
- CLI: `init --encrypt`, `unlock [--remember]`, `lock`, `passwd`, `--remember` on `init` and
  `join`, the global `--password-stdin` flag, and `BOTHY_PASSWORD` / `BOTHY_NEW_PASSWORD`.
  `status` shows whether the database is encrypted and locked.
- CLI: `bothy inspect scan <text>...` downloads every remote file of the database and fails
  if any contains the text, to check that an encrypted database leaks nothing.
- Conformance vectors for the crypto primitives and encrypted segments and snapshots; a fuzz
  target for key files and sealed JSON; encrypted seeds in the simulation.

### Changed

- A segment with the encrypted flag and no key now fails with `KeyRequired` instead of
  `FormatTooNew` (vector `segment/invalid.json`, case `encrypted_without_key`).

### Fixed

- On Android the Drive backend verifies TLS against the Mozilla root store (`webpki-roots`);
  the platform verifier needs JNI setup there and panicked on the first request.

## [0.1.0]

First working release: the local-first engine, the Google Drive backend and the CLI.
On-remote format `bothy_format = 1`.

### Added

- Data model: documents of JSON-like values with distinct integers and floats, validated
  collection names, document ids and field names.
- Canonical CBOR codec (RFC 8949 core deterministic encoding) with a strict decoder.
- Hybrid logical clock with drift clamping; field-level last-writer-wins merge with delete
  stamps and a log of writes that lost to another device.
- Local SQLite store (WAL, `synchronous=FULL`): merge state, JSON read view and outbox updated
  in one transaction; a second process gets `DatabaseLocked`.
- Remote file formats: segments (`.bseg`) and chunked snapshots (`.bsnap`) with BLAKE3
  trailers, `manifest.json` and `device.json`, logical keys. Conformance vectors in
  `spec/vectors` with a generator and checker crate.
- Sync engine:
  - idempotent uploads that survive crashes and lost responses;
  - change-feed or listing pulls, plus change events;
  - auto sync with debounce and exponential backoff.
- Snapshots, joining from the most complete snapshot, gap repair after compaction.
- Compaction of a device's own files after a grace period (default 30 days).
- Detection of restored copies that share a device id (fork to a new id without losing writes).
- `FolderRemote` (local or shared directory) and `DriveRemote` (Google Drive API v3):
  - Drive roots: the hidden `appDataFolder`, or a visible `Bothy/<name>` folder;
  - retries with backoff, rate-limit and quota handling;
  - rustls with the ring provider.
- `blocking` facade for callers without an async runtime.
- `bothy` CLI:
  - `auth`, `init`, `join`, `put`, `get`, `list`, `del`, `sync`, `status`, `devices`, `snapshot`, `compact`, `inspect`, `export`;
  - browser sign-in with PKCE, and the device flow for headless boards;
  - keyring or `0600` token storage; `--json` output and documented exit codes.
- Seeded multi-device simulation with injected faults, crashes, clock skew, compaction and
  restored copies (500 seeds per CI run, 50,000 nightly).

### Changed

- Minimum supported Rust version is 1.88 (was 1.85 in 0.0.1).

## [0.0.1]

Name reservation: the hybrid logical clock type and the format version constant.

[Unreleased]: https://github.com/bothy-sync/bothy/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/bothy-sync/bothy/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/bothy-sync/bothy/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/bothy-sync/bothy/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/bothy-sync/bothy/compare/v0.0.1...v0.1.0
[0.0.1]: https://github.com/bothy-sync/bothy/releases/tag/v0.0.1
