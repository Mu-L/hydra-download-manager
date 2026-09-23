//! Online concurrency admission: dynamically probe and scale connection counts
//! based on measured marginal goodput.
//!
//! When per-source rate limits and per-connection capacities are unknown, opening
//! extra connections on an already saturated path adds request overhead without
//! improving throughput.
//!
//! This module implements incremental greedy admission: it probes connections
//! one at a time, checks whether throughput increases by more than a minimum gain
//! threshold, and settles at the optimal connection count upon diminishing returns.

/// Decision returned by the controller after each probe interval.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Admit {
    /// Open one more connection to this source and keep probing.
    Add,
    /// Saturated: the last admission did not pay for itself. Settle here.
    Stop,
}

/// Per-source incremental admission controller.
///
/// Feed it the aggregate goodput observed at each concurrency level. It compares
/// the marginal gain against `min_gain_frac` of the goodput achieved by the first
/// connection — a scale-free threshold, so it behaves identically on a 1 MB/s
/// and a 1 GB/s path.
#[derive(Clone, Debug)]
pub struct Admission {
    /// Goodput observed, one entry per measurement window.
    samples: Vec<f64>,
    /// The connection count each sample was measured at, parallel to `samples`.
    ///
    /// Kept explicitly because the sample index is NOT the level: the in-band ramp
    /// doubles, so sample 3 describes four connections. Inferring one from the other
    /// is the defect this field exists to prevent.
    levels: Vec<usize>,
    /// Marginal gain required to justify one more connection, as a fraction of
    /// the single-connection goodput.
    min_gain_frac: f64,
    /// Hard ceiling regardless of measurement (politeness, not physics).
    max_conns: usize,
    settled: Option<usize>,
}

impl Admission {
    pub fn new(min_gain_frac: f64, max_conns: usize) -> Self {
        Self {
            samples: Vec::new(),
            levels: Vec::new(),
            min_gain_frac,
            max_conns: max_conns.max(1),
            settled: None,
        }
    }

    /// Record the aggregate goodput (bytes/s) observed with `self.level() + 1`
    /// connections, and decide whether to admit another.
    /// Record the goodput measured while `level` connections were active.
    ///
    /// # Why the level is a parameter and not the sample count
    ///
    /// This originally inferred the level from `samples.len()`, which is correct only
    /// if callers admit one connection per observation. The in-band ramp doubles
    /// (1, 2, 4, 8) because incrementing takes `max - 1` windows and costs more clock
    /// than the concurrency saves. Under doubling the third sample describes FOUR
    /// connections, so returning `samples.len() - 1` returned a sample index dressed
    /// as a connection count.
    ///
    /// HISTORICAL MEASUREMENT (pre-fix implementation; retained to document why this
    /// design exists, not as a current result). 11 MB object, five repetitions: settled counts came back
    /// `[2, 8, 8, 8, 8]` on a path a single stream already saturates. The `2` is the
    /// index bug reporting sample 3 as "2"; the four `8`s are the ceiling arm firing
    /// because the sample-count test `n >= max_conns` needs eight samples and doubling
    /// only ever takes four. The search could not settle anywhere sensible, and the
    /// mode was no faster than hard-coding the ceiling (0.99x, p = 0.63) while a
    /// single connection was 1.97x faster than both.
    pub fn observe_at(&mut self, level: usize, goodput: f64) -> Admit {
        let level = level.max(1);
        self.samples.push(goodput.max(0.0));
        self.levels.push(level);
        let n = self.samples.len();

        // Ceiling test on the LEVEL, not on how many samples it took to get here.
        if level >= self.max_conns {
            self.settled = Some(self.best_level());
            return Admit::Stop;
        }
        if n == 1 {
            return Admit::Add;
        }

        if !self.step_pays(n - 1) {
            // Settle at the best level MEASURED, not the previous one: a noisy
            // window can make an intermediate level the true peak.
            self.settled = Some(self.best_level());
            Admit::Stop
        } else {
            Admit::Add
        }
    }

    /// Did the step into sample `i` deliver enough to justify the connections it added?
    ///
    /// Measured as a fraction of PROPORTIONAL scaling, not of the single-connection
    /// rate: on a warming path throughput rises whether or not the extra concurrency
    /// does anything (judging against `samples[0]` sent the search to the ceiling in
    /// 9 of 12 runs where one connection was 1.8-3.2x faster). Doubling on a link
    /// with headroom roughly doubles delivery; doubling on a saturated one leaves it
    /// flat. The ratio separates the two without knowing capacity or RTT and is
    /// scale-free, so 1 -> 2 is judged like 4 -> 8. `min_gain_frac` is the share of
    /// perfect scaling required: 0.15 admits a genuinely parallel path (ratio near
    /// 1.0) and refuses a saturated one (near 0).
    fn step_pays(&self, i: usize) -> bool {
        if i == 0 || i >= self.samples.len() {
            return false;
        }
        let prev_rate = self.samples[i - 1].max(1.0);
        let prev_level = self.levels[i - 1].max(1) as f64;
        let this_level = self.levels[i].max(1) as f64;
        if this_level <= prev_level {
            return false;
        }
        let ideal = prev_rate * (this_level / prev_level);
        let headroom = (ideal - prev_rate).max(1e-9);
        (self.samples[i] - prev_rate) / headroom >= self.min_gain_frac
    }

