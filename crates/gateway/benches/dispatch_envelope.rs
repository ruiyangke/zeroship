use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};

#[derive(serde::Deserialize)]
struct HttpEnvelope {
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    #[serde(default)]
    body: String,
}

fn old_json_string_encode_decode(body: &[u8]) -> usize {
    let body_str = String::from_utf8_lossy(body);
    let envelope = serde_json::json!({
        "method": "POST",
        "url": "https://example.test/upload?x=1",
        "headers": [
            ["content-type", "application/octet-stream"],
            ["x-request-id", "bench-request"],
        ],
        "body": &*body_str,
    });
    let envelope_bytes = serde_json::to_vec(&envelope).expect("serialize envelope");
    let decoded: HttpEnvelope = serde_json::from_slice(&envelope_bytes).expect("decode envelope");
    decoded.method.len() + decoded.url.len() + decoded.headers.len() + decoded.body.len()
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
                let observed = old_json_string_encode_decode(black_box(body));
                black_box(observed);
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_dispatch_envelope);
criterion_main!(benches);
