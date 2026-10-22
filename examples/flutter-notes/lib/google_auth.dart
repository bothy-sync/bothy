import 'dart:async';
import 'dart:convert';
import 'dart:io';
import 'dart:math';

import 'package:crypto/crypto.dart';
import 'package:google_sign_in/google_sign_in.dart';
import 'package:http/http.dart' as http;
import 'package:url_launcher/url_launcher.dart';

import 'config.dart';
import 'secrets.dart';

/// Google sign-in for the Drive app folder. Bothy itself never signs in: it asks this
/// object for access tokens.
abstract class GoogleAuth {
  factory GoogleAuth(SecretStore secrets) =>
      isMobile ? _MobileAuth() : _DesktopAuth(secrets);

  /// Signs in silently with what is stored. Returns whether that worked.
  Future<bool> restore();

  /// Interactive sign-in.
  Future<void> signIn();

  /// A Drive access token, or `null` when signed out.
  Future<String?> token(bool forceRefresh);

  Future<void> signOut();
}

/// Android and iOS: the platform account picker (`google_sign_in`).
class _MobileAuth implements GoogleAuth {
  static Future<void>? _initialized;
  GoogleSignInAccount? _user;
  String? _token;

  Future<void> _init() => _initialized ??= GoogleSignIn.instance.initialize(
    clientId: Platform.isIOS && iosClientId.isNotEmpty ? iosClientId : null,
    serverClientId: Platform.isAndroid && webClientId.isNotEmpty
        ? webClientId
        : null,
  );

  @override
  Future<bool> restore() async {
    await _init();
    _user = await GoogleSignIn.instance.attemptLightweightAuthentication();
    return await token(false) != null;
  }

  @override
  Future<void> signIn() async {
    await _init();
    _user = await GoogleSignIn.instance.authenticate(scopeHint: [driveScope]);
    final auth = await _user!.authorizationClient.authorizeScopes([driveScope]);
    _token = auth.accessToken;
  }

  @override
  Future<String?> token(bool forceRefresh) async {
    if (!forceRefresh && _token != null) return _token;
    final auth = await _user?.authorizationClient.authorizationForScopes([
      driveScope,
    ]);
    return _token = auth?.accessToken;
  }

  @override
  Future<void> signOut() async {
    await _init();
    await GoogleSignIn.instance.signOut();
    _user = null;
    _token = null;
  }
}

/// macOS, Linux, Windows: the browser, with a loopback redirect and PKCE (RFC 8252).
class _DesktopAuth implements GoogleAuth {
  _DesktopAuth(this._secrets);

  final SecretStore _secrets;
  String? _refreshToken;
  String? _token;
  DateTime _expires = DateTime.fromMillisecondsSinceEpoch(0);

  static const _refreshKey = 'google_refresh_token';

  @override
  Future<bool> restore() async {
    _refreshToken = await _secrets.read(_refreshKey);
    if (_refreshToken == null) return false;
    try {
      return await token(true) != null;
    } on SocketException {
      // Offline: stay signed in; sync reports the network error.
      return true;
    }
  }

  @override
  Future<void> signIn() async {
    if (desktopClientId.isEmpty) {
      throw StateError('GOOGLE_DESKTOP_CLIENT_ID is not set; see the README');
    }
    final server = await HttpServer.bind(InternetAddress.loopbackIPv4, 0);
    final redirect = 'http://127.0.0.1:${server.port}';
    final verifier = _random(64);
    final state = _random(24);
    final challenge = base64Url
        .encode(sha256.convert(ascii.encode(verifier)).bytes)
        .replaceAll('=', '');
    final url = Uri.https('accounts.google.com', '/o/oauth2/v2/auth', {
      'client_id': desktopClientId,
      'redirect_uri': redirect,
      'response_type': 'code',
      'scope': driveScope,
      'code_challenge': challenge,
      'code_challenge_method': 'S256',
      'state': state,
      'access_type': 'offline',
      'prompt': 'consent',
    });
    try {
      await launchUrl(url, mode: LaunchMode.externalApplication);
      final request = await server.first.timeout(const Duration(minutes: 5));
      final q = request.uri.queryParameters;
      request.response
        ..headers.contentType = ContentType.html
        ..write(
          '<p>Signed in. You can close this tab and return to the app.</p>',
        );
      await request.response.close();
      if (q['state'] != state || q['code'] == null) {
        throw StateError('sign-in was cancelled (${q['error'] ?? 'no code'})');
      }
      final body = await _post({
        'grant_type': 'authorization_code',
        'code': q['code']!,
        'redirect_uri': redirect,
        'code_verifier': verifier,
      });
      _refreshToken = body['refresh_token'] as String?;
      if (_refreshToken == null) {
        throw StateError('Google returned no refresh token');
      }
      await _secrets.write(_refreshKey, _refreshToken!);
      _remember(body);
    } finally {
      await server.close(force: true);
    }
  }

  @override
  Future<String?> token(bool forceRefresh) async {
    final refresh = _refreshToken;
    if (refresh == null) return null;
    if (!forceRefresh && _token != null && DateTime.now().isBefore(_expires)) {
      return _token;
    }
    final body = await _post({
      'grant_type': 'refresh_token',
      'refresh_token': refresh,
    });
    _remember(body);
    return _token;
  }

  @override
  Future<void> signOut() async {
    final refresh = _refreshToken;
    _refreshToken = _token = null;
    await _secrets.delete(_refreshKey);
    if (refresh != null) {
      await http
          .post(
            Uri.https('oauth2.googleapis.com', '/revoke'),
            body: {'token': refresh},
          )
          .catchError((_) => http.Response('', 200));
    }
  }

  void _remember(Map<String, Object?> body) {
    _token = body['access_token'] as String?;
    final ttl = (body['expires_in'] as num?)?.toInt() ?? 3600;
    // Refresh a minute early.
    _expires = DateTime.now().add(Duration(seconds: ttl - 60));
  }

  Future<Map<String, Object?>> _post(Map<String, String> form) async {
    final res = await http.post(
      Uri.https('oauth2.googleapis.com', '/token'),
      body: {
        ...form,
        'client_id': desktopClientId,
        'client_secret': desktopClientSecret,
      },
    );
    final body = (jsonDecode(res.body) as Map).cast<String, Object?>();
    if (res.statusCode != 200) {
      // Never include tokens in the message; Google's error code is enough.
      throw StateError(
        'Google sign-in failed: ${body['error'] ?? res.statusCode}',
      );
    }
    return body;
  }

  static String _random(int length) {
    const chars =
        'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~';
    final r = Random.secure();
    return String.fromCharCodes(
      List.generate(length, (_) => chars.codeUnitAt(r.nextInt(chars.length))),
    );
  }
}
