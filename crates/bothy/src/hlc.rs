//! Hybrid logical clock: 48-bit physical milliseconds + 16-bit logical counter.

use std::fmt;
use std::str::FromStr;

const LOGICAL_BITS: u32 = 16;
const LOGICAL_MASK: u64 = (1 << LOGICAL_BITS) - 1;
const MAX_PHYSICAL_MS: u64 = (1 << (64 - LOGICAL_BITS)) - 1;

/// Default limit on how far a remote timestamp may pull the local clock ahead of the wall clock.
pub const DEFAULT_MAX_DRIFT_MS: u64 = 60 * 60 * 1000;

/// A hybrid logical clock timestamp.
///
/// Ordered by physical time, then by logical counter. Ties between devices are broken by the
/// caller (Bothy compares `(Hlc, device_id)`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Hlc(u64);

impl Hlc {
    /// The zero timestamp, smaller than any timestamp a clock produces.
    pub const ZERO: Hlc = Hlc(0);

    /// Builds a timestamp from its parts. Returns `None` if `physical_ms` exceeds 48 bits.
    pub const fn from_parts(physical_ms: u64, logical: u16) -> Option<Hlc> {
        if physical_ms > MAX_PHYSICAL_MS {
            return None;
        }
        Some(Hlc((physical_ms << LOGICAL_BITS) | logical as u64))
    }

    /// Wraps a raw 64-bit value as stored on the wire.
    pub const fn from_u64(raw: u64) -> Hlc {
        Hlc(raw)
    }

    /// Raw 64-bit value as stored on the wire.
    pub const fn as_u64(self) -> u64 {
        self.0
    }

    /// Physical component in milliseconds since the Unix epoch.
    pub const fn physical_ms(self) -> u64 {
        self.0 >> LOGICAL_BITS
    }

    /// Logical counter component.
    pub const fn logical(self) -> u16 {
        (self.0 & LOGICAL_MASK) as u16
    }

    /// Fixed-width, lowercase hex form (16 characters); sorts like the timestamp itself.
    pub fn to_hex(self) -> String {
        format!("{:016x}", self.0)
    }
}

impl fmt::Display for Hlc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

/// Error returned when parsing an [`Hlc`] from its hex form fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseHlcError;

impl fmt::Display for ParseHlcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("HLC must be exactly 16 lowercase hex characters")
    }
}

impl std::error::Error for ParseHlcError {}

impl FromStr for Hlc {
    type Err = ParseHlcError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let valid = s.len() == 16 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        if !valid {
            return Err(ParseHlcError);
        }
        u64::from_str_radix(s, 16)
            .map(Hlc)
            .map_err(|_| ParseHlcError)
    }
}

/// Wall-clock source in milliseconds since the Unix epoch. Tests inject a fake one.
pub trait Clock: Send + Sync {
    /// Current wall-clock time in milliseconds.
    fn now_ms(&self) -> u64;
}

/// The operating system clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64)
    }
}

/// Produces monotonically increasing [`Hlc`] timestamps for one device.
///
/// The wall clock is passed in explicitly so callers (and tests) control the time source.
#[derive(Clone, Debug)]
pub struct HlcClock {
    last: Hlc,
    max_drift_ms: u64,
}

impl Default for HlcClock {
    fn default() -> Self {
        Self::new()
    }
}

impl HlcClock {
    /// A clock starting at [`Hlc::ZERO`] with [`DEFAULT_MAX_DRIFT_MS`].
    pub fn new() -> Self {
        Self::resume(Hlc::ZERO)
    }

    /// A clock that continues after a previously persisted timestamp.
    pub fn resume(last: Hlc) -> Self {
        Self {
            last,
            max_drift_ms: DEFAULT_MAX_DRIFT_MS,
        }
    }

    /// Sets how far a remote timestamp may move this clock ahead of the wall clock.
    pub fn with_max_drift_ms(mut self, max_drift_ms: u64) -> Self {
        self.max_drift_ms = max_drift_ms;
        self
    }

    /// The most recent timestamp produced or observed.
    pub fn last(&self) -> Hlc {
        self.last
    }

    /// Timestamp for a local event. Strictly greater than every earlier result, even if the
    /// wall clock moves backwards.
    pub fn now(&mut self, wall_ms: u64) -> Hlc {
        self.advance(wall_ms.min(MAX_PHYSICAL_MS), Hlc::ZERO)
    }

    /// Merges a timestamp received from another device and returns a timestamp greater than
    /// both it and every earlier local result.
    ///
    /// A remote timestamp more than `max_drift_ms` ahead of `wall_ms` is clamped before it
    /// moves the local clock, so one device with a wrong clock cannot drag every other device
    /// into the future. The remote operation itself must still be applied by the caller.
    pub fn observe(&mut self, remote: Hlc, wall_ms: u64) -> Hlc {
        let wall_ms = wall_ms.min(MAX_PHYSICAL_MS);
        let limit = wall_ms
            .saturating_add(self.max_drift_ms)
            .min(MAX_PHYSICAL_MS);
        let remote = if remote.physical_ms() > limit {
            Hlc::from_parts(limit, u16::MAX).expect("limit is within 48 bits")
        } else {
            remote
        };
        self.advance(wall_ms, remote)
    }

