# bothy-cli

**The `bothy` command: local-first databases that sync through your own Google Drive.**

> ⚠️ **Early release (0.1).** Commands and the on-remote format can still change before 1.0.

```sh
cargo install bothy-cli      # installs the `bothy` binary
```

## Google Cloud setup (once)

Bothy has no server and no OAuth app of its own yet, so you use a client of **your own Google
Cloud project**. All devices of one database must use clients from the **same project**:
Google Drive only shows a project the files that project's clients created.

1. Go to <https://console.cloud.google.com/>, create a project, and enable the **Google Drive
   API** (APIs & Services → Library).
2. **OAuth consent screen** (Google Auth Platform): user type *External*, fill in the app name
   and your email, and add your Google account under **Audience → Test users**.
3. **Data access:** add the scopes `.../auth/drive.appdata` and `.../auth/drive.file`. Both are
   non-sensitive, so no Google review is needed.
4. **Clients → Create client**:
   - type **Desktop app**, for browser sign-in on computers;
   - for headless boards, also type **TVs and Limited Input devices**.

   Download each client's JSON file.
5. Store them:

   ```sh
   bothy auth setup --client ~/Downloads/client_secret_desktop.json
   bothy auth setup --device-client ~/Downloads/client_secret_tv.json   # optional
   ```

While the consent screen is in *Testing* status, Google expires sign-ins after **7 days**:
run `bothy auth login` again, or publish the app (*In production*). An unverified app shows a
warning screen at sign-in, which is fine for your own project.

## appdata or folder?

| `--remote` | Where the files are | Sign-in | Use when |
|---|---|---|---|
| `appdata` (default) | the hidden per-app folder (`appDataFolder`) | browser only | computers and phones |
| `folder[:<name>]` | a visible `Bothy/<name>` folder | browser or device code | a headless board will join |
| `path:<dir>` | a local or shared directory | none | LAN shares, testing |

Google rejects the `drive.appdata` scope in the device (TV) sign-in flow, so a board signed in
with `bothy auth login --device` can only reach **folder** databases. Choose `folder` when you
create the database if a headless device will ever join it.

## Two machines

```sh
# machine 1
bothy auth login
bothy init --remote folder:notes
bothy put notes n1 '{"title": "Hello"}'
bothy sync

# machine 2 (a headless board)
bothy auth setup --device-client client_secret_tv.json
bothy auth login --device          # shows a code to enter at google.com/device
bothy join --remote folder:notes
bothy get notes n1
```

[`examples/cli-two-machines`](https://github.com/bothy-sync/bothy/tree/main/examples/cli-two-machines)
runs this between your computer and a Linux container.

## Commands

```
bothy auth setup --client <json> [--device-client <json>]
bothy auth login [--device] | logout | status
bothy init  [--name <db>] [--remote appdata|folder[:<name>]|path:<dir>] [--app-id <id>]
bothy join  [--name <db>] [--remote ...] [--db-id <id>]
bothy put   <collection> <id> <json|@file|@-> [--patch] [--unset a,b]
bothy get   <collection> <id> [--with-meta]
bothy list  <collection> [--limit N] [--after <id>]
bothy del   <collection> <id>
bothy sync  [--watch]
bothy status
bothy devices list | rename <id> <name> | prune <id> --yes
bothy snapshot create
bothy compact [--dry-run]
bothy inspect remote
bothy export <collection> | --all
```

Global options: `--db <name>` picks a local database (default `default`), and `--json` prints
machine-readable output.

Exit codes: 0 ok, 1 error, 2 usage, 3 sign-in needed, 4 offline, 5 remote inconsistent or
format too new.

## Files

- Config, OAuth clients and the fallback token file: `~/.config/bothy` (Linux), the
  platform config directory elsewhere.
- Databases: `~/.local/share/bothy/<db>.db` (Linux), the platform data directory elsewhere.
- `BOTHY_HOME=<dir>` puts both under one directory.
- The sign-in (refresh token) goes to the OS keyring. Without one (e.g. headless Linux
  without a Secret Service), it goes to a file readable only by you, and the CLI warns.
  `BOTHY_TOKEN_STORE=file` forces the file. Tokens are never printed.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
