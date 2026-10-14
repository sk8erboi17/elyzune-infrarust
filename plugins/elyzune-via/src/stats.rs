//! Opt-in translation timing, for measuring what elyzune-via costs.
//!
//! With `ELYZUNE_VIA_STATS` set (to anything but `0` or empty) in the
//! proxy's environment, every translator times each packet it handles, per
//! direction, and logs one line when its connection ends:
//!
//! ```text
//! elyzune-via stats: connection=7 client=774 cb_packets=… cb_bytes=… cb_mean_ns=…
//!     cb_p99_ns=… cb_max_ns=… sb_packets=… sb_bytes=… sb_mean_ns=… sb_p99_ns=… sb_max_ns=…
//!     cb_slowest=play/0x27 sb_slowest=config/0x07
//! ```
//!
//! `slowest` names the packet behind `max_ns`: the protocol state it was
//! read in and its id on the wire it came from (the backend's for
//! clientbound, the client's for serverbound).
//!
//! Translation runs inline on the connection's proxy task, before the packet
//! is written on, so the time measured here is the latency translation adds
//! to that packet (and the CPU it costs). `bytes` are the bodies as they
//! arrived, before translation.
//!
//! Off by default: then a translator holds no timing state and pays one
//! `Option` test per packet. On, two `Instant::now` per packet and a
//! fixed 2 KiB per connection.
//!
//! The 99th percentile comes from a log-linear histogram (four buckets per
//! power of two, so within 25 % of the true value), reported as the upper
//! bound of its bucket and never above the slowest packet seen.

use std::sync::OnceLock;
use std::time::Duration;

/// Sub-buckets per power of two.
const SUB_BITS: u32 = 2;
const SUB: u64 = 1 << SUB_BITS;
/// Enough buckets for any `u64` nanosecond count.
const BUCKETS: usize = 64 << SUB_BITS;

/// Whether `ELYZUNE_VIA_STATS` asks for timing; read once per process.
#[must_use]
pub fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var_os("ELYZUNE_VIA_STATS").is_some_and(|value| !value.is_empty() && value != "0")
    })
}

/// The bucket of a duration of `ns` nanoseconds: `ns` itself below
/// [`SUB`], else `SUB` buckets per power of two.
fn bucket(ns: u64) -> usize {
    if ns < SUB {
        // Below SUB, which fits any usize.
        return ns as usize;
    }
    let log = u64::from(63 - ns.leading_zeros());
    let sub = (ns >> (log - u64::from(SUB_BITS))) & (SUB - 1);
    // log < 64 and sub < SUB: the index is below BUCKETS.
    (log * SUB + sub) as usize
}

/// The largest value [`bucket`] puts in bucket `index`.
fn upper(index: usize) -> u64 {
    let index = index as u64;
    if index < SUB {
        return index;
    }
    let (log, sub) = (index / SUB, index % SUB);
    let shift = log - u64::from(SUB_BITS);
    // The top bucket's bound is 2^64: computed wide, then clamped.
    let top = (u128::from(SUB + sub + 1) << shift) - 1;
    u64::try_from(top).unwrap_or(u64::MAX)
}

/// The timing of one direction of one connection.
#[derive(Debug, Clone)]
pub struct Direction {
    pub packets: u64,
    pub bytes: u64,
    pub total_ns: u64,
    pub max_ns: u64,
    /// State and packet id of the slowest packet (see the module docs).
    pub slowest: (&'static str, i32),
    histogram: [u32; BUCKETS],
}

impl Default for Direction {
    fn default() -> Self {
        Self {
            packets: 0,
            bytes: 0,
            total_ns: 0,
            max_ns: 0,
            slowest: ("none", -1),
            histogram: [0; BUCKETS],
        }
    }
}

impl Direction {
    /// Counts one packet of `bytes`, `packet` = (state, id), that took
    /// `elapsed` to translate.
    pub fn add(&mut self, bytes: usize, elapsed: Duration, packet: (&'static str, i32)) {
        let ns = u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX);
        self.packets += 1;
        self.bytes = self.bytes.saturating_add(bytes as u64);
        self.total_ns = self.total_ns.saturating_add(ns);
        if ns >= self.max_ns {
            self.max_ns = ns;
            self.slowest = packet;
        }
        let slot = &mut self.histogram[bucket(ns)];
        *slot = slot.saturating_add(1);
    }

    /// Mean translation time, 0 with no packets.
    #[must_use]
    pub fn mean_ns(&self) -> u64 {
        self.total_ns.checked_div(self.packets).unwrap_or(0)
    }

