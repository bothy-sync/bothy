# Two machines with the CLI

This host and a Linux container act as two devices of one database. Each machine writes while
offline, both sync, and `bothy export --all` must give the same bytes on both.

```sh
./run.sh path     # shared folder remote; automatic, no Google account needed
./run.sh drive    # Google Drive (folder mode); the container signs in with the device flow
```

`drive` mode uses your own Google Cloud OAuth clients (see the setup guide in
[`crates/bothy-cli`](../../crates/bothy-cli/README.md)):

```sh
DESKTOP_CLIENT=~/Downloads/desktop-client.json \
DEVICE_CLIENT=~/Downloads/tv-client.json \
./run.sh drive
```

The host signs in through the browser. The container prints a code to approve at
`google.com/device`. A headless device can only get the `drive.file` scope, so the database
uses a visible `Bothy/cli-two-machines` folder instead of the hidden app folder.

Files go to `.work/` next to this script.
