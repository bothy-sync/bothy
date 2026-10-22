# bothy (Flutter)

**Serverless, local-first sync engine that uses the user's own Google Drive as the transport.**

> ⚠️ **Early development.** The API may still change between minor versions.

Bothy lets Flutter apps sync structured data between phones, desktops and embedded devices
without a server of their own. Every device keeps a local SQLite database and works offline;
devices exchange immutable files through the user's own Google Drive. Concurrent edits to
different fields merge automatically, and the files on Drive can be end-to-end encrypted.

The engine is the [Rust core](https://github.com/bothy-sync/bothy), bundled as a native
library. Building an app that uses this package needs a Rust toolchain
([rustup](https://rustup.rs)); the native code is compiled for each platform during the
Flutter build. Supported: Android, iOS, macOS, Linux and Windows.

## Usage

```dart
import 'package:bothy/bothy.dart';

await Bothy.initialize();
final db = await Bothy.open(
  path: '${supportDir.path}/notes.db',
  appId: 'com.example.notes',
  remote: BothyRemoteSpec.googleDriveAppData(
    (forceRefresh) => mySignIn.accessToken(refresh: forceRefresh),
  ),
);
await db.initOrJoin();

final notes = db.collection('notes');
await notes.put('n1', {'title': 'Hello', 'done': false});
await notes.patch('n1', {'done': true});
final note = await notes.get('n1');

notes.watch().listen((change) => refresh(change.id));
db.startAutoSync();
```

### Google sign-in

The package never signs in by itself. The app runs the OAuth flow (for example with
`google_sign_in`) and gives Bothy a callback that returns an access token with the
`https://www.googleapis.com/auth/drive.appdata` scope (or `drive.file` for
`BothyRemoteSpec.googleDriveFolder`), or `null` when the user is signed out. Bothy calls it
with `forceRefresh: true` after Drive rejects a token.

### Encryption

```dart
final db = await Bothy.open(..., encrypt: true, password: password); // first device
final db = await Bothy.open(..., password: password);                // other devices
```

Keep `db.exportKey()` in secure storage (for example `flutter_secure_storage`) and open with
`key:` next time, so the user is not asked for the password on every start. A wrong password
throws `BothyException` with `BothyErrorKind.wrongPassword`; an encrypted database opened
without a secret is locked: local reads and writes work, `sync` throws
`BothyErrorKind.keyRequired` until `unlock`. `changePassword` changes it on every device.

### Conflicts

Writes that lose to a newer write from another device are kept in a log:
`db.conflicts(since: ...)` and `db.clearConflicts()`.

## Testing

`flutter test` runs the engine on the host. Build the native library first:

```sh
cargo build --release --manifest-path rust/Cargo.toml
flutter test
```

## License

MIT OR Apache-2.0 (see `LICENSE`).
