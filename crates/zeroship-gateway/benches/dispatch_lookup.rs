use std::collections::HashMap;

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use zeroship_bundle::{
    AuthLevel, Manifest, ProcedureKind, RateLimit, RateLimitPer, ResourceEntry, StaticAction,
};
use zeroship_gateway::compiled::CompiledManifest;

struct Case {
    name: &'static str,
    path: &'static str,
}

fn rate_limit(rpm: u32) -> RateLimit {
    RateLimit {
        rpm: Some(rpm),
        rps: None,
        per: RateLimitPer::Ip,
    }
}

fn resource_entry(rpm: u32) -> ResourceEntry {
    ResourceEntry {
        auth: Some(AuthLevel::Anon),
        publicly_accessible: Some(true),
        rate_limit: Some(rate_limit(rpm)),
        ..Default::default()
    }
}

fn compiled_manifest() -> CompiledManifest {
    let mut resources = HashMap::new();

    resources.insert(
        "*".to_string(),
        ResourceEntry {
            auth: Some(AuthLevel::Anon),
            publicly_accessible: Some(true),
            ..Default::default()
        },
    );

    for i in 0..96 {
        resources.insert(format!("/api/v1/items/{i:03}"), resource_entry(1_000 + i));
    }
    for i in 0..32 {
        resources.insert(
            format!("/api/v1/deep/area/feature/item/{i:03}"),
            resource_entry(2_000 + i),
        );
    }

    for section in [
        "blog",
        "docs",
        "teams",
        "projects",
        "reports",
        "billing",
        "settings",
        "admin",
    ] {
        resources.insert(format!("/{section}/[slug]"), resource_entry(3_000));
        resources.insert(format!("/{section}/[...rest]"), resource_entry(3_100));
    }

    for rpc in [
        ("todos.list", ProcedureKind::Query),
        ("todos.add", ProcedureKind::Mutation),
        ("billing.charge", ProcedureKind::Mutation),
        ("reports.stream", ProcedureKind::Stream),
    ] {
        resources.insert(
            format!("rpc:{}", rpc.0),
            ResourceEntry {
                kind: Some(rpc.1),
                rate_limit: Some(rate_limit(4_000)),
                ..Default::default()
            },
        );
    }

    resources.insert(
        "/static/logo.svg".to_string(),
        ResourceEntry {
            r#static: Some(StaticAction {
                r#try: vec!["/static/logo.svg".to_string()],
            }),
            rate_limit: Some(rate_limit(5_000)),
            ..Default::default()
        },
    );

    let manifest = Manifest {
        version: 1,
        resources,
        ..Manifest::default()
    };
    CompiledManifest::compile(&manifest)
}

fn bench_dispatch_lookup(c: &mut Criterion) {
    let manifest = compiled_manifest();
    let cases = [
        Case {
            name: "literal_hit",
            path: "/api/v1/items/042",
        },
        Case {
            name: "glob_hit",
            path: "/blog/performance-notes",
        },
        Case {
            name: "deep_canonicalized_hit",
            path: "/api/v1/deep/area/./feature/item/007/",
        },
        Case {
            name: "rpc_hit",
            path: "/__zeroship/v1/todos.list",
        },
    ];

    let mut group = c.benchmark_group("dispatch_lookup/resource_resolution");
    for case in cases {
        group.bench_with_input(BenchmarkId::from_parameter(case.name), &case.path, |b, path| {
            b.iter(|| {
                let resolved = manifest
                    .lookup_resource_resolved(black_box(path))
                    .expect("benchmark path resolves");
                black_box(resolved.policy.rate_limit.as_ref().map(|rl| rl.rpm));
                black_box(resolved.key.as_ref().len());
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_dispatch_lookup);
criterion_main!(benches);
