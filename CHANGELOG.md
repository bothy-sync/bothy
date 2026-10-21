# Changelog

All notable changes to the Rust crates (`bothy`, `bothy-cli`) are listed here. The on-remote
format has its own version (`bothy_format`), independent of crate versions.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Versions follow
[Semantic Versioning](https://semver.org/); before 1.0 a minor release may break the API.

## [Unreleased]

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

[Unreleased]: https://github.com/bothy-sync/bothy/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/bothy-sync/bothy/compare/v0.0.1...v0.1.0
[0.0.1]: https://github.com/bothy-sync/bothy/releases/tag/v0.0.1
