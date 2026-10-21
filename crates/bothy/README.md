# bothy

**Serverless, local-first sync engine that uses the user's own cloud storage as the transport.**

> ⚠️ **Early release (0.1).** The API and the on-remote format can still change before 1.0.

Each device keeps a local SQLite database as the source of truth and works offline. Devices
exchange immutable files through the user's own Google Drive (or a shared directory). There is
no backend and no account of ours.

## Example

```rust
use std::sync::Arc;
use bothy::{Config, Database, remote::FolderRemote};
use serde_json::json;

let remote = Arc::new(FolderRemote::new("/mnt/shared/notes")?);
let db = Database::open(Config::new("notes.db", "com.example.notes"), remote).await?;
db.init_or_join().await?;

let notes = db.collection("notes")?;
notes.put("n1", json!({"title": "Hello", "pinned": false})).await?;   // whole document
notes.patch("n1", json!({"pinned": true}), &[]).await?;               // some fields
let note = notes.get("n1").await?;

let report = db.sync().await?;              // push local changes, pull remote ones
let mut events = db.subscribe();            // local and remote change events
let auto = db.start_auto_sync(Default::default());   // inside an async context
```

For Google Drive, implement `bothy::auth::AccessTokenProvider` (OAuth stays in your app) and
use `remote::drive::DriveRemote::appdata(provider)` for the hidden app folder (scope
`drive.appdata`), or `DriveRemote::folder(provider, "notes")` for a visible `Bothy/notes`
folder (scope `drive.file`, also reachable from the device sign-in flow of headless boards).

## How it merges

- Every change gets a hybrid logical clock stamp. Fields merge separately, and the larger
  `(hlc, device)` stamp wins.
- Edits to different fields of the same document both survive. On the same field the later
  write wins; `Collection::conflicts` returns the losing value.
- Deletes keep a tombstone. An edit made before the delete never revives the document; an edit
  made after it does, with only the new fields.
- Nested objects and arrays are one value. Model concurrent lists as separate documents.

## Feature flags

| Feature | Default | What it adds |
|---|---|---|
| `drive` | yes | `remote::drive::DriveRemote` (reqwest over rustls with the ring provider; no OpenSSL) |
| `blocking` | yes | `bothy::blocking`, a synchronous API with its own runtime |
| `test-util` | no | `remote::faulty::FaultyRemote` for fault-injection tests |

## Links

- Repository and roadmap: <https://github.com/bothy-sync/bothy>
- CLI: [`bothy-cli`](https://crates.io/crates/bothy-cli)
- Conformance vectors for other implementations: [`spec/vectors`](https://github.com/bothy-sync/bothy/tree/main/spec/vectors)

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