    /// The smallest level that is within `min_gain_frac` of the best goodput measured.
    ///
    /// Not simply "the highest sample". A level whose marginal gain was refused must
    /// not then be settled on — that would reject a level and adopt it in the same
    /// breath. A 2% improvement from doubling the connections is inside the noise this
    /// threshold exists to reject, so the answer is the *cheapest* level that performs
    /// indistinguishably from the best one.
    ///
    /// Equal throughput on fewer connections is strictly better: fewer handshakes, less
    /// origin load, and less exposure to the repair machinery. That is what lets the
    /// search return "one" on a path a single stream already saturates, which is the
    /// case that motivated the whole in-band ramp.
    ///
    /// The last level whose own step paid, by the SAME `step_pays` rule the
    /// admission test applies, stopping at the first that did not: levels beyond
    /// it were reached on earlier gains, not their own. Scoring against a band
    /// around the peak instead let a level be refused and adopted in the same
    /// breath, because accumulated gains make the top sample the highest even
    /// when the final step was worthless.
    fn best_level(&self) -> usize {
        if self.samples.is_empty() {
            return 1;
        }
        let mut best = self.levels.first().copied().unwrap_or(1);
        for i in 1..self.samples.len() {
            if self.step_pays(i) {
                best = self.levels[i];
            } else {
                break;
            }
        }
        best.clamp(1, self.max_conns)
    }

    /// Lower the ceiling the search may reach.
    ///
    /// Raising it is deliberately not possible: the caller lowers this when the
    /// ORIGIN has refused a request, and a ceiling learned from a refusal must not
    /// be undone by the search that provoked it. Recovery from a momentary limit is
    /// the transfer loop's business, not the controller's.
    pub fn clamp_max(&mut self, max: usize) {
        let max = max.max(1);
        if max < self.max_conns {
            self.max_conns = max;
            if let Some(n) = self.settled {
                self.settled = Some(n.min(max));
            }
        }
    }

    /// Back-compatible entry point for callers that admit one connection at a time.
    pub fn observe(&mut self, goodput: f64) -> Admit {
        let level = self.samples.len() + 1;
        self.observe_at(level, goodput)
    }

    /// Connections currently probed.
    pub fn level(&self) -> usize {
        self.samples.len()
    }

    /// The settled allocation, once probing has stopped.
    pub fn settled(&self) -> Option<usize> {
        self.settled
    }

    /// Best goodput seen, for reporting.
    pub fn best_goodput(&self) -> f64 {
        self.samples.iter().cloned().fold(0.0, f64::max)
    }

    /// Every measurement taken, as `(connections, bytes per second)` in the
    /// order it was taken. For reporting what the search saw, not for deciding.
    pub fn samples(&self) -> impl Iterator<Item = (usize, f64)> + '_ {
        self.levels
            .iter()
            .copied()
            .zip(self.samples.iter().copied())
    }
}

/// Online estimate of the per-request setup cost `delta`.
///
/// A configured constant is not good enough: the same code saw `delta = 5 ms`
/// against a loopback-equivalent origin and `420 ms` against a real proxied
/// origin, and `delta` sets the repair deadband `theta`. Underestimating it by
/// 2.8x caused measurable over-repair — each unnecessary repair costs a full
/// `delta`, so the error compounds.
#[derive(Clone, Copy, Debug)]
pub struct DeltaEstimator {
    ewma: f64,
    alpha: f64,
    n: u32,
}

impl DeltaEstimator {
    pub fn new(prior_s: f64) -> Self {
        Self {
            ewma: prior_s.max(1e-4),
            alpha: 0.3,
            n: 0,
        }
    }

    /// Record an observed request-to-first-byte latency.
    pub fn observe(&mut self, ttfb_s: f64) {
        let x = ttfb_s.clamp(1e-4, 30.0);
        if self.n == 0 {
            self.ewma = x;
        } else {
            self.ewma = (1.0 - self.alpha) * self.ewma + self.alpha * x;
        }
        self.n = self.n.saturating_add(1);
    }

    pub fn get(&self) -> f64 {
        self.ewma
    }

