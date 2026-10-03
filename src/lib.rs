//! Lock-free limit order book — price-time priority, single-writer/
//! multi-reader snapshots, zero-allocation hot path, generational ABA
//! safety.
//!
//! book-kit is the state substrate *under* a matching engine or feed
//! handler, not one: it applies typed lifecycle commands
//! ([`Command`]: add/cancel/replace/execute) and publishes consistent
//! lock-free snapshot reads. Matching logic, routing, and persistence are
//! out of scope.
//!
//! # Layout
//!
//! - One writer thread: [`Book::apply`] mutates the book through typed
//!   commands; every failure is a typed [`Reject`] (REQ-BOOK-003).
//! - Many readers, no locks: [`Book::try_read`] / [`Book::read`] fill a
//!   reader-owned [`BookBuf`] through a seqlock protocol
//!   (REQ-BOOK-002); a returned snapshot is version-consistent, and torn
//!   reads are reported as [`Torn`], never observed.
//! - Views: [`Book::bids`] / [`Book::asks`] hand out side-typed
//!   [`SideView`]s (sealed [`Side`] trait — `Bid`/`Ask` are distinct
//!   types, never `bool`, REQ-BOOK-005) with O(1) [`SideView::best`] and
//!   ordered [`SideView::depth`] iteration (REQ-BOOK-006).
//! - Numeric discipline: prices are `i64` ticks ([`Price`]), quantities
//!   `u64` ([`Qty`]) — **no `f64` exists on the hot path** (REQ-BOOK-004).
//! - Memory discipline: orders live in a pre-allocated generational arena
//!   ([`OrderArena`], the slab-pool pattern); the writer path performs
//!   zero heap allocation (REQ-BOOK-008).
//! - Determinism: [`replay`] rebuilds a book from an ordered `(seq,
//!   Command)` feed with explicit [`GapPolicy`] (REQ-BOOK-009), and
//!   [`Book::checksum`] hashes full state with FNV-1a-64 for
//!   snapshot/replay verification (REQ-BOOK-010).
//!
//! # The concurrency protocol
//!
//! The seqlock (writer bumps an odd/even counter around each mutation;
//! readers reject odd or changed counters) is verified under loom
//! (REQ-BOOK-007). The full ordering matrix lives on
//! [`book`](self) module docs and `ORDERING.md`; the one-line summary:
//! every protocol access is `SeqCst` — data words included — because a
//! seqlock with relaxed data copies is unsound on the C++/Rust memory
//! model (see the book module docs for the argument and the deliberate
//! divergence from the shm-rings zero-`SeqCst` banner).
//!
//! # Single-writer contract
//!
//! [`Book::apply`] takes `&mut self`, so two writers are a compile error.
//! Readers attach with [`Book::reader`] and share the book lock-free.
//!
//! # Features
//!
//! - `alloc` (default): convenience constructors (`Book::with_capacity`,
//!   `BookBuf::new`, `Book::reader`). Internals are core-only either way;
//!   the `no_std` *claim* is deferred (spec §Decisions 6 — the v1 target
//!   is Linux HFT hosts, `u64` atomics assumed).
//! - `loom`: loom model doubles + models; run with
//!   `RUSTFLAGS="--cfg loom" LOOM_MAX_PREEMPTIONS=2 cargo test --release
//!   --features loom loom`.
//!
//! # Quickstart
//!
//! ```
//! use book_kit::{Book, BookBuf, Command, GapPolicy, OrderId, Price};
//!
//! // Writer side (exactly one thread owns the Book).
//! let mut book = Book::with_capacity(64, 64);
//! let cmd = Command::AddBid { id: OrderId::new(1), price: 100, qty: 5, ts_mono: 42 };
//! let applied = book.apply(cmd).expect("fresh book accepts");
//!
//! // Reader side (any thread, any number):
//! let reader = book.reader();
//! let mut buf = BookBuf::new(16, 64);
//! let version = reader.try_read(&mut buf).expect("quiet book reads first try");
//! let best_bid = reader.bids(&buf).best().expect("we just added one");
//! assert_eq!(best_bid.price(), 100);
//! assert_eq!(best_bid.qty(), 5);
//! assert_eq!(best_bid.orders().next().expect("L3 FIFO").id, OrderId::new(1));
//!
//! // Deterministic checksum over the full state (REQ-BOOK-010).
//! let before = book.checksum();
//! book.apply(Command::Execute { id: OrderId::new(1), qty: 2 }).expect("partial fill");
//! assert_ne!(book.checksum(), before);
//! ```

