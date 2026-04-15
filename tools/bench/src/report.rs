use crate::config::Config;
use crate::stats::Summary;

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
