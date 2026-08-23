use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct TimingSnapshot {
    pub count: u64,
    pub total_us: u64,
}

/// A monotonic, process-local latency accumulator.
///
/// Nanoseconds are accumulated internally and converted only when a snapshot
/// is read. That keeps sub-microsecond mutex acquisitions observable instead
/// of truncating every individual sample to zero.
#[derive(Debug, Default)]
pub(crate) struct TimingCounter {
    count: AtomicU64,
    total_ns: AtomicU64,
}

impl TimingCounter {
    pub(crate) fn observe(&self, elapsed: Duration) {
        saturating_add(&self.count, 1);
        let elapsed_ns = elapsed.as_nanos().min(u64::MAX as u128) as u64;
        saturating_add(&self.total_ns, elapsed_ns);
    }

    pub(crate) fn snapshot(&self) -> TimingSnapshot {
        TimingSnapshot {
            count: self.count.load(Ordering::Relaxed),
            total_us: self.total_ns.load(Ordering::Relaxed) / 1_000,
        }
    }
}

fn saturating_add(counter: &AtomicU64, amount: u64) {
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_add(amount))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregates_before_converting_to_microseconds() {
        let timing = TimingCounter::default();
        timing.observe(Duration::from_nanos(400));
        timing.observe(Duration::from_nanos(600));

        assert_eq!(
            timing.snapshot(),
            TimingSnapshot {
                count: 2,
                total_us: 1,
            }
        );
    }

    #[test]
    fn aggregation_saturates_instead_of_wrapping() {
        let timing = TimingCounter::default();
        timing.total_ns.store(u64::MAX - 4, Ordering::Relaxed);
        timing.count.store(u64::MAX, Ordering::Relaxed);
        timing.observe(Duration::from_nanos(10));

        assert_eq!(timing.count.load(Ordering::Relaxed), u64::MAX);
        assert_eq!(timing.total_ns.load(Ordering::Relaxed), u64::MAX);
    }
}
