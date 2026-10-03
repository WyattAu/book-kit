//! Loom model doubles of the seqlock protocol (REQ-BOOK-007).
//!
//! Compiled **only** under `--features loom` **and** `--cfg loom`, so the
//! mandated invocation is the only way they run:
//!
//! ```text
//! RUSTFLAGS="--cfg loom" LOOM_MAX_PREEMPTIONS=2 \
//!   cargo test --release --features loom loom -- --test-threads=1
//! ```
//!
//! Following the shm-rings `loom_ring` pattern: loom cannot model the real
//! book's arena walk at useful scale, so these doubles re-implement the
//! *exact* seqlock protocol of [`crate::book`] — same orderings (all
//! protocol accesses `SeqCst`, see the book module docs for why the data
//! words must join the total order), same check structure — over loom
//! atomics, with a tiny state (two data words per version) so every
//! interleaving is explored. The ordering-matrix text is shared with
//! shm-rings so the evidence transfers between the crates.
//!
//! Models (every invariant ≥ 2 interleaving scenarios, plus a seeded-race
//! non-vacuousness proof):
//!
//! 1. `loom_reader_never_accepts_torn_state` — single reader vs two
//!    writer applies; an accepted snapshot is exactly one version's state.
//! 2. `loom_reader_rejects_every_midwindow_interleaving` — the reader
//!    interleaves *inside* the write window; odd versions are rejected,
//!    partial windows are never accepted.
//! 3. `loom_two_readers_independent` — fan-out: readers never block each
//!    other and each accepted snapshot is version-consistent.
//! 4. `loom_broken_reader_observes_torn_state` — the seeded-race proof:
//!    the same protocol with the R3 re-check removed MUST observe an
//!    inconsistent copy under the same preemption bound, proving the
//!    correct models' assertions are exercised non-vacuously.

#![cfg(all(feature = "loom", loom))]

use loom::sync::atomic::{
    AtomicU64,
    Ordering::{Relaxed, SeqCst},
};
use loom::sync::Arc;

/// The model state: the version pivot plus two data words (the minimum
/// shape that can expose a *mixed* copy — one word could only be wholly
/// old or wholly new). Every protocol access is `SeqCst`, mirroring the
/// real book's ordering matrix.
struct ModelBook {
    version: AtomicU64,
    data_a: AtomicU64,
    data_b: AtomicU64,
}

impl ModelBook {
    fn new() -> Self {
        Self {
            version: AtomicU64::new(0),
            data_a: AtomicU64::new(0),
            data_b: AtomicU64::new(0),
        }
    }

    /// W1: enter the write window (odd). The pre-window read is the
    /// writer's own `Relaxed` self-read, as in `Book::apply`.
    fn apply_begin(&self) -> u64 {
        let v = self.version.load(Relaxed);
        self.version.store(v + 1, SeqCst);
        v
    }

    /// W2 (data writes, `SeqCst`) then W3 (publish, `SeqCst`).
    fn apply_commit(&self, v: u64, a: u64, b: u64) {
        self.data_a.store(a, SeqCst);
        self.data_b.store(b, SeqCst);
        self.version.store(v + 2, SeqCst);
    }