#![cfg_attr(not(feature = "alloc"), no_std)]
#![deny(missing_docs)]
#![warn(clippy::undocumented_unsafe_blocks)]
#![forbid(unsafe_code)]
// Unit tests assert preconditions with expect/unwrap; the lints target
// production paths.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

#[cfg(feature = "alloc")]
extern crate alloc;

mod arena;
mod book;
mod buf;
mod command;
mod error;
mod fnv;
mod levels;
#[cfg(all(feature = "loom", loom))]
mod loom_book;
mod replay;
mod side;

pub use arena::{GenIndex, GenIndex as GenerationalIndex, OrderArena, OrderRec, NIL_SLOT};
pub use book::Book;
#[cfg(feature = "alloc")]
pub use book::BookReader;
pub use buf::{BookBuf, DepthIter, LevelView, OrderRef, SideView};
pub use command::{Applied, Command, OrderId};
pub use error::{Reject, Torn};
pub use levels::LevelArena;
pub use replay::{replay, FeedEvent, GapPolicy, ReplayError, ReplayReport};
pub use side::{Ask, Bid, Side};

/// Prices are `i64` ticks — REQ-BOOK-004. No `f64` anywhere on the hot
/// path; float conversion is an edge concern outside this crate.
pub type Price = i64;

/// Quantities are `u64` fixed-point units — REQ-BOOK-004.
pub type Qty = u64;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::Command;

    /// REQ-BOOK-004 — the hot-path API carries no `f64`: `Price` is `i64`
    /// and `Qty` is `u64` by definition (type-level pin).
    #[test]
    fn price_is_i64_no_f64_in_hot_api() {
        fn assert_i64<T: Into<i64>>() {}
        fn assert_u64<T: Into<u64>>() {}
        assert_i64::<Price>();
        assert_u64::<Qty>();
        // Quantities/prices round-trip without float loss.
        let p: Price = 123_456_789;
        let q: Qty = 9_876_543_210;
        assert_eq!(p as i64, 123_456_789);
        assert_eq!(q as u64, 9_876_543_210);
    }

    /// REQ-BOOK-003 — the full happy path: add both sides, read both
    /// sides, execute, cancel.
    #[test]
    fn apply_lifecycle_end_to_end() {
        let mut book = Book::with_capacity(16, 16);
        let a = OrderId::new(1);
        let b = OrderId::new(2);
        book.apply(Command::AddBid {
            id: a,
            price: 100,
            qty: 10,
            ts_mono: 1,
        })
        .expect("add bid");
        book.apply(Command::AddAsk {
            id: b,
            price: 102,
            qty: 7,
            ts_mono: 2,
        })
        .expect("add ask");
        assert_eq!(book.version(), 4);

        let mut buf = BookBuf::new(8, 16);
        book.try_read(&mut buf).expect("read");
        assert_eq!(
            book.bids(&buf).best().map(|l| (l.price(), l.qty())),
            Some((100, 10))
        );
        assert_eq!(
            book.asks(&buf).best().map(|l| (l.price(), l.qty())),
            Some((102, 7))
        );

        let ex = book
            .apply(Command::Execute { id: a, qty: 4 })
            .expect("partial fill");
        assert_eq!((ex.executed, ex.remaining), (4, 6));
        let cx = book.apply(Command::Cancel { id: b }).expect("cancel");
        assert_eq!(cx.remaining, 0);
        assert_eq!(
            book.apply(Command::Cancel { id: b }),
            Err(Reject::UnknownOrder)
        );
    }

    /// REQ-BOOK-008 — generational rejection at the book level: cancel,
    /// reuse the slot, then the stale index resolves to
    /// `Reject::StaleGeneration`.
    #[test]
    fn order_at_rejects_stale_generation() {
        let mut book = Book::with_capacity(4, 4);
        let id = OrderId::new(7);
        book.apply(Command::AddBid {
            id,
            price: 50,
            qty: 1,
            ts_mono: 0,
        })
        .expect("add");
        let idx = book.gen_index_of(id).expect("resting");
        book.apply(Command::Cancel { id }).expect("cancel");
        assert_eq!(book.order_at(idx), Err(Reject::StaleGeneration));
        // Recycle the slot; the old index is still stale, the new resolve.
        book.apply(Command::AddBid {
            id: OrderId::new(8),
            price: 51,
            qty: 1,
            ts_mono: 0,
        })
        .expect("reuse");
        assert_eq!(book.order_at(idx), Err(Reject::StaleGeneration));
        let fresh = book.gen_index_of(OrderId::new(8)).expect("resting");
        assert_eq!(fresh.slot, idx.slot);
        assert_ne!(fresh.gen, idx.gen);
        assert!(book.order_at(fresh).is_ok());
    }
}
