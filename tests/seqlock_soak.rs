//! REQ-BOOK-002 — `reader_never_sees_torn_state_under_hot_writer`: real
//! threads, a hot single writer, many lock-free readers. Every snapshot
//! any reader completes must be version-consistent: L2 totals equal L3
//! sums *within the same buffer*, the captured version is monotone
//! per-reader, and depth is ordered. Torn reads surface as typed `Torn`,
//! never as observed garbage.
//!
//! Thread shape (the documented usage): the main test thread OWNS the
//! `Book` and hammers `apply` (the single writer); reader handles from
//! `Book::reader()` live on the other threads.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use book_kit::{Book, BookBuf, Command, OrderId, Side, SideView, Torn};

/// Side-generic consistency check on a completed snapshot: L2 == ΣL3 per
/// level and the side total equals the depth sum (REQ-BOOK-001).
fn check_l2_l3<S: Side>(view: SideView<'_, S>, r: usize) {
    let mut total = 0u64;
    for level in view.depth() {
        let l3: u64 = level.orders().map(|o| o.qty).sum();
        assert_eq!(
            level.qty(),
            l3,
            "reader {r}: torn L2/L3 at price {}",
            level.price()
        );
        total += level.qty();
    }
    assert_eq!(
        view.total_qty(),
        total,
        "reader {r}: torn side total ({})",
        if S::is_bid() { "bids" } else { "asks" }
    );
}

/// Side-generic depth ordering: bids strictly descending, asks strictly
/// ascending (REQ-BOOK-006).
fn check_depth_order<S: Side>(view: SideView<'_, S>, r: usize) {
    let prices: Vec<i64> = view.depth().map(|l| l.price()).collect();
    if S::is_bid() {
        assert!(
            prices.windows(2).all(|w| w[0] > w[1]),
            "reader {r}: unordered bid depth {prices:?}"
        );
    } else {
        assert!(
            prices.windows(2).all(|w| w[0] < w[1]),
            "reader {r}: unordered ask depth {prices:?}"
        );
    }
}

#[test]
fn reader_never_sees_torn_state_under_hot_writer() {
    const WRITE_MS: u64 = 700;
    const READER_COUNT: usize = 4;

    // The single writer owns the book for the whole test.
    let mut book = Book::with_capacity(512, 64);
    for i in 0..8u64 {
        book.apply(Command::AddBid {
            id: OrderId::new(i),
            price: 100 + i as i64,
            qty: 10,
            ts_mono: i,
        })
        .expect("seed bid");
        book.apply(Command::AddAsk {
            id: OrderId::new(50 + i),
            price: 200 + i as i64,
            qty: 10,
            ts_mono: i,
        })
        .expect("seed ask");
    }

    let stop = Arc::new(AtomicU64::new(0));

    // Readers attach first, then the writer churns — real contention.
    let mut readers = Vec::new();
    for r in 0..READER_COUNT {
        let reader = book.reader();
        let stop = Arc::clone(&stop);
        readers.push(thread::spawn(move || {
            let mut buf = BookBuf::new(64, 512);
            let mut last_version = 0u64;
            let mut snapshots = 0u64;
            let mut torn_rejections = 0u64;
            while stop.load(Ordering::Relaxed) == 0 {
                match reader.read(&mut buf, 64) {
                    Ok(v) => {
                        assert!(
                            v >= last_version,
                            "reader {r}: version went backwards: {v} < {last_version}"
                        );
                        last_version = v;
                        snapshots += 1;
                        check_l2_l3(reader.bids(&buf), r);
                        check_l2_l3(reader.asks(&buf), r);
                        check_depth_order(reader.bids(&buf), r);
                        check_depth_order(reader.asks(&buf), r);
                    }
                    Err(t) => {
                        // Typed rejection — this is what "never observe
                        // torn state" means: the reader is TOLD, and the
                        // buffer is not consulted.
                        assert!(matches!(t, Torn::WriteInProgress | Torn::VersionChanged));
                        torn_rejections += 1;
                    }
                }
            }
            (snapshots, torn_rejections, last_version)
        }));
    }

    // ---- the single writer: churn storm (arena-balanced: every add is
    // paired with a retirement so `ArenaFull` can never panic the writer
    // and strand the readers) ----
    let deadline = Instant::now() + Duration::from_millis(WRITE_MS);
    let mut i = 1000u64;
    let mut applies = 0u64;
    while Instant::now() < deadline {
        book.apply(Command::AddBid {
            id: OrderId::new(i),
            price: 100 + (i % 9) as i64,
            qty: 1 + (i % 5),
            ts_mono: i,
        })
        .expect("hot add bid");
        book.apply(Command::AddAsk {
            id: OrderId::new(i + 100_000),
            price: 200 + (i % 7) as i64,
            qty: 1 + (i % 3),
            ts_mono: i,
        })
        .expect("hot add ask");
        let _ = book.apply(Command::Cancel {
            id: OrderId::new(i - 16),
        });
        let _ = book.apply(Command::Cancel {
            id: OrderId::new(i + 100_000 - 16),
        });
        if i % 3 == 0 {
            let _ = book.apply(Command::Execute {
                id: OrderId::new(i - 8),
                qty: 1,
            });
        }
        if i % 5 == 0 {
            let _ = book.apply(Command::Replace {
                id: OrderId::new(i - 12),
                new_price: 105,
                new_qty: 7,
            });
        }
        i += 1;
        applies += 4;
    }
    stop.store(1, Ordering::Relaxed);

    // Writer-side integrity: the live book must never contain cross-side
    // prices (asks ≥ 200, bids ≤ 108 in this churn pattern).
    {
        let mut wbuf = BookBuf::new(64, 512);
        book.try_read(&mut wbuf).expect("writer post-read");
        for level in book.asks(&wbuf).depth() {
            assert!(
                level.price() >= 200,
                "writer state corrupted: ask level at {}",
                level.price()
            );
            let l3: u64 = level.orders().map(|o| o.qty).sum();
            assert_eq!(
                level.qty(),
                l3,
                "writer state inconsistent at {}",
                level.price()
            );
        }
        for level in book.bids(&wbuf).depth() {
            let l3: u64 = level.orders().map(|o| o.qty).sum();
            assert_eq!(
                level.qty(),
                l3,
                "writer state inconsistent at {}",
                level.price()
            );
        }
    }

    // ---- aggregate reader outcomes ----
    let mut total_snapshots = 0u64;
    let mut total_torn = 0u64;
    let mut max_version = 0u64;
    for h in readers {
        let (snaps, torn, ver) = h.join().expect("reader joined");
        total_snapshots += snaps;
        total_torn += torn;
        max_version = max_version.max(ver);
    }
    assert!(applies > 0, "writer ran no commands");
    assert!(
        total_snapshots >= READER_COUNT as u64 * 10,
        "soak made no progress: {total_snapshots} snapshots"
    );
    assert!(
        max_version >= 2,
        "writer progress invisible to readers (max version {max_version})"
    );
    // A torn rejection at least once proves contention was real; a quiet
    // run on a very fast machine is legal, so this stays informational.
    let _ = total_torn;
}
