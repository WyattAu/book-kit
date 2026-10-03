# book-kit

[![docs.rs](https://docs.rs/book-kit/badge.svg)](https://docs.rs/book-kit)
[![crates.io](https://img.shields.io/crates/v/book-kit.svg)](https://crates.io/crates/book-kit)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](LICENSE)

Lock-free **limit order book** — price-time priority, single-writer/
multi-reader snapshots, zero-allocation hot path, generational ABA safety.

book-kit is the state substrate *under* a matching engine or feed handler,
not one: it applies typed lifecycle commands and publishes consistent
lock-free snapshot reads. Matching logic, order routing, and persistence are
out of scope.

```
                 ┌────────────────────────── Book ──────────────────────────┐
writer thread ──▶│ apply(AddBid|AddAsk|Cancel|Replace|Execute) -> Applied   │
   (&mut Book)   │        │                                                 │
                 │   seqlock version (even=stable / odd=mid-apply)          │
                 │        │                                                 │
reader threads ─▶│ try_read / read -> BookBuf snapshot (version-consistent) │
   (BookReader)  │        │                                                 │
                 │   bids(&buf) / asks(&buf) -> SideView<Bid|Ask>           │
                 │   best() O(1) · depth() best→worst · L2 + L3 FIFO        │
                 └──────────────────────────────────────────────────────────┘
```

- **Price-time priority**: aggregated-per-price (L2) and order-by-order
  (L3) FIFO views, mutually consistent at every version
- **Typed everything**: `apply(Command) -> Result<Applied, Reject>` — six
  typed rejects, no panics, no silent drops; `Bid`/`Ask` are distinct types
  behind a sealed `Side` trait, never `bool`
- **Numeric discipline**: `i64` tick prices, `u64` quantities — **no `f64`
  anywhere on the hot path**
- **Zero allocation**: `apply`, `try_read`/`read`, `checksum`, and every
  view iterator are allocation-free once the book and buffer exist
  (counting-allocator proof in `tests/zero_alloc_apply.rs`)
- **ABA-safe arenas**: orders live in pre-allocated generational slabs
  (`GenIndex`); a freed and reused slot invalidates stale handles
- **Deterministic**: `replay(feed, GapPolicy)` rebuilds state from an
  ordered `(seq, Command)` feed; `checksum()` (FNV-1a-64) changes whenever
  observable state changes

## The concurrency protocol

One writer bumps an odd/even seqlock counter around each mutation; readers
reject odd or changed counters (`Torn` is a *report*, never an observed
state). `try_read` is one attempt; `read(buf, max_spins)` is bounded retry
with a spin hint — the bound is caller-visible. Livelock under a hot writer
is possible by design and documented (spec §Risk register).

The full ordering matrix lives in [`ORDERING.md`](ORDERING.md) and on the
[`book`](https://docs.rs/book-kit) module docs. Headline: **every protocol
access is `SeqCst` — data words included** — because a seqlock with relaxed
data copies is unsound on the C++/Rust memory model even with a `SeqCst`
version counter (the Boehm seqlock result; the argument is spelled out in
the module docs). This is the one deliberate divergence from the
shm-rings zero-`SeqCst` banner: unlike a ring with slot ownership, a
seqlock has no exclusivity to lean on, so it pays for the total order.

The protocol is **loom-verified** (REQ-BOOK-007 standard, evidence
interchangeable with shm-rings): torn-state, mid-window-rejection, fan-out,
and a seeded-race non-vacuousness proof (the same protocol with the
re-check removed *must* observe the tear under the same preemption bound).

```bash
RUSTFLAGS="--cfg loom" LOOM_MAX_PREEMPTIONS=2 \
  cargo test --release --features loom loom -- --test-threads=1
```

## Quickstart

```rust
use book_kit::{Book, BookBuf, Command, GapPolicy, OrderId};

// Exactly one writer thread owns the Book.
let mut book = Book::with_capacity(1024, 256);
book.apply(Command::AddBid { id: OrderId::new(1), price: 100, qty: 5, ts_mono: 42 })?;

// Any number of reader threads attach lock-free.
let reader = book.reader();
let mut buf = BookBuf::new(64, 256);
reader.try_read(&mut buf)?; // one attempt; `read(&mut buf, spins)` retries
let best = reader.bids(&buf).best().expect("non-empty bid side");
assert_eq!((best.price(), best.qty()), (100, 5));
for order in best.orders() {
    // L3 FIFO, first-in-first-out at this price
}
```

Replay a journal deterministically:

```rust
use book_kit::{replay, GapPolicy};
let feed = [(100u64, Command::AddBid { id: OrderId::new(1), price: 100, qty: 5, ts_mono: 0 })];
let mut book = Book::with_capacity(1024, 256);
let report = replay(&mut book, feed, GapPolicy::Fail)?;
assert_eq!(report.applied, 1);
```

## Capacity & memory

`Book::with_capacity(max_orders, max_levels)` builds the arenas over
process-lifetime (leaked) heap storage — books are long-lived substrate
objects; the crate stays `unsafe`-free. `Book::with_arena(orders, levels)`
accepts caller-owned static memory in core-only builds. `BookBuf` capacity
is explicit; a snapshot deeper than the buffer fails with
`Torn::BufferTooSmall` — truncation is a typed error, never silent partial
state.

Generation counters are `u32`: a slot must be recycled 2³² times before a
stale handle can alias again (documented wrap bound, spec §Risk register).

## Benchmarks

Criterion baselines (`cargo bench`): `apply(Add|Cancel|Execute)` ns/op and
`read()` snapshot ns/op at three depth points. CI runs them as a smoke
gate, never as a perf regression gate.

## Feature flags

| flag      | default | effect                                                     |
|-----------|---------|------------------------------------------------------------|
| `alloc`   | ✅      | `with_capacity`, `BookBuf::new`, `Book::reader`            |
| `loom`    | —       | loom model doubles + models (`--cfg loom` invocation)      |

Internals are core-only either way. The `no_std` *claim* is deferred
(spec §Decisions 6): the v1 target is Linux HFT hosts, `u64` atomics
assumed — revisit before any no_std announcement.

## Requirements traceability

Every `REQ-BOOK-NNN` maps to ≥ 1 named test (spec §Traceability matrix):

| REQ | test |
|-----|------|
| BOOK-001 | `l2_aggregation_equals_l3_summation` (+ soak consistency) |
| BOOK-002 | `reader_never_sees_torn_state_under_hot_writer` + loom models |
| BOOK-003 | `every_lifecycle_rejection_is_typed` (table-driven) |
| BOOK-004 | `price_is_i64_no_f64_in_hot_api` (type-level) |
| BOOK-005 | `sides_are_distinct_types_not_bool` |
| BOOK-006 | `best_is_o1_depth_iter_is_ordered_no_alloc` |
| BOOK-007 | `loom_seqlock_*` + `loom_seeded_race_*` (headline gate) |
| BOOK-008 | `apply_paths_zero_alloc` + `stale_generation_index_rejected` |
| BOOK-009 | `replay_is_deterministic_and_gap_policy_honored` |
| BOOK-010 | `checksum_detects_any_state_delta` |

## License

MIT OR Apache-2.0. See [SECURITY.md](SECURITY.md) for reporting policy.
