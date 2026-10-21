# Bothy

**Serverless, local-first sync engine that uses the user's own cloud storage as the transport.**

> ⚠️ **Early development.** Nothing here is ready for real data yet.

In Scotland and northern England, a *bothy* is an unlocked mountain shelter that belongs to no
one: anyone may use it, and travellers leave notes and supplies for whoever comes next. Bothy
works the same way: there is no server, the data stays in the user's own cloud account, and
devices drop off and pick up files at a shared place.

## Goals

- **Local-first.** Each device keeps a SQLite database as the source of truth and works offline.
- **No server.** Devices exchange immutable, append-only files through the user's own Google
  Drive (`appDataFolder`). OneDrive and WebDAV may follow.
- **Deterministic merge.** Hybrid logical clocks and per-field last-writer-wins: every device
  converges to the same state.
- **Open format.** A documented, versioned on-remote format (`SPEC.md`) with conformance vectors.
- **Many languages.** Rust core with bindings for Flutter/Dart, Python and C, from phones to
  embedded Linux boards.

## Packages

| Registry | Package | Path |
|---|---|---|
| crates.io | [`bothy`](https://crates.io/crates/bothy) | [`crates/bothy`](crates/bothy) |
| crates.io | [`bothy-cli`](https://crates.io/crates/bothy-cli) | [`crates/bothy-cli`](crates/bothy-cli) |
| pub.dev | [`bothy`](https://pub.dev/packages/bothy) | [`bindings/flutter`](bindings/flutter) |
| PyPI | [`bothy`](https://pypi.org/project/bothy/) | [`bindings/python`](bindings/python) |
| npm | [`@bothy-sync/core`](https://www.npmjs.com/package/@bothy-sync/core) | [`bindings/js`](bindings/js) |

## Minimum supported Rust version

The Rust crates build with Rust **1.88** or newer. Raising the MSRV is not a breaking change
before 1.0, but it is always noted in the changelog.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion
in this project by you, as defined in the Apache-2.0 license, shall be dual licensed as above,
without any additional terms or conditions.
