import 'dart:async';
import 'dart:convert';
import 'dart:ffi' show DynamicLibrary;
import 'dart:io' show Platform;
import 'dart:typed_data';

import 'package:flutter_rust_bridge/flutter_rust_bridge_for_generated.dart'
    show ExternalLibrary;

import 'rust/api/bothy.dart';
import 'rust/frb_generated.dart';

/// Returns a Google OAuth access token with a Drive scope, or `null` if the user is not
/// signed in. [forceRefresh] is `true` when Drive rejected the previous token.
///
/// The package never signs in by itself: the app owns the OAuth flow (for example with
/// `google_sign_in`) and hands out tokens here.
typedef AccessTokenCallback = FutureOr<String?> Function(bool forceRefresh);

/// Where a database's shared files live.
class BothyRemoteSpec {
  const BothyRemoteSpec._(this._kind, this._location, this._tokens);

  /// A local directory: tests, or a folder that another tool keeps in sync.
  factory BothyRemoteSpec.path(String dir) =>
      BothyRemoteSpec._(BothyRemoteKind.path, dir, null);

  /// The app's hidden folder in the user's Google Drive (`drive.appdata` scope).
  factory BothyRemoteSpec.googleDriveAppData(AccessTokenCallback tokens) =>
      BothyRemoteSpec._(BothyRemoteKind.googleDriveAppData, null, tokens);

  /// A visible folder `Bothy/<name>` in the user's Google Drive (`drive.file` scope).
  factory BothyRemoteSpec.googleDriveFolder(
    String name,
    AccessTokenCallback tokens,
  ) => BothyRemoteSpec._(BothyRemoteKind.googleDriveFolder, name, tokens);

  final BothyRemoteKind _kind;
  final String? _location;
  final AccessTokenCallback? _tokens;

  Future<String> _token(bool forceRefresh) async {
    final tokens = _tokens;
    if (tokens == null) return '';
    try {
      return await tokens(forceRefresh) ?? '';
    } catch (_) {
      // Reported to the engine as "not signed in" (AuthRequired).
      return '';
    }
  }
}

/// A document and its id.
class BothyDocument {
  const BothyDocument(this.id, this.data);

  final String id;
  final Map<String, Object?> data;
}

Map<String, Object?> _decode(String json) =>
    (jsonDecode(json) as Map).cast<String, Object?>();

/// A local-first database that syncs through the user's own storage.
///
/// Reads and writes are local and never wait for the network; [sync] or [startAutoSync]
/// exchange changes with other devices. Call [Bothy.initialize] once before [Bothy.open].
class Bothy {
  Bothy._(this._db);

  final BothyDatabase _db;
  StreamController<BothyChange>? _changes;
  StreamSubscription<BothyChange>? _source;

  /// Loads the native library. Call once, before anything else.
  static Future<void> initialize() async {
    if (RustLib.instance.initialized) return;
    // On iOS and macOS the library is linked statically into the app; elsewhere (and in
    // `flutter test` on a host) it is a separate file that the default loader finds.
    final linked =
        (Platform.isIOS || Platform.isMacOS) &&
        DynamicLibrary.process().providesSymbol('frb_get_rust_content_hash');
    await RustLib.init(
      externalLibrary: linked
          ? ExternalLibrary.process(iKnowHowToUseIt: true)
          : null,
    );
  }

  /// Opens (or creates) the local database at [path].
  ///
  /// For an encrypted database pass [password] (or a [key] kept from [exportKey]); with
  /// [encrypt] a database this device creates is encrypted with [password]. Without them,
  /// an encrypted database opens locked: local reads and writes work, sync fails with
  /// [BothyErrorKind.keyRequired] until [unlock].
  ///
  /// [conflictLogLimit] is how many lost writes are kept for [conflicts]; `null` turns the
  /// log off.
  static Future<Bothy> open({
    required String path,
    required String appId,
    required BothyRemoteSpec remote,
    String? deviceName,
    bool encrypt = false,
    String? password,
    Uint8List? key,
    int? conflictLogLimit = 1000,
  }) async {
    final db = await BothyDatabase.open(
      options: BothyOptions(
        path: path,
        appId: appId,
        deviceName: deviceName,
        encrypt: encrypt,
        password: password,
        key: key,
        conflictLogLimit: conflictLogLimit,
      ),
      remote: BothyRemote(kind: remote._kind, location: remote._location),
      getToken: remote._token,
    );
    return Bothy._(db);
  }

