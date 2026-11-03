---
title: Robots and ROS 2
description: A robot takes missions from a phone and sends telemetry back, with no server.
---

[`examples/pi-agent`](https://github.com/bothy-sync/bothy/tree/main/examples/pi-agent) is a
complete agent for Raspberry Pi class boards:

- it takes missions that the Flutter demo app writes;
- it marks them `accepted` and then `done`;
- it writes readings of the board (CPU temperature, load, memory) as telemetry.

It works offline and catches up later. It was tested on a 4-core Cortex-A53 board through 48
hours of offline telemetry (2,880 records), with peak memory under 32 MiB while syncing.

```sh
pip install bothy
python3 agent.py login                                    # once: code to enter on your phone
BOTHY_PASSWORD='…' python3 agent.py run --folder robot    # first run joins the database
python3 agent.py run --folder robot --p2p                 # later runs use the kept key
```

- **`--p2p`:** connects directly to the phone while both are online, so a mission arrives in
  about a second.
- **The data key** is kept in `~/.local/share/bothy-agent/key` (mode `0600`), so later runs
  need no password and no Argon2 memory.
- **As a service:** `bothy-agent.service` is a user-level systemd unit.

## ROS 2

With a sourced ROS 2 environment, `--ros2` bridges the database to two topics:

- each accepted mission is published as JSON on `/bothy/missions`;
- JSON messages on `/bothy/telemetry` are stored as telemetry.

```sh
source /opt/ros/humble/setup.bash
python3 agent.py run --folder robot --ros2
ros2 topic echo /bothy/missions
ros2 topic pub --once /bothy/telemetry std_msgs/String "{data: '{\"battery\": 0.8}'}"
```

For C++ nodes, see [C and C++](/guides/c-abi/).

## Why a folder database

Boards sign in with the device code flow, which Google only allows with the `drive.file`
scope. So the robot and the phone share a visible `Bothy/robot` folder. In the demo app, set
`BOTHY_DRIVE_FOLDER=robot`.
