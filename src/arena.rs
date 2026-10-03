//! Pre-allocated order arena with generational indices — the `slab-pool`
//! pattern re-implemented locally (REQ-BOOK-008, spec §Decisions/§Cross-
//! references): a freed and reused slot invalidates stale indices because
//! the generation no longer matches, killing ABA hazards.
//!
//! Generation wrap: the counter is `u32`; a slot must be recycled 2³² times
//! before a stale handle can alias again. At one recycle per nanosecond
//! that is over two hours of *continuous single-slot churn* — the
//! documented wrap bound (spec §Risk register). Replay tests drive reuse
//! cycles to exercise the bump path.
//!
//! Every word a reader can observe is an atomic. Writer-only words (the
//! id→index table, the free chain) are also stored as atomics so the arena
//! needs no `UnsafeCell` and `Book` is `Sync` by construction; the writer
//! is simply the only thread holding `&mut Book`.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::command::OrderId;
use crate::Reject;

/// Arena slot handle: slot + generation (REQ-BOOK-008).
///
/// `slot == NIL_SLOT` is reserved as "no such slot" and never handed out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct GenIndex {
    /// Slab slot.
    pub slot: u32,
    /// Generation counter of the slot at handle-acquisition time.
    pub gen: u32,
}

/// Slot value never allocated (also the NIL link word in packed form).
pub const NIL_SLOT: u32 = u32::MAX;
/// Packed NIL for next/prev/empty words: `slot == NIL_SLOT`, `gen == 0`.
///
/// The pack format is `(slot: u64) << 32 | gen`, so the NIL word is the
/// slot in the *high* half — `0xFFFF_FFFF_0000_0000`.
pub(crate) const NIL_PACKED: u64 = (NIL_SLOT as u64) << 32;
/// Tombstone marker for linear-probe deletion: `slot = NIL_SLOT - 1`.
const TOMB_SLOT: u32 = u32::MAX - 1;

impl GenIndex {
    /// The NIL handle (`slot == NIL_SLOT`).
    pub const NIL: Self = Self {
        slot: NIL_SLOT,
        gen: 0,
    };

    /// `true` for the NIL handle.
    pub const fn is_nil(self) -> bool {
        self.slot == NIL_SLOT
    }

    /// Pack into one `u64` word: `slot:u32 | gen:u32`.
    pub(crate) const fn pack(self) -> u64 {
        ((self.slot as u64) << 32) | (self.gen as u64)
    }

    /// Unpack a packed word.
    pub(crate) const fn unpack(w: u64) -> Self {
        Self {
            slot: (w >> 32) as u32,
            gen: w as u32,
        }
    }
}

/// Per-order storage. All fields are atomics: id, quantity and the FIFO
/// links are visible to snapshot readers (through the seqlock protocol);
/// price and timestamp are writer-maintained. One node is one slab slot.
///
/// Public because [`OrderArena::from_static`] takes caller-owned static
/// node storage (core-only builds); construct with [`OrderNode::new_const`]
/// semantics — zeroed, free, NIL links.
pub struct OrderNode {
    /// Resting order id.
    pub(crate) id: AtomicU64,
    /// Price in ticks (`i64` bits, positive).
    pub(crate) price: AtomicU64,
    /// Resting quantity.
    pub(crate) qty: AtomicU64,
    /// Raw `u64` nanos carried opaquely from the command.
    pub(crate) ts_mono: AtomicU64,
    /// Next order in the level FIFO, packed `GenIndex` (NIL at the tail).
    /// While free: the next free slot in the recycling chain.
    pub(crate) next: AtomicU64,
    /// Previous order in the level FIFO, packed `GenIndex` (NIL at the
    /// head) — O(1) unlink on cancel/replace/execute.
    pub(crate) prev: AtomicU64,
    /// `gen:u32 | used:bit(32)` — the slot's generation and occupancy flag.
    pub(crate) meta: AtomicU64,
}

pub(crate) const USED_BIT: u64 = 1 << 32;
/// Side marker in `meta` (bit 33): set for bids. An order's side is part
/// of its identity — the FIFO it unlinks from is the side it was added
/// to, never "wherever this price happens to rest" (a crossed book can
/// have the same price resting on both sides).
pub(crate) const BID_BIT: u64 = 1 << 33;

