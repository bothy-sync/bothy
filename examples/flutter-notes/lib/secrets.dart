import 'dart:convert';
import 'dart:io';
import 'dart:typed_data';

import 'package:flutter_secure_storage/flutter_secure_storage.dart';
import 'package:path_provider/path_provider.dart';

import 'config.dart';

bool get isMobile => Platform.isAndroid || Platform.isIOS;

/// Small secrets: the database key and, on desktop, the Google refresh token. Also which
/// Google account is signed in, so a database never opens under another account.
///
/// Phones use the platform keystore (Keychain, Android Keystore). Desktop builds of this
/// demo keep a file readable only by the user, because the macOS Keychain needs a signed
/// app; a real desktop app should use the system keychain.
class SecretStore {
  static const _secure = FlutterSecureStorage();

  Future<File> _file(String name) async {
    final dir = await getApplicationSupportDirectory();
    return File('${dir.path}/secrets/$name');
  }

  Future<String?> read(String name) async {
    if (isMobile) return _secure.read(key: name);
    final f = await _file(name);
    return await f.exists() ? f.readAsString() : null;
  }

  Future<void> write(String name, String value) async {
    if (isMobile) return _secure.write(key: name, value: value);
    final f = await _file(name);
    await f.parent.create(recursive: true);
    await f.writeAsString(value, flush: true);
    if (!Platform.isWindows) {
      await Process.run('chmod', ['600', f.path]);
    }
  }

  Future<void> delete(String name) async {
    if (isMobile) return _secure.delete(key: name);
    final f = await _file(name);
    if (await f.exists()) await f.delete();
  }

  static const _accountKey = 'google_account';

  Future<String?> readAccount() => read(_accountKey);

  Future<void> writeAccount(String id) => write(_accountKey, id);

  Future<void> deleteAccount() => delete(_accountKey);

  /// Tells apart what each Google account keeps on this device. A local-folder run has no
  /// account, and [storageTag] keeps it apart from the Drive databases.
  Future<String> accountTag() async {
    if (localRemote.isNotEmpty) return storageTag;
    final account = await readAccount();
    if (account == null) throw StateError('no Google account is signed in');
    return '-$account';
  }

  Future<String> _keyName() async =>
      'database_key${(await accountTag()).replaceAll('-', '_')}';

  Future<Uint8List?> readKey() async {
    final s = await read(await _keyName());
    return s == null ? null : base64Decode(s);
  }

  Future<void> writeKey(Uint8List key) async =>
      write(await _keyName(), base64Encode(key));

  Future<void> deleteKey() async => delete(await _keyName());
}
