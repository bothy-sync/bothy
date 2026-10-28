# Bothy robot agent

A headless agent for a robot or sensor board (Raspberry Pi, T3 Gemstone, any Linux board):

- It takes **missions** from a phone and marks each one `accepted` and then `done`.
- It writes **telemetry** (CPU temperature, load, free memory) back, once a minute.
- There is no server and no MQTT broker in between: both sides sync through a folder in the
  user's own Google Drive.

It works offline. Readings stay in the local database and merge without loss when the
network is back, even after days. With the data key kept on the board, the agent's peak
memory stays under 32 MiB.

The phone side is the Flutter demo ([`examples/flutter-notes`](../flutter-notes)) with
`BOTHY_DRIVE_FOLDER=robot`; it has a "Robot" screen. The [CLI](../../crates/bothy-cli) works
too:

```sh
bothy join --remote folder:robot --app-id dev.bothy.notes
bothy put missions m1 '{"goal": "dock", "state": "new"}' && bothy sync
```

## Run it

You need Python ≥ 3.9 and the `bothy` package (`pip install bothy`, or unpack a wheel and set
`PYTHONPATH`).

1. **Sign in once.** The board has no browser, so it uses the device flow: it shows a code to
   enter on your phone.

   ```sh
   python3 agent.py login
   ```

   This uses Bothy's official OAuth client, like the Bothy CLI and the Bothy Notes app. If the
   phone app uses clients of your own Cloud project, pass that project's "TVs and Limited
   Input devices" client instead (`--client client_tv.json`): every device of a database must
   sign in through the same project.

2. **First run.** It joins the database the phone created, or creates it:

   ```sh
   BOTHY_PASSWORD='the notes password' python3 agent.py run --folder robot
   ```

   Add `--p2p` to connect directly to the phone while both are online, so missions arrive
   within about a second instead of at the next poll.

   It keeps the database's data key in `~/.local/share/bothy-agent/key` (mode `0600`). Later
   runs need no password, and syncing then needs no Argon2 memory.

3. **As a service.** Use the user-level unit `bothy-agent.service`; its header has the
   install steps.

Options:

- `--interval` sets the seconds between readings.
- `--offline` never touches the network.
- `--simulate-hours 48` first writes 48 hours of per-minute readings at once, for testing a
  long offline period.
- `--once` runs one round and exits.
- `--remote-dir DIR` syncs through a directory instead of Drive.
- `python3 agent.py status` prints the local counters.

The device flow can only grant the `drive.file` scope, so the database lives in a visible
`Bothy/robot` folder rather than the hidden app folder.

## ROS 2

`--ros2` (with a sourced ROS 2 environment) bridges the database to two topics:

- each accepted mission is published as JSON on `/bothy/missions`;
- JSON messages on `/bothy/telemetry` are stored as telemetry.

There is nothing to build:

```sh
source /opt/ros/jazzy/setup.bash
python3 agent.py run --folder robot --ros2
ros2 topic echo /bothy/missions
ros2 topic pub --once /bothy/telemetry std_msgs/String "{data: '{\"battery\": 0.8}'}"
```

## Measure a board

```sh
python3 measure.py     # unlock time (Argon2id), peak memory while syncing 48 h of telemetry
```

Each phase runs in its own process. Everything happens in a temporary directory that is
removed afterwards.

## Test

```sh
python3 -m unittest discover -s tests    # uses a local folder, no Google account
```
