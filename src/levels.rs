//! Price-level storage: a per-side flat array sorted best→worst, so
//! `best` is index 0 (O(1), REQ-BOOK-006) and depth iteration walks the
//! array in order. Insert/remove shift the tail of a bounded array — no
//! heap allocation, and the complexity contract of REQ-BOOK-006 is
//! unaffected (it fixes best/depth, not mutation cost).
//!
//! Like the arena, every word is atomic: the level arrays are exactly what
//! snapshot readers copy through the seqlock protocol.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::arena::NIL_PACKED;
use crate::Price;

/// Leak a boxed slice of `len` atomics initialized to `init` (alloc-mode
/// storage helper; the storage is process-lifetime by design).
#[cfg(feature = "alloc")]
fn leak_words(len: usize, init: u64) -> &'static [AtomicU64] {
    Box::leak(
        (0..len)
            .map(|_| AtomicU64::new(init))
            .collect::<alloc::vec::Vec<_>>()
            .into_boxed_slice(),
    )
}

/// Backing arrays for one side of the book.
pub(crate) struct SideStorage {
    /// Number of live levels (index 0 is always the best).
    pub(crate) n_levels: AtomicU64,
    /// Level prices in ticks, sorted best→worst for this side.
    pub(crate) price: &'static [AtomicU64],
    /// Aggregated resting quantity per level (L2).
    pub(crate) qty: &'static [AtomicU64],
    /// FIFO head per level, packed `GenIndex` (NIL when empty).
    pub(crate) head: &'static [AtomicU64],
    /// FIFO tail per level, packed `GenIndex` (NIL when empty).
    pub(crate) tail: &'static [AtomicU64],
    /// Order count per level (L3 cardinality).
    pub(crate) count: &'static [AtomicU64],
}

/// True price ordering: `true` when `a` is *better* than `b` on this side.
pub(crate) fn better(is_bid: bool, a: Price, b: Price) -> bool {
    if is_bid {
        a > b
    } else {
        a < b
    }
}

impl SideStorage {
    pub(crate) fn capacity(&self) -> usize {
        self.price.len()
    }

    pub(crate) fn len(&self, o: Ordering) -> usize {
        self.n_levels.load(o) as usize
    }

    pub(crate) fn price_at(&self, i: usize, o: Ordering) -> Price {
        self.price[i].load(o) as isize as Price
    }

    pub(crate) fn qty_at(&self, i: usize, o: Ordering) -> u64 {
        self.qty[i].load(o)
    }

    pub(crate) fn count_at(&self, i: usize, o: Ordering) -> u64 {
        self.count[i].load(o)
    }

    pub(crate) fn head_at(&self, i: usize, o: Ordering) -> crate::GenIndex {
        crate::GenIndex::unpack(self.head[i].load(o))
    }

    pub(crate) fn tail_at(&self, i: usize, o: Ordering) -> crate::GenIndex {
        crate::GenIndex::unpack(self.tail[i].load(o))
    }