    pub fn samples(&self) -> u32 {
        self.n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A DOUBLING caller must get connection counts back, not sample indices.
    ///
    /// This is the defect that made `--adaptive` useless in the field. `observe` inferred
    /// the level from `samples.len()`, which holds only when the caller admits one
    /// connection per window. The in-band ramp doubles, because incrementing takes
    /// `max - 1` windows and costs more clock than the concurrency saves — so sample 3
    /// describes FOUR connections.
    ///
    /// Two things broke at once, and the field data showed both. Settled counts over
    /// HISTORICAL MEASUREMENT (pre-fix implementation; retained to document why this test
    /// exists, not as a current result). Five repetitions on an 11 MB object came back
    /// `[2, 8, 8, 8, 8]` on a path a single stream already saturates: the `2` is sample 3
    /// reported as level 2, and the four `8`s are the ceiling arm, whose
    /// `samples.len() >= max_conns` test needs eight samples while doubling only ever
    /// produces four. The mode ended up no better than hard-coding the ceiling
    /// (0.99x, p = 0.63) while one connection was 1.97x faster than either.
    #[test]
    fn a_doubling_caller_settles_on_a_real_connection_count() {
        // Saturated path: 1 -> 2 -> 4 all deliver the same, so the answer is 1.
        let mut a = Admission::new(0.15, 8);
        assert_eq!(a.observe_at(1, 1.00e6), Admit::Add);
        assert_eq!(
            a.observe_at(2, 1.01e6),
            Admit::Stop,
            "1% per added conn is noise"
        );
        assert_eq!(
            a.settled(),
            Some(1),
            "must settle at ONE connection, not at a sample index"
        );

        // A path with real headroom: doubling keeps paying, so it must reach the
        // ceiling and report the CEILING, not the number of samples it took.
        let mut b = Admission::new(0.15, 8);
        assert_eq!(b.observe_at(1, 1.0e6), Admit::Add);
        assert_eq!(b.observe_at(2, 2.0e6), Admit::Add);
        assert_eq!(b.observe_at(4, 4.0e6), Admit::Add);
        assert_eq!(b.observe_at(8, 8.0e6), Admit::Stop, "ceiling is a stop");
        assert_eq!(
            b.settled(),
            Some(8),
            "a path that scales to the ceiling must settle AT the ceiling; \
             four samples reached level 8 and the old sample-count test never fired"
        );
    }

    /// Gain must be judged per connection added, not per window.
    ///
    /// Doubling from 4 to 8 adds four connections; incrementing from 1 to 2 adds one.
    /// Holding both to the same absolute bar lets a large step pass on noise, which is
    /// how a saturated path ran away to the ceiling.
    #[test]
    fn gain_is_normalised_by_connections_added() {
        let mut a = Admission::new(0.15, 16);
        assert_eq!(a.observe_at(1, 1.00e6), Admit::Add);
        assert_eq!(a.observe_at(2, 1.20e6), Admit::Add, "20% for one conn pays");
        // +0.40e6 across four added connections is 0.10e6 each — below the 0.15 bar,
        // even though the raw step is larger than the one that just passed.
        assert_eq!(
            a.observe_at(8, 1.60e6),
            Admit::Stop,
            "a 6x jump in connections must not pass on the strength of the raw delta"
        );
        assert_eq!(a.settled(), Some(2), "settle at the level that last paid");
    }

    #[test]
    fn saturated_path_settles_at_one() {
        // A path already saturated by one connection: adding more yields nothing.
        let mut a = Admission::new(0.15, 8);
        assert_eq!(a.observe(1.0e6), Admit::Add);
        assert_eq!(a.observe(1.02e6), Admit::Stop, "2% gain must be refused");
        assert_eq!(
            a.settled(),
            Some(1),
            "must settle at ONE, not at the probed 2"
        );
    }

    #[test]
    fn scalable_path_admits_until_knee() {
        // rho = 4 * gamma: goodput rises linearly to 4 connections then flattens.
        let mut a = Admission::new(0.15, 12);
        let curve = [1.0e6, 2.0e6, 3.0e6, 4.0e6, 4.0e6, 4.0e6];
        let mut last = Admit::Add;
        for g in curve {
            last = a.observe(g);
            if last == Admit::Stop {
                break;
            }
        }
        assert_eq!(last, Admit::Stop);
        assert_eq!(a.settled(), Some(4), "must find the knee at rho/gamma = 4");
    }

    #[test]
    fn respects_politeness_ceiling() {
        let mut a = Admission::new(0.01, 3);
        for g in [1.0e6, 2.0e6, 3.0e6] {
            a.observe(g);
        }
        assert_eq!(
            a.settled(),
            Some(3),
            "ceiling binds even when gains continue"
        );
    }

    #[test]
    fn delta_estimator_tracks_a_step_change() {
        let mut d = DeltaEstimator::new(0.15);
        for _ in 0..12 {
            d.observe(0.42);
        }
        assert!(
            (d.get() - 0.42).abs() < 0.02,
            "estimator must converge on the observed cost, got {}",
            d.get()
        );
        assert_eq!(d.samples(), 12);
    }

    #[test]
    fn delta_estimator_first_sample_replaces_prior() {
        let mut d = DeltaEstimator::new(0.005);
        d.observe(0.40);
        assert!(
            d.get() > 0.3,
            "a wildly wrong prior must not survive one sample"
        );
    }
}
