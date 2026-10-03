# Coverage notes — book-kit

Tier A floor: **≥ 90% line coverage** on the lib. Current: see the
coverage job output (`cargo llvm-cov --all-features --fail-under-lines 90`).

## Exclusions (cfg-unviable doctrine)

- **`src/loom_book.rs`** — the loom model doubles are gated
  `#![cfg(all(feature = "loom", loom))]`: they are only *compiled* under
  the mandated loom invocation (`RUSTFLAGS="--cfg loom"`), which the
  host-coverage run does not set. The module is therefore outside the
  coverage denominator entirely — and the seqlock protocol it models is
  covered by the **dedicated loom job** (the covering evidence, per
  MUTATION.md cfg-unviable doctrine). The loom models are likewise
  excluded from mutation-testing globs (`mutants.toml`), with the loom
  harness named as the covering evidence.

## Measured under `--all-features`

The host suite runs `cargo test --all-features` (without `--cfg loom`),
so the loom module compiles out and everything else — writer paths,
rejection paths, replay/gap policies, checksum probes, arena/generation
churn, id-table rehash — is covered by:

- `tests/views.rs` (REQ-BOOK-001/004/005/006)
- `tests/lifecycle.rs` (REQ-BOOK-003/008 + rehash resurrection regression)
- `tests/proptest.rs` (REQ-BOOK-001/003/009/010, 1000-case model
  equivalence)
- `tests/replay.rs`-style units inside `src/replay.rs` (empty feed,
  typed rejects)
- `tests/zero_alloc_apply.rs` (REQ-BOOK-008)
- `tests/seqlock_soak.rs` (REQ-BOOK-002, hot-writer soak)
- `src/loom_book.rs` models — the loom job (REQ-BOOK-007)

## Known thin spots (accepted)

- `Torn`/`Reject`/`ReplayError` `Display` strings — covered, but string
  content is not a contract (consumers match on variants).
- `BookBuf::clone` (alloc convenience) — exercised indirectly.
