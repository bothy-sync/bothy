---
title: Rust library
description: Use the engine directly from async or blocking Rust.
---

```toml
[dependencies]
bothy = "0.5"
```

```rust
use std::sync::Arc;
use bothy::{AutoSync, Config, Database, remote::drive::DriveRemote};
use serde_json::json;

let remote = Arc::new(DriveRemote::appdata(my_token_provider)?);
let config = Config::new("notes.db", "com.example.notes").p2p(bothy::p2p::P2pConfig::new());
let db = Database::open(config, remote).await?;
db.init_or_join().await?;

let notes = db.collection("notes")?;
notes.put("n1", json!({"title": "Hello"})).await?;
let mut changes = db.subscribe();
let sync = db.start_auto_sync(AutoSync::default());
```

- **Async and blocking:** the async API needs a Tokio runtime. `bothy::blocking::Database` is
  the same API for code without one; it runs its own small runtime.
- **Tokens:** `my_token_provider` implements `bothy::auth::AccessTokenProvider` (access token,
  and invalidation after Drive rejects one). The
  [`bothy-oauth`](https://crates.io/crates/bothy-oauth) crate has a ready one, with loopback
  and device code sign-in.
- **Remotes:** `RemoteStore` is a small trait. `FolderRemote` and `DriveRemote` implement it,
  and other backends can.
- **Features:** `blocking`, `crypto`, `drive` and `p2p` are on by default; `test-util` adds a
  fault-injecting remote for tests.

The API reference is on [docs.rs/bothy](https://docs.rs/bothy).
