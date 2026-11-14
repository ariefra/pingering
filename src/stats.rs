//! Pure computation over ping samples: running totals, the summary rendered
//! by the detail pane, and the nice-scale snap for graph scaling.

/// Running aggregate over all samples since start. Kept separately from the
/// 512-sample graph ring so loss stats never lose history.
#[derive(Debug, Default, Clone)]
pub struct Totals {
    received: u64,
    lost: u64,
    sum_ms: f64,
    min_ms: Option<f64>,
    max_ms: Option<f64>,
    last_success_ms: Option<f64>,
}

impl Totals {
    pub fn record(&mut self, rtt_ms: Option<f64>) {
        match rtt_ms {
            Some(ms) => {
                self.received += 1;
                self.sum_ms += ms;
                self.min_ms = Some(self.min_ms.map_or(ms, |m| m.min(ms)));
                self.max_ms = Some(self.max_ms.map_or(ms, |m| m.max(ms)));
                self.last_success_ms = Some(ms);
            }
            None => self.lost += 1,
        }
    }
}

/// Point-in-time summary rendered by the detail pane.
#[derive(Debug, Clone, PartialEq)]
pub struct Stats {
    pub current: Option<f64>,
    pub min: Option<f64>,
    pub avg: Option<f64>,
    pub max: Option<f64>,
    pub received: u64,
    pub lost: u64,
}

impl Stats {
    pub fn from_totals(totals: &Totals) -> Stats {
        Stats {
            current: totals.last_success_ms,
            min: totals.min_ms,
            avg: (totals.received > 0).then(|| totals.sum_ms / totals.received as f64),
            max: totals.max_ms,
            received: totals.received,
            lost: totals.lost,
        }
    }

    /// Loss percent; None before the first sample (0/0).
    pub fn loss_pct(&self) -> Option<f64> {
        let total = self.received + self.lost;
        (total > 0).then(|| self.lost as f64 / total as f64 * 100.0)
    }
}

/// Smallest of {1,2,5,10,20,50,100,200,500,1000,2000,5000} ms that is >=
/// `max_rtt`; floor 1 ms (protects the divide and sub-ms loopback pings);
/// caps at 5000 ms.
pub fn nice_scale(max_rtt: Option<f64>) -> f64 {
    const SCALES: [f64; 12] = [
        1.0, 2.0, 5.0, 10.0, 20.0, 50.0, 100.0, 200.0, 500.0, 1000.0, 2000.0, 5000.0,
    ];
    let Some(max) = max_rtt else { return 1.0 };
    SCALES.into_iter().find(|s| *s >= max).unwrap_or(5000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn min_avg_max_over_successes() {
        let mut totals = Totals::default();
        for rtt in [3.0, 1.0, 2.0] {
            totals.record(Some(rtt));
        }
        let stats = Stats::from_totals(&totals);
        assert_eq!(stats.min, Some(1.0));
        assert_eq!(stats.avg, Some(2.0));
        assert_eq!(stats.max, Some(3.0));
    }

    #[test]
    fn current_is_last_success() {
        let mut totals = Totals::default();
        totals.record(Some(5.0));
        totals.record(None);
        let stats = Stats::from_totals(&totals);
        assert_eq!(stats.current, Some(5.0));
    }

    #[test]
    fn ten_samples_two_lost() {
        let mut totals = Totals::default();
        for i in 0..10 {
            totals.record((i != 3 && i != 7).then_some(i as f64));
        }
        let stats = Stats::from_totals(&totals);
        assert_eq!(stats.received, 8);
        assert_eq!(stats.lost, 2);
        let loss = stats.loss_pct().unwrap();
        assert!((loss - 20.0).abs() < 1e-9, "loss {loss}");
    }

    #[test]
    fn empty_state_is_all_none() {
        let stats = Stats::from_totals(&Totals::default());
        assert_eq!(stats.current, None);
        assert_eq!(stats.min, None);
        assert_eq!(stats.avg, None);
        assert_eq!(stats.max, None);
        assert_eq!(stats.loss_pct(), None);
    }

    #[test]
    fn scale_snaps() {
        assert_eq!(nice_scale(Some(3.4)), 5.0);
        assert_eq!(nice_scale(Some(1200.0)), 2000.0);
        assert_eq!(nice_scale(Some(0.5)), 1.0);
        assert_eq!(nice_scale(Some(1.0)), 1.0);
        assert_eq!(nice_scale(Some(6000.0)), 5000.0);
        assert_eq!(nice_scale(None), 1.0);
    }
}
