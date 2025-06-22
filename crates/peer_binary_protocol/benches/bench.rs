use criterion::{Criterion, criterion_group, criterion_main};
use librqbit_peer_protocol::DoubleBufHelper;

pub fn criterion_benchmark(c: &mut Criterion) {
    c.bench_function("DoubleBufHelper::read_u32_be_normal", |b| {
        b.iter(|| std::hint::black_box(DoubleBufHelper::new(&[0, 0, 0, 42], &[]).read_u32_be()))
    });

    c.bench_function("DoubleBufHelper::read_u32_be_split", |b| {
        b.iter(|| std::hint::black_box(DoubleBufHelper::new(&[0, 0], &[0, 42]).read_u32_be()))
    });

    c.bench_function("DoubleBufHelper::read_u8_first", |b| {
        b.iter(|| std::hint::black_box(DoubleBufHelper::new(&[42], &[]).read_u8()))
    });

    c.bench_function("DoubleBufHelper::read_u8_second", |b| {
        b.iter(|| std::hint::black_box(DoubleBufHelper::new(&[], &[42]).read_u32_be()))
    });
}

criterion_group!(benches, criterion_benchmark);
criterion_main!(benches);
