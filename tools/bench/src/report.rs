use crate::config::Config;
use crate::stats::{Summary, SseSummary, WsSummary};

/// Print wrk-compatible benchmark output. If `config.json_output` is set,
/// also print a JSON block after the text report.
pub fn print_wrk_format(summary: &Summary, config: &Config) {
    let dur_secs = summary.duration.as_secs_f64();
    let rps = summary.requests_per_sec();
    let bps = summary.bytes_per_sec();

    let lat_avg = summary.latency.mean();
    let lat_stdev = summary.latency.stdev();
    let lat_max = summary.latency.max() as f64;

    println!("  Thread Stats   Avg      Stdev     Max    +/- Stdev");
    println!(
        "    Latency   {:>10}  {:>10}  {:>10}    {:.2}%",
        format_time(lat_avg),
        format_time(lat_stdev),
        format_time(lat_max),
        within_stdev_pct(&summary.latency),
    );
    println!(
        "    Req/Sec   {:.1}k",
        rps / 1000.0 / config.threads as f64
    );

    if config.print_latency {
        println!("  Latency Distribution");
        for pct in [50.0_f64, 75.0, 90.0, 99.0, 99.9] {
            let val = summary.latency.value_at_percentile(pct) as f64;
            println!("    {:>5.1}%    {}", pct, format_time(val));
        }
    }

    println!(
        "  {} requests in {:.2}s, {} read",
        format_count(summary.requests),
        dur_secs,
        format_bytes(summary.bytes),
    );

    let total_errors = summary.total_errors();
    if total_errors > 0 {
        println!(
            "  Socket errors: connect {}, read {}, write {}, timeout {}",
            summary.errors_connect,
            summary.errors_read,
            summary.errors_write,
            summary.errors_timeout,
        );
        if summary.errors_status > 0 {
            println!("  Non-2xx or 3xx responses: {}", summary.errors_status);
        }
    }

    println!("Requests/sec: {:.2}", rps);
    println!("Transfer/sec: {}", format_bytes_rate(bps));

    if config.json_output {
        let json = serde_json::json!({
            "requests": summary.requests,
            "bytes": summary.bytes,
            "duration_ms": summary.duration.as_millis(),
            "requests_per_sec": rps,
            "bytes_per_sec": bps,
            "latency_avg_us": lat_avg,
            "latency_stdev_us": lat_stdev,
            "latency_max_us": lat_max,
            "latency_p50_us": summary.latency.value_at_percentile(50.0),
            "latency_p75_us": summary.latency.value_at_percentile(75.0),
            "latency_p90_us": summary.latency.value_at_percentile(90.0),
            "latency_p99_us": summary.latency.value_at_percentile(99.0),
            "errors_connect": summary.errors_connect,
            "errors_read": summary.errors_read,
            "errors_write": summary.errors_write,
            "errors_timeout": summary.errors_timeout,
            "errors_status": summary.errors_status,
            "errors_total": total_errors,
        });
        println!("\n{}", serde_json::to_string_pretty(&json).unwrap());
    }
}

/// Print SSE benchmark results.
pub fn print_sse_format(summary: &SseSummary, config: &Config) {
    let dur_secs = summary.duration.as_secs_f64();
    let chunks_per_sec = summary.chunks_per_sec();
    let chunks_per_conn = if config.connections > 0 {
        chunks_per_sec / config.connections as f64
    } else {
        0.0
    };

    let ttfb_avg = summary.ttfb.mean();
    let ttfb_p50 = summary.ttfb.value_at_percentile(50.0) as f64;
    let ttfb_p99 = summary.ttfb.value_at_percentile(99.0) as f64;
    let ttfb_max = summary.ttfb.max() as f64;

    let clat_avg = summary.chunk_latency.mean();
    let clat_p50 = summary.chunk_latency.value_at_percentile(50.0) as f64;
    let clat_p99 = summary.chunk_latency.value_at_percentile(99.0) as f64;
    let clat_max = summary.chunk_latency.max() as f64;

    println!("  {:<12}{:<9}{:<9}{:<9}{}", "TTFB", "Avg", "p50", "p99", "Max");
    println!(
        "  {:<12}{:<9}{:<9}{:<9}{}",
        "",
        format_time(ttfb_avg),
        format_time(ttfb_p50),
        format_time(ttfb_p99),
        format_time(ttfb_max),
    );
    println!(
        "  Chunks/s    {:.0} (total)   {:.0} (per conn)",
        chunks_per_sec,
        chunks_per_conn,
    );
    println!("  {:<12}{:<9}{:<9}{:<9}{}", "Chunk Lat", "Avg", "p50", "p99", "Max");
    println!(
        "  {:<12}{:<9}{:<9}{:<9}{}",
        "",
        format_time(clat_avg),
        format_time(clat_p50),
        format_time(clat_p99),
        format_time(clat_max),
    );
    println!(
        "  {} chunks in {:.2}s, {} read",
        format_count(summary.total_chunks),
        dur_secs,
        format_bytes(summary.total_bytes),
    );

    let total_errors = summary.errors_connect + summary.errors_read;
    if total_errors > 0 {
        println!(
            "  Socket errors: connect {}, read {}",
            summary.errors_connect,
            summary.errors_read,
        );
    }

    println!("Chunks/sec:   {:.2}", chunks_per_sec);
    println!("Transfer/sec: {}", format_bytes_rate(summary.total_bytes as f64 / dur_secs));

    if config.json_output {
        let json = serde_json::json!({
            "total_chunks": summary.total_chunks,
            "total_bytes": summary.total_bytes,
            "completed_streams": summary.completed_streams,
            "duration_ms": summary.duration.as_millis(),
            "chunks_per_sec": chunks_per_sec,
            "ttfb_avg_us": ttfb_avg,
            "ttfb_p50_us": ttfb_p50,
            "ttfb_p99_us": ttfb_p99,
            "ttfb_max_us": ttfb_max,
            "chunk_lat_avg_us": clat_avg,
            "chunk_lat_p50_us": clat_p50,
            "chunk_lat_p99_us": clat_p99,
            "chunk_lat_max_us": clat_max,
            "errors_connect": summary.errors_connect,
            "errors_read": summary.errors_read,
        });
        println!("\n{}", serde_json::to_string_pretty(&json).unwrap());
    }
}

