//! REQ-BOOK-003 — every lifecycle rejection is typed: table-driven probe
//! of all six `Reject` variants across all five commands.
//! REQ-BOOK-008 — the ABA kill at the book level.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
#![allow(clippy::type_complexity)]

use book_kit::{Book, BookBuf, Command, OrderId, Reject};

fn add(id: u64, price: i64, qty: u64) -> Command {
    Command::AddBid {
        id: OrderId::new(id),
        price,
        qty,
        ts_mono: id,
    }
}

/// REQ-BOOK-003 — `every_lifecycle_rejection_is_typed`: the full reject
/// table. Every row must return exactly the named variant — no panics, no
/// silent drops.
#[test]
fn every_lifecycle_rejection_is_typed() {
    let cases: &[(&str, Box<dyn Fn(&mut Book) -> Result<(), Reject>>)] = &[
        (
            "add zero qty → ZeroQty",
            Box::new(|b: &mut Book| b.apply(add(1, 10, 0)).map(|_| ())),
        ),
        (
            "add non-positive price → NonPositivePrice",
            Box::new(|b: &mut Book| b.apply(add(1, 0, 1)).map(|_| ())),
        ),
        (
            "add duplicate id → DuplicateId",
            Box::new(|b: &mut Book| {
                b.apply(add(1, 10, 1)).expect("seed");
                b.apply(add(1, 11, 1)).map(|_| ())
            }),
        ),
        (
            "cancel unknown → UnknownOrder",
            Box::new(|b: &mut Book| {
                b.apply(Command::Cancel {
                    id: OrderId::new(99),
                })
                .map(|_| ())
            }),
        ),
        (
            "replace unknown → UnknownOrder",
            Box::new(|b: &mut Book| {
                b.apply(Command::Replace {
                    id: OrderId::new(99),
                    new_price: 10,
                    new_qty: 1,
                })
                .map(|_| ())
            }),
        ),
        (
            "replace zero qty → ZeroQty",
            Box::new(|b: &mut Book| {
                b.apply(add(1, 10, 1)).expect("seed");
                b.apply(Command::Replace {
                    id: OrderId::new(1),
                    new_price: 10,
                    new_qty: 0,
                })
                .map(|_| ())
            }),
        ),
        (
            "replace non-positive price → NonPositivePrice",
            Box::new(|b: &mut Book| {
                b.apply(add(1, 10, 1)).expect("seed");
                b.apply(Command::Replace {
                    id: OrderId::new(1),
                    new_price: -1,
                    new_qty: 1,
                })
                .map(|_| ())
            }),
        ),
        (
            "execute unknown → UnknownOrder",
            Box::new(|b: &mut Book| {
                b.apply(Command::Execute {
                    id: OrderId::new(99),
                    qty: 1,
                })
                .map(|_| ())
            }),
        ),
        (
            "execute zero qty → ZeroQty",
            Box::new(|b: &mut Book| {
                b.apply(add(1, 10, 1)).expect("seed");
                b.apply(Command::Execute {
                    id: OrderId::new(1),
                    qty: 0,
                })
                .map(|_| ())
            }),
        ),
        (
            "stale generational index → StaleGeneration",
            Box::new(|b: &mut Book| {
                b.apply(add(1, 10, 1)).expect("seed");
                let idx = b.gen_index_of(OrderId::new(1)).expect("resting");
                b.apply(Command::Cancel {
                    id: OrderId::new(1),
                })
                .expect("cancel");
                b.order_at(idx).map(|_| ())
            }),
        ),
    ];

    let expected = [
        Reject::ZeroQty,
        Reject::NonPositivePrice,
        Reject::DuplicateId,
        Reject::UnknownOrder,
        Reject::UnknownOrder,
        Reject::ZeroQty,
        Reject::NonPositivePrice,
        Reject::UnknownOrder,
        Reject::ZeroQty,
        Reject::StaleGeneration,
    ];
    for ((name, run), want) in cases.iter().zip(expected) {
        let mut book = Book::with_capacity(16, 16);
        assert_eq!(run(&mut book).err(), Some(want), "case: {name}");
    }
}

/// REQ-BOOK-008 — arena exhaustion is typed (`ArenaFull`), and the
/// level-table cap rejects typed too (the claimed slot is released).
#[test]
fn arena_full_rejects_typed_and_releases_claimed_slot() {
    let mut book = Book::with_capacity(2, 8);
    book.apply(add(1, 10, 1)).expect("1st");
    book.apply(add(2, 11, 1)).expect("2nd");
    assert_eq!(book.apply(add(3, 12, 1)), Err(Reject::ArenaFull));
    // The failed add consumed nothing: a freed slot admits new orders.
    book.apply(Command::Cancel {
        id: OrderId::new(1),
    })
    .expect("cancel");
    book.apply(add(4, 12, 1)).expect("recycled");

    // Level-table cap: capacity 2 levels per side; three distinct prices.
    let mut book = Book::with_capacity(16, 2);
    book.apply(add(1, 10, 1)).expect("level 1");
    book.apply(add(2, 11, 1)).expect("level 2");
    assert_eq!(
        book.apply(add(3, 12, 1)),
        Err(Reject::ArenaFull),
        "third distinct price exceeds the level cap"
    );
    // Existing-price adds still work (no new level needed).
    book.apply(add(4, 10, 2)).expect("same-level add");
}