impl OrderNode {
    /// Zeroed node: generation 0, free, NIL links. `const`, for `static`
    /// arena declarations.
    pub const fn new_const() -> Self {
        Self {
            id: AtomicU64::new(0),
            price: AtomicU64::new(0),
            qty: AtomicU64::new(0),
            ts_mono: AtomicU64::new(0),
            next: AtomicU64::new(NIL_PACKED),
            prev: AtomicU64::new(NIL_PACKED),
            meta: AtomicU64::new(0),
        }
    }

    /// Current generation (low 32 bits of `meta`).
    pub(crate) fn gen(&self, o: Ordering) -> u32 {
        self.meta.load(o) as u32
    }

    /// The node's side (`true` = bid). Valid while used.
    pub(crate) fn is_bid(&self, o: Ordering) -> bool {
        self.meta.load(o) & BID_BIT != 0
    }
}

/// Immutable snapshot of one resting order (introspection and tests).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OrderRec {
    /// Resting order id.
    pub id: OrderId,
    /// Price in ticks.
    pub price: crate::Price,
    /// Resting quantity.
    pub qty: crate::Qty,
    /// Raw `u64` nanos carried from the add command.
    pub ts_mono: u64,
}

/// Pre-allocated order slab (REQ-BOOK-008: zero heap allocation on the
/// apply path; the arena is built once, up front).
///
/// The `alloc` constructor leaks its backing storage deliberately: a book's
/// arena lives for the process lifetime (HFT substrate posture), which keeps
/// the whole crate free of `unsafe` and raw-pointer ownership. Use
/// [`OrderArena::from_static`] to back the arena with caller-owned `static`
/// memory in core-only builds.
pub struct OrderArena {
    nodes: &'static [OrderNode],
    /// Linear-probe id→`GenIndex` table: parallel id / packed-loc words.
    /// `id_loc` words are `NIL_PACKED` (empty) or a tombstone
    /// (`slot == NIL_SLOT - 1`) after deletion.
    id_key: &'static [AtomicU64],
    id_loc: &'static [AtomicU64],
    id_mask: u64,
    /// Free-chain head: packed `(slot:u32 | gen:u32)` of the next free
    /// slot; `NIL_PACKED` when full.
    free_head: AtomicU64,
    /// Number of occupied slots (diagnostic; writer-maintained).
    live: AtomicU64,
    /// Number of non-`NIL` words in the id table (live keys + tombstones).
    /// When this reaches the table length there is no `NIL` left, and a
    /// probe that terminates only on `NIL` would spin forever — the table
    /// is rehashed (tombstones cleared, live keys reinserted) before that
    /// can matter (see [`OrderArena::id_insert`]).
    entries: AtomicU64,
    /// Table marker: `id_loc` word for a deleted hash entry.
    tomb_empty: u64,
}

// Audit anchor: the arena is the book's shared state and contains no plain
// shared mutation — every field is atomic or immutable-after-build, so the
// auto `Send`/`Sync` apply without any `unsafe impl`.
const _: () = {
    const fn assert_sync<T: Sync>() {}
    const _: () = assert_sync::<OrderArena>();
};