    /// Binary search for the first index whose price is *worse than or
    /// equal to* `price` on this side — i.e. where a level with `price`
    /// sits, or should be inserted.
    /// Returns `n` when every existing level is better.
    pub(crate) fn find_slot(&self, is_bid: bool, price: Price) -> usize {
        let n = self.len(Ordering::SeqCst);
        let (mut lo, mut hi) = (0usize, n);
        while lo < hi {
            let mid = (lo + hi) / 2;
            let p = self.price_at(mid, Ordering::SeqCst);
            if p == price {
                return mid;
            }
            if better(is_bid, p, price) {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    /// Insert a fresh level at `pos`, shifting the tail. Panics only on
    /// programmer error (caller checked capacity).
    pub(crate) fn insert_level(&self, pos: usize, price: Price) {
        let n = self.len(Ordering::SeqCst);
        debug_assert!(pos <= n && n < self.price.len());
        let mut i = n;
        while i > pos {
            self.price[i].store(self.price[i - 1].load(Ordering::SeqCst), Ordering::SeqCst);
            self.qty[i].store(self.qty[i - 1].load(Ordering::SeqCst), Ordering::SeqCst);
            self.head[i].store(self.head[i - 1].load(Ordering::SeqCst), Ordering::SeqCst);
            self.tail[i].store(self.tail[i - 1].load(Ordering::SeqCst), Ordering::SeqCst);
            self.count[i].store(self.count[i - 1].load(Ordering::SeqCst), Ordering::SeqCst);
            i -= 1;
        }
        self.price[pos].store(price as u64, Ordering::SeqCst);
        self.qty[pos].store(0, Ordering::SeqCst);
        self.head[pos].store(NIL_PACKED, Ordering::SeqCst);
        self.tail[pos].store(NIL_PACKED, Ordering::SeqCst);
        self.count[pos].store(0, Ordering::SeqCst);
        self.n_levels.store(n as u64 + 1, Ordering::SeqCst);
    }

    /// Remove the level at `pos`, shifting the tail down.
    pub(crate) fn remove_level(&self, pos: usize) {
        let n = self.len(Ordering::SeqCst);
        debug_assert!(pos < n);
        let mut i = pos;
        while i + 1 < n {
            self.price[i].store(self.price[i + 1].load(Ordering::SeqCst), Ordering::SeqCst);
            self.qty[i].store(self.qty[i + 1].load(Ordering::SeqCst), Ordering::SeqCst);
            self.head[i].store(self.head[i + 1].load(Ordering::SeqCst), Ordering::SeqCst);
            self.tail[i].store(self.tail[i + 1].load(Ordering::SeqCst), Ordering::SeqCst);
            self.count[i].store(self.count[i + 1].load(Ordering::SeqCst), Ordering::SeqCst);
            i += 1;
        }
        self.tail[n - 1].store(NIL_PACKED, Ordering::SeqCst);
        self.head[n - 1].store(NIL_PACKED, Ordering::SeqCst);
        self.n_levels.store(n as u64 - 1, Ordering::SeqCst);
    }
}

/// Pre-allocated price-level storage for **both** sides (REQ-BOOK-006/
/// REQ-BOOK-008: bounded, allocation-free after construction).
///
/// Constructed from leaked heap memory by [`LevelArena::heap`] (alloc
/// feature; the storage lives for the process, keeping the crate
/// `unsafe`-free) or over caller-owned statics by [`LevelArena::from_static`].
pub struct LevelArena {
    pub(crate) bids: SideStorage,
    pub(crate) asks: SideStorage,
}

impl LevelArena {
    /// Build over caller-owned static memory (core-only path, zero heap).
    /// All arrays must start zeroed except `head`/`tail`, which must start
    /// at `NIL_PACKED` (`0xFFFF_FFFF`).
    ///
    /// # Panics
    /// If any slice is shorter than `per_side`.
    #[allow(clippy::too_many_arguments)]
    pub fn from_static(
        per_side: usize,
        bids_price: &'static [AtomicU64],
        bids_qty: &'static [AtomicU64],
        bids_head: &'static [AtomicU64],
        bids_tail: &'static [AtomicU64],
        bids_count: &'static [AtomicU64],
        asks_price: &'static [AtomicU64],
        asks_qty: &'static [AtomicU64],
        asks_head: &'static [AtomicU64],
        asks_tail: &'static [AtomicU64],
        asks_count: &'static [AtomicU64],
    ) -> Self {
        macro_rules! chk {
            ($a:expr) => {
                assert!($a.len() >= per_side, "level array shorter than capacity");
            };
        }
        chk!(bids_price);
        chk!(bids_qty);
        chk!(bids_head);
        chk!(bids_tail);
        chk!(bids_count);
        chk!(asks_price);
        chk!(asks_qty);
        chk!(asks_head);
        chk!(asks_tail);
        chk!(asks_count);
        Self {
            bids: SideStorage {
                n_levels: AtomicU64::new(0),
                price: bids_price,
                qty: bids_qty,
                head: bids_head,
                tail: bids_tail,
                count: bids_count,
            },
            asks: SideStorage {
                n_levels: AtomicU64::new(0),
                price: asks_price,
                qty: asks_qty,
                head: asks_head,
                tail: asks_tail,
                count: asks_count,
            },
        }
    }

    /// Capacity per side.
    pub fn per_side(&self) -> usize {
        self.bids.price.len()
    }

    /// A `LevelArena` over freshly leaked heap storage: every word zeroed,
    /// links NIL. The storage lives for the process — books are
    /// process-lifetime substrate objects (see type docs).
    #[cfg(feature = "alloc")]
    pub fn heap(per_side: usize) -> Self {
        let mk = || SideStorage {
            n_levels: AtomicU64::new(0),
            price: leak_words(per_side, 0),
            qty: leak_words(per_side, 0),
            head: leak_words(per_side, NIL_PACKED),
            tail: leak_words(per_side, NIL_PACKED),
            count: leak_words(per_side, 0),
        };
        Self {
            bids: mk(),
            asks: mk(),
        }
    }
}

/// Compile-time NIL sanity: the packed NIL word decodes to `slot ==
/// NIL_SLOT`, and both differ from the tombstone slot.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::arena::{GenIndex, NIL_SLOT};

    const fn zeros<const N: usize>() -> [AtomicU64; N] {
        [const { AtomicU64::new(0) }; N]
    }
    const fn nils<const N: usize>() -> [AtomicU64; N] {
        [const { AtomicU64::new(NIL_PACKED) }; N]
    }

    #[test]
    fn nil_packing_round_trip() {
        assert!(GenIndex::unpack(NIL_PACKED).is_nil());
        assert_eq!(GenIndex::unpack(NIL_PACKED).slot, NIL_SLOT);
        assert_eq!(GenIndex::NIL.pack(), NIL_PACKED);
        let g = GenIndex { slot: 7, gen: 9 };
        assert_eq!(GenIndex::unpack(g.pack()), g);
    }

    /// Sorted insert keeps bids descending and asks ascending; removal
    /// keeps the order and the level count honest.
    #[test]
    fn level_array_sorted_insert_remove() {
        static PRICE: [AtomicU64; 4] = zeros::<4>();
        static QTY: [AtomicU64; 4] = zeros::<4>();
        static HEAD: [AtomicU64; 4] = nils::<4>();
        static TAIL: [AtomicU64; 4] = nils::<4>();
        static COUNT: [AtomicU64; 4] = zeros::<4>();
        static A_PRICE: [AtomicU64; 4] = zeros::<4>();
        static A_QTY: [AtomicU64; 4] = zeros::<4>();
        static A_HEAD: [AtomicU64; 4] = nils::<4>();
        static A_TAIL: [AtomicU64; 4] = nils::<4>();
        static A_COUNT: [AtomicU64; 4] = zeros::<4>();
        let la = LevelArena::from_static(
            4, &PRICE, &QTY, &HEAD, &TAIL, &COUNT, &A_PRICE, &A_QTY, &A_HEAD, &A_TAIL, &A_COUNT,
        );
        let b = &la.bids;
        // Insert 100, 300, 200 → descending [300, 200, 100].
        for p in [100i64, 300, 200] {
            let pos = b.find_slot(true, p);
            assert_ne!(pos, b.price.len(), "capacity reserved");
            b.insert_level(pos, p);
        }
        assert_eq!(b.len(Ordering::SeqCst), 3);
        assert_eq!(b.price_at(0, Ordering::SeqCst), 300);
        assert_eq!(b.price_at(1, Ordering::SeqCst), 200);
        assert_eq!(b.price_at(2, Ordering::SeqCst), 100);
        // Equal price finds the existing slot.
        assert_eq!(b.find_slot(true, 200), 1);

        // Asks ascending: insert 100, 300, 200 → [100, 200, 300].
        let a = &la.asks;
        for p in [100i64, 300, 200] {
            let pos = a.find_slot(false, p);
            a.insert_level(pos, p);
        }
        assert_eq!(a.price_at(0, Ordering::SeqCst), 100);
        assert_eq!(a.price_at(2, Ordering::SeqCst), 300);

        // Remove middle; order preserved, count drops.
        b.remove_level(1);
        assert_eq!(b.len(Ordering::SeqCst), 2);
        assert_eq!(b.price_at(1, Ordering::SeqCst), 100);
    }
}
