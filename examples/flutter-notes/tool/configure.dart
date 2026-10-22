// Writes oauth.json (for --dart-define-from-file) and ios/Flutter/OAuth.xcconfig from your
// OAuth clients. Both files are git-ignored.
//
//   dart run tool/configure.dart --desktop client_secret_xxx.json \
//       --ios-client-id 123-abc.apps.googleusercontent.com \
//       --web-client-id 123-def.apps.googleusercontent.com
import 'dart:convert';
import 'dart:io';

void main(List<String> args) {
  String? arg(String name) {
    final i = args.indexOf('--$name');
    return i >= 0 && i + 1 < args.length ? args[i + 1] : null;
  }

  final defines = <String, String>{};
  final desktop = arg('desktop');
  if (desktop != null) {
    final json = jsonDecode(File(desktop).readAsStringSync()) as Map;
    final installed = (json['installed'] ?? json['web']) as Map;
    defines['GOOGLE_DESKTOP_CLIENT_ID'] = installed['client_id'] as String;
    defines['GOOGLE_DESKTOP_CLIENT_SECRET'] =
        installed['client_secret'] as String;
  }
  final ios = arg('ios-client-id');
  if (ios != null) defines['GOOGLE_IOS_CLIENT_ID'] = ios;
  final web = arg('web-client-id');
  if (web != null) defines['GOOGLE_WEB_CLIENT_ID'] = web;
  final local = arg('local-remote');
  if (local != null) defines['BOTHY_LOCAL_REMOTE'] = local;
  if (defines.isEmpty) {
    stderr.writeln(
      'usage: dart run tool/configure.dart [--desktop client.json] '
      '[--ios-client-id ID] [--web-client-id ID] [--local-remote DIR]',
    );
    exit(2);
  }

  final out = File('oauth.json');
  out.writeAsStringSync(const JsonEncoder.withIndent('  ').convert(defines));
  if (!Platform.isWindows) Process.runSync('chmod', ['600', out.path]);
  stdout.writeln('wrote oauth.json');

  if (ios != null) {
    // The iOS sign-in redirect uses the client id reversed as a URL scheme.
    final reversed = ios.split('.').reversed.join('.');
    File('ios/Flutter/OAuth.xcconfig')
        .writeAsStringSync('GOOGLE_REVERSED_CLIENT_ID=$reversed\n');
    stdout.writeln('wrote ios/Flutter/OAuth.xcconfig');
  }
}
