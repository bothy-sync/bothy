import 'dart:async';
import 'dart:io';

import 'package:bothy/bothy.dart';
import 'package:flutter/foundation.dart';
import 'package:path_provider/path_provider.dart';

import 'config.dart';
import 'google_auth.dart';
import 'secrets.dart';

enum Stage { loading, signIn, password, notes }

/// Where the app is: signed in, unlocked, and the open database.
class AppState extends ChangeNotifier {
  AppState() : auth = GoogleAuth(SecretStore());

  final secrets = SecretStore();
  final GoogleAuth auth;

  Stage stage = Stage.loading;
  Bothy? db;
  String? error;

  /// `true` when the password screen creates or joins a database, `false` when it unlocks
  /// one this device already has.
  bool firstSetup = true;

  /// Latest background sync outcome, for the status line.
  BothySyncEvent? lastSync;
  bool syncing = false;
  StreamSubscription<BothySyncEvent>? _autoSync;

  /// Direct channels, for the status line.
  BothyP2pStatus? p2pStatus;
  Timer? _p2pTimer;
  StreamSubscription<BothyChange>? _bench;

  /// Each Google account gets its own file, so another account on this device neither sees
  /// the notes nor syncs them into its Drive. A local-folder run gets its own file too.
  static Future<String> databasePath() async =>
      '${(await getApplicationSupportDirectory()).path}/'
      'notes${await SecretStore().accountTag()}.db';

  /// The folder for `BOTHY_LOCAL_REMOTE`; a relative path is inside the app's support
  /// directory (sandboxed macOS apps cannot write elsewhere).
  static Future<String> localRemoteDir() async =>
      localRemote.startsWith('/') || localRemote.contains(':')
      ? localRemote
      : '${(await getApplicationSupportDirectory()).path}/$localRemote';

  static Future<BothyRemoteSpec> remoteFor(GoogleAuth auth) async =>
      localRemote.isNotEmpty
      ? BothyRemoteSpec.path(await localRemoteDir())
      : driveFolder.isNotEmpty
      ? BothyRemoteSpec.googleDriveFolder(driveFolder, auth.token)
      : BothyRemoteSpec.googleDriveAppData(auth.token);

  Future<void> start() => _guard(() async {
    if (localRemote.isEmpty && !await auth.restore()) {
      stage = Stage.signIn;
      return;
    }
    if (localRemote.isEmpty) await _identify();
    await _open();
  });

  Future<void> signIn() => _guard(() async {
    await auth.signIn();
    String account;
    try {
      account = await driveAccountId(auth);
    } on SignInExpired {
      // The platform handed out a dead token and has now dropped it: this time it asks.
      await auth.signIn();
      account = await driveAccountId(auth);
    }
    await secrets.writeAccount(account);
    await _open();
  });

  /// Rechecks who is signed in. Offline, the account from the last sign-in stands.
  Future<void> _identify() async {
    try {
      await secrets.writeAccount(await driveAccountId(auth));
    } on SocketException {
      if (await secrets.readAccount() == null) rethrow;
    }
  }

  Future<void> signOut() => _guard(() async {
    await _close();
    await auth.signOut();
    await secrets.deleteAccount();
    stage = Stage.signIn;
  });

  Future<void> _open({String? password, bool encrypt = false}) async {
    await _close();
    final key = password == null ? await secrets.readKey() : null;
    final db = await Bothy.open(
      path: await databasePath(),
      appId: appId,
      remote: await remoteFor(auth),
      deviceName: _deviceName(),
      encrypt: encrypt,
      password: password,
      key: key,
      p2p: p2p,
    );
    this.db = db;
    if (await db.dbId() == null) {
      if (password == null) {
        // New on this device: ask for the password to create or join the database.
        firstSetup = true;
        stage = Stage.password;
        return;
      }
      await db.initOrJoin();
    }
    if (await db.isLocked()) {
      firstSetup = false;
      stage = Stage.password;
      return;
    }
    _startSync();
    stage = Stage.notes;
  }

