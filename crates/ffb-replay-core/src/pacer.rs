//! Fixed-period loop pacing: sleep for most of the tick, spin for the last
//! stretch so the wake-up lands on the deadline rather than a scheduler
//! quantum after it. If the loop falls more than a period behind it
//! resynchronises instead of bursting to catch up.

use std::time::{Duration, Instant};

pub struct Pacer {
    period: Duration,
    next: Instant,
    spin_margin: Duration,
    /// Ticks where the deadline had already passed on entry.
    pub late_ticks: u64,
    pub ticks: u64,
}

/// Longest spin-to-deadline window. The spin must stay a small share of the
/// tick, or a 1 kHz loop turns into a busy-wait.
const MAX_SPIN_MARGIN: Duration = Duration::from_micros(250);

impl Pacer {
    pub fn new(period: Duration) -> Self {
        Pacer {
            period,
            next: Instant::now() + period,
            spin_margin: (period / 4).min(MAX_SPIN_MARGIN),
            late_ticks: 0,
            ticks: 0,
        }
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
                std::thread::sleep(remaining - self.spin_margin);
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
}
