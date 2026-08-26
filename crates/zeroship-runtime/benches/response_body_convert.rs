use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};

fn lossy_string_roundtrip(buf: &[u8]) -> Vec<u8> {
    String::from_utf8_lossy(buf).into_owned().into_bytes()
}

fn bytes_to_vec(buf: &[u8]) -> Vec<u8> {
    buf.to_vec()
}

fn mostly_ascii(size: usize) -> Vec<u8> {
    let mut buf = Vec::with_capacity(size);
    for i in 0..size {
        let b = match i % 64 {
            0 => b'\n',
            1 => b'{',
            2 => b'}',
            _ => b'a' + (i % 26) as u8,
        };
        buf.push(b);
    }
    buf
}

fn binary(size: usize) -> Vec<u8> {
    let mut buf = Vec::with_capacity(size);
    for i in 0..size {
        buf.push(((i * 131) ^ (i >> 3) ^ 0xA5) as u8);
    }
    buf
}

fn bench_response_body_convert(c: &mut Criterion) {
    let cases = [
        ("1KiB", 1024usize),
        ("100KiB", 100 * 1024usize),
        ("1MiB", 1024 * 1024usize),
    ];

    let mut group = c.benchmark_group("response_body_convert/mostly_ascii");
    for (name, size) in cases {
        let body = mostly_ascii(size);
        group.bench_with_input(
            BenchmarkId::new("before_lossy_string_roundtrip", name),
            &body,
            |b, body| {
                b.iter(|| {
                    let out = lossy_string_roundtrip(black_box(body));
                    black_box(out);
                });
            },
        );
        group.bench_with_input(BenchmarkId::new("after_to_vec", name), &body, |b, body| {
            b.iter(|| {
                let out = bytes_to_vec(black_box(body));
                black_box(out);
            });
        });
    }
    group.finish();

    let mut group = c.benchmark_group("response_body_convert/binary");
    for (name, size) in cases {
        let body = binary(size);
        group.bench_with_input(
            BenchmarkId::new("before_lossy_string_roundtrip", name),
            &body,
            |b, body| {
                b.iter(|| {
                    let out = lossy_string_roundtrip(black_box(body));
                    black_box(out);
                });
            },
        );
        group.bench_with_input(BenchmarkId::new("after_to_vec", name), &body, |b, body| {
            b.iter(|| {
                let out = bytes_to_vec(black_box(body));
                black_box(out);
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_response_body_convert);
criterion_main!(benches);
