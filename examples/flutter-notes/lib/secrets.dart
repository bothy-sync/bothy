import 'dart:convert';
import 'dart:io';
import 'dart:typed_data';

import 'package:flutter_secure_storage/flutter_secure_storage.dart';
import 'package:path_provider/path_provider.dart';

import 'config.dart';

bool get isMobile => Platform.isAndroid || Platform.isIOS;

/// Small secrets: the database key and, on desktop, the Google refresh token.
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

  /// A local-folder run keeps its key apart from the Drive database's.
  static String get _keyName =>
      localRemote.isEmpty ? 'database_key' : 'database_key_local';

  Future<Uint8List?> readKey() async {
    final s = await read(_keyName);
    return s == null ? null : base64Decode(s);
  }

  Future<void> writeKey(Uint8List key) => write(_keyName, base64Encode(key));

  Future<void> deleteKey() => delete(_keyName);
}
