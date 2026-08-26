use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};

fn framed_encode_decode(body: &[u8]) -> usize {
    let headers = [
        (
            "content-type".to_string(),
            "application/octet-stream".to_string(),
        ),
        ("x-request-id".to_string(), "bench-request".to_string()),
    ];
    let frame = zeroship_core::dispatch_frame::encode_dispatch_frame(
        "POST",
        "https://example.test/upload?x=1",
        &headers,
        body,
    )
    .expect("serialize dispatch frame");
    let decoded =
        zeroship_core::dispatch_frame::decode_dispatch_frame(&frame).expect("decode dispatch frame");
    decoded.metadata.method.len()
        + decoded.metadata.url.len()
        + decoded.metadata.headers.len()
        + decoded.body.len()
}

fn bench_dispatch_envelope(c: &mut Criterion) {
    let cases = [
        ("1KiB", 1024usize),
        ("100KiB", 100 * 1024usize),
        ("1MiB", 1024 * 1024usize),
    ];

    let mut group = c.benchmark_group("dispatch_envelope/encode_decode");
    for (name, size) in cases {
        let body = vec![b'a'; size];
        group.bench_with_input(BenchmarkId::from_parameter(name), &body, |b, body| {
            b.iter(|| {
                let observed = framed_encode_decode(black_box(body));
                black_box(observed);
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_dispatch_envelope);
criterion_main!(benches);
