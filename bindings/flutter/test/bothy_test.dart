// Runs the native engine on the host against a path remote. Build it first:
// `cargo build --release --manifest-path rust/Cargo.toml`.
import 'dart:io';
import 'dart:typed_data';

import 'package:bothy/bothy.dart';
import 'package:flutter_test/flutter_test.dart';

const appId = 'dev.bothy.test';

void main() {
  late Directory dir;
  late BothyRemoteSpec remote;

  setUpAll(Bothy.initialize);

  setUp(() {
    dir = Directory.systemTemp.createTempSync('bothy_test');
    remote = BothyRemoteSpec.path('${dir.path}/remote');
  });

  tearDown(() => dir.deleteSync(recursive: true));

  Future<Bothy> device(
    String name, {
    bool encrypt = false,
    String? password,
    List<int>? key,
  }) => Bothy.open(
    path: '${dir.path}/$name.db',
    appId: appId,
    remote: remote,
    deviceName: name,
    encrypt: encrypt,
    password: password,
    key: key == null ? null : Uint8List.fromList(key),
  );

  test('two devices sync documents, deletes and changes', () async {
    final a = await device('a');
    final b = await device('b');
    final dbId = await a.initOrJoin();
    expect(await b.initOrJoin(), dbId);

    final notes = a.collection('notes');
    await notes.put('n1', {
      'title': 'hello',
      'n': 1,
      'x': 1.5,
      'tags': ['a'],
    });
    await notes.put('n2', {'title': 'bye'});
    await a.sync();

    final changes = <BothyChange>[];
    final sub = b.collection('notes').watch().listen(changes.add);
    final report = await b.sync();
    expect(report.changedDocs, 2);
    expect(await b.collection('notes').get('n1'), {
      'title': 'hello',
      'n': 1,
      'x': 1.5,
      'tags': ['a'],
    });
    await Future<void>.delayed(const Duration(milliseconds: 50));
    expect(changes.where((c) => c.remote).map((c) => c.id).toSet(), {
      'n1',
      'n2',
    });
    await sub.cancel();

    await b.collection('notes').patch('n1', {'done': true}, unset: ['n']);
    expect(await b.collection('notes').delete('n2'), isTrue);
    await b.sync();
    await a.sync();
    final rows = await notes.list();
    expect(rows.map((r) => r.id), ['n1']);
    expect(rows.single.data, {
      'title': 'hello',
      'x': 1.5,
      'tags': ['a'],
      'done': true,
    });

    final st = await a.status();
    expect(st.documents, 1);
    expect(st.encrypted, isFalse);
    await a.close();
    await b.close();
  });

  test('a lost write shows up in conflicts until cleared', () async {
    final a = await device('a');
    final b = await device('b');
    await a.initOrJoin();
    await b.initOrJoin();
    await a.collection('notes').put('n', {'v': 'first'});
    await Future<void>.delayed(const Duration(milliseconds: 5));
    await b.collection('notes').put('n', {'v': 'second'});
    await a.sync();
    await b.sync();
    await a.sync();
    expect(await a.collection('notes').get('n'), {'v': 'second'});

    final lost = await a.conflicts();
    expect(lost, hasLength(1));
    expect(lost.single.id, 'n');
    expect(lost.single.loserValueJson, '"first"');
    expect(
      await a.conflicts(since: DateTime.now().add(const Duration(hours: 1))),
      isEmpty,
    );
    expect(await a.clearConflicts(), 1);
    expect(await a.conflicts(), isEmpty);
    await a.close();
    await b.close();
  });

  test('encrypted databases need the password or a kept key', () async {
    final a = await device('a', encrypt: true, password: 'pw');
    await a.initOrJoin();
    await a.collection('notes').put('n', {'secret': 'SECRET-MARKER'});
    await a.sync();
    expect(await a.isEncrypted(), isTrue);
    final key = a.exportKey();
    expect(key, hasLength(32));

    final wrong = await device('b', password: 'nope');
    await expectLater(
      wrong.initOrJoin(),
      throwsA(
        isA<BothyException>().having(
          (e) => e.kind,
          'kind',
          BothyErrorKind.wrongPassword,
        ),
      ),
    );
    await wrong.close();

    final b = await device('c', password: 'pw');
    await b.initOrJoin();
    expect(await b.collection('notes').get('n'), {'secret': 'SECRET-MARKER'});
    await b.close();

    // Reopened without a secret: locked until unlocked with the kept key.
    await a.close();
    final again = await device('a');
    expect(await again.isLocked(), isTrue);
    await expectLater(
      again.sync(),
      throwsA(
        isA<BothyException>().having(
          (e) => e.kind,
          'kind',
          BothyErrorKind.keyRequired,
        ),
      ),
    );
    await again.unlockWithKey(key);
    await again.sync();
    await again.close();

    for (final f in Directory('${dir.path}/remote').listSync(recursive: true)) {
      if (f is File) {
        expect(
          String.fromCharCodes(f.readAsBytesSync()).contains('SECRET-MARKER'),
          isFalse,
          reason: f.path,
        );
      }
    }
  });
}