    /// The 99th percentile translation time (see the module docs), 0 with no
    /// packets.
    #[must_use]
    pub fn p99_ns(&self) -> u64 {
        if self.packets == 0 {
            return 0;
        }
        // ceil(packets * 99 / 100) packets are at or below the percentile.
        let wanted = (self.packets * 99).div_ceil(100);
        let mut seen = 0_u64;
        for (index, count) in self.histogram.iter().enumerate() {
            seen += u64::from(*count);
            if seen >= wanted {
                return upper(index).min(self.max_ns);
            }
        }
        self.max_ns
    }
}

/// The timing of one connection.
#[derive(Debug, Clone, Default)]
pub struct Timing {
    pub clientbound: Direction,
    pub serverbound: Direction,
}

impl Timing {
    /// The log line of the module docs.
    #[must_use]
    pub fn line(&self, connection: u64, client: i32) -> String {
        let (cb, sb) = (&self.clientbound, &self.serverbound);
        format!(
            "elyzune-via stats: connection={connection} client={client} cb_packets={} cb_bytes={} \
             cb_mean_ns={} cb_p99_ns={} cb_max_ns={} sb_packets={} sb_bytes={} sb_mean_ns={} \
             sb_p99_ns={} sb_max_ns={} cb_slowest={}/{:#04x} sb_slowest={}/{:#04x}",
            cb.packets,
            cb.bytes,
            cb.mean_ns(),
            cb.p99_ns(),
            cb.max_ns,
            sb.packets,
            sb.bytes,
            sb.mean_ns(),
            sb.p99_ns(),
            sb.max_ns,
            cb.slowest.0,
            cb.slowest.1,
            sb.slowest.0,
            sb.slowest.1,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_are_within_a_quarter_and_ordered() {
        let mut last = 0;
        for ns in [
            0_u64,
            1,
            3,
            4,
            5,
            7,
            8,
            9,
            15,
            16,
            100,
            1_000,
            65_535,
            1 << 40,
            u64::MAX,
        ] {
            let index = bucket(ns);
            assert!(index < BUCKETS, "{ns}");
            assert!(index >= last, "buckets must not go down: {ns}");
            last = index;
            let top = upper(index);
            assert!(top >= ns, "{ns} above its bucket's top {top}");
            assert!(top - ns <= ns / 4 + 1, "{ns}: bucket top {top} too far");
            // The next value past the top lands in a later bucket.
            if top < u64::MAX {
                assert!(bucket(top + 1) > index, "{ns}");
            }
        }
    }

    #[test]
    fn mean_p99_and_max() {
        let mut d = Direction::default();
        assert_eq!((d.mean_ns(), d.p99_ns()), (0, 0));
        for _ in 0..99 {
            d.add(10, Duration::from_nanos(1_000), ("play", 1));
        }
        d.add(10, Duration::from_nanos(1_000_000), ("play", 2));
        assert_eq!(d.packets, 100);
        assert_eq!(d.bytes, 1_000);
        assert_eq!(d.max_ns, 1_000_000);
        assert_eq!(d.mean_ns(), (99 * 1_000 + 1_000_000) / 100);
        // 99 of 100 took 1 us: the percentile is in 1 us's bucket.
        let p99 = d.p99_ns();
        assert!((1_000..=1_250).contains(&p99), "{p99}");
        d.add(10, Duration::from_nanos(1_000_000), ("play", 2));
        // 2 of 101 slow: ceil(99.99) = 100 packets needed, so a slow one.
        assert_eq!(d.p99_ns(), 1_000_000);
    }

    #[test]
    fn the_line_names_every_field() {
        let mut timing = Timing::default();
        timing
            .clientbound
            .add(5, Duration::from_nanos(2_000), ("play", 0x27));
        timing
            .serverbound
            .add(7, Duration::from_nanos(3_000), ("config", 7));
        let line = timing.line(9, 774);
        for field in [
            "connection=9 ",
            "client=774 ",
            "cb_packets=1 ",
            "cb_bytes=5 ",
            "cb_mean_ns=2000 ",
            "cb_max_ns=2000 ",
            "sb_packets=1 ",
            "sb_bytes=7 ",
            "sb_mean_ns=3000 ",
            "sb_max_ns=3000 ",
            "cb_slowest=play/0x27 ",
            "sb_slowest=config/0x07",
        ] {
            assert!(line.contains(field), "{field} missing from {line}");
        }
    }
}
