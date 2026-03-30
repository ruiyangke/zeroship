//! CPU time metering using Linux `CLOCK_THREAD_CPUTIME_ID`.
//!
//! Only counts actual CPU cycles consumed by the current thread.
//! I/O wait (network, disk) is excluded — matching Cloudflare Workers' billing model.

use std::time::Duration;

/// Get current thread's CPU time.
///
/// # Safety
/// Calls `libc::clock_gettime` which is a well-defined POSIX syscall.
#[allow(unsafe_code)]
pub fn thread_cpu_time() -> Duration {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime with CLOCK_THREAD_CPUTIME_ID is always safe
    // and writes to a valid, properly aligned timespec struct.
    unsafe {
        libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts);
    }
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

/// Tracks accumulated CPU time for an isolate.
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
}
