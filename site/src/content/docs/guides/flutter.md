---
title: Flutter apps
description: Add sync to a Flutter app on Android, iOS, macOS, Linux and Windows.
---

The [`bothy`](https://pub.dev/packages/bothy) package wraps the Rust engine as a native
library. Building an app with it needs a Rust toolchain ([rustup](https://rustup.rs)); the
native code is compiled for each platform during the Flutter build.

```yaml
dependencies:
  bothy: ^0.5.0
```

## Open, write, watch

```dart
import 'package:bothy/bothy.dart';

await Bothy.initialize();
final db = await Bothy.open(
  path: '${supportDir.path}/notes.db',
  appId: 'com.example.notes',
  remote: BothyRemoteSpec.googleDriveAppData(
    (forceRefresh) => mySignIn.accessToken(refresh: forceRefresh),
  ),
  p2p: true,
);
await db.initOrJoin();

final notes = db.collection('notes');
await notes.put('n1', {'title': 'Hello', 'done': false});
await notes.patch('n1', {'done': true});
final note = await notes.get('n1');

notes.watch().listen((change) => refresh(change.id));
db.startAutoSync();
```

## Google sign-in

The package never signs in by itself. Your app runs the OAuth flow, for example with
`google_sign_in` on Android and iOS or a browser loopback on desktop, and gives Bothy a
callback that returns an access token:

- with the `drive.appdata` scope for `BothyRemoteSpec.googleDriveAppData`;
- or with the `drive.file` scope for `BothyRemoteSpec.googleDriveFolder(name)`.

Return `null` when the user is signed out. Bothy calls the callback with `forceRefresh: true`
after Drive rejects a token. Create the clients as in
[Set up Google sign-in](/start/google-cloud/); on Android you need an Android client
(package name and signing SHA-1) and a Web client.

## Encryption

```dart
final db = await Bothy.open(..., encrypt: true, password: password); // first device
final db = await Bothy.open(..., password: password);                // other devices
```

Keep `await db.exportKey()` in `flutter_secure_storage` and open with `key:` next time. A
wrong password throws `BothyException` with `BothyErrorKind.wrongPassword`. A database opened
without a secret is locked: local reads and writes work, and `sync` throws
`BothyErrorKind.keyRequired` until `unlock`.

## Background sync

`startAutoSync()` syncs while the app runs. For sync while the app is closed, schedule
`db.sync()` with WorkManager (Android) or `BGTaskScheduler` (iOS); the demo app shows both.
Direct channels only run while the app is in the foreground.

## The demo app

[`examples/flutter-notes`](https://github.com/bothy-sync/bothy/tree/main/examples/flutter-notes)
is a complete notes app: sign-in on every platform, encryption, conflicts, a Robot screen and
direct channels. Its README covers the OAuth client setup.
