#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Criterion baselines for the hot paths (spec §Gate plan):
//! `apply(Add|Cancel|Execute)` ns/op and `read()` snapshot ns/op at three
//! depth points. Contention-level degradation (reader lapping under a hot
//! writer) is called out in the README as bench-visible; these baselines
//! run in CI as a smoke gate (`cargo bench -- --test`), never as a perf
//! regression gate.

use book_kit::{Book, BookBuf, Command, OrderId};
use criterion::{black_box, criterion_group, criterion_main, BatchSize, Criterion};

fn bench_apply(c: &mut Criterion) {
    c.bench_function("apply_add", |b| {
        let mut book = Book::with_capacity(4096, 512);
        let mut id = 0u64;
        b.iter(|| {
            let cmd = Command::AddBid {
                id: OrderId::new(black_box(id)),
                price: black_box(100 + (id % 64) as i64),
                qty: black_box(10),
                ts_mono: black_box(id),
            };
            id += 1;
            let _ = book.apply(cmd);
        });
    });

    c.bench_function("apply_cancel_readd", |b| {
        let mut book = Book::with_capacity(4096, 512);
        for i in 0..1024u64 {
            let _ = book.apply(Command::AddBid {
                id: OrderId::new(i),
                price: 100 + (i % 64) as i64,
                qty: 1,
                ts_mono: i,
            });
        }
        let mut round = 0u64;
        b.iter_batched(
            || {
                let id = OrderId::new(black_box(round % 1024));
                round += 1;
                id
            },
            |id| {
                let _ = book.apply(Command::Cancel { id });
                let _ = book.apply(Command::AddBid {
                    id,
                    price: 100 + (id.raw() % 64) as i64,
                    qty: 1,
                    ts_mono: id.raw(),
                });
            },
            BatchSize::SmallInput,
        );
    });

    c.bench_function("apply_execute", |b| {
        let mut book = Book::with_capacity(4096, 512);
        // A pool of resting orders; the bench partial-fills each in
        // turn (qty 100 per fill of 7 keeps every order resting, so the
        // workload is a steady-state pure `Execute`).
        for i in 0..4096u64 {
            let _ = book.apply(Command::AddBid {
                id: OrderId::new(i),
                price: 200,
                qty: 700,
                ts_mono: 0,
            });
        }
        let mut next = 0u64;
        b.iter(|| {
            let id = OrderId::new(black_box(next % 4096));
            next += 1;
            let _ = book.apply(Command::Execute {
                id,
                qty: black_box(7),
            });
        });
    });
}

fn bench_read(c: &mut Criterion) {
    let mut group = c.benchmark_group("read_snapshot");
    for levels in [1usize, 16, 128] {
        // Build the book first; the writer is done (and its `&mut`
        // finished) before any reader exists — the single-writer contract.
        let mut book = Book::with_capacity(4096, 512);
        for i in 0..levels {
            book.apply(Command::AddBid {
                id: OrderId::new(i as u64),
                price: 100 + i as i64,
                qty: 1,
                ts_mono: i as u64,
            })
            .unwrap();
            book.apply(Command::AddAsk {
                id: OrderId::new((512 + i) as u64),
                price: 100_000 + i as i64,
                qty: 1,
                ts_mono: i as u64,
            })
            .unwrap();
        }
        let reader = book.reader();
        let mut buf = BookBuf::new(256, 1024);
        group.bench_function(format!("depth_{levels}_levels"), |b| {
            b.iter(|| {
                black_box(reader.try_read(&mut buf).ok());
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_apply, bench_read);
criterion_main!(benches);
