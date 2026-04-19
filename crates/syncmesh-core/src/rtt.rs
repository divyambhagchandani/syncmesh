//! Per-peer round-trip-time estimator.
//!
//! Implements an exponentially-weighted moving average matching Syncplay's
//! `avrRtt` (α = 0.15). The estimator is fed by the net layer from iroh's
//! `ConnectionStats`; we do not run an application-level ping/pong.

/// EWMA smoothing factor. Same constant Syncplay uses.
const ALPHA: f64 = 0.15;

#[derive(Debug, Clone, Copy)]
pub struct RttEstimator {
    /// Current estimate, in milliseconds. `None` until the first sample.
    current_ms: Option<f64>,
}

impl Default for RttEstimator {
    fn default() -> Self {
        Self::new()
    }
}

impl RttEstimator {
    pub const fn new() -> Self {
        Self { current_ms: None }
    }

    /// Ingest a new RTT sample in milliseconds. The first sample is taken as-is;
    /// subsequent samples are blended with `ALPHA = 0.15`.
    pub fn sample(&mut self, rtt_ms: u32) {
        let sample = f64::from(rtt_ms);
        self.current_ms = Some(match self.current_ms {
            None => sample,
            Some(prev) => ALPHA.mul_add(sample, (1.0 - ALPHA) * prev),
        });
    }

    /// Current estimate in whole milliseconds, or `None` if no samples have
    /// been taken yet.
    pub fn estimate_ms(&self) -> Option<u32> {
        self.current_ms.map(|v| {
            // RTTs are non-negative by construction and fit in u32 in every
            // plausible universe (max = 49.7 days). We clamp defensively.
            let rounded = v.round().max(0.0).min(f64::from(u32::MAX));
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            {
                rounded as u32
            }
        })
    }

    /// Half the current estimate — the one-way delay compensation applied to
    /// remote timestamps. Returns `0` if no samples exist yet, which is the
    /// safest default (no adjustment).
    pub fn one_way_ms(&self) -> u32 {
        self.estimate_ms().map_or(0, |rtt| rtt / 2)
    }

    /// Whether at least one sample has been taken.
    pub const fn has_sample(&self) -> bool {
        self.current_ms.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_has_no_estimate() {
        let e = RttEstimator::new();
        assert!(!e.has_sample());
        assert_eq!(e.estimate_ms(), None);
        assert_eq!(e.one_way_ms(), 0);
    }

    #[test]
    fn first_sample_is_taken_verbatim() {
        let mut e = RttEstimator::new();
        e.sample(40);
        assert_eq!(e.estimate_ms(), Some(40));
        assert_eq!(e.one_way_ms(), 20);
    }

    #[test]
    fn subsequent_samples_blend_with_alpha() {
        let mut e = RttEstimator::new();
        e.sample(100);
        e.sample(200);
        // 0.85 * 100 + 0.15 * 200 = 115
        assert_eq!(e.estimate_ms(), Some(115));
    }

    #[test]
    fn steady_state_converges_toward_sample() {
        let mut e = RttEstimator::new();
        e.sample(0);
        for _ in 0..200 {
            e.sample(50);
        }
        // After enough samples at 50ms starting from 0ms, the EWMA should be
        // extremely close to 50.
        let est = e.estimate_ms().unwrap();
        assert!((i64::from(est) - 50).abs() <= 1, "converged to {est}");
    }

    #[test]
    fn is_resilient_to_spike() {
        let mut e = RttEstimator::new();
        for _ in 0..50 {
            e.sample(30);
        }
        let before = e.estimate_ms().unwrap();
        e.sample(10_000); // One huge spike.
        let after = e.estimate_ms().unwrap();
        // With α=0.15 the new estimate should be roughly 0.85 * before + 0.15 * 10_000,
        // which is ~1525ms — big shift but far from being pinned to the spike.
        assert!(after > before);
        assert!(after < 2_000, "single spike moved estimate to {after}");
    }

    #[test]
    fn one_way_is_half_rounded_down() {
        let mut e = RttEstimator::new();
        e.sample(31);
        assert_eq!(e.one_way_ms(), 15);
        let mut e = RttEstimator::new();
        e.sample(1);
        assert_eq!(e.one_way_ms(), 0);
    }
}
