use bytes::Buf;
use criterion::{Criterion, criterion_group, criterion_main};
use librqbit_peer_protocol::DoubleBufHelper;

use std::hint::black_box as bb;

fn make_bufs() -> (usize, [&'static [u8]; 2]) {
    (257, [&[0u8; 514], &[0u8; 514]])
}

pub fn criterion_benchmark(c: &mut Criterion) {
    c.bench_function("DoubleBufHelper::read_u32_be", |b| {
        let (size, [b1, b2]) = make_bufs();
        b.iter(bb(|| {
            let mut b = DoubleBufHelper::new(bb(b1), bb(b2));
            for _ in 0..size {
                unsafe { bb(b.read_u32_be()).unwrap_unchecked() };
            }
        }))
    });

    c.bench_function("Chain::read_u32_be", |b| {
        let (size, [b1, b2]) = make_bufs();
        b.iter(|| {
            let mut b = bb(b1).chain(bb(b2));
            for _ in 0..size {
                unsafe { bb(b.try_get_u32_ne()).unwrap_unchecked() };
            }
        })
    });
}

criterion_group!(benches, criterion_benchmark);
criterion_main!(benches);
