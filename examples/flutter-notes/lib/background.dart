import 'dart:ui';

import 'package:bothy/bothy.dart';
import 'package:flutter/widgets.dart';
import 'package:workmanager/workmanager.dart';

import 'app_state.dart';
import 'config.dart';
import 'google_auth.dart';
import 'secrets.dart';

/// Schedules a periodic sync while the app is closed (Android WorkManager, iOS
/// BGTaskScheduler). The system decides the exact time; 15 minutes is the shortest.
Future<void> scheduleBackgroundSync() async {
  if (!isMobile) return;
  await Workmanager().initialize(backgroundEntry);
  await Workmanager().registerPeriodicTask(
    syncTaskId,
    syncTaskId,
    frequency: const Duration(minutes: 15),
    constraints: Constraints(networkType: NetworkType.connected),
    existingWorkPolicy: ExistingPeriodicWorkPolicy.keep,
  );
}

@pragma('vm:entry-point')
void backgroundEntry() {
  Workmanager().executeTask((task, input) async {
    WidgetsFlutterBinding.ensureInitialized();
    DartPluginRegistrant.ensureInitialized();
    try {
      await _syncOnce();
    } on BothyException catch (e) {
      // The app is open and holds the database: it syncs by itself.
      if (e.kind == BothyErrorKind.databaseLocked) return true;
      return e.kind != BothyErrorKind.offline ? true : false;
    }
    return true;
  });
}

/// One sync with the remembered key. Without one there is nothing to do: an encrypted
/// database needs the user to unlock it first.
Future<void> _syncOnce() async {
  await Bothy.initialize();
  final secrets = SecretStore();
  final key = await secrets.readKey();
  final auth = GoogleAuth(secrets);
  if (localRemote.isEmpty && !await auth.restore()) return;
  final db = await Bothy.open(
    path: await AppState.databasePath(),
    appId: appId,
    remote: await AppState.remoteFor(auth),
    key: key,
  );
  try {
    if (await db.dbId() != null && !await db.isLocked()) await db.sync();
  } finally {
    await db.close();
  }
}
