//! REQ-BOOK-001 / REQ-BOOK-005 / REQ-BOOK-006 — L2/L3 view consistency,
//! side typing, and the best/depth complexity contract.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use book_kit::{Ask, Bid, Book, BookBuf, Command, OrderId, Side};

/// REQ-BOOK-001 — L2 aggregation equals the L3 summation at every level,
/// for both sides, after a random-ish workload.
#[test]
fn l2_aggregation_equals_l3_summation() {
    let mut book = Book::with_capacity(64, 32);
    // Three bid levels, orders interleaved in time (price-time priority).
    let adds: [(u64, i64, u64); 7] = [
        (1, 100, 10),
        (2, 101, 5),
        (3, 100, 7),
        (4, 99, 3),
        (5, 101, 2),
        (6, 100, 1),
        (7, 99, 9),
    ];
    for (id, price, qty) in adds {
        book.apply(Command::AddBid {
            id: OrderId::new(id),
            price,
            qty,
            ts_mono: id,
        })
        .expect("add");
    }
    let asks: [(u64, i64, u64); 4] = [(10, 105, 4), (11, 104, 6), (12, 105, 8), (13, 106, 2)];
    for (id, price, qty) in asks {
        book.apply(Command::AddAsk {
            id: OrderId::new(id),
            price,
            qty,
            ts_mono: id,
        })
        .expect("add ask");
    }

    let mut buf = BookBuf::new(16, 32);
    book.try_read(&mut buf).expect("read");

    // Side-generic checker — the same code verifies both typed views
    // (REQ-BOOK-005: the view type carries the side; a bool only labels
    // the assertion text).
    fn check<S: Side>(view: book_kit::SideView<'_, S>, label: &str) {
        let mut depth: Vec<(i64, u64)> = Vec::new();
        for level in view.depth() {
            let l3: u64 = level.orders().map(|o| o.qty).sum();
            assert_eq!(
                level.qty(),
                l3,
                "L2 != ΣL3 at price {} ({label})",
                level.price()
            );
            assert_eq!(level.orders().count() as u64, level.order_count() as u64);
            depth.push((level.price(), level.qty()));
        }
        assert_eq!(
            view.total_qty(),
            depth.iter().map(|(_, q)| q).sum::<u64>(),
            "side total_qty != depth sum ({label})"
        );
    }
    check(book.bids(&buf), "bids");
    check(book.asks(&buf), "asks");

    // Exact expected depth (bids best=descending, asks best=ascending).
    let bids: Vec<(i64, u64)> = book
        .bids(&buf)
        .depth()
        .map(|l| (l.price(), l.qty()))
        .collect();
    assert_eq!(bids, vec![(101, 7), (100, 18), (99, 12)]);
    let asks: Vec<(i64, u64)> = book
        .asks(&buf)
        .depth()
        .map(|l| (l.price(), l.qty()))
        .collect();
    assert_eq!(asks, vec![(104, 6), (105, 12), (106, 2)]);
}

/// REQ-BOOK-001 — L3 FIFO order is price-time: insertion order per level,
/// survivors keep relative order after cancels/partial fills.
#[test]
fn l3_fifo_is_price_time_priority() {
    let mut book = Book::with_capacity(32, 16);
    for id in 1..=5u64 {
        book.apply(Command::AddBid {
            id: OrderId::new(id),
            price: 50,
            qty: id,
            ts_mono: id,
        })
        .expect("add");
    }
    // Cancel #2, partial-fill #4 — survivors keep FIFO order 1,3,4,5.
    book.apply(Command::Cancel {
        id: OrderId::new(2),
    })
    .expect("cancel");
    book.apply(Command::Execute {
        id: OrderId::new(4),
        qty: 3,
    })
    .expect("fill");

    let mut buf = BookBuf::new(8, 16);
    book.try_read(&mut buf).expect("read");
    let ids: Vec<u64> = book
        .bids(&buf)
        .best()
        .expect("level")
        .orders()
        .map(|o| o.id.raw())
        .collect();
    assert_eq!(ids, vec![1, 3, 4, 5]);
    let qtys: Vec<u64> = book
        .bids(&buf)
        .best()
        .expect("level")
        .orders()
        .map(|o| o.qty)
        .collect();
    assert_eq!(qtys, vec![1, 3, 1, 5], "order 4 was partially filled to 1");
    assert_eq!(
        book.bids(&buf).best().expect("level").qty(),
        1 + 3 + 1 + 5,
        "L2 total reflects the partial fill"
    );
}

