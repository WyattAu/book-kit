# ORDERING.md — memory-ordering matrix (REQ-BOOK-007)

book-kit's reader protocol follows the **shm-rings estate ordering
standard**: every atomic operation names its ordering and pays for it in a
justification; the matrix is textually shared with shm-rings so review and
loom evidence transfer between the two crates. This file is the
authoritative copy for book-kit; `src/book.rs` module docs carry the same
matrix.

## The seqlock protocol

One `AtomicU64` version pivot; **even = stable, odd = writer mid-apply**.
All accesses below are on protocol words (the version counter, the level
arrays, the order nodes). Writer-only words (id table, free chain, live
count, the writer's own pre-window read) are `Relaxed` — no reader ever
touches them.

| # | Actor  | Operation                                    | Ordering |
|---|--------|----------------------------------------------|----------|
| W1| writer | `version.store(v + 1)` — enter write window  | `SeqCst` |
| W2| writer | mutate observable words                      | `SeqCst` |
| W3| writer | `version.store(v + 2)` — publish             | `SeqCst` |
| R1| reader | `version.load()` → reject odd                | `SeqCst` |
| R2| reader | copy observable words                        | `SeqCst` |
| R3| reader | `version.load()` → reject changed            | `SeqCst` |

## Justification

* **W3 → R1**: the reader's `SeqCst` load of the published even value
  synchronizes with every W2 write of that version, so an accepted snapshot
  observes complete v1 state.

* **Why everything is `SeqCst` (including the data words)** — the one
  deliberate divergence from shm-rings' zero-`SeqCst` banner. A seqlock
  whose data copies are `Relaxed` (or plain `Acquire`/`Release`) is
  formally unsound on the C++/Rust memory model *even with* a `SeqCst`
  version counter: relaxed accesses sit outside the `SeqCst` total order
  `S`, so a reader's data copy can observe a write from the *next* window
  while its version re-check still returns the old value, and no rule is
  violated (the classic Boehm seqlock result). With every protocol access
  in `S`, correctness becomes a cycle check: a polluting write implies
  `W1 <_s W2 <_s R2 <_s R3 <_s W1`, which `S`'s acyclicity forbids —
  acceptance ⇒ the copy is exactly the pre-window or post-window state.
  shm-rings does not need this because its ring grants each slot a single
  owner at a time (slot-ownership exclusivity); a seqlock has no such
  exclusivity, so it pays for the total order.

* **W2/R2 `SeqCst`, not `Relaxed`**: see above — this is the load-bearing
  choice. The loom models (`src/loom_book.rs`) encode this exact matrix.

* **Termination, not liveness**: `R3` rejection may loop forever under a
  hot writer (livelock is possible by design). The API boundary makes the
  bound explicit: `try_read` (single attempt) and `read(buf, max_spins)`
  (bounded retries, spin hint between). Rejections are the typed `Torn`
  values — a reader never *observes* torn state, it is told it did not get
  a snapshot.

## Non-protocol state

| Word                          | Ordering  | Why                                        |
|-------------------------------|-----------|--------------------------------------------|
| id-table key/loc words        | `Relaxed` | writer-only (single `&mut Book` contract)  |
| free chain (`free_head`, `next` while free) | `Relaxed` | writer-only |
| `live` counter                | `Relaxed` | writer-only diagnostic                     |
| writer's pre-window version read | `Relaxed` | single-writer self-coherence; W1 is the first protocol op |

Note `node.next` doubles as the free-chain link while the slot is free and
as the FIFO link while live — it is `SeqCst` in both roles because readers
walk it in the live role.

## Loom evidence (REQ-BOOK-007)

`src/loom_book.rs` re-implements this exact matrix over loom atomics
(`--features loom` + `--cfg loom`):

| model | invariant | scenarios |
|-------|-----------|-----------|
| `loom_reader_never_accepts_torn_state` | accepted snapshot = exactly one version's state | writer-before-reader, interleaved applies |
| `loom_reader_rejects_every_midwindow_interleaving` | odd/mixed never accepted | reader inside the write window |
| `loom_two_readers_independent` | fan-out independence + consistency | two readers, one writer |
| `loom_broken_reader_observes_torn_state` | non-vacuousness: R3 removed ⇒ tear observed | seeded-race proof |

Run:

```bash
RUSTFLAGS="--cfg loom" LOOM_MAX_PREEMPTIONS=2 \
  cargo test --release --features loom loom -- --test-threads=1
```
