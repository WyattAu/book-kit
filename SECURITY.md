# Security Policy — book-kit

## Supported versions

| Version | Supported |
|---------|-----------|
| 0.1.x   | ✅        |

## Reporting a vulnerability

Report privately via [GitHub security advisories] for this repository, or
email **wyatt_au@protonmail.com**. Do **not** open a public issue for
security reports.

You will receive an acknowledgement within **72 hours**. Coordinated
disclosure: we ask for up to 90 days before public disclosure while a
patch ships.

## Scope notes

book-kit is an in-memory order-book substrate. Security-relevant surfaces:

- **Integrity, not adversarial hashing**: `checksum()` is FNV-1a-64, a
  non-cryptographic *detection* hash (spec §Risk register). Do not use it
  as an authenticity mechanism — snapshot verification against accidental
  corruption only. If stronger hashing is required, compose a
  cryptographic hash at the consumer edge.
- **Single-writer contract**: `apply` takes `&mut Book` (two writers are a
  compile error); concurrent readers are safe through the seqlock
  protocol. Feeding the same `Book` to two writer threads requires
  breaking Rust's aliasing rules and is outside the threat model.
- **No untrusted input parsing**: commands are typed values. Journal/feed
  bytes are decoded upstream (wire-kit's burden); the fuzz target covers
  replay *streams*, not wire formats.
- **No `unsafe`**: the crate is `#![forbid(unsafe_code)]`; memory-safety
  bugs would have to be soundness bugs in safe Rust and are loom/miri
  gated in CI.

[GitHub security advisories]: https://github.com/WyattAu/book-kit/security/advisories
