// Build-time settings, passed with `--dart-define-from-file=oauth.json` (see README).

/// Application id stored in the database manifest. The CLI joins the same database with
/// `bothy join --app-id dev.bothy.notes`.
const appId = 'dev.bothy.notes';

/// Google Drive's hidden per-app folder. Every OAuth client of the same Cloud project sees
/// the same folder, so the phone, desktop and CLI share one database.
const driveScope = 'https://www.googleapis.com/auth/drive.appdata';

/// Identifier of the background sync task (Android WorkManager, iOS BGTaskScheduler).
const syncTaskId = 'dev.bothy.notes.sync';

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