impl OrderArena {
    /// Build over caller-owned static memory (core-only path, zero heap).
    ///
    /// `nodes` must start zeroed (`OrderNode::new_const` semantics); the
    /// `id_loc` words must start at `NIL_PACKED` (`0xFFFF_FFFF`); the
    /// id table must be a power-of-two length ≥ `nodes.len().max(16)`.
    ///
    /// # Panics
    /// If the table is not a power of two, or smaller than the arena
    /// capacity — the table could then not hold every live id.
    pub fn from_static(
        nodes: &'static [OrderNode],
        id_key: &'static [AtomicU64],
        id_loc: &'static [AtomicU64],
    ) -> Self {
        assert!(
            id_key.len() == id_loc.len() && id_key.len().is_power_of_two(),
            "id table must be a power-of-two length"
        );
        assert!(
            id_key.len() >= nodes.len().max(16),
            "id table smaller than arena capacity"
        );
        // Chain every slot onto the free list: 0 → 1 → … → cap-1 → NIL.
        for (i, n) in nodes.iter().enumerate() {
            let next = if i + 1 < nodes.len() {
                ((i + 1) as u64) << 32
            } else {
                NIL_PACKED
            };
            n.next.store(next, Ordering::SeqCst);
        }
        Self {
            nodes,
            id_key,
            id_loc,
            id_mask: (id_key.len() - 1) as u64,
            free_head: AtomicU64::new(0), // head = slot 0 (gen 0)
            live: AtomicU64::new(0),
            entries: AtomicU64::new(0),
            tomb_empty: (TOMB_SLOT as u64) << 32,
        }
    }

    /// Capacity in orders.
    pub fn capacity(&self) -> usize {
        self.nodes.len()
    }

    /// A fresh arena over leaked heap storage: zeroed nodes with the free
    /// chain linked, id table empty. The storage lives for the process
    /// (see the type docs for the posture).
    #[cfg(feature = "alloc")]
    pub fn heap(cap: usize) -> Self {
        let nodes = Box::leak(
            (0..cap)
                .map(|_| OrderNode::new_const())
                .collect::<alloc::vec::Vec<_>>()
                .into_boxed_slice(),
        );
        let table_len = cap.max(16).next_power_of_two();
        let id_key = Box::leak(
            (0..table_len)
                .map(|_| AtomicU64::new(0))
                .collect::<alloc::vec::Vec<_>>()
                .into_boxed_slice(),
        );
        let id_loc = Box::leak(
            (0..table_len)
                .map(|_| AtomicU64::new(NIL_PACKED))
                .collect::<alloc::vec::Vec<_>>()
                .into_boxed_slice(),
        );
        Self::from_static(nodes, id_key, id_loc)
    }

    /// Occupied slots (writer-maintained diagnostic).
    pub fn live(&self) -> usize {
        self.live.load(Ordering::SeqCst) as usize
    }

    /// Current generation of a slot (`None` beyond capacity).
    pub fn slot_gen(&self, slot: u32) -> Option<u32> {
        self.nodes
            .get(slot as usize)
            .map(|n| n.gen(Ordering::SeqCst))
    }

    /// Read an order through a generational handle: `None` when the handle
    /// is NIL, out of range, or **stale** — the slot's generation moved on
    /// since the handle was taken (REQ-BOOK-008: the ABA kill).
    pub fn get(&self, idx: GenIndex) -> Option<OrderRec> {
        if idx.is_nil() {
            return None;
        }
        let node = self.nodes.get(idx.slot as usize)?;
        let meta = node.meta.load(Ordering::SeqCst);
        if meta & USED_BIT == 0 || meta as u32 != idx.gen {
            return None;
        }
        Some(OrderRec {
            id: OrderId::new(node.id.load(Ordering::SeqCst)),
            price: node.price.load(Ordering::SeqCst) as isize as crate::Price,
            qty: node.qty.load(Ordering::SeqCst),
            ts_mono: node.ts_mono.load(Ordering::SeqCst),
        })
    }

    /// FNV-1a over the raw id → linear-probe start index.
    pub(crate) fn hash_id(id: OrderId, mask: u64) -> u64 {
        let mut h = crate::fnv::Fnv1a::new();
        h.write_u64(id.raw());
        h.finish() & mask
    }

