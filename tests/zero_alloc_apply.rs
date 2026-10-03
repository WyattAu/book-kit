//! REQ-BOOK-008 — the zero-allocation gate: a counting global allocator
//! proves `apply` (all five commands, accept AND reject paths),
//! `try_read`/`read`, checksum, and the view iterators never touch the
//! heap once the book and buffer exist.
//!
//! Mirrors the shm-rings `zero_alloc_ring_ops` pattern (a counting
//! allocator cannot lie about code reading).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};

use book_kit::{Book, BookBuf, Command, OrderId};

/// Counting wrapper over the system allocator.
struct Counting;

static ALLOCS: AtomicU64 = AtomicU64::new(0);
static DEALLOCS: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        DEALLOCS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn alloc_delta<T>(f: impl FnOnce() -> T) -> (T, u64) {
    let before = ALLOCS.load(Ordering::Relaxed);
    let out = f();
    let after = ALLOCS.load(Ordering::Relaxed);
    (out, after - before)
}

/// Absorb the harness's lazy one-off allocations (libtest capture setup,
/// format machinery, thread-locals) so the measured windows are quiet.
fn warmup() {
    for _ in 0..25 {
        let (_, d) = alloc_delta(|| {
            let s = format!("warm {}", 1);
            std::hint::black_box(s.len());
            ALLOCS.fetch_add(0, Ordering::Relaxed);
        });
        if d == 0 {
            // Two consecutive quiet windows: warm enough.
            let (_, d2) = alloc_delta(|| ());
            if d2 == 0 {
                return;
            }
        }
    }
}

/// `apply_paths_zero_alloc` — every command, every path, zero heap.
#[test]
fn apply_paths_zero_alloc() {
    warmup();
    let mut book = Book::with_capacity(64, 32);
    let mut buf = BookBuf::new(32, 64);

    // Warm-up + fill (these allocations are construction, allowed).
    for i in 0..20u64 {
        book.apply(Command::AddBid {
            id: OrderId::new(i),
            price: 100 + (i % 7) as i64,
            qty: 10 + i,
            ts_mono: i,
        })
        .expect("add bid");
        book.apply(Command::AddAsk {
            id: OrderId::new(100 + i),
            price: 200 + (i % 5) as i64,
            qty: 3 + i,
            ts_mono: i,
        })
        .expect("add ask");
    }
    book.try_read(&mut buf).expect("warm read");

    // -- Add (new level, front-insert + back-insert): zero allocs. --
    let (_, d) = alloc_delta(|| {
        book.apply(Command::AddBid {
            id: OrderId::new(1000),
            price: 500, // new best level: front insert
            qty: 1,
            ts_mono: 0,
        })
        .expect("front add");
        book.apply(Command::AddBid {
            id: OrderId::new(1001),
            price: 90, // new worst level: back insert
            qty: 1,
            ts_mono: 0,
        })
        .expect("back add");
        book.apply(Command::AddBid {
            id: OrderId::new(1002),
            price: 100, // existing level append
            qty: 1,
            ts_mono: 0,
        })
        .expect("append add");
    });
    assert_eq!(d, 0, "apply(Add) allocated");

    // -- Replace (price-move across levels + qty-only): zero allocs. --
    let (_, d) = alloc_delta(|| {
        book.apply(Command::Replace {
            id: OrderId::new(1002),
            new_price: 300,
            new_qty: 5,
        })
        .expect("price replace");
        book.apply(Command::Replace {
            id: OrderId::new(1002),
            new_price: 300,
            new_qty: 7,
        })
        .expect("qty replace");
    });
    assert_eq!(d, 0, "apply(Replace) allocated");

    // -- Execute (partial + full consume): zero allocs. --
    let (_, d) = alloc_delta(|| {
        book.apply(Command::Execute {
            id: OrderId::new(1002),
            qty: 2,
        })
        .expect("partial fill");
        book.apply(Command::Execute {
            id: OrderId::new(1002),
            qty: 99,
        })
        .expect("full consume");
    });
    assert_eq!(d, 0, "apply(Execute) allocated");

    // -- Cancel (including the level-emptying removal): zero allocs. --
    let (_, d) = alloc_delta(|| {
        book.apply(Command::Cancel {
            id: OrderId::new(1000),
        })
        .expect("cancel");
        book.apply(Command::Cancel {
            id: OrderId::new(1001),
        })
        .expect("cancel");
    });
    assert_eq!(d, 0, "apply(Cancel) allocated");

    // -- Reject paths: zero allocs (validations are pure). --
    let (_, d) = alloc_delta(|| {
        for cmd in [
            Command::Cancel {
                id: OrderId::new(9999),
            },
            // id 5 is still resting (seed fill): DuplicateId.
            Command::AddBid {
                id: OrderId::new(5),
                price: 1,
                qty: 1,
                ts_mono: 0,
            },
            Command::AddBid {
                id: OrderId::new(2000),
                price: 0,
                qty: 1,
                ts_mono: 0,
            },
            Command::AddBid {
                id: OrderId::new(2001),
                price: 5,
                qty: 0,
                ts_mono: 0,
            },
            Command::Execute {
                id: OrderId::new(9999),
                qty: 1,
            },
            Command::Replace {
                id: OrderId::new(9999),
                new_price: 1,
                new_qty: 1,
            },
        ] {
            assert!(book.apply(cmd).is_err(), "expected reject");
        }
    });
    assert_eq!(d, 0, "apply(reject path) allocated");

    // -- Snapshot reads: zero allocs. --
    let (_, d) = alloc_delta(|| {
        for _ in 0..100 {
            book.try_read(&mut buf).expect("read");
            let bids = book.bids(&buf);
            let _ = bids.best().map(|l| {
                for o in l.orders() {
                    std::hint::black_box((o.id, o.qty));
                }
                (l.price(), l.qty())
            });
            for level in bids.depth() {
                std::hint::black_box((level.price(), level.qty(), level.order_count()));
            }
            let _ = bids.total_qty();
        }
    });
    assert_eq!(d, 0, "try_read + views allocated");

    // -- Bounded-retry read + checksum + introspection: zero allocs. --
    let (_, d) = alloc_delta(|| {
        book.read(&mut buf, 8).expect("bounded read");
        let _ = book.checksum();
        let idx = book.gen_index_of(OrderId::new(5)).expect("resting");
        let _ = book.order_at(idx).expect("resolve");
        let _ = book.version();
    });
    assert_eq!(d, 0, "read/checksum/introspection allocated");

    // -- Generational churn (free + recycle cycles): zero allocs — the
    // ABA machinery is pure slab bookkeeping. --
    let mut small = Book::with_capacity(4, 4);
    for round in 0..100u64 {
        let (_, d) = alloc_delta(|| {
            small
                .apply(Command::AddBid {
                    id: OrderId::new(round),
                    price: 10 + (round % 2) as i64,
                    qty: 1,
                    ts_mono: round,
                })
                .expect("add");
            small
                .apply(Command::Cancel {
                    id: OrderId::new(round),
                })
                .expect("cancel");
        });
        assert_eq!(d, 0, "add/cancel churn allocated in round {round}");
    }
}