/// Print WebSocket benchmark results.
///
/// ```text
/// Running 10s WebSocket test @ ws://localhost:8080/echo
///   100 connections
///   Msg Stats    Avg      p50      p99      Max
///     RTT       0.15ms   0.12ms   0.45ms   2.1ms
///     Msg/Sec   85,200
///   852,000 messages in 10.00s
/// ```
pub fn print_ws_format(summary: &WsSummary, config: &Config) {
    let dur_secs = summary.duration.as_secs_f64();
    let mps = summary.messages_per_sec();

    let rtt_avg = summary.rtt.mean();
    let rtt_p50 = summary.rtt.value_at_percentile(50.0) as f64;
    let rtt_p99 = summary.rtt.value_at_percentile(99.0) as f64;
    let rtt_max = summary.rtt.max() as f64;

    println!("  {:<14}{:<9}{:<9}{:<9}{}", "Msg Stats", "Avg", "p50", "p99", "Max");
    println!(
        "    {:<12}{:<9}{:<9}{:<9}{}",
        "RTT",
        format_time(rtt_avg),
        format_time(rtt_p50),
        format_time(rtt_p99),
        format_time(rtt_max),
    );
    println!("    Msg/Sec   {:.0}", mps);
    println!(
        "  {} messages in {:.2}s, {} read",
        format_count(summary.total_messages),
        dur_secs,
        format_bytes(summary.total_bytes),
    );

    let total_errors = summary.total_errors();
    if total_errors > 0 {
        println!(
            "  Errors: connect {}, upgrade {}, read {}, write {}",
            summary.errors_connect,
            summary.errors_upgrade,
            summary.errors_read,
            summary.errors_write,
        );
    }

    println!("Messages/sec: {:.2}", mps);
    println!("Transfer/sec: {}", format_bytes_rate(summary.total_bytes as f64 / dur_secs));

    if config.json_output {
        let json = serde_json::json!({
            "total_messages": summary.total_messages,
            "total_bytes": summary.total_bytes,
            "duration_ms": summary.duration.as_millis(),
            "messages_per_sec": mps,
            "rtt_avg_us": rtt_avg,
            "rtt_p50_us": rtt_p50,
            "rtt_p99_us": rtt_p99,
            "rtt_max_us": rtt_max,
            "errors_connect": summary.errors_connect,
            "errors_upgrade": summary.errors_upgrade,
            "errors_read": summary.errors_read,
            "errors_write": summary.errors_write,
            "errors_total": total_errors,
        });
        println!("\n{}", serde_json::to_string_pretty(&json).unwrap());
    }
}

// --- formatting helpers ---

fn format_time(micros: f64) -> String {
    if micros < 1_000.0 {
        format!("{:.2}us", micros)
    } else if micros < 1_000_000.0 {
        format!("{:.2}ms", micros / 1_000.0)
    } else {
        format!("{:.2}s", micros / 1_000_000.0)
    }
}

fn format_count(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.2}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        format!("{n}")
    }
}

fn format_bytes(n: u64) -> String {
    if n >= 1_073_741_824 {
        format!("{:.2}GB", n as f64 / 1_073_741_824.0)
    } else if n >= 1_048_576 {
        format!("{:.2}MB", n as f64 / 1_048_576.0)
    } else if n >= 1_024 {
        format!("{:.2}KB", n as f64 / 1_024.0)
    } else {
        format!("{n}B")
    }
}

fn format_bytes_rate(bps: f64) -> String {
    format!("{}/s", format_bytes(bps as u64))
}

/// Percentage of samples within one standard deviation of the mean.
fn within_stdev_pct(h: &hdrhistogram::Histogram<u64>) -> f64 {
    if h.len() == 0 {
        return 0.0;
    }
    let mean = h.mean();
    let stdev = h.stdev();
    let low = (mean - stdev).max(0.0) as u64;
    let high = (mean + stdev) as u64;
    let within = h.count_between(low, high);
    (within as f64 / h.len() as f64) * 100.0
}