    /// Insert `id -> idx`. The table cannot overflow while the arena holds
    /// (entries ≤ live orders ≤ capacity ≤ table length), so no error path.
    pub(crate) fn id_insert(&self, id: OrderId, idx: GenIndex) {
        // Tombstone saturation: with no NIL word left, a NIL-terminating
        // probe would spin forever. Rehash first — EXCLUDING the slot this
        // insert is about to claim, so the key is written exactly once
        // (a rehash that swept up the already-`USED` current node would
        // create a duplicate entry, and a later `id_remove` would leave
        // the stale twin resolvable — the resurrection bug).
        if self.entries.load(Ordering::SeqCst) as usize == self.id_loc.len() {
            self.rehash_excluding(idx.slot);
        }
        let mut i = Self::hash_id(id, self.id_mask);
        loop {
            let loc = self.id_loc[i as usize].load(Ordering::SeqCst);
            if loc == NIL_PACKED {
                self.entries.fetch_add(1, Ordering::SeqCst);
            }
            if loc == NIL_PACKED || loc == self.tomb_empty {
                self.id_key[i as usize].store(id.raw(), Ordering::SeqCst);
                self.id_loc[i as usize].store(idx.pack(), Ordering::SeqCst);
                return;
            }
            i = (i + 1) & self.id_mask;
        }
    }

