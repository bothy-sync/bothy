---
title: Limits and quota
description: What one database and one Cloud project can carry.
---

## Per database

Bothy 0.x is designed and tested for:

- up to **100,000 documents** and **20 devices** per database;
- documents up to **1 MiB**, field values up to **256 KiB**;
- no large binary attachments: store files elsewhere and keep a reference.

## Google Drive quota

Drive API quota belongs to the **Cloud project**, shared by all of its users: by default 1,000,000
requests (quota units) per minute, plus a per-user limit. An idle Bothy device makes about one request every
30 s in the foreground and every 5 minutes in the background, and one upload per segment. That
leaves room for a few thousand devices online at the same time on one project; direct
channels reduce polling further.

If a project hits the limit, requests fail with "rate limited". Bothy backs off and retries,
and data is never lost; it only syncs later. A popular app can ask Google for a higher quota
in the Cloud console.

Bothy's official client is one project too, so everyone who signs in through it shares one
quota. That suits personal use and trying things out. Apps and fleets should use a
[project of their own](/start/google-cloud/). All devices of a database must use clients of
the same project, so moving a database to another project means creating a new database.

## Platforms

Linux (x86_64, aarch64, armv7), macOS, Windows, Android and iOS. The Rust crates need Rust
1.88 or newer. On embedded boards, syncing with a kept key peaks below 32 MiB of memory.
