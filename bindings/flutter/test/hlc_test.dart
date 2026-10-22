import 'package:bothy/bothy.dart';
import 'package:flutter_test/flutter_test.dart';

const t = 1700000000000;

void main() {
  test('matches the Rust test vector', () {
    expect(Hlc(t, 1).toHex(), '018bcfe568000001');
    expect(Hlc.parse('018bcfe568000001'), Hlc(t, 1));
  });

  test('hex ordering matches timestamp ordering', () {
    final a = Hlc(t, 1);
    final b = Hlc(t + 1, 0);
    expect(a < b, isTrue);
    expect(a.toHex().compareTo(b.toHex()), lessThan(0));
  });

  test('rejects malformed hex', () {
    for (final s in [
      '',
      '018bcfe56800000',
      '018bcfe5680000011',
      '018BCFE568000001',
      '018bcfe56800000g',
    ]) {
      expect(() => Hlc.parse(s), throwsFormatException, reason: s);
    }
  });

  test('rejects out-of-range parts', () {
    expect(() => Hlc(1 << 48, 0), throwsRangeError);
    expect(() => Hlc(t, 1 << 16), throwsRangeError);
  });

  test('format version', () {
    expect(bothyFormatVersion, 1);
  });
}
