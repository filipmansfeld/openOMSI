//! Dedicated-server pacing and periodic measurements, independent of the renderer.

use std::time::{Duration, Instant};

const REPORT_EVERY: Duration = Duration::from_secs(10);

fn sleep_budget(period: Duration, work: Duration) -> Duration {
    // Overloaded ticks continue without sleeping. Every new tick gets its own budget;
    // no accumulated deadline can cause a burst of catch-up simulation steps.
    period.saturating_sub(work)
}

#[derive(Default)]
pub(crate) struct TickMetrics {
    report_started: Option<Instant>,
    ticks: u64,
    work_total: Duration,
    work_max: Duration,
    late: u64,
    late_max: Duration,
}

impl TickMetrics {
    fn record(&mut self, work: Duration, period: Duration) {
        self.ticks += 1;
        self.work_total += work;
        self.work_max = self.work_max.max(work);
        if work > period {
            self.late += 1;
            self.late_max = self.late_max.max(work.saturating_sub(period));
        }
    }

    /// Called after all of a dedicated tick's work, including its network and status work.
    pub(crate) fn finish(&mut self, started: Instant, period: Duration, players: usize) {
        let report_started = *self.report_started.get_or_insert(started);
        let work = started.elapsed();
        self.record(work, period);
        let wait = sleep_budget(period, work);
        if !wait.is_zero() {
            std::thread::sleep(wait);
        }
        let now = Instant::now();
        let window = now.duration_since(report_started);
        if window >= REPORT_EVERY {
            log::info!(
                "server ticks: {:.2} Hz, {} players, work avg {:.2} ms / max {:.2} ms, late {}/{} (max {:.2} ms over budget)",
                self.ticks as f64 / window.as_secs_f64(),
                players,
                self.work_total.as_secs_f64() * 1000.0 / self.ticks as f64,
                self.work_max.as_secs_f64() * 1000.0,
                self.late,
                self.ticks,
                self.late_max.as_secs_f64() * 1000.0,
            );
            *self = Self { report_started: Some(now), ..Self::default() };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waits_only_for_the_unused_tick_budget() {
        let period = Duration::from_secs_f64(1.0 / 30.0);
        for work in [Duration::ZERO, Duration::from_millis(10), period] {
            assert_eq!(work + sleep_budget(period, work), period);
        }
    }

    #[test]
    fn overload_has_no_sleep_or_catch_up_debt() {
        let period = Duration::from_millis(33);
        assert_eq!(sleep_budget(period, Duration::from_secs(3)), Duration::ZERO);
        assert_eq!(sleep_budget(period, Duration::from_millis(1)), Duration::from_millis(32));
        assert_eq!(sleep_budget(Duration::ZERO, Duration::from_millis(1)), Duration::ZERO);
    }

    #[test]
    fn records_work_and_overloaded_ticks() {
        let mut metrics = TickMetrics::default();
        let period = Duration::from_millis(33);
        for ms in [25, 45, 1] {
            metrics.record(Duration::from_millis(ms), period);
        }
        assert_eq!(metrics.ticks, 3);
        assert_eq!(metrics.work_total, Duration::from_millis(71));
        assert_eq!(metrics.work_max, Duration::from_millis(45));
        assert_eq!(metrics.late, 1);
        assert_eq!(metrics.late_max, Duration::from_millis(12));
    }

    #[test]
    fn finish_measures_overruns_and_resets_report_windows() {
        let mut metrics = TickMetrics::default();
        let started = Instant::now() - Duration::from_secs(1);
        metrics.finish(started, Duration::from_millis(33), 50);
        assert_eq!(metrics.ticks, 1);
        assert_eq!(metrics.late, 1);
        assert!(metrics.work_total >= Duration::from_secs(1));
        assert!(metrics.late_max >= Duration::from_millis(967));

        metrics.report_started = Some(Instant::now() - REPORT_EVERY);
        metrics.finish(Instant::now(), Duration::ZERO, 50);
        assert_eq!(metrics.ticks, 0);
        assert_eq!(metrics.late, 0);
        assert_eq!(metrics.work_total, Duration::ZERO);
        assert!(metrics.report_started.unwrap().elapsed() < REPORT_EVERY);
    }
}
