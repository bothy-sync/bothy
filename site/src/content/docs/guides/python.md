---
title: Python
description: Sync from Python scripts, services and embedded boards.
---

```sh
pip install bothy
```

Wheels are abi3 for CPython 3.9 or newer on Linux (x86_64, aarch64, armv7), macOS and
Windows. The API is synchronous and thread-safe, and calls release the GIL.

```python
import bothy, bothy.auth

grant = bothy.auth.device_login()                      # once, on a board without a browser
grant.save("token.json")                               # readable only by you
tokens = bothy.auth.TokenProvider(None, bothy.auth.Grant.load("token.json"))

db = bothy.Database.open("robot.db", app_id="com.example.robot",
                         remote=bothy.DriveRemote.folder("robot", tokens), p2p=True)
db.init_or_join()

missions = db.collection("missions")
missions.put("m1", {"goal": "dock", "speed": 0.5})
missions.patch("m1", {"state": "accepted"})

db.start_auto_sync()
for change in db.changes(timeout=30):     # local and remote changes
    print(change.collection, change.id, change.remote)
```

## Signing in

The wheels on PyPI include Bothy's official clients, which are used when you pass no client
file. An app of your own should use a
[Cloud project of its own](/start/google-cloud/): pass its client JSON file as the first
argument to these calls and to `TokenProvider`. All devices of a database must use the same
project.

- **Boards without a browser:** `bothy.auth.device_login` prints a URL and a code to enter on
  a phone. It only grants `drive.file`, so use `DriveRemote.folder`. Its client type is "TVs
  and Limited Input devices".
- **Desktops:** `bothy.auth.loopback_login` opens the browser and can use
  `DriveRemote.appdata`. Its client type is "Desktop app".
- **Your own tokens:** any callable `(force_refresh: bool) -> str` works as `tokens`.

## Encryption

```python
db = bothy.Database.open(..., encrypt=True, password=pw)   # the device that creates it
db = bothy.Database.open(..., password=pw)                 # devices that join
key = db.export_key()                                      # keep it to skip the password
db = bothy.Database.open(..., key=key)
```

## Errors

Every engine error is a `bothy.BothyError`, with one subclass per kind: `OfflineError`,
`AuthRequiredError`, `WrongPasswordError`, `KeyRequiredError`, `InvalidInputError` and so on.