  /// Attaches to the oldest database of this app on the remote, or creates one. Returns
  /// its id. Does nothing if already attached.
  Future<String> initOrJoin() => _db.initOrJoin();

  /// The remote database id, or `null` before [initOrJoin].
  Future<String?> dbId() => _db.dbId();

  /// This device's id.
  String get deviceId => _db.deviceId();

  /// A collection. Names match `[a-z0-9_]{1,64}`.
  BothyCollection collection(String name) => BothyCollection._(this, name);

  /// Every change to any document, made here or pulled from another device. Listeners can
  /// come and go; the stream ends on [close].
  Stream<BothyChange> watch() {
    final changes = _changes ??= StreamController<BothyChange>.broadcast();
    _source ??= _db.watch().listen(
      changes.add,
      onError: changes.addError,
      onDone: changes.close,
    );
    return changes.stream;
  }

  /// Pushes local changes and pulls remote ones.
  Future<BothySyncReport> sync() => _db.sync_();

  /// Syncs in the background after local writes and every few minutes, until
  /// [stopAutoSync]. The stream reports each outcome.
  Stream<BothySyncEvent> startAutoSync() => _db.startAutoSync();

  /// Stops background sync.
  Future<void> stopAutoSync() => _db.stopAutoSync();

  /// Local counters and the lock state.
  Future<BothyStatus> status() => _db.status();

  /// Writes from other devices that lost to a newer write, newest first.
  Future<List<BothyConflict>> conflicts({DateTime? since}) =>
      _db.conflicts(sinceMs: since?.millisecondsSinceEpoch);

  /// Deletes every conflict record. Returns how many there were.
  Future<int> clearConflicts() => _db.clearConflicts();

  /// Whether the database is end-to-end encrypted.
  Future<bool> isEncrypted() => _db.isEncrypted();

  /// Whether the database is encrypted and not unlocked.
  Future<bool> isLocked() => _db.isLocked();

  /// Unlocks with the password. Throws [BothyException] with
  /// [BothyErrorKind.wrongPassword].
  Future<void> unlock(String password) => _db.unlock(password: password);

  /// Unlocks with a key from [exportKey].
  Future<void> unlockWithKey(Uint8List key) => _db.unlockWithKey(key: key);

  /// Forgets the key in memory.
  void lock() => _db.lock();

  /// The 32-byte data key, for the app to keep in secure storage (for example
  /// `flutter_secure_storage`) so the user is not asked for the password on every start.
  /// It stays valid after a password change.
  Uint8List exportKey() => _db.exportKey();

  /// Changes the password on every device.
  Future<void> changePassword(String oldPassword, String newPassword) =>
      _db.changePassword(old: oldPassword, new_: newPassword);

  /// Stops background sync, ends [watch] streams and releases the native database.
  Future<void> close() async {
    await _db.close();
    await _changes?.close();
    _db.dispose();
  }
}

/// A set of documents, each a JSON object.
class BothyCollection {
  BothyCollection._(this._bothy, this.name);

  final Bothy _bothy;

  /// The collection name.
  final String name;

  BothyDatabase get _db => _bothy._db;

  /// Replaces the document [id]. Returns whether anything changed.
  Future<bool> put(String id, Map<String, Object?> data) =>
      _db.put(collection: name, id: id, json: jsonEncode(data));

  /// Sets the fields in [data] and removes the fields in [unset].
  Future<bool> patch(
    String id,
    Map<String, Object?> data, {
    List<String> unset = const [],
  }) =>
      _db.patch(collection: name, id: id, json: jsonEncode(data), unset: unset);

  /// The document, or `null`.
  Future<Map<String, Object?>?> get(String id) async {
    final json = await _db.get_(collection: name, id: id);
    return json == null ? null : _decode(json);
  }

  /// Up to [limit] documents in id order, after the id [after].
  Future<List<BothyDocument>> list({int limit = 100, String? after}) async {
    final rows = await _db.list(collection: name, limit: limit, after: after);
    return [for (final r in rows) BothyDocument(r.id, _decode(r.json))];
  }

  /// Deletes the document. Returns whether it existed.
  Future<bool> delete(String id) => _db.delete(collection: name, id: id);

  /// Changes to documents of this collection.
  Stream<BothyChange> watch() =>
      _bothy.watch().where((c) => c.collection == name);
}
