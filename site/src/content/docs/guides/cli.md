---
title: Command line
description: The bothy command, for scripts, servers and trying things out.
---

```sh
brew install bothy-sync/tap/bothy                    # macOS and Linux
curl -fsSL https://bothysync.dev/install.sh | sh     # macOS and Linux, without Homebrew
```

On Windows, download the `.zip` from the
[latest release](https://github.com/bothy-sync/bothy/releases/latest). These builds sign in with
Bothy's official client. `cargo install bothy-cli` builds from source, without it, so it needs
a [Cloud project of your own](/start/google-cloud/).

```
bothy auth setup --client <json> [--device-client <json>] | --official
bothy auth login [--device] | logout | status
bothy init  [--remote appdata|folder[:<name>]|path:<dir>] [--app-id <id>] [--encrypt [--remember]]
bothy join  [--remote ...] [--db-id <id>] [--remember]
bothy unlock [--remember] | lock | passwd
bothy put   <collection> <id> <json|@file|@-> [--patch] [--unset a,b]
bothy get   <collection> <id> [--with-meta]
bothy list  <collection> [--limit N] [--after <id>]
bothy del   <collection> <id>
bothy sync  [--watch [--p2p [--stun host:port,…|--no-stun]]]
bothy status
bothy devices list | rename <id> <name> | prune <id> --yes
bothy snapshot create
bothy compact [--dry-run]
bothy conflicts [--since 2h|2026-10-21] | --clear
bothy inspect remote | scan <text>…
bothy export <collection> | --all
```

## Global options

- `--db <name>` picks a local database (default `default`).
- `--json` prints machine-readable output.
- `--password-stdin` reads passwords from stdin, one per line.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | ok |
| 1 | error |
| 2 | usage |
| 3 | sign-in needed |
| 4 | offline |
| 5 | remote inconsistent or format too new |

## Two machines

```sh
# machine 1
bothy auth login
bothy init --remote folder:notes
bothy put notes n1 '{"title": "Hello"}'
bothy sync

# machine 2, a headless board
bothy auth login --device          # shows a code to enter at google.com/device
bothy join --remote folder:notes
bothy get notes n1
```

## Checking encryption

`bothy inspect scan <text>…` downloads every remote file of the database and fails if any
contains the text: a quick check that nothing you care about reaches Drive in plain text.

## Files

- **Config and OAuth clients:** `~/.config/bothy` on Linux, the platform config directory
  elsewhere.
- **Databases:** `~/.local/share/bothy/<db>.db` on Linux.
- **`BOTHY_HOME=<dir>`** puts everything under one directory.
- **The sign-in** goes to the OS keyring, or to a file readable only by you where there is
  none (`BOTHY_TOKEN_STORE=file` forces the file). Tokens are never printed.
