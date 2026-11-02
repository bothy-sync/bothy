// Build-time settings, passed with `--dart-define-from-file=oauth.json` (see README).

/// Application id stored in the database manifest. The CLI joins the same database with
/// `bothy join --app-id dev.bothy.notes`.
const appId = 'dev.bothy.notes';

/// A visible Drive folder `Bothy/<name>` instead of the hidden app folder. Needed to share
/// a database with a headless device (the robot agent), whose device sign-in can only get
/// `drive.file`.
const driveFolder = String.fromEnvironment('BOTHY_DRIVE_FOLDER');

/// The Drive scope: the hidden per-app folder by default. Every OAuth client of the same
/// Cloud project sees the same files, so the phone, desktop and CLI share one database.
const driveScope = driveFolder == ''
    ? 'https://www.googleapis.com/auth/drive.appdata'
    : 'https://www.googleapis.com/auth/drive.file';

/// Separates the local database and key of each remote, so switching remotes never mixes
/// databases.
const storageTag = localRemote != ''
    ? '-local'
    : driveFolder != ''
    ? '-folder-$driveFolder'
    : '';

/// Direct channels to other online devices while the app is open, so their writes show up
/// within about a second. `--dart-define=BOTHY_P2P=false` turns them off.
const p2p = bool.fromEnvironment('BOTHY_P2P', defaultValue: true);

/// Acknowledges the writes of `bothy bench propagate` (collection `bench`), to measure how
/// long a write takes to get here. Only for measurements.
const benchEcho = bool.fromEnvironment('BOTHY_BENCH');

/// Identifier of the background sync task (Android WorkManager, iOS BGTaskScheduler).
const syncTaskId = 'dev.bothy.notes.sync';

/// The version shown under About; release builds set it from their tag.
const appVersion = String.fromEnvironment(
  'BOTHY_APP_VERSION',
  defaultValue: 'development build',
);

const sourceUrl = 'https://github.com/bothy-sync/bothy';
const privacyUrl = 'https://bothysync.dev/privacy/';

/// "Desktop app" OAuth client, for the browser sign-in on macOS, Linux and Windows.
const desktopClientId = String.fromEnvironment('GOOGLE_DESKTOP_CLIENT_ID');

/// Installed-app clients are not confidential; Google still requires the secret.
const desktopClientSecret = String.fromEnvironment(
  'GOOGLE_DESKTOP_CLIENT_SECRET',
);

/// "iOS" OAuth client.
const iosClientId = String.fromEnvironment('GOOGLE_IOS_CLIENT_ID');

/// "Web application" OAuth client; Android sign-in needs it as the server client id.
const webClientId = String.fromEnvironment('GOOGLE_WEB_CLIENT_ID');

/// A local folder to sync through instead of Google Drive, to try the app without OAuth.
const localRemote = String.fromEnvironment('BOTHY_LOCAL_REMOTE');
