use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use hdrhistogram::Histogram;

/// Shared atomic counters that worker threads update and the TUI reads.
/// All fields use `Relaxed` ordering — approximate real-time display is fine.
pub struct LiveStats {
    pub requests: AtomicU64,
    pub bytes: AtomicU64,
    pub errors: AtomicU64,
}

impl LiveStats {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            requests: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            errors: AtomicU64::new(0),
        })
    }

    pub fn add_request(&self, bytes: u64) {
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn add_error(&self) {
        self.errors.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> (u64, u64, u64) {
        (
            self.requests.load(Ordering::Relaxed),
            self.bytes.load(Ordering::Relaxed),
            self.errors.load(Ordering::Relaxed),
        )
    }
}

/// Per-thread SSE statistics. Lock-free — each thread owns its own instance.
pub struct SseThreadStats {
    /// Time from TCP connect to first data byte (microseconds).
    pub ttfb: Histogram<u64>,
    /// Time between consecutive `data:` chunks (microseconds).
    pub chunk_latency: Histogram<u64>,
    pub total_chunks: u64,
    pub total_bytes: u64,
    pub completed_streams: u64,
    pub errors_connect: u64,
    pub errors_read: u64,
}

impl SseThreadStats {
    pub fn new() -> Self {
        Self {
            ttfb: Histogram::new_with_bounds(1, 60_000_000, 3).unwrap(),
            chunk_latency: Histogram::new_with_bounds(1, 60_000_000, 3).unwrap(),
            total_chunks: 0,
            total_bytes: 0,
            completed_streams: 0,
            errors_connect: 0,
            errors_read: 0,
        }
    }

    pub fn record_ttfb(&mut self, micros: u64) {
        let _ = self.ttfb.record(micros.max(1));
    }

    pub fn record_chunk_latency(&mut self, micros: u64) {
        let _ = self.chunk_latency.record(micros.max(1));
    }
}

/// Aggregated SSE stats from all threads.
pub struct SseSummary {
    pub ttfb: Histogram<u64>,
    pub chunk_latency: Histogram<u64>,
    pub total_chunks: u64,
    pub total_bytes: u64,
    pub completed_streams: u64,
    pub errors_connect: u64,
    pub errors_read: u64,
    pub duration: std::time::Duration,
}

impl SseSummary {
    pub fn merge(thread_stats: Vec<SseThreadStats>, duration: std::time::Duration) -> Self {
        let mut ttfb = Histogram::new_with_bounds(1, 60_000_000, 3).unwrap();
        let mut chunk_latency = Histogram::new_with_bounds(1, 60_000_000, 3).unwrap();
        let mut total_chunks = 0u64;
        let mut total_bytes = 0u64;
        let mut completed_streams = 0u64;
        let mut errors_connect = 0u64;
        let mut errors_read = 0u64;

        for ts in &thread_stats {
            ttfb.add(&ts.ttfb).ok();
            chunk_latency.add(&ts.chunk_latency).ok();
            total_chunks += ts.total_chunks;
            total_bytes += ts.total_bytes;
            completed_streams += ts.completed_streams;
            errors_connect += ts.errors_connect;
            errors_read += ts.errors_read;
        }

        SseSummary {
            ttfb,
            chunk_latency,
            total_chunks,
            total_bytes,
            completed_streams,
            errors_connect,
            errors_read,
            duration,
        }
    }

    pub fn chunks_per_sec(&self) -> f64 {
        self.total_chunks as f64 / self.duration.as_secs_f64()
    }
}

/// Per-thread WebSocket statistics. Lock-free — each thread owns its own instance.
pub struct WsThreadStats {
    /// Round-trip time per message (microseconds): send text frame → receive response frame.
    pub rtt: hdrhistogram::Histogram<u64>,
    pub total_messages: u64,
    pub total_bytes: u64,
    pub errors_connect: u64,
    pub errors_upgrade: u64,
    pub errors_read: u64,
    pub errors_write: u64,
}

impl WsThreadStats {
    pub fn new() -> Self {
        Self {
            rtt: hdrhistogram::Histogram::new_with_bounds(1, 60_000_000, 3).unwrap(),
            total_messages: 0,
            total_bytes: 0,
            errors_connect: 0,
            errors_upgrade: 0,
            errors_read: 0,
            errors_write: 0,
        }
    }

    pub fn record_rtt(&mut self, micros: u64) {
        let _ = self.rtt.record(micros.max(1));
    }

    pub fn record_message(&mut self, bytes: u64) {
        self.total_messages += 1;
        self.total_bytes += bytes;
    }
}

/// Aggregated WebSocket stats from all threads.
pub struct WsSummary {
    pub rtt: hdrhistogram::Histogram<u64>,
    pub total_messages: u64,
    pub total_bytes: u64,
    pub errors_connect: u64,
    pub errors_upgrade: u64,
    pub errors_read: u64,
    pub errors_write: u64,
    pub duration: std::time::Duration,
}

impl WsSummary {
    pub fn merge(thread_stats: Vec<WsThreadStats>, duration: std::time::Duration) -> Self {
        let mut rtt = hdrhistogram::Histogram::new_with_bounds(1, 60_000_000, 3).unwrap();
        let mut total_messages = 0u64;
        let mut total_bytes = 0u64;
        let mut errors_connect = 0u64;
        let mut errors_upgrade = 0u64;
        let mut errors_read = 0u64;
        let mut errors_write = 0u64;

        for ts in &thread_stats {
            rtt.add(&ts.rtt).ok();
            total_messages += ts.total_messages;
            total_bytes += ts.total_bytes;
            errors_connect += ts.errors_connect;
            errors_upgrade += ts.errors_upgrade;
            errors_read += ts.errors_read;
            errors_write += ts.errors_write;
        }

        WsSummary {
            rtt,
            total_messages,
            total_bytes,
            errors_connect,
            errors_upgrade,
            errors_read,
            errors_write,
            duration,
        }
    }

    pub fn messages_per_sec(&self) -> f64 {
        self.total_messages as f64 / self.duration.as_secs_f64()
    }

    pub fn total_errors(&self) -> u64 {
        self.errors_connect + self.errors_upgrade + self.errors_read + self.errors_write
    }
}

/// Per-thread statistics. Lock-free — each thread owns its own instance.
pub struct ThreadStats {
    pub latency: Histogram<u64>,
    pub requests: u64,
    pub bytes: u64,
    pub errors_connect: u64,
    pub errors_read: u64,
    pub errors_write: u64,
    pub errors_timeout: u64,
    pub errors_status: u64,
}

impl ThreadStats {
    pub fn new() -> Self {
        Self {
            // 1µs to 60s range, 3 significant digits
            latency: Histogram::new_with_bounds(1, 60_000_000, 3).unwrap(),
            requests: 0,
            bytes: 0,
            errors_connect: 0,
            errors_read: 0,
            errors_write: 0,
            errors_timeout: 0,
            errors_status: 0,
        }
    }

    pub fn record_latency(&mut self, micros: u64) {
        let _ = self.latency.record(micros);
    }

    pub fn record_request(&mut self, bytes: u64) {
        self.requests += 1;
        self.bytes += bytes;
    }
}

/// Aggregated stats from all threads.
pub struct Summary {
    pub latency: Histogram<u64>,
    pub requests: u64,
    pub bytes: u64,
    pub duration: std::time::Duration,
    pub errors_connect: u64,
    pub errors_read: u64,
    pub errors_write: u64,
    pub errors_timeout: u64,
    pub errors_status: u64,
}

impl Summary {
    pub fn merge(thread_stats: Vec<ThreadStats>, duration: std::time::Duration) -> Self {
        let mut merged = Histogram::new_with_bounds(1, 60_000_000, 3).unwrap();
        let mut requests = 0u64;
        let mut bytes = 0u64;
        let mut ec = 0u64;
        let mut er = 0u64;
        let mut ew = 0u64;
        let mut et = 0u64;
        let mut es = 0u64;

        for ts in &thread_stats {
            merged.add(&ts.latency).ok();
            requests += ts.requests;
            bytes += ts.bytes;
            ec += ts.errors_connect;
            er += ts.errors_read;
            ew += ts.errors_write;
            et += ts.errors_timeout;
            es += ts.errors_status;
        }

        Summary {
            latency: merged,
            requests,
            bytes,
            duration,
            errors_connect: ec,
            errors_read: er,
            errors_write: ew,
            errors_timeout: et,
            errors_status: es,
        }
    }

    pub fn requests_per_sec(&self) -> f64 {
        self.requests as f64 / self.duration.as_secs_f64()
    }

    pub fn bytes_per_sec(&self) -> f64 {
        self.bytes as f64 / self.duration.as_secs_f64()
    }

    pub fn total_errors(&self) -> u64 {
        self.errors_connect + self.errors_read + self.errors_write + self.errors_timeout + self.errors_status
    }
}