    fn advance(&mut self, wall_ms: u64, remote: Hlc) -> Hlc {
        let base = self.last.max(remote);
        let next = if wall_ms > base.physical_ms() {
            Hlc::from_parts(wall_ms, 0)
        } else if base.logical() < u16::MAX {
            Hlc::from_parts(base.physical_ms(), base.logical() + 1)
        } else {
            // Logical counter exhausted within one millisecond: borrow the next millisecond.
            Hlc::from_parts(base.physical_ms() + 1, 0)
        };
        self.last = next.unwrap_or(Hlc(u64::MAX));
        self.last
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: u64 = 1_700_000_000_000;

    #[test]
    fn parts_round_trip() {
        let h = Hlc::from_parts(T, 7).unwrap();
        assert_eq!(h.physical_ms(), T);
        assert_eq!(h.logical(), 7);
        assert_eq!(Hlc::from_u64(h.as_u64()), h);
    }

    #[test]
    fn rejects_physical_over_48_bits() {
        assert!(Hlc::from_parts(MAX_PHYSICAL_MS, 0).is_some());
        assert!(Hlc::from_parts(MAX_PHYSICAL_MS + 1, 0).is_none());
    }

    #[test]
    fn hex_round_trip_and_ordering() {
        let a = Hlc::from_parts(T, 1).unwrap();
        let b = Hlc::from_parts(T + 1, 0).unwrap();
        assert_eq!(a.to_hex(), "018bcfe568000001");
        assert_eq!(a.to_hex().parse::<Hlc>().unwrap(), a);
        assert!(a < b);
        assert!(a.to_hex() < b.to_hex());
    }

    #[test]
    fn rejects_malformed_hex() {
        for s in [
            "",
            "018bcfe56800000",
            "018bcfe5680000011",
            "018BCFE568000001",
            "018bcfe56800000g",
        ] {
            assert_eq!(s.parse::<Hlc>(), Err(ParseHlcError), "{s:?}");
        }
    }

    #[test]
    fn now_is_strictly_monotonic_when_wall_clock_stalls_or_goes_back() {
        let mut c = HlcClock::new();
        let a = c.now(T);
        let b = c.now(T);
        let d = c.now(T - 5_000);
        assert!(a < b && b < d);
        assert_eq!(b, Hlc::from_parts(T, 1).unwrap());
        assert_eq!(d, Hlc::from_parts(T, 2).unwrap());
    }

    #[test]
    fn now_resets_logical_when_wall_clock_advances() {
        let mut c = HlcClock::new();
        c.now(T);
        c.now(T);
        assert_eq!(c.now(T + 1), Hlc::from_parts(T + 1, 0).unwrap());
    }

    #[test]
    fn logical_overflow_borrows_next_millisecond() {
        let mut c = HlcClock::resume(Hlc::from_parts(T, u16::MAX).unwrap());
        assert_eq!(c.now(T), Hlc::from_parts(T + 1, 0).unwrap());
    }

    #[test]
    fn observe_moves_past_remote() {
        let mut c = HlcClock::new();
        c.now(T);
        let remote = Hlc::from_parts(T + 10, 3).unwrap();
        let after = c.observe(remote, T);
        assert_eq!(after, Hlc::from_parts(T + 10, 4).unwrap());
        assert!(c.now(T) > after);
    }

    #[test]
    fn observe_clamps_remote_far_in_the_future() {
        let mut c = HlcClock::new().with_max_drift_ms(1_000);
        let remote = Hlc::from_parts(T + 3_600_000, 0).unwrap();
        let after = c.observe(remote, T);
        assert_eq!(after, Hlc::from_parts(T + 1_001, 0).unwrap());
    }

    #[test]
    fn resume_continues_after_persisted_value() {
        let persisted = Hlc::from_parts(T + 500, 2).unwrap();
        let mut c = HlcClock::resume(persisted);
        assert!(c.now(T) > persisted);
    }

    #[test]
    fn system_clock_is_after_2020() {
        assert!(SystemClock.now_ms() > 1_577_836_800_000);
    }

    proptest::proptest! {
        // Local and observed timestamps are strictly increasing whatever the wall clock does.
        #[test]
        fn strictly_increasing_under_any_wall_clock(
            steps in proptest::collection::vec(
                (0u64..=MAX_PHYSICAL_MS + 10, proptest::option::of(proptest::num::u64::ANY)),
                1..200,
            )
        ) {
            let mut c = HlcClock::new();
            let mut prev = Hlc::ZERO;
            for (wall, remote) in steps {
                let next = match remote {
                    Some(r) => c.observe(Hlc::from_u64(r), wall),
                    None => c.now(wall),
                };
                if prev == Hlc(u64::MAX) {
                    break; // saturated: the 48-bit range is exhausted
                }
                proptest::prop_assert!(next > prev);
                prev = next;
            }
        }
    }
}
