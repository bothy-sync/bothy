// Drives the app against a local folder instead of Google Drive:
//   flutter test integration_test -d macos --dart-define=BOTHY_LOCAL_REMOTE=it-remote
import 'dart:io';

import 'package:bothy_notes/app_state.dart';
import 'package:bothy_notes/config.dart';
import 'package:bothy_notes/main.dart' as app;
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:integration_test/integration_test.dart';

void main() {
  IntegrationTestWidgetsFlutterBinding.ensureInitialized();

  testWidgets('set a password, write a note, find it after a restart', (
    tester,
  ) async {
    expect(
      localRemote,
      isNotEmpty,
      reason: 'pass --dart-define=BOTHY_LOCAL_REMOTE=<dir>',
    );
    final remote = Directory(await AppState.localRemoteDir());
    if (remote.existsSync()) remote.deleteSync(recursive: true);
    final db = File(await AppState.databasePath());
    for (final f in [db, File('${db.path}-wal'), File('${db.path}-shm')]) {
      if (f.existsSync()) f.deleteSync();
    }
    await AppState().secrets.deleteKey();

    await app.main();
    await tester.pumpAndSettle();
    expect(find.text('Set up'), findsOneWidget);

    await tester.enterText(find.byType(TextField), 'correct horse');
    await tester.tap(find.text('Continue'));
    await tester.pumpAndSettle(const Duration(seconds: 2));
    expect(find.text('No notes yet'), findsOneWidget);

    await tester.tap(find.byTooltip('New note'));
    await tester.pumpAndSettle();
    await tester.enterText(find.byType(TextField).first, 'Groceries');
    await tester.enterText(find.byType(TextField).last, 'SECRET-MARKER milk');
    await tester.pageBack();
    await tester.pumpAndSettle(const Duration(seconds: 1));
    expect(find.text('Groceries'), findsOneWidget);

    await tester.tap(find.byTooltip('Sync now'));
    await tester.pumpAndSettle(const Duration(seconds: 2));
    expect(find.textContaining('Synced at'), findsOneWidget);

    // Nothing readable on the remote.
    for (final f in remote.listSync(recursive: true).whereType<File>()) {
      expect(
        String.fromCharCodes(f.readAsBytesSync()).contains('SECRET-MARKER'),
        isFalse,
        reason: f.path,
      );
    }
  });
}