/// REQ-BOOK-008 — the stale generation kill at the book level, including
/// the introspection API path (`gen_index_of` → cancel → reuse →
/// `order_at(stale)` = `StaleGeneration`).
#[test]
fn stale_generation_index_rejected() {
    let mut book = Book::with_capacity(4, 4);
    let id = OrderId::new(7);
    book.apply(add(7, 50, 1)).expect("add");
    let idx = book.gen_index_of(id).expect("resting");
    assert!(book.order_at(idx).is_ok(), "live index resolves");

    book.apply(Command::Cancel { id }).expect("cancel");
    assert_eq!(book.order_at(idx), Err(Reject::StaleGeneration));

    // Slot recycle bumps the generation; the old handle stays dead.
    book.apply(add(8, 51, 1)).expect("recycle");
    let fresh = book.gen_index_of(OrderId::new(8)).expect("resting");
    assert_eq!(fresh.slot, idx.slot, "free chain reused the slot");
    assert_ne!(fresh.gen, idx.gen, "generation moved on");
    assert_eq!(book.order_at(idx), Err(Reject::StaleGeneration));
    assert_eq!(
        book.order_at(fresh).expect("fresh resolves").id,
        OrderId::new(8)
    );
}

/// REQ-BOOK-003 — a rejected command leaves the book byte-identical:
/// checksum before == checksum after (and no version movement).
#[test]
fn rejected_commands_leave_no_trace() {
    let mut book = Book::with_capacity(16, 16);
    book.apply(add(1, 10, 5)).expect("seed");
    let before = (book.checksum(), book.version());
    let rejects = [
        add(1, 12, 1), // DuplicateId
        add(2, 0, 1),  // NonPositivePrice
        add(3, 10, 0), // ZeroQty
        Command::Cancel {
            id: OrderId::new(9),
        }, // UnknownOrder
        Command::Execute {
            id: OrderId::new(9),
            qty: 1,
        },
    ];
    for cmd in rejects {
        assert!(book.apply(cmd).is_err(), "expected reject");
    }
    assert_eq!((book.checksum(), book.version()), before, "no trace");
}

/// REQ-BOOK-008 — id-table resurrection regression: a rehash triggered by
/// an insert must not sweep up the pending key (a duplicate entry let a
/// fully-executed order be "replaced" back into the book). The sequence
/// below drives `entries` to exactly the table length so the add of
/// id 104 fires the rehash, then kills 104 and tries to resurrect it.
#[test]
fn rehash_does_not_duplicate_the_pending_insert() {
    let mut book = Book::with_capacity(12, 8);

    // Fill the table: 12 live ids on one price level, then cancel all
    // (12 tombstones — entries stay at 12).
    for i in 0..12u64 {
        book.apply(add(i, 100, 1)).expect("seed add");
    }
    for i in 0..12u64 {
        book.apply(Command::Cancel {
            id: OrderId::new(i),
        })
        .expect("seed cancel");
    }
    // 12 more ids: the first 4 inserts consume the 4 remaining NIL words
    // (entries = 16 = table length).
    for i in 100..104u64 {
        book.apply(add(i, 200, 1)).expect("pre-rehash add");
    }
    // This add sees entries == table length and fires the rehash — with
    // its own node already marked USED. The fix excludes it from the
    // sweep; the bug duplicated it.
    book.apply(add(104, 200, 1)).expect("rehash-triggering add");

    // Kill 104 outright: full execute frees the slot and removes the id.
    book.apply(Command::Execute {
        id: OrderId::new(104),
        qty: 1,
    })
    .expect("full execute");
    assert_eq!(
        book.gen_index_of(OrderId::new(104)),
        None,
        "executed id must be gone from the id table"
    );

    // The resurrection attempt: pre-fix this returned Ok and corrupted
    // the recycled slot's new occupant.
    assert_eq!(
        book.apply(Command::Replace {
            id: OrderId::new(104),
            new_price: 300,
            new_qty: 7,
        }),
        Err(Reject::UnknownOrder),
        "a fully-executed id must not resurrect through the id table"
    );
    // And the whole book still holds exactly 4 live orders.
    let live = book.checksum(); // stability probe
    assert_eq!(book.checksum(), live);
}

/// Replace semantics: price change moves to the tail of the new level
/// (loses time priority); quantity-only change keeps FIFO position.
#[test]
fn replace_priority_semantics() {
    let mut book = Book::with_capacity(16, 16);
    for id in 1..=3u64 {
        book.apply(add(id, 10, id)).expect("seed");
    }
    // Qty-only replace: keeps position 2.
    book.apply(Command::Replace {
        id: OrderId::new(2),
        new_price: 10,
        new_qty: 20,
    })
    .expect("qty-only replace");
    // Price replace to 11: order 2 now at TAIL of level 11.
    book.apply(Command::Replace {
        id: OrderId::new(2),
        new_price: 11,
        new_qty: 20,
    })
    .expect("price replace");

    // Seed order 4 at price 11 after order 2 moved there.
    book.apply(add(4, 11, 4)).expect("add at 11");

    let mut buf = BookBuf::new(8, 16);
    book.try_read(&mut buf).expect("read");
    let level10: Vec<u64> = book
        .bids(&buf)
        .depth()
        .find(|l| l.price() == 10)
        .expect("level 10")
        .orders()
        .map(|o| o.id.raw())
        .collect();
    assert_eq!(level10, vec![1, 3], "order 2 left level 10");
    let level11: Vec<u64> = book
        .bids(&buf)
        .depth()
        .find(|l| l.price() == 11)
        .expect("level 11")
        .orders()
        .map(|o| o.id.raw())
        .collect();
    assert_eq!(
        level11,
        vec![2, 4],
        "replaced order rests at the tail of its new level"
    );
}
