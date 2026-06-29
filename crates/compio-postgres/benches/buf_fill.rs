#![allow(dead_code)]

use std::io;
use std::time::Duration;

use compio::buf::{BufResult, IoBuf, IoBufMut};
use compio::io::{AsyncRead, AsyncWrite};
use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};

#[derive(Debug)]
pub struct Error(io::Error);

impl Error {
    pub(crate) fn io(error: io::Error) -> Self {
        Self(error)
    }
}

#[path = "../src/buf_stream.rs"]
mod buf_stream;

const READ_CHUNK: usize = 16 * 1024;

struct ChunkedReadStream<'a> {
    remaining: &'a [u8],
}

impl<'a> ChunkedReadStream<'a> {
    fn new(payload: &'a [u8]) -> Self {
        Self { remaining: payload }
    }
}

impl AsyncRead for ChunkedReadStream<'_> {
    async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
        self.remaining.read(buf).await
    }
}

impl AsyncWrite for ChunkedReadStream<'_> {
    async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
        BufResult(Ok(buf.buf_len()), buf)
    }

    async fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }

    async fn shutdown(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn payload(size: usize) -> Vec<u8> {
    (0..size)
        .map(|i| ((i.wrapping_mul(131) ^ (i >> 7) ^ 0xA5) & 0xFF) as u8)
        .collect()
}

fn fill_payload(rt: &compio::runtime::Runtime, body: &[u8]) -> usize {
    let stream = ChunkedReadStream::new(body);
    let mut stream = buf_stream::BufStream::new(stream);
    rt.block_on(async {
        stream.fill(body.len()).await.expect("fill payload");
        black_box(stream.buf().len())
    })
}

fn bench_buf_fill(c: &mut Criterion) {
    let rt = compio::runtime::Runtime::new().expect("compio runtime");
    let mut group = c.benchmark_group("compio_postgres/buf_stream/fill");
    group.sample_size(20);
    group.warm_up_time(Duration::from_millis(500));
    group.measurement_time(Duration::from_secs(3));

    for size in [1024 * 1024, 16 * 1024 * 1024] {
        let body = payload(size);
        let label = format!("{}MiB/{}x16KiB", size / (1024 * 1024), size / READ_CHUNK);
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(label), &body, |b, body| {
            b.iter(|| {
                let len = fill_payload(&rt, black_box(body));
                assert_eq!(len, body.len());
            });
        });
    }

    group.finish();
}

criterion_group!(benches, bench_buf_fill);
criterion_main!(benches);
