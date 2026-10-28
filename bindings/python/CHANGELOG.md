## Unreleased

- `bothy.auth.device_login()`, `loopback_login()` and `TokenProvider(None, grant)` use Bothy's
  official OAuth clients when no client file is given. Release wheels include them; a build
  from source has none and needs a client file of your own Cloud project.

## 0.4.0

Built on `bothy` 0.4.0.

- `Database.open(..., p2p=True, stun=[...], p2p_host=...)` and `Database.p2p_status()`:
  direct channels to other online devices while `start_auto_sync` runs.
- `changes()` subscribes when called, so a sync between `changes()` and the first `next()`
  is no longer missed.

## 0.3.0

First release with the sync engine: a native module (pyo3, abi3 wheels for CPython 3.9+ on
Linux x86_64/aarch64/armv7, macOS and Windows) around the Rust core's blocking API.

- `Database.open` with a folder or Google Drive remote (`DriveRemote.appdata`,
  `DriveRemote.folder`), `init_or_join`, collections (`put`, `patch`, `get`, `list`, `all`,
  `delete`), `sync`, `changes`, `start_auto_sync`, `status`, `conflicts`, `clear_conflicts`;
  usable as a context manager. Calls release the GIL.
- Encryption: `encrypt`/`password`/`key` on open, `unlock`, `lock`, `export_key`,
  `change_password`.
- `bothy.auth`: device flow and loopback sign-in, `Grant` (saved with owner-only permissions)
  and `TokenProvider`; any `(force_refresh) -> str` callable works as a token source.
- One exception subclass per engine error kind under `BothyError`.

## 0.0.1

- Name reservation: the hybrid logical clock type.
