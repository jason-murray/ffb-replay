//! Fixed-period loop pacing: sleep for most of the tick, spin for the last
//! stretch so the wake-up lands on the deadline rather than a scheduler
//! quantum after it. If the loop falls more than a period behind it
//! resynchronises instead of bursting to catch up.

use std::time::{Duration, Instant};

pub struct Pacer {
    period: Duration,
    next: Instant,
    /// How long before the deadline to stop sleeping and spin. Starts
    /// small and grows to cover the sleep overshoot actually observed on
    /// this machine (Linux wakes within ~100 µs; macOS can be 2 ms late),
    /// then decays slowly so one bad wake does not pin the loop spinning.
    spin_margin: Duration,
    base_margin: Duration,
    /// Ticks where the deadline had already passed on entry.
    pub late_ticks: u64,
    pub ticks: u64,
    /// Worst sleep overshoot seen (decayed), for diagnostics.
    pub max_overshoot: Duration,
}

/// Spin window to start from. Enough for a precise sleeper; the margin
/// adapts upwards from here when the platform oversleeps.
const BASE_SPIN_MARGIN: Duration = Duration::from_micros(250);
/// Headroom added on top of the observed overshoot.
const MARGIN_HEADROOM: Duration = Duration::from_micros(50);

impl Pacer {
    pub fn new(period: Duration) -> Self {
        let base_margin = (period / 4).min(BASE_SPIN_MARGIN);
        Pacer {
            period,
            next: Instant::now() + period,
            spin_margin: base_margin,
            base_margin,
            late_ticks: 0,
            ticks: 0,
            max_overshoot: Duration::ZERO,
        }
    }

    /// Current spin window, for diagnostics.
    pub fn spin_margin(&self) -> Duration {
        self.spin_margin
    }

    pub fn from_hz(hz: u32) -> Self {
        Pacer::new(Duration::from_nanos(1_000_000_000 / hz.max(1) as u64))
    }

    pub fn period(&self) -> Duration {
        self.period
    }

    /// Block until the next tick deadline.
    pub fn wait(&mut self) {
        let now = Instant::now();
        if now >= self.next {
            self.late_ticks += 1;
        }
        loop {
            let now = Instant::now();
            if now >= self.next {
                break;
            }
            let remaining = self.next - now;
            if remaining > self.spin_margin {
                let block = remaining - self.spin_margin;
                let before = Instant::now();
                std::thread::sleep(block);
                let overshoot = before.elapsed().saturating_sub(block);
                // Decay the running maximum by ~0.1% per sleep, then take
                // the larger of it and what just happened.
                self.max_overshoot =
                    (self.max_overshoot - self.max_overshoot / 1000).max(overshoot);
                self.spin_margin =
                    (self.max_overshoot + MARGIN_HEADROOM).clamp(self.base_margin, self.period);
            } else {
                std::hint::spin_loop();
            }
        }
        self.ticks += 1;
        self.next += self.period;
        let now = Instant::now();
        if self.next < now {
            self.next = now + self.period;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holds_its_period_roughly() {
        // Best of three: shared CI machines stall threads for tens of ms.
        let period = Duration::from_millis(2);
        let ticks = 50u32;
        let expected = period * ticks;
        let mut last = Duration::ZERO;
        for _ in 0..3 {
            let mut p = Pacer::from_hz(500);
            let start = Instant::now();
            for _ in 0..ticks {
                p.wait();
            }
            last = start.elapsed();
            assert!(last + Duration::from_millis(1) >= expected, "ran fast: {last:?}");
            if last < expected + Duration::from_millis(25) {
                return;
            }
        }
        panic!("expected ~{expected:?}, got {last:?}");
    }

    /// A sleeper that overshoots must not drag the tick rate down: the
    /// margin grows until the loop spins past the overshoot.
    #[test]
    fn margin_adapts_to_oversleep() {
        let mut p = Pacer::from_hz(1000);
        assert_eq!(p.spin_margin(), BASE_SPIN_MARGIN);
        let start = Instant::now();
        for _ in 0..300 {
            p.wait();
        }
        let elapsed = start.elapsed();
        // Even on a coarse sleeper, 300 ticks at 1 kHz must not take far
        // longer than 300 ms once the margin has adapted.
        assert!(elapsed < Duration::from_millis(450), "{elapsed:?}, margin {:?}", p.spin_margin());
        assert!(p.spin_margin() <= p.period());
    }
}
