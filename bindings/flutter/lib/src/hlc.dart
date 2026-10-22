const int _maxPhysicalMs = 0xFFFFFFFFFFFF; // 48 bits, safe on the web (< 2^53)
const int _maxLogical = 0xFFFF;
final RegExp _hexPattern = RegExp(r'^[0-9a-f]{16}$');

/// Hybrid logical clock timestamp: 48-bit physical milliseconds + 16-bit logical counter.
///
/// Kept as two fields instead of one 64-bit `int` so it also works when compiled to
/// JavaScript, where integers are limited to 53 bits.
final class Hlc implements Comparable<Hlc> {
  /// Creates a timestamp, validating that both parts fit their bit widths.
  Hlc(this.physicalMs, this.logical) {
    if (physicalMs < 0 || physicalMs > _maxPhysicalMs) {
      throw RangeError.range(physicalMs, 0, _maxPhysicalMs, 'physicalMs');
    }
    if (logical < 0 || logical > _maxLogical) {
      throw RangeError.range(logical, 0, _maxLogical, 'logical');
    }
  }

  /// Parses the fixed-width, lowercase 16-character hex form used in file names.
  factory Hlc.parse(String text) {
    if (!_hexPattern.hasMatch(text)) {
      throw FormatException(
        'HLC must be exactly 16 lowercase hex characters',
        text,
      );
    }
    return Hlc(
      int.parse(text.substring(0, 12), radix: 16),
      int.parse(text.substring(12), radix: 16),
    );
  }

  /// Milliseconds since the Unix epoch.
  final int physicalMs;

  /// Counter that orders events within the same millisecond.
  final int logical;

  /// Fixed-width hex form; sorts like the timestamp itself.
  String toHex() =>
      physicalMs.toRadixString(16).padLeft(12, '0') +
      logical.toRadixString(16).padLeft(4, '0');

  @override
  int compareTo(Hlc other) {
    final byPhysical = physicalMs.compareTo(other.physicalMs);
    return byPhysical != 0 ? byPhysical : logical.compareTo(other.logical);
  }

  bool operator <(Hlc other) => compareTo(other) < 0;
  bool operator >(Hlc other) => compareTo(other) > 0;

  @override
  bool operator ==(Object other) =>
      other is Hlc &&
      other.physicalMs == physicalMs &&
      other.logical == logical;

  @override
  int get hashCode => Object.hash(physicalMs, logical);

  @override
  String toString() => toHex();
}
