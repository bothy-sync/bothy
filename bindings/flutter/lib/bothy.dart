/// Bothy: serverless, local-first sync over the user's own Google Drive.
///
/// Documents live in a local SQLite database; devices exchange changes through files in the
/// user's Drive, optionally end-to-end encrypted. The engine is the Rust core
/// (https://github.com/bothy-sync/bothy), bundled as a native library.
library;

export 'src/bothy.dart';
export 'src/hlc.dart';
export 'src/rust/api/bothy.dart'
    show
        BothyChange,
        BothyConflict,
        BothyErrorKind,
        BothyException,
        BothyStatus,
        BothySyncEvent,
        BothySyncReport;

/// Version of the on-remote file format (`bothy_format` in `manifest.json`).
const int bothyFormatVersion = 1;
