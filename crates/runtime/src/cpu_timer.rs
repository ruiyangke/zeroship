use std::time::Duration;

/// Get current thread's CPU time (only counts actual CPU cycles, not I/O wait).
/// Uses Linux `CLOCK_THREAD_CPUTIME_ID`.
///
/// # Safety
/// Calls `libc::clock_gettime` which is a well-defined POSIX syscall.
#[allow(unsafe_code)]
pub fn thread_cpu_time() -> Duration {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime with CLOCK_THREAD_CPUTIME_ID is always safe to call
    // and writes to a valid, properly aligned timespec struct.
    unsafe {
        libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts);
    }
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

/// Measure CPU time consumed by a closure.
pub fn measure_cpu<F, R>(f: F) -> (R, Duration)
where
    F: FnOnce() -> R,
{
    let before = thread_cpu_time();
    let result = f();
    let after = thread_cpu_time();
    (result, after.saturating_sub(before))
}

/// Async version — measures CPU time around an async block.
/// Note: this measures the thread CPU time of the current thread,
/// which is correct for single-threaded tokio (current_thread runtime).
pub async fn measure_cpu_async<F, R>(f: F) -> (R, Duration)
where
    F: std::future::Future<Output = R>,
{
    let before = thread_cpu_time();
    let result = f.await;
    let after = thread_cpu_time();
    (result, after.saturating_sub(before))
}

/// CPU time limit configuration per request.
#[derive(Debug, Clone, Copy)]
pub struct CpuLimits {
    /// Maximum CPU time per request. None = unlimited.
    pub max_cpu_per_request: Option<Duration>,
    /// Maximum CPU time per app (accumulated). None = unlimited.
    pub max_cpu_total: Option<Duration>,
}

impl Default for CpuLimits {
    fn default() -> Self {
        Self {
            max_cpu_per_request: Some(Duration::from_millis(50)), // workerd default
            max_cpu_total: None,
        }
    }
}

impl CpuLimits {
    pub fn unlimited() -> Self {
        Self {
            max_cpu_per_request: None,
            max_cpu_total: None,
        }
    }

    pub fn with_max_per_request(mut self, ms: u64) -> Self {
        self.max_cpu_per_request = Some(Duration::from_millis(ms));
        self
    }

    pub fn with_max_total(mut self, ms: u64) -> Self {
        self.max_cpu_total = Some(Duration::from_millis(ms));
        self
    }
}

/// Tracks accumulated CPU time for an app/isolate.
#[derive(Debug, Default)]
pub struct CpuUsage {
    pub total: Duration,
    pub request_count: u64,
}

impl CpuUsage {
    pub fn record(&mut self, cpu_time: Duration) {
        self.total += cpu_time;
        self.request_count += 1;
    }

    pub fn avg_per_request(&self) -> Duration {
        if self.request_count == 0 {
            Duration::ZERO
        } else {
            self.total / self.request_count as u32
        }
    }

    /// Check if the total CPU time exceeds the limit.
    pub fn check_total_limit(&self, limits: &CpuLimits) -> Result<(), CpuLimitExceeded> {
        if let Some(max) = limits.max_cpu_total {
            if self.total > max {
                return Err(CpuLimitExceeded {
                    used: self.total,
                    limit: max,
                    kind: LimitKind::Total,
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct CpuLimitExceeded {
    pub used: Duration,
    pub limit: Duration,
    pub kind: LimitKind,
}

#[derive(Debug)]
pub enum LimitKind {
    PerRequest,
    Total,
}

impl std::fmt::Display for CpuLimitExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.kind {
            LimitKind::PerRequest => write!(
                f,
                "CPU time limit exceeded: {:.1}ms used, {:.1}ms allowed per request",
                self.used.as_secs_f64() * 1000.0,
                self.limit.as_secs_f64() * 1000.0
            ),
            LimitKind::Total => write!(
                f,
                "Total CPU time limit exceeded: {:.1}ms used, {:.1}ms allowed",
                self.used.as_secs_f64() * 1000.0,
                self.limit.as_secs_f64() * 1000.0
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measure_cpu_time() {
        let (_, cpu) = measure_cpu(|| {
            // Burn some CPU
            let mut sum = 0u64;
            for i in 0..1_000_000 {
                sum = sum.wrapping_add(i);
            }
            sum
        });
        // Should be > 0 (we did actual work)
        assert!(cpu.as_nanos() > 0, "CPU time should be non-zero");
        // Should be < 100ms (it's a simple loop)
        assert!(cpu.as_millis() < 100, "CPU time should be reasonable");
    }

    #[test]
    fn cpu_usage_tracking() {
        let mut usage = CpuUsage::default();
        usage.record(Duration::from_millis(5));
        usage.record(Duration::from_millis(3));
        usage.record(Duration::from_millis(7));

        assert_eq!(usage.request_count, 3);
        assert_eq!(usage.total, Duration::from_millis(15));
        assert_eq!(usage.avg_per_request(), Duration::from_millis(5));
    }

    #[test]
    fn cpu_limit_check() {
        let mut usage = CpuUsage::default();
        let limits = CpuLimits::default().with_max_total(10);

        usage.record(Duration::from_millis(5));
        assert!(usage.check_total_limit(&limits).is_ok());

        usage.record(Duration::from_millis(6));
        assert!(usage.check_total_limit(&limits).is_err());
    }
}
