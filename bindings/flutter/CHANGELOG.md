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