    /// The reader protocol, verbatim from `Shared::try_read`, plus a
    /// broken-mode switch used by the non-vacuousness proof.
    fn try_read(&self, out: &mut (u64, u64), broken: bool) -> Result<u64, ReadErr> {
        // R1: reject odd.
        let v1 = self.version.load(SeqCst);
        if v1 % 2 == 1 {
            return Err(ReadErr::InProgress);
        }
        // R2: copy (`SeqCst` — in the total order, like the real reader).
        let a = self.data_a.load(SeqCst);
        let b = self.data_b.load(SeqCst);
        // R3: reject changed — *omitted in broken mode* (the seeded bug).
        if broken {
            *out = (a, b);
            return Ok(v1);
        }
        let v2 = self.version.load(SeqCst);
        if v2 != v1 {
            return Err(ReadErr::Lapped);
        }
        *out = (a, b);
        Ok(v1)
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ReadErr {
    InProgress,
    Lapped,
}

/// Branch accumulators for the non-vacuousness (liveness) side of each
/// model. Safety asserts run inside every branch (fail fast); liveness
/// asserts run once, over the accumulated whole state space, after
/// `loom::model` — a branch where the interesting interleaving did not
/// occur is not a failure, a state space where it never occurs is.
macro_rules! accumulator {
    ($name:ident) => {
        static $name: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    };
}

accumulator!(ACC1);
accumulator!(ACC2);
accumulator!(ACC3);
accumulator!(ACC4);

/// Model 1 — REQ-BOOK-002 core invariant: an accepted snapshot is exactly
/// one version's state, never a mix.
///
/// Scenario shape: one writer applies v0→v1 (state 1,1) then v1→v2 (state
/// 2,2); one reader snapshots repeatedly with bounded attempts. For every
/// schedule loom generates, any `Ok` must hold `(1,1)` or `(2,2)` (or the
/// initial `(0,0)`), never `(1,2)`/`(2,1)`; an odd version is never
/// accepted. (Scenario A of the torn-state invariant; model 2 is
/// scenario B, with the reader inside the window.)
pub fn loom_reader_never_accepts_torn_state() {
    const ATTEMPTS: usize = 4;
    loom::model(move || {
        let book = Arc::new(ModelBook::new());
        let rb = Arc::clone(&book);

        let writer = loom::thread::spawn(move || {
            let v = book.apply_begin();
            book.apply_commit(v, 1, 1);
            let v = book.version.load(Relaxed);
            book.apply_begin();
            book.apply_commit(v, 2, 2);
        });

        let reader = loom::thread::spawn(move || {
            for _ in 0..ATTEMPTS {
                let mut out = (0u64, 0u64);
                if let Ok(v) = rb.try_read(&mut out, false) {
                    let expected = match v {
                        0 => (0, 0),
                        2 => (1, 1),
                        4 => (2, 2),
                        other => panic!("accepted an odd/partial version {other}"),
                    };
                    assert_eq!(out, expected, "torn snapshot accepted at v{v}");
                    ACC1.fetch_or(1, std::sync::atomic::Ordering::Relaxed);
                }
            }
        });

        writer.join().unwrap();
        reader.join().unwrap();
    });
    // Non-vacuousness over the whole state space: at least one explored
    // schedule completed a snapshot.
    assert!(
        ACC1.load(std::sync::atomic::Ordering::Relaxed) == 1,
        "no snapshot accepted in any explored schedule: model vacuous"
    );
}

/// Model 2 — the reader interleaves *inside* the write window: W1 has
/// run, the data words are mid-mutation, W3 has not. Invariants for every
/// schedule: (a) the reader observing the odd version reports
/// `InProgress` and accepts nothing; (b) the reader copying mid-window
/// data without observing odd must reject at R3 — it can never accept a
/// mix. (Scenario B of the torn-state invariant.)
pub fn loom_reader_rejects_every_midwindow_interleaving() {
    const ATTEMPTS: usize = 4;
    loom::model(move || {
        let book = Arc::new(ModelBook::new());
        let rb = Arc::clone(&book);

        let writer = loom::thread::spawn(move || {
            let v = book.apply_begin();
            // Window deliberately split across the two data words: store
            // the first, give loom the interleaving point, then the second
            // and the publish.
            book.data_a.store(9, SeqCst);
            book.data_b.store(9, SeqCst);
            book.version.store(v + 2, SeqCst);
        });

        let reader = loom::thread::spawn(move || {
            for _ in 0..ATTEMPTS {
                let mut out = (0u64, 0u64);
                match rb.try_read(&mut out, false) {
                    Ok(v) => {
                        // Accepted ⇒ either the pre-window state or the
                        // fully published post-window state, never a mix.
                        // (Safety: fails the branch — and the model — on
                        // the first torn acceptance.)
                        let expected = if v == 0 { (0, 0) } else { (9, 9) };
                        assert_eq!(out, expected, "mixed snapshot accepted");
                        assert_eq!(v % 2, 0, "odd version accepted");
                    }
                    Err(_) => {
                        ACC2.fetch_or(1, std::sync::atomic::Ordering::Relaxed);
                    }
                }
            }
        });

        writer.join().unwrap();
        reader.join().unwrap();
    });
    // Non-vacuousness over the whole state space: at least one explored
    // schedule tripped a rejection (the window did overlap a read).
    assert!(
        ACC2.load(std::sync::atomic::Ordering::Relaxed) == 1,
        "window never overlapped a read in any explored schedule: model vacuous"
    );
}

/// Model 3 — fan-out: two readers against one writer. Invariants for
/// every schedule: each accepted snapshot is version-consistent, readers
/// never block each other (no lock — acceptance counts are bounded only
/// by attempt budgets, not contention), and the published state is
/// observed by at least one reader (non-vacuousness).
pub fn loom_two_readers_independent() {
    const ATTEMPTS: usize = 3;
    loom::model(move || {
        let book = Arc::new(ModelBook::new());

        let rw = Arc::clone(&book);
        let writer = loom::thread::spawn(move || {
            let v = rw.apply_begin();
            rw.apply_commit(v, 5, 5);
        });

        let mk_reader = |b: Arc<ModelBook>| {
            loom::thread::spawn(move || {
                for _ in 0..ATTEMPTS {
                    let mut out = (0u64, 0u64);
                    if let Ok(v) = b.try_read(&mut out, false) {
                        assert_eq!(v % 2, 0, "odd version accepted");
                        let expected = if v == 0 { (0, 0) } else { (5, 5) };
                        assert_eq!(out, expected, "torn snapshot accepted at v{v}");
                        if v >= 2 {
                            ACC3.fetch_or(1, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                }
            })
        };

        let r1 = mk_reader(Arc::clone(&book));
        let r2 = mk_reader(Arc::clone(&book));
        writer.join().unwrap();
        r1.join().unwrap();
        r2.join().unwrap();
    });
    // Non-vacuousness over the whole state space: at least one reader, in
    // at least one explored schedule, observed the published state.
    assert!(
        ACC3.load(std::sync::atomic::Ordering::Relaxed) == 1,
        "no reader ever saw the published state in any explored schedule: model vacuous"
    );
}

/// Model 4 — seeded-race non-vacuousness proof (REQ-BOOK-007 gate
/// discipline): the same protocol with the R3 re-check **removed** MUST
/// observe a torn snapshot under the same preemption bound. This proves
/// the correct models' assertions are load-bearing: the interleaving that
/// would break them is inside the explored state space, so a passing
/// model means the protocol (not the schedule space) is what passes it.
///
/// Single reader attempt keeps the state space tiny: with
/// `LOOM_MAX_PREEMPTIONS=2` loom explores the exact schedule
/// `R1 → W1 → W2a → R2 → accept`, which is precisely the tear the real
/// R3 check rejects.
pub fn loom_broken_reader_observes_torn_state() {
    loom::model(move || {
        let book = Arc::new(ModelBook::new());
        let rb = Arc::clone(&book);

        let writer = loom::thread::spawn(move || {
            let v = book.apply_begin();
            // Split-mutation window: first word belongs to the new state,
            // the second is still pending when the torn copy is taken.
            book.data_a.store(7, SeqCst);
            book.version.store(v + 2, SeqCst);
        });

        let reader = loom::thread::spawn(move || {
            let mut out = (0u64, 0u64);
            // A tear at version 0 specifically: the true v0 state is
            // (0, 0), so an accepted (7, 0) at v1 == 0 is the NEW `data_a`
            // next to the OLD `data_b` — exactly the torn state the real
            // R3 check exists to reject. (Accepting (7, 0) at v2 would be
            // the legitimate final state, not a tear.)
            if matches!(rb.try_read(&mut out, true), Ok(0)) && out.0 == 7 && out.1 == 0 {
                ACC4.fetch_or(1, std::sync::atomic::Ordering::Relaxed);
            }
        });

        writer.join().unwrap();
        reader.join().unwrap();
    });
    // Non-vacuousness over the whole state space: the broken reader MUST
    // have observed the tear in at least one explored schedule — this is
    // what makes the correct models' assertions load-bearing.
    assert_eq!(
        ACC4.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "broken reader never observed a torn snapshot in any explored schedule: \
         the correct models may be vacuous — widen the bound"
    );
}

#[cfg(all(test, feature = "loom", loom))]
mod tests {
    //! Test entry points; names carry the `loom_` prefix so the mandated
    //! filter (`cargo test … loom`) selects exactly these.

    use super::*;

    /// REQ-BOOK-007 — headline model: accepted snapshots are exactly one
    /// version's state (scenarios: writer-before-reader, reader-inside-
    /// window via preemption, interleaved applies).
    #[test]
    fn loom_seqlock_matches_shm_rings_matrix_torn_state() {
        loom_reader_never_accepts_torn_state();
    }

    /// REQ-BOOK-007 — mid-window interleavings all end in a typed
    /// rejection (`WriteInProgress` / `Lapped`), never an acceptance.
    #[test]
    fn loom_seqlock_matches_shm_rings_matrix_midwindow_reject() {
        loom_reader_rejects_every_midwindow_interleaving();
    }

    /// REQ-BOOK-002 — fan-out: two readers, one writer; readers are
    /// independent and version-consistent.
    #[test]
    fn loom_seqlock_fanout_two_readers_independent() {
        loom_two_readers_independent();
    }

    /// Non-vacuousness proof: the seeded broken reader (R3 removed) MUST
    /// observe the torn state under the same bound, proving the correct
    /// models' assertions are load-bearing.
    #[test]
    fn loom_seeded_race_broken_reader_observes_torn_state() {
        loom_broken_reader_observes_torn_state();
    }
}