    /// Rebuild the id table: drop every tombstone, reinsert all live ids
    /// **except** the order at `skip_slot` (the caller is inserting it).
    /// Writer-thread only, O(table + live). Live keys other than the
    /// skipped one are < table length, so a NIL word always remains and
    /// probes terminate.
    fn rehash_excluding(&self, skip_slot: u32) {
        for i in 0..self.id_loc.len() {
            self.id_loc[i].store(NIL_PACKED, Ordering::SeqCst);
            self.id_key[i].store(0, Ordering::SeqCst);
        }
        self.entries.store(0, Ordering::SeqCst);
        for (slot, node) in self.nodes.iter().enumerate() {
            if slot as u32 == skip_slot {
                continue; // the caller's insert will claim this key
            }
            let meta = node.meta.load(Ordering::SeqCst);
            if meta & USED_BIT == 0 {
                continue;
            }
            let id = node.id.load(Ordering::SeqCst);
            let mut i = Self::hash_id(OrderId::new(id), self.id_mask);
            loop {
                let loc = self.id_loc[i as usize].load(Ordering::SeqCst);
                if loc == NIL_PACKED {
                    break;
                }
                i = (i + 1) & self.id_mask;
            }
            self.id_key[i as usize].store(id, Ordering::SeqCst);
            self.id_loc[i as usize].store(
                GenIndex {
                    slot: slot as u32,
                    gen: meta as u32,
                }
                .pack(),
                Ordering::SeqCst,
            );
            self.entries.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Remove an id; returns the handle it mapped to (or NIL). A match
    /// whose handle is *stale* (generation moved on — a leftover duplicate
    /// or a post-free relic) is tombstoned for cleanup and probing
    /// continues; only a live handle is removed and returned.
    pub(crate) fn id_remove(&self, id: OrderId) -> GenIndex {
        let mut i = Self::hash_id(id, self.id_mask);
        for _ in 0..self.id_loc.len() {
            let loc = self.id_loc[i as usize].load(Ordering::SeqCst);
            if loc == NIL_PACKED {
                return GenIndex::NIL;
            }
            if loc != self.tomb_empty && self.id_key[i as usize].load(Ordering::SeqCst) == id.raw()
            {
                let idx = GenIndex::unpack(loc);
                if self.gen_live(idx) {
                    self.id_loc[i as usize].store(self.tomb_empty, Ordering::SeqCst);
                    return idx;
                }
                // Stale twin: clean it up, keep probing.
                self.id_loc[i as usize].store(self.tomb_empty, Ordering::SeqCst);
            }
            i = (i + 1) & self.id_mask;
        }
        GenIndex::NIL
    }

    /// Look up the handle for a resting id (`NIL` when absent). The probe
    /// is bounded by the table length — after a full wrap the key is
    /// absent, guaranteeing termination even in an all-tombstone table.
    /// A key match whose handle is **stale** (generation moved on: the
    /// order was freed/replaced and this entry is a leftover) reads as
    /// absent — a stale entry can never resurrect dead data.
    pub(crate) fn id_find(&self, id: OrderId) -> GenIndex {
        let mut i = Self::hash_id(id, self.id_mask);
        for _ in 0..self.id_loc.len() {
            let loc = self.id_loc[i as usize].load(Ordering::SeqCst);
            if loc == NIL_PACKED {
                return GenIndex::NIL;
            }
            if loc != self.tomb_empty && self.id_key[i as usize].load(Ordering::SeqCst) == id.raw()
            {
                let idx = GenIndex::unpack(loc);
                if self.gen_live(idx) {
                    return idx;
                }
                return GenIndex::NIL; // stale: the id is gone
            }
            i = (i + 1) & self.id_mask;
        }
        GenIndex::NIL
    }

    /// `true` when `idx` points at a live node whose generation matches
    /// the handle (the ABA check of [`OrderArena::get`], table-side).
    fn gen_live(&self, idx: GenIndex) -> bool {
        if idx.is_nil() {
            return false;
        }
        match self.nodes.get(idx.slot as usize) {
            Some(node) => {
                let meta = node.meta.load(Ordering::SeqCst);
                meta & USED_BIT != 0 && meta as u32 == idx.gen
            }
            None => false,
        }
    }

    /// Claim a free slot, bumping its generation (REQ-BOOK-008: reuse
    /// invalidates every outstanding handle into the slot).
    pub(crate) fn alloc_slot(&self) -> Result<GenIndex, Reject> {
        let head = self.free_head.load(Ordering::SeqCst);
        let slot = (head >> 32) as u32;
        let Some(node) = self.nodes.get(slot as usize) else {
            return Err(Reject::ArenaFull);
        };
        // The free chain rides in `node.next` while the slot is free.
        let next_free = node.next.load(Ordering::SeqCst);
        self.free_head.store(next_free, Ordering::SeqCst);
        let gen = node.gen(Ordering::SeqCst).wrapping_add(1);
        node.meta.store((gen as u64) | USED_BIT, Ordering::SeqCst);
        self.live.fetch_add(1, Ordering::SeqCst);
        Ok(GenIndex { slot, gen })
    }

    /// Release a slot: bump the generation, clear occupancy, push onto the
    /// free chain. After this call every outstanding handle into the slot
    /// is stale.
    pub(crate) fn free_slot(&self, idx: GenIndex) {
        let node = &self.nodes[idx.slot as usize];
        let head = self.free_head.load(Ordering::SeqCst);
        node.next.store(head, Ordering::SeqCst);
        node.prev.store(NIL_PACKED, Ordering::SeqCst);
        let gen = idx.gen.wrapping_add(1);
        node.meta.store(gen as u64, Ordering::SeqCst);
        self.free_head.store(idx.pack(), Ordering::SeqCst);
        self.live.fetch_sub(1, Ordering::SeqCst);
    }

    pub(crate) fn node(&self, slot: u32) -> &OrderNode {
        &self.nodes[slot as usize]
    }

    /// Iterate live handles in slot order (introspection/tests).
    pub fn iter_live(&self) -> impl Iterator<Item = (GenIndex, OrderRec)> + '_ {
        self.nodes.iter().enumerate().filter_map(move |(s, n)| {
            let meta = n.meta.load(Ordering::SeqCst);
            if meta & USED_BIT == 0 {
                return None;
            }
            let idx = GenIndex {
                slot: s as u32,
                gen: meta as u32,
            };
            self.get(idx).map(|rec| (idx, rec))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn nodes<const N: usize>() -> [OrderNode; N] {
        [const { OrderNode::new_const() }; N]
    }
    const fn table<const N: usize>() -> [AtomicU64; N] {
        let mut t = [const { AtomicU64::new(0) }; N];
        let mut i = 0;
        while i < N {
            t[i] = AtomicU64::new(NIL_PACKED);
            i += 1;
        }
        t
    }

    /// REQ-BOOK-008 — stale generation is rejected: after free + reuse the
    /// old handle no longer resolves (ABA killed at the arena level).
    #[test]
    fn stale_generation_index_rejected() {
        static NODES: [OrderNode; 8] = nodes::<8>();
        static ID_KEY: [AtomicU64; 16] = table::<16>();
        static ID_LOC: [AtomicU64; 16] = table::<16>();
        let a = OrderArena::from_static(&NODES, &ID_KEY, &ID_LOC);

        let idx = a.alloc_slot().expect("fresh arena has room");
        assert_eq!(idx.slot, 0);
        assert_eq!(
            a.get(idx).expect("live handle resolves").id,
            OrderId::new(0)
        );
        a.free_slot(idx);
        assert!(a.get(idx).is_none(), "freed handle does not resolve");

        // Reuse the slot: the generation must have moved on. Each
        // free→alloc cycle bumps twice (once on each edge).
        let idx2 = a.alloc_slot().expect("recycled");
        assert_eq!(idx2.slot, idx.slot, "free chain reuses the same slot");
        assert_eq!(idx2.gen, idx.gen + 2);
        assert!(
            a.get(idx).is_none(),
            "old handle is stale after reuse (ABA)"
        );
        assert!(a.get(idx2).is_some(), "fresh handle resolves");
    }

    /// REQ-BOOK-008 — the arena is bounded: exhaustion is a typed
    /// `ArenaFull`, never a panic or a grow.
    #[test]
    fn arena_full_is_typed() {
        static NODES: [OrderNode; 4] = nodes::<4>();
        static ID_KEY: [AtomicU64; 16] = table::<16>();
        static ID_LOC: [AtomicU64; 16] = table::<16>();
        let a = OrderArena::from_static(&NODES, &ID_KEY, &ID_LOC);

        let mut handles = Vec::new();
        for _ in 0..4 {
            handles.push(a.alloc_slot().expect("within capacity"));
        }
        assert_eq!(a.alloc_slot(), Err(Reject::ArenaFull));
        assert_eq!(a.live(), 4);
        a.free_slot(handles[3]);
        assert_eq!(a.live(), 3);
        assert!(a.alloc_slot().is_ok(), "freed slot recycles");
        assert_eq!(a.alloc_slot(), Err(Reject::ArenaFull));
    }

    /// Id table: insert/find/remove round trip, probing over tombstones.
    #[test]
    fn id_table_round_trip() {
        static NODES: [OrderNode; 8] = nodes::<8>();
        static ID_KEY: [AtomicU64; 16] = table::<16>();
        static ID_LOC: [AtomicU64; 16] = table::<16>();
        let a = OrderArena::from_static(&NODES, &ID_KEY, &ID_LOC);

        let mut handles = Vec::new();
        for raw in 0..8u64 {
            let idx = a.alloc_slot().expect("capacity");
            a.id_insert(OrderId::new(raw), idx);
            handles.push(idx);
        }
        for raw in 0..8u64 {
            assert_eq!(a.id_find(OrderId::new(raw)), handles[raw as usize]);
        }
        assert_eq!(a.id_find(OrderId::new(999)), GenIndex::NIL);
        assert_eq!(a.id_remove(OrderId::new(3)), handles[3]);
        assert_eq!(a.id_find(OrderId::new(3)), GenIndex::NIL);
        // Reinsert across the tombstone.
        a.id_insert(OrderId::new(3), handles[3]);
        assert_eq!(a.id_find(OrderId::new(3)), handles[3]);
        // Every other id still resolves (deletion kept the probe chain).
        for raw in 0..8u64 {
            if raw != 3 {
                assert_eq!(a.id_find(OrderId::new(raw)), handles[raw as usize]);
            }
        }
    }

    /// Generation bump cycles drive the wrap path (`wrapping_add`): the
    /// same slot recycled many times keeps invalidating older handles.
    #[test]
    fn repeated_reuse_keeps_invalidating_handles() {
        static NODES: [OrderNode; 1] = nodes::<1>();
        static ID_KEY: [AtomicU64; 16] = table::<16>();
        static ID_LOC: [AtomicU64; 16] = table::<16>();
        let a = OrderArena::from_static(&NODES, &ID_KEY, &ID_LOC);

        let first = a.alloc_slot().expect("capacity 1");
        a.free_slot(first);
        for round in 0..64u32 {
            let next = a.alloc_slot().expect("recycled the single slot");
            assert_eq!(next.slot, 0);
            // Two bumps per cycle: monotone, never re-presenting an old
            // generation to outstanding handles.
            assert_eq!(next.gen, first.gen + 2 * (round + 1));
            a.free_slot(next);
        }
        assert!(a.get(first).is_none());
    }
}
