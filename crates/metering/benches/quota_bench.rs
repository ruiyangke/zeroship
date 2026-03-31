use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use std::sync::atomic::{AtomicU32, AtomicU8};
use std::sync::Arc;

use appbase_core::billing::SpendAction;
use appbase_core::plugin::{Aggregation, MeterResource, PluginMeter, PluginQuota};
use appbase_enforcement::concurrency::ConcurrencyGuard;
use appbase_enforcement::rate_limit::RateLimiter;
use appbase_metering::meter::{AppPluginMeter, AppQuotaChecker, MeterRegistry};
use appbase_metering::plan::QuotaPlan;
use appbase_metering::registry::RegistryBuilder;

fn make_plugin_resources() -> Vec<MeterResource> {
    ["db.reads", "db.writes", "kv.reads", "kv.writes"]
        .into_iter()
        .map(|name| MeterResource {
            name: name.into(),
            unit: "ops".into(),
            aggregation: Aggregation::Sum,
            category: "database".into(),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// 1. CounterRegistry operations
// ---------------------------------------------------------------------------

fn bench_counter_registry(c: &mut Criterion) {
    let mut group = c.benchmark_group("counter_registry");

    let mut builder = RegistryBuilder::new();
    let core = builder.register_core();
    for res in &make_plugin_resources() {
        builder.register(res.clone());
    }
    let registry = builder.build();

    // increment by handle — the hottest path (every plugin op)
    group.bench_function("increment_by_handle", |b| {
        b.iter(|| {
            registry.increment(black_box(core.requests), 1);
        });
    });

    // get by name — HashMap lookup (enforcer, quota check)
    group.bench_function("get_by_name", |b| {
        b.iter(|| {
            black_box(registry.get("db.reads"));
        });
    });

    // snapshot — full snapshot (reconciler, warning headers)
    group.bench_function("snapshot_9_resources", |b| {
        // Pre-populate so there's something to snapshot
        registry.increment(core.requests, 100_000);
        registry.increment(core.cpu_us, 50_000);
        b.iter(|| {
            black_box(registry.snapshot());
        });
    });

    // pending_deltas — two-phase flush
    group.bench_function("pending_deltas", |b| {
        b.iter(|| {
            let (deltas, snapshot) = registry.pending_deltas();
            black_box(deltas);
            black_box(snapshot);
        });
    });

    // commit_flush — watermark advance
    group.bench_function("commit_flush", |b| {
        b.iter(|| {
            let (_, snapshot) = registry.pending_deltas();
            registry.commit_flush(black_box(&snapshot));
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// 2. Quota check (point-of-use)
// ---------------------------------------------------------------------------

fn bench_quota_check(c: &mut Criterion) {
    let mut group = c.benchmark_group("quota_check");

    let plugin_resources = make_plugin_resources();
    let registry = Arc::new(MeterRegistry::new(QuotaPlan::free(), plugin_resources));

    // Pre-populate app1: well under the 500K db.reads limit
    let meter1 = registry.get_or_create("app1");
    if let Some(h) = meter1.counters.handle_for("db.reads") {
        meter1.counters.increment(h, 1_000);
    }
    let checker_under = AppQuotaChecker::new(registry.clone(), "app1".to_string());

    group.bench_function("check_under_limit", |b| {
        b.iter(|| {
            black_box(checker_under.check("db.reads")).unwrap();
        });
    });

    // Pre-populate app2: at limit (499_999 — one below)
    let meter2 = registry.get_or_create("app2");
    if let Some(h) = meter2.counters.handle_for("db.reads") {
        meter2.counters.increment(h, 499_999);
    }
    let checker_at = AppQuotaChecker::new(registry.clone(), "app2".to_string());

    group.bench_function("check_at_limit", |b| {
        b.iter(|| {
            black_box(checker_at.check("db.reads")).unwrap();
        });
    });

    // Pre-populate app3: over limit
    let meter3 = registry.get_or_create("app3");
    if let Some(h) = meter3.counters.handle_for("db.reads") {
        meter3.counters.increment(h, 600_000);
    }
    let checker_over = AppQuotaChecker::new(registry.clone(), "app3".to_string());

    group.bench_function("check_over_limit", |b| {
        b.iter(|| {
            let _ = black_box(checker_over.check("db.reads"));
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// 3. Rate limiter
// ---------------------------------------------------------------------------

fn bench_rate_limiter(c: &mut Criterion) {
    let mut group = c.benchmark_group("rate_limiter");

    // High burst so we don't exhaust tokens during the bench
    let limiter = RateLimiter::new(100_000, 500_000);
    // Warm up the bucket for app1
    let _ = limiter.check("app1");

    group.bench_function("check_single_app", |b| {
        b.iter(|| {
            black_box(limiter.check("app1"));
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// 4. SpendAction
// ---------------------------------------------------------------------------

fn bench_spend_action(c: &mut Criterion) {
    let mut group = c.benchmark_group("spend_action");

    let atom = AtomicU8::new(0);

    group.bench_function("load", |b| {
        b.iter(|| {
            black_box(SpendAction::load(&atom));
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// 5. Plugin meter (name-based increment through registry)
// ---------------------------------------------------------------------------

fn bench_plugin_meter(c: &mut Criterion) {
    let mut group = c.benchmark_group("plugin_meter");

    let plugin_resources = make_plugin_resources();
    let registry = Arc::new(MeterRegistry::new(QuotaPlan::free(), plugin_resources));
    let _ = registry.get_or_create("app1");
    let plugin_meter = AppPluginMeter::new(registry.clone(), "app1".to_string());

    group.bench_function("increment_via_plugin_meter", |b| {
        b.iter(|| {
            plugin_meter.increment(black_box("db.reads"), 1);
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// 6. Full pipeline simulation (per-request overhead)
// ---------------------------------------------------------------------------

fn bench_full_pipeline(c: &mut Criterion) {
    let mut group = c.benchmark_group("full_pipeline");

    let plugin_resources = make_plugin_resources();
    let registry = Arc::new(MeterRegistry::new(QuotaPlan::free(), plugin_resources));
    let meter = registry.get_or_create("app1");
    let limiter = RateLimiter::new(1_000_000, 5_000_000); // very high to avoid denial
    let gauge = Arc::new(AtomicU32::new(0));
    let checker = AppQuotaChecker::new(registry.clone(), "app1".to_string());
    let plugin_meter = AppPluginMeter::new(registry.clone(), "app1".to_string());

    // Warm up rate limiter bucket
    let _ = limiter.check("app1");

    group.bench_function("full_request_overhead", |b| {
        b.iter(|| {
            // 1. Rate limit check
            limiter.check(black_box("app1"));
            // 2. Concurrency guard (acquire + RAII drop)
            let _guard = ConcurrencyGuard::try_acquire(&gauge, 1000).unwrap();
            // 3. Spend action check
            let _ = SpendAction::load(&meter.spend_action);
            // 4. Plugin quota check (point of use)
            let _ = checker.check("db.reads");
            // 5. Plugin meter increment
            plugin_meter.increment("db.reads", 1);
            // 6. Core metrics (request done)
            meter.record_request(5_000, 10_000, 1_024, 256);
            // guard drops here
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// 7. Memory / scaling: creating N AppMeters
// ---------------------------------------------------------------------------

fn bench_memory(c: &mut Criterion) {
    let mut group = c.benchmark_group("memory");

    for count in [1, 10, 100, 1000] {
        group.bench_with_input(
            BenchmarkId::new("create_meters", count),
            &count,
            |b, &n| {
                let plugin_resources = make_plugin_resources();
                b.iter(|| {
                    let registry =
                        MeterRegistry::new(QuotaPlan::free(), plugin_resources.clone());
                    for i in 0..n {
                        registry.get_or_create(&format!("app{i}"));
                    }
                    black_box(&registry);
                });
            },
        );
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_counter_registry,
    bench_quota_check,
    bench_rate_limiter,
    bench_spend_action,
    bench_plugin_meter,
    bench_full_pipeline,
    bench_memory,
);
criterion_main!(benches);