  /// Creates or joins the database with [password], or unlocks this device's copy.
  Future<void> submitPassword(String password, {required bool remember}) =>
      _guard(() async {
        if (firstSetup) {
          // Encrypts a database this device creates; an existing one is joined as it is.
          await _open(password: password, encrypt: true);
        } else {
          await db!.unlock(password);
          _startSync();
          stage = Stage.notes;
        }
        if (remember && await db!.isEncrypted()) {
          await secrets.writeKey(db!.exportKey());
        }
      });

  /// Locks and forgets the remembered key.
  Future<void> lock() => _guard(() async {
    await secrets.deleteKey();
    await _autoSync?.cancel();
    await db?.stopAutoSync();
    db?.lock();
    firstSetup = false;
    stage = Stage.password;
  });

  Future<void> syncNow() => _guard(() async {
    syncing = true;
    notifyListeners();
    try {
      final report = await db!.sync();
      lastSync = BothySyncEvent(report: report);
    } finally {
      syncing = false;
    }
  });

  void _startSync() {
    _autoSync?.cancel();
    _autoSync = db!.startAutoSync().listen((e) {
      lastSync = e;
      notifyListeners();
    });
    _p2pTimer?.cancel();
    if (p2p) {
      _p2pTimer = Timer.periodic(const Duration(seconds: 1), (_) {
        final now = db?.p2pStatus();
        if (_p2pKey(now) != _p2pKey(p2pStatus)) {
          p2pStatus = now;
          notifyListeners();
        }
      });
    }
    if (benchEcho) _startBenchEcho();
    // A first sync right away, so a newly opened app shows fresh notes.
    unawaited(syncNow());
  }

  static String _p2pKey(BothyP2pStatus? s) => s == null
      ? ''
      : '${s.running} ${s.discovering} '
            '${s.peers.map((p) => '${p.deviceId}=${p.state}').join(',')}';

  /// Answers `bothy bench propagate`: for each `p…` write from another device, writes `a…`
  /// with the time it arrived here.
  void _startBenchEcho() {
    _bench?.cancel();
    final bench = db!.collection('bench');
    _bench = bench.watch().listen((c) async {
      if (!c.remote) return;
      if (c.id.startsWith('s')) {
        // `bench propagate --reverse` asks for timed writes from this device.
        final run = c.id.substring(1);
        final doc = await bench.get(c.id);
        final count = ((doc?['count'] as num?) ?? 0).toInt().clamp(0, 10000);
        final pause = Duration(
          milliseconds: ((doc?['interval_ms'] as num?) ?? 1000).toInt(),
        );
        for (var i = 0; i < count; i++) {
          await bench.put('q$run-${i.toString().padLeft(4, '0')}', {
            'sent_ms': DateTime.now().millisecondsSinceEpoch,
          });
          await Future<void>.delayed(pause);
        }
        return;
      }
      if (!c.id.startsWith('p')) return;
      final recv = DateTime.now().millisecondsSinceEpoch;
      final doc = await bench.get(c.id);
      if (doc == null) return;
      await bench.put('a${c.id.substring(1)}', {
        'sent_ms': doc['sent_ms'],
        'recv_ms': recv,
      });
    });
  }

  Future<void> _close() async {
    _p2pTimer?.cancel();
    _p2pTimer = null;
    await _bench?.cancel();
    _bench = null;
    await _autoSync?.cancel();
    _autoSync = null;
    final db = this.db;
    this.db = null;
    await db?.close();
  }

  Future<void> _guard(Future<void> Function() action) async {
    error = null;
    try {
      await action();
    } on BothyException catch (e) {
      error = switch (e.kind) {
        BothyErrorKind.wrongPassword => 'Wrong password.',
        BothyErrorKind.offline => 'Offline. Changes are kept and sync later.',
        BothyErrorKind.authRequired => 'Sign in to Google again.',
        _ => e.message,
      };
    } catch (e) {
      error = '$e';
    }
    if (stage == Stage.loading && db == null && error != null) {
      stage = localRemote.isEmpty ? Stage.signIn : Stage.loading;
    }
    notifyListeners();
  }

  static String _deviceName() => switch (defaultTargetPlatform) {
    TargetPlatform.android => 'Android',
    TargetPlatform.iOS => 'iPhone',
    TargetPlatform.macOS => 'Mac',
    TargetPlatform.windows => 'Windows',
    TargetPlatform.linux => 'Linux',
    _ => 'device',
  };
}
