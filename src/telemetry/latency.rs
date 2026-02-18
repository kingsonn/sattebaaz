use std::collections::VecDeque;
use std::time::{Duration, Instant};
use tracing::info;

/// Tracks latency histograms for each system component.
pub struct LatencyTracker {
    histograms: dashmap::DashMap<String, VecDeque<Duration>>,
    max_samples: usize,
}

impl LatencyTracker {
    pub fn new(max_samples: usize) -> Self {
        Self {
            histograms: dashmap::DashMap::new(),
            max_samples,
        }
    }

    /// Record a latency sample for a named operation.
    pub fn record(&self, operation: &str, duration: Duration) {
        let max = self.max_samples;
        self.histograms
            .entry(operation.to_string())
            .and_modify(|hist| {
                if hist.len() >= max {
                    hist.pop_front();
                }
                hist.push_back(duration);
            })
            .or_insert_with(|| {
                let mut dq = VecDeque::with_capacity(max);
                dq.push_back(duration);
                dq
            });
    }

    /// Start a timer that records on drop.
    pub fn start_timer(&self, operation: &str) -> Timer<'_> {
        Timer {
            operation: operation.to_string(),
            start: Instant::now(),
            tracker: self,
        }
    }

    /// Get p50, p95, p99 latencies for an operation.
    pub fn percentiles(&self, operation: &str) -> Option<(Duration, Duration, Duration)> {
        let hist = self.histograms.get(operation)?;
        if hist.is_empty() {
            return None;
        }

        let mut sorted: Vec<Duration> = hist.iter().copied().collect();
        sorted.sort();

        let len = sorted.len();
        let p50 = sorted[len / 2];
        let p95 = sorted[(len as f64 * 0.95) as usize];
        let p99 = sorted[(len as f64 * 0.99).min((len - 1) as f64) as usize];

        Some((p50, p95, p99))
    }

    /// Log all latency summaries.
    pub fn log_summary(&self) {
        for entry in self.histograms.iter() {
            if let Some((p50, p95, p99)) = self.percentiles(entry.key()) {
                info!(
                    "Latency [{}]: p50={:.1}ms p95={:.1}ms p99={:.1}ms samples={}",
                    entry.key(),
                    p50.as_secs_f64() * 1000.0,
                    p95.as_secs_f64() * 1000.0,
                    p99.as_secs_f64() * 1000.0,
                    entry.value().len(),
                );
            }
        }
    }
}

pub struct Timer<'a> {
    operation: String,
    start: Instant,
    tracker: &'a LatencyTracker,
}

impl<'a> Drop for Timer<'a> {
    fn drop(&mut self) {
        let elapsed = self.start.elapsed();
        self.tracker.record(&self.operation, elapsed);
    }
}