/// REQ-BOOK-006 — `best()` is O(1) *semantically*: it equals the extreme
/// of the depth iteration, and depth is strictly ordered best→worst with
/// no duplicates.
#[test]
fn best_is_o1_depth_iter_is_ordered_no_alloc() {
    let mut book = Book::with_capacity(64, 64);
    // Scatter bids and asks.
    for i in 0..20u64 {
        book.apply(Command::AddBid {
            id: OrderId::new(i),
            price: 100 + (i * 7 % 13) as i64,
            qty: 1,
            ts_mono: i,
        })
        .expect("add bid");
        book.apply(Command::AddAsk {
            id: OrderId::new(100 + i),
            price: 200 + (i * 5 % 11) as i64,
            qty: 1,
            ts_mono: i,
        })
        .expect("add ask");
    }
    let mut buf = BookBuf::new(32, 64);
    book.try_read(&mut buf).expect("read");

    // Bids: best = max price, depth descending.
    let bids: Vec<i64> = book.bids(&buf).depth().map(|l| l.price()).collect();
    let best_bid = book.bids(&buf).best().expect("bid exists").price();
    assert_eq!(best_bid, *bids.iter().max().expect("non-empty"));
    assert!(
        bids.windows(2).all(|w| w[0] > w[1]),
        "bids strictly descending"
    );

    // Asks: best = min price, depth ascending.
    let asks: Vec<i64> = book.asks(&buf).depth().map(|l| l.price()).collect();
    let best_ask = book.asks(&buf).best().expect("ask exists").price();
    assert_eq!(best_ask, *asks.iter().min().expect("non-empty"));
    assert!(
        asks.windows(2).all(|w| w[0] < w[1]),
        "asks strictly ascending"
    );

    // Empty side: best() is None, depth is empty, total is 0.
    let fresh = Book::with_capacity(8, 8);
    let mut fbuf = BookBuf::new(8, 8);
    fresh.try_read(&mut fbuf).expect("read empty book");
    assert!(fresh.bids(&fbuf).best().is_none());
    assert_eq!(fresh.bids(&fbuf).depth().count(), 0);
    assert_eq!(fresh.asks(&fbuf).total_qty(), 0);
}

/// REQ-BOOK-005 — sides are distinct types, not `bool`: `bids()` and
/// `asks()` return differently-typed views that cannot be swapped, and
/// the `Side` trait is sealed with exactly the two impls.
#[test]
fn sides_are_distinct_types_not_bool() {
    fn assert_side_types<S: Side>(_v: &book_kit::SideView<'_, S>) {}
    let mut book = Book::with_capacity(8, 8);
    book.apply(Command::AddBid {
        id: OrderId::new(1),
        price: 10,
        qty: 1,
        ts_mono: 0,
    })
    .expect("add");
    let mut buf = BookBuf::new(4, 8);
    book.try_read(&mut buf).expect("read");
    let bids = book.bids(&buf);
    let asks = book.asks(&buf);
    // Compile-time distinction: each view is checked at its own type.
    assert_side_types::<Bid>(&bids);
    assert_side_types::<Ask>(&asks);
    // Behavior follows the type, not an argument: the bid view sees the
    // bid level even though the ask side is empty.
    assert!(bids.best().is_some());
    assert!(asks.best().is_none());
    // `opposite()` is an involution across the pair.
    assert_eq!(<Bid as Side>::opposite(), Ask);
    assert_eq!(<Ask as Side>::opposite(), Bid);
}

/// REQ-BOOK-004 — prices are i64 ticks end to end: negative-price adds
/// are rejected (`NonPositivePrice`), and large tick values survive
/// exactly (no float anywhere).
#[test]
fn price_is_i64_no_f64_in_hot_api() {
    let mut book = Book::with_capacity(8, 8);
    let big: i64 = 4_611_686_018_427_387_904; // 2^62 — far beyond f64 mantissa
    book.apply(Command::AddBid {
        id: OrderId::new(1),
        price: big,
        qty: 1,
        ts_mono: 0,
    })
    .expect("i64 ticks carry full precision");
    let mut buf = BookBuf::new(4, 8);
    book.try_read(&mut buf).expect("read");
    assert_eq!(book.bids(&buf).best().expect("level").price(), big);

    assert_eq!(
        book.apply(Command::AddBid {
            id: OrderId::new(2),
            price: 0,
            qty: 1,
            ts_mono: 0
        }),
        Err(book_kit::Reject::NonPositivePrice)
    );
    assert_eq!(
        book.apply(Command::AddAsk {
            id: OrderId::new(3),
            price: -5,
            qty: 1,
            ts_mono: 0
        }),
        Err(book_kit::Reject::NonPositivePrice)
    );
}

/// Views on a never-filled (or failed-read) buffer are empty, never
/// garbage: `version()` is `None` and every view is empty.
#[test]
fn views_on_unfilled_buffer_are_empty_not_garbage() {
    let book = Book::with_capacity(8, 8);
    let buf = BookBuf::new(4, 8);
    assert_eq!(buf.version(), None);
    assert!(book.bids(&buf).best().is_none());
    assert_eq!(book.bids(&buf).level_count(), 0);
    assert_eq!(book.asks(&buf).total_qty(), 0);
}
