// Copyright (C) 2026 Javad Rajabzadeh
// SPDX-License-Identifier: MIT OR Apache-2.0

//! A smoothed transfer rate and the ETA derived from it, for progress readouts.
//!
//! The scheduler's own per-connection estimates are a control signal and twitch
//! by design; a readout built from them jumps every refresh. This is the other
//! kind of rate: bytes over wall clock through a time constant, which reads as
//! a steady counter whatever the caller's refresh cadence is.

/// Exponentially weighted moving average of a byte counter's rate.
///
/// Time-based: the weight of each sample depends on how much wall clock it
/// covers, not on how many samples were taken, so the smoothing has the same
/// time constant at 50 Hz and at one sample a second. The estimate starts at
/// zero and is driven by the samples alone; the first call only records where
/// the counter stood.
#[derive(Clone, Debug, PartialEq)]
pub struct RateMeter {
    tau: f64,
    rate: f64,
    mark: Option<(f64, u64)>,
}

impl RateMeter {
    /// Below this the rate is startup noise and an ETA built from it counts
    /// down from nonsense; [`eta_secs`](Self::eta_secs) reports none instead.
    pub const ETA_FLOOR: f64 = 1024.0;

    /// A meter that forgets about 63% of the past every `tau_secs` seconds.
    pub fn new(tau_secs: f64) -> Self {
        RateMeter {
            tau: tau_secs.max(f64::MIN_POSITIVE),
            rate: 0.0,
            mark: None,
        }
    }

    /// Feed the counter's value at `now_secs` and get the smoothed rate back.
    ///
    /// A sample that does not advance the clock is ignored, and a counter that
    /// went backwards (a retried chunk) reads as zero bytes moved, not as a
    /// negative rate.
    pub fn sample(&mut self, now_secs: f64, done: u64) -> f64 {
        if let Some((at, prev)) = self.mark {
            let dt = now_secs - at;
            if dt <= 0.0 {
                return self.rate;
            }
            let inst = done.saturating_sub(prev) as f64 / dt;
            let keep = (-dt / self.tau).exp();
            self.rate = self.rate * keep + inst * (1.0 - keep);
        }
        self.mark = Some((now_secs, done));
        self.rate
    }

    /// The smoothed rate in bytes per second.
    pub fn rate(&self) -> f64 {
        self.rate
    }

    /// Seconds until `remaining` bytes arrive at the current rate, once the
    /// rate has warmed past [`ETA_FLOOR`](Self::ETA_FLOOR).
    pub fn eta_secs(&self, remaining: u64) -> Option<f64> {
        if self.rate > Self::ETA_FLOOR {
            Some(remaining as f64 / self.rate)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() <= 1e-6 * b.abs().max(1.0)
    }

    #[test]
    fn the_first_sample_only_sets_the_mark() {
        let mut m = RateMeter::new(1.5);
        assert_eq!(m.sample(0.0, 5000), 0.0);
        assert_eq!(m.rate(), 0.0);
        assert_eq!(m.eta_secs(1), None);
    }

    #[test]
    fn a_steady_transfer_converges_on_its_true_rate() {
        let mut m = RateMeter::new(1.5);
        let mut done = 0;
        for i in 0..200 {
            done += 10_000;
            m.sample(i as f64 * 0.1, done);
        }
        assert!((m.rate() - 100_000.0).abs() < 1.0, "{}", m.rate());
    }

    #[test]
    fn one_time_constant_forgets_sixty_three_percent_of_the_past() {
        let mut m = RateMeter::new(2.0);
        m.sample(0.0, 0);
        m.sample(2.0, 2_000_000);
        let expected = 1_000_000.0 * (1.0 - (-1.0f64).exp());
        assert!(close(m.rate(), expected), "{} vs {expected}", m.rate());
    }

    #[test]
    fn the_time_constant_does_not_depend_on_the_sample_cadence() {
        let mut fast = RateMeter::new(1.5);
        let mut slow = RateMeter::new(1.5);
        for i in 0..=30 {
            fast.sample(i as f64 * 0.1, i * 100_000);
        }
        for i in 0..=3 {
            slow.sample(i as f64, i * 1_000_000);
        }
        let expected = 1_000_000.0 * (1.0 - (-3.0f64 / 1.5).exp());
        assert!(
            close(fast.rate(), expected),
            "{} vs {expected}",
            fast.rate()
        );
        assert!(
            close(slow.rate(), expected),
            "{} vs {expected}",
            slow.rate()
        );
    }

    #[test]
    fn a_stalled_transfer_decays_towards_zero() {
        let mut m = RateMeter::new(1.0);
        m.sample(0.0, 0);
        m.sample(1.0, 1_000_000);
        let busy = m.rate();
        m.sample(2.0, 1_000_000);
        m.sample(3.0, 1_000_000);
        assert!(m.rate() < busy * 0.2, "{} after {busy}", m.rate());
        assert!(m.rate() > 0.0);
    }

    #[test]
    fn a_clock_that_did_not_advance_and_a_counter_that_went_backwards_are_harmless() {
        let mut m = RateMeter::new(1.5);
        m.sample(0.0, 0);
        m.sample(1.0, 500_000);
        let r = m.rate();
        assert_eq!(m.sample(1.0, 900_000), r);
        assert_eq!(m.sample(0.5, 900_000), r);
        let after = m.sample(2.0, 100_000);
        assert!(after < r && after >= 0.0, "{after} after {r}");
    }

    #[test]
    fn eta_is_withheld_until_the_rate_clears_the_floor() {
        let mut m = RateMeter::new(0.001);
        m.sample(0.0, 0);
        m.sample(1.0, 1024);
        assert!(close(m.rate(), 1024.0));
        assert_eq!(m.eta_secs(4096), None, "exactly the floor is still noise");
        m.sample(2.0, 1024 + 2048);
        assert!(m.rate() > RateMeter::ETA_FLOOR);
        let eta = m.eta_secs(4096).unwrap();
        assert!(close(eta, 4096.0 / m.rate()), "{eta}");
        assert_eq!(m.eta_secs(0), Some(0.0));
    }

    #[test]
    fn a_degenerate_time_constant_does_not_produce_nan() {
        let mut m = RateMeter::new(0.0);
        m.sample(0.0, 0);
        let r = m.sample(0.5, 1000);
        assert!(r.is_finite());
        assert!(close(r, 2000.0), "{r}");
    }
}
