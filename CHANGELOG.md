# Changelog

All notable changes to this project are documented here. Format: [Keep a
Changelog](https://keepachangelog.com/) — versions follow [semver](https://semver.org).

## [Unreleased]

## [0.1.0] - 2026-10-03

### Added

- Initial release, per `engineering-standards/specs/book-kit.md`
  (REQ-BOOK-001…010).
- `Book`: single-writer `apply(Command) -> Result<Applied, Reject>` over
  five typed commands (`AddBid`/`AddAsk`/`Cancel`/`Replace`/`Execute`);
  six typed `Reject`s — no panics, no silent drops.
- Lock-free multi-reader snapshots: `try_read` / `read(buf, max_spins)`
  fill a reader-owned `BookBuf` through the seqlock protocol; torn reads
  are typed reports (`Torn`), never observed state.
- Views: `bids(&buf)` / `asks(&buf)` → `SideView<Bid|Ask>` (sealed `Side`
  trait), O(1) `best()`, ordered `depth()`, L3 FIFO via `orders()`.
- Generational order arena (`GenIndex`, slab-pool pattern) — zero
  allocation on all apply/read paths (counting-allocator proof).
- `replay(feed, GapPolicy)` deterministic journal rebuild;
  `checksum()` — FNV-1a-64 over full book state.
- Loom models of the seqlock protocol (torn-state, mid-window rejection,
  fan-out, seeded-race non-vacuousness proof) — the headline gate.
- Criterion baselines for `apply` and `read` paths; fuzz target over
  replay input streams.
- `i64` tick prices, `u64` quantities — no `f64` on the hot path.

### Decisions (spec §Decisions, implemented as documented)

- Seqlock (not epoch) as the default read scheme; epoch is the named
  fallback if contention benches show reader starvation.
- Raw `u64` nanos timestamps on commands (`ts_mono`) — zero estate deps.
- FNV-1a-64 checksum (non-cryptographic detection, documented).
- L1 layer declaration per the slab-pool precedent ("lift only, never
  lower") despite an L0-eligible dependency graph.
- `no_std` claim deferred: internals are core-only; the v1 target is
  Linux HFT hosts.
