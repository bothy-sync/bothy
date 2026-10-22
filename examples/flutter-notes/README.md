# Bothy Notes (Flutter demo)

A notes app on top of the [`bothy`](../../bindings/flutter) Flutter package. It covers:

- Notes stay in a local database and sync through a hidden folder in your own Google Drive
  (`drive.appdata`). There is no server.
- Notes are end-to-end encrypted with a password. "Remember on this device" keeps the
  database key in the platform keystore.
- Edits to different fields merge. Edits that lose to a newer edit of the same field show
  up under **Conflicts**.
- Sync runs while the app is open. On Android (WorkManager) and iOS (BGTaskScheduler) it
  also runs in the background, about every 15 minutes, when the key is remembered.
- It runs on Android, iOS, macOS, Linux and Windows.

The [CLI](../../crates/bothy-cli) can open the same database:

```sh
bothy join --app-id dev.bothy.notes --remember   # asks for the same password
bothy list notes
```

## Try it without Google

This syncs through a local folder instead of Drive. A relative path is resolved inside the
app's support directory.

```sh
flutter run -d macos --dart-define=BOTHY_LOCAL_REMOTE=shared
```

## Set up Google sign-in

Every OAuth client of **one** Google Cloud project sees the same `drive.appdata` folder, so
create all clients in the same project.

1. Create a project and enable the **Google Drive API**.
2. Set up the **OAuth consent screen**:
   - user type *External*;
   - add the scope `.../auth/drive.appdata`;
   - while the app is in *Testing*, add yourself under *Test users*.
3. Under **Credentials → Create credentials → OAuth client ID**, create these clients:

   | Client | Type | Settings | Used by |
   |---|---|---|---|
   | Desktop | Desktop app | (none) | macOS, Linux, Windows, and the CLI |
   | Android | Android | package `dev.bothy.notes` and the SHA-1 of your signing key | Android |
   | Web | Web application | no redirect URIs | Android (as the server client id) |
   | iOS | iOS | bundle ID `dev.bothy.notes` | iOS |

   The debug signing key's SHA-1:

   ```sh
   keytool -list -v -keystore ~/.android/debug.keystore -alias androiddebugkey -storepass android | grep SHA1
   ```

4. Write the settings for the app. The two output files are git-ignored.

   ```sh
   dart run tool/configure.dart \
     --desktop path/to/client_secret_desktop.json \
     --ios-client-id 123-ios.apps.googleusercontent.com \
     --web-client-id 123-web.apps.googleusercontent.com
   ```

   This writes `oauth.json`, plus `ios/Flutter/OAuth.xcconfig` for the iOS URL scheme.

5. Run:

   ```sh
   flutter run -d macos --dart-define-from-file=oauth.json
   flutter run -d emulator-5554 --dart-define-from-file=oauth.json
   flutter run -d "iPhone 17" --dart-define-from-file=oauth.json
   ```

On desktop, the app opens the browser and receives the code on a `127.0.0.1` redirect,
using PKCE. The refresh token is stored in the app's support directory, readable only by
you.

> **Note:** a real app should keep the token in the system keychain instead. This demo
> doesn't, because the macOS Keychain requires a signed app.

On phones, `google_sign_in` uses the system account picker.

## Tests

```sh
flutter test integration_test -d macos --dart-define=BOTHY_LOCAL_REMOTE=it-remote
```

The test sets a password, writes a note and syncs it. Then it checks that no remote file
contains the note's text.
