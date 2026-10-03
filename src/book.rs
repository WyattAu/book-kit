//! The book: single writer, many lock-free readers (REQ-BOOK-002).
//!
//! # The seqlock protocol (REQ-BOOK-002, REQ-BOOK-007)
//!
//! One `AtomicU64` version counter guards every observable word. The
//! counter is **even when stable, odd while the writer is mid-apply**:
//!
//! | # | Actor  | Operation                                   | Ordering |
//! |---|--------|---------------------------------------------|----------|
//! | W1| writer | `version.store(v + 1)` — enter write window | `SeqCst` |
//! | W2| writer | mutate observable words                     | `SeqCst` |
//! | W3| writer | `version.store(v + 2)` — publish            | `SeqCst` |
//! | R1| reader | `version.load()` → reject odd               | `SeqCst` |
//! | R2| reader | copy observable words                       | `SeqCst` |
//! | R3| reader | `version.load()` → reject changed           | `SeqCst` |
//!
//! Ordering justification (the shm-rings standard: every ordering is
//! named and paid for; ORDERING.md carries the full matrix):
//!
//! * **W3 → R1**: the reader's `SeqCst` load of the published even value
//!   synchronizes with every W2 write of that version, so an accepted
//!   snapshot observes complete v1 state.
//! * **Everything in the protocol is `SeqCst` — data words included.**
//!   This is deliberate and it deviates from shm-rings' zero-`SeqCst`
//!   banner, for a reason worth restating: a seqlock whose data copies are
//!   `Relaxed` is formally unsound on the C++/Rust memory model *even*
//!   with a `SeqCst` version counter, because relaxed accesses sit
//!   outside the `SeqCst` total order `S` — a reader's data copy could
//!   observe a write from the *next* window while its version re-check
//!   still returns the old value, and no ordering violation would be
//!   derivable (the classic Boehm seqlock result). With every protocol
//!   access in `S`, the accepted-snapshot argument is a cycle check:
//!   a polluting write implies `W1 <_s W2 <_s R2 <_s R3 <_s W1`, which
//!   `S`'s acyclicity forbids. Unlike the shm-rings ring, a seqlock has
//!   no slot-ownership exclusivity to lean on, so it pays for the total
//!   order. Writer-only words (id table, free chain, live count, the
//!   writer's own pre-window version read) stay `Relaxed` — no reader
//!   ever touches them.
//! * Livelock is possible by design (a hot writer can lap readers
//!   forever); that is why both [`Book::try_read`] (one attempt) and
//!   [`Book::read`] (bounded retries with a spin hint, bound
//!   caller-visible) exist. Rejections are the typed [`Torn`] values — a
//!   reader never *observes* torn state, it is told it did not get a
//!   snapshot.
//!
//! # Single-writer contract
//!
//! `apply` takes `&mut self`, so two writers are a compile error. `Book`
//! is not `Clone` and never hands out another writer; readers attach
//! through [`Book::reader`] (alloc feature), which exposes only read
//! methods. All observable state is atomic, so a writer and readers on
//! distinct threads are sound with `unsafe` nowhere in the crate.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::arena::{GenIndex, NIL_PACKED};
use crate::buf::{BookBuf, LevelSnap, OrderSnap, SideSnap};
use crate::command::{Applied, Command, OrderId};
use crate::error::Torn;
use crate::fnv::Fnv1a;
use crate::levels::{LevelArena, SideStorage};
use crate::{Ask, Bid, Qty, Reject};

/// The shared book state: one writer (`&mut Book`), any number of readers
/// (`&Book` / [`BookReader`]). Everything observable is atomic.
pub(crate) struct Shared {
    /// Seqlock counter: even = stable, odd = writer mid-apply.
    version: AtomicU64,
    bids: SideStorage,
    asks: SideStorage,
    arena: crate::arena::OrderArena,
}

/// Lock-free limit order book — the state substrate *under* a matching
/// engine or feed handler, not one (REQ-BOOK-001, REQ-BOOK-003).
pub struct Book {
    #[cfg(feature = "alloc")]
    shared: alloc::sync::Arc<Shared>,
    #[cfg(not(feature = "alloc"))]
    shared: Shared,
}

/// Lock-free reader handle onto a [`Book`] (alloc feature).
///
/// Holds only read methods; the writer keeps exclusive `&mut Book`. Any
/// number of handles may read concurrently with the writer and each other
/// — the seqlock protocol arbitrates, never a lock.
#[cfg(feature = "alloc")]
pub struct BookReader {
    shared: alloc::sync::Arc<Shared>,
}

impl Book {
    /// Assemble a book over caller-provided memory (REQ-BOOK-008: the
    /// arena and level storage are pre-allocated; the hot path never
    /// allocates).
    pub fn with_arena(orders: crate::arena::OrderArena, levels: LevelArena) -> Self {
        let shared = Shared {
            version: AtomicU64::new(0),
            bids: levels.bids,
            asks: levels.asks,
            arena: orders,
        };
        Self {
            #[cfg(feature = "alloc")]
            shared: alloc::sync::Arc::new(shared),
            #[cfg(not(feature = "alloc"))]
            shared,
        }
    }

    /// A book with freshly allocated arenas (alloc feature). Storage is
    /// leaked intentionally — see [`LevelArena`] / [`crate::OrderArena`]
    /// type docs for the process-lifetime posture.
    #[cfg(feature = "alloc")]
    pub fn with_capacity(max_orders: usize, max_levels: usize) -> Self {
        Self::with_arena(
            crate::arena::OrderArena::heap(max_orders),
            LevelArena::heap(max_levels),
        )
    }

    /// Attach a lock-free reader (alloc feature). Readers never take
    /// locks; see the module docs for the consistency contract.
    #[cfg(feature = "alloc")]
    pub fn reader(&self) -> BookReader {
        BookReader {
            shared: alloc::sync::Arc::clone(&self.shared),
        }
    }

    fn sh(&self) -> &Shared {
        #[cfg(feature = "alloc")]
        {
            &self.shared
        }
        #[cfg(not(feature = "alloc"))]
        {
            &self.shared
        }
    }

    // ---- writer path (single thread; zero-alloc) — REQ-BOOK-008 ----

    /// Apply one lifecycle command (REQ-BOOK-003).
    ///
    /// Every failure is a typed [`Reject`] — no panics, no silent drops.
    /// Rejected commands leave the book unmodified and do **not** open a
    /// write window (pure validations reject before W1). Zero heap
    /// allocation on every path (REQ-BOOK-008).
    pub fn apply(&mut self, cmd: Command) -> Result<Applied, Reject> {
        let sh = self.sh();
        sh.validate(&cmd)?;
        // W1: enter the write window (odd).
        // The writer's own pre-window read: single-writer self-coherence
        // makes `Relaxed` sufficient here — W1 below is the first protocol
        // op and is `SeqCst`.
        let v = sh.version.load(Ordering::Relaxed);
        debug_assert_eq!(v % 2, 0, "writer saw an odd version");
        sh.version.store(v + 1, Ordering::SeqCst);
        let r = sh.mutate(cmd);
        // W3: publish (even, carrying every W2 write to any R1).
        sh.version.store(v + 2, Ordering::SeqCst);
        r.map(|mut a| {
            a.version = v + 2;
            a
        })
    }

    /// The current seqlock version (even between applies).
    pub fn version(&self) -> u64 {
        self.sh().version.load(Ordering::SeqCst)
    }

    /// The generational index a resting order id maps to (`None` when the
    /// id is not resting) — for journals and tests that exercise ABA
    /// semantics explicitly (REQ-BOOK-008).
    pub fn gen_index_of(&self, id: OrderId) -> Option<GenIndex> {
        let idx = self.sh().arena.id_find(id);
        (!idx.is_nil()).then_some(idx)
    }

    /// Resolve a generational index. `Err(Reject::StaleGeneration)` is the
    /// documented ABA outcome: the slot's generation moved on since the
    /// index was taken, so the handle cannot resurrect old data
    /// (REQ-BOOK-008).
    pub fn order_at(&self, idx: GenIndex) -> Result<crate::arena::OrderRec, Reject> {
        self.sh().arena.get(idx).ok_or(Reject::StaleGeneration)
    }

    /// Deterministic FNV-1a-64 over the full book state: version, then per
    /// side the level array (price, L2 quantity) and per level the L3 FIFO
    /// in order (id, quantity) — REQ-BOOK-010.
    ///
    /// Writer-side utility: call between applies (version even). The hash
    /// is *detection*, not adversarial protection (non-cryptographic by
    /// spec decision).
    pub fn checksum(&self) -> u64 {
        self.sh().checksum()
    }

    // ---- reader path (lock-free, consistent) — REQ-BOOK-002 ----

    /// One snapshot attempt into the reader-owned buffer (REQ-BOOK-002).
    ///
    /// `Ok(version)` means the buffer now holds exactly that version's
    /// state; `Err(Torn)` means it holds nothing usable. Never allocates.
    pub fn try_read(&self, out: &mut BookBuf) -> Result<u64, Torn> {
        self.sh().try_read(out)
    }

    /// Bounded-retry snapshot: up to `max_spins` attempts with a spin hint
    /// between them (REQ-BOOK-002: the retry bound is caller-visible).
    /// Returns the last [`Torn`] on exhaustion.
    pub fn read(&self, out: &mut BookBuf, max_spins: u32) -> Result<u64, Torn> {
        self.sh().read(out, max_spins)
    }

    /// View the bid side of a completed snapshot (REQ-BOOK-005). Views are
    /// empty until a successful read has filled the buffer.
    pub fn bids<'a>(&self, buf: &'a BookBuf) -> crate::buf::SideView<'a, Bid> {
        crate::buf::SideView::new(buf, true)
    }

    /// View the ask side of a completed snapshot (REQ-BOOK-005).
    pub fn asks<'a>(&self, buf: &'a BookBuf) -> crate::buf::SideView<'a, Ask> {
        crate::buf::SideView::new(buf, false)
    }
}

/// Lock-free reader handle methods (alloc feature).
#[cfg(feature = "alloc")]
impl BookReader {
    /// One snapshot attempt (see [`Book::try_read`]).
    pub fn try_read(&self, out: &mut BookBuf) -> Result<u64, Torn> {
        self.shared.try_read(out)
    }

    /// Bounded-retry snapshot (see [`Book::read`]).
    pub fn read(&self, out: &mut BookBuf, max_spins: u32) -> Result<u64, Torn> {
        self.shared.read(out, max_spins)
    }

    /// The current seqlock version as observed by this reader.
    pub fn version(&self) -> u64 {
        self.shared.version.load(Ordering::SeqCst)
    }

    /// View the bid side of a completed snapshot.
    pub fn bids<'a>(&self, buf: &'a BookBuf) -> crate::buf::SideView<'a, Bid> {
        crate::buf::SideView::new(buf, true)
    }

    /// View the ask side of a completed snapshot.
    pub fn asks<'a>(&self, buf: &'a BookBuf) -> crate::buf::SideView<'a, Ask> {
        crate::buf::SideView::new(buf, false)
    }
}

impl Shared {
    /// Validation pass — pure reads of writer-owned state, no window.
    fn validate(&self, cmd: &Command) -> Result<(), Reject> {
        match *cmd {
            Command::AddBid { id, price, qty, .. } | Command::AddAsk { id, price, qty, .. } => {
                if qty == 0 {
                    return Err(Reject::ZeroQty);
                }
                if price <= 0 {
                    return Err(Reject::NonPositivePrice);
                }
                if !self.arena.id_find(id).is_nil() {
                    return Err(Reject::DuplicateId);
                }
            }
            Command::Cancel { id } => {
                if self.arena.id_find(id).is_nil() {
                    return Err(Reject::UnknownOrder);
                }
            }
            Command::Replace {
                id,
                new_price,
                new_qty,
            } => {
                if new_qty == 0 {
                    return Err(Reject::ZeroQty);
                }
                if new_price <= 0 {
                    return Err(Reject::NonPositivePrice);
                }
                if self.arena.id_find(id).is_nil() {
                    return Err(Reject::UnknownOrder);
                }
            }
            Command::Execute { id, qty } => {
                if qty == 0 {
                    return Err(Reject::ZeroQty);
                }
                if self.arena.id_find(id).is_nil() {
                    return Err(Reject::UnknownOrder);
                }
            }
        }
        Ok(())
    }

    /// The mutation pass — runs inside the odd write window (W2 zone).
    fn mutate(&self, cmd: Command) -> Result<Applied, Reject> {
        match cmd {
            Command::AddBid {
                id,
                price,
                qty,
                ts_mono,
            } => self.add(true, id, price, qty, ts_mono),
            Command::AddAsk {
                id,
                price,
                qty,
                ts_mono,
            } => self.add(false, id, price, qty, ts_mono),
            Command::Cancel { id } => self.cancel(id),
            Command::Replace {
                id,
                new_price,
                new_qty,
            } => self.replace(id, new_price, new_qty),
            Command::Execute { id, qty } => self.execute(id, qty),
        }
    }

    fn add(
        &self,
        is_bid: bool,
        id: OrderId,
        price: crate::Price,
        qty: Qty,
        ts_mono: u64,
    ) -> Result<Applied, Reject> {
        let idx = self.arena.alloc_slot()?;
        let st = self.side(is_bid);
        let pos = st.find_slot(is_bid, price);
        let existing =
            pos < st.len(Ordering::SeqCst) && st.price_at(pos, Ordering::SeqCst) == price;
        if !existing && st.len(Ordering::SeqCst) >= st.capacity() {
            // Level table full — release the claimed slot, reject typed.
            self.arena.free_slot(idx);
            return Err(Reject::ArenaFull);
        }
        if !existing {
            st.insert_level(pos, price);
        }
        let level = pos;
        // Form the node fully before linking, and publish it in the id
        // table (DuplicateId validations read this table). The side goes
        // into the node's meta word: every later unlink is side-exact.
        let node = self.arena.node(idx.slot);
        node.id.store(id.raw(), Ordering::SeqCst);
        node.price.store(price as u64, Ordering::SeqCst);
        node.qty.store(qty, Ordering::SeqCst);
        node.ts_mono.store(ts_mono, Ordering::SeqCst);
        if is_bid {
            node.meta.fetch_or(crate::arena::BID_BIT, Ordering::SeqCst);
        }
        self.arena.id_insert(id, idx);
        link_tail(st, &self.arena, level, idx);
        st.qty[level].fetch_add(qty, Ordering::SeqCst);
        Ok(Applied {
            id,
            executed: 0,
            remaining: qty,
            version: 0, // patched by `apply`
        })
    }

    fn cancel(&self, id: OrderId) -> Result<Applied, Reject> {
        let idx = self.arena.id_remove(id);
        if idx.is_nil() {
            return Err(Reject::UnknownOrder);
        }
        self.unlink_and_free(idx);
        Ok(Applied {
            id,
            executed: 0,
            remaining: 0,
            version: 0,
        })
    }

    fn replace(
        &self,
        id: OrderId,
        new_price: crate::Price,
        new_qty: Qty,
    ) -> Result<Applied, Reject> {
        let idx = self.arena.id_find(id);
        if idx.is_nil() {
            return Err(Reject::UnknownOrder);
        }
        let node = self.arena.node(idx.slot);
        let old_price = node.price.load(Ordering::SeqCst) as isize as crate::Price;
        let old_qty = node.qty.load(Ordering::SeqCst);
        // The side is the order's own identity bit — never "wherever this
        // price rests" (a crossed book may rest the same price on both
        // sides).
        let is_bid = node.is_bid(Ordering::SeqCst);
        let Some(old_level) = self.level_on(is_bid, old_price) else {
            return Err(Reject::UnknownOrder);
        };
        let st = self.side(is_bid);
        if new_price == old_price {
            // Quantity-only amend: keep the FIFO position.
            st.qty[old_level].fetch_sub(old_qty, Ordering::SeqCst);
            st.qty[old_level].fetch_add(new_qty, Ordering::SeqCst);
            node.qty.store(new_qty, Ordering::SeqCst);
        } else {
            // Price change: unlink from the old level, re-link at the tail
            // of the new price level (loses time priority — documented on
            // `Command::Replace`).
            unlink(st, &self.arena, old_level, idx);
            st.qty[old_level].fetch_sub(old_qty, Ordering::SeqCst);
            if st.count_at(old_level, Ordering::SeqCst) == 0 {
                st.remove_level(old_level);
            }
            let pos = st.find_slot(is_bid, new_price);
            let needs_new_level =
                pos == st.len(Ordering::SeqCst) || st.price_at(pos, Ordering::SeqCst) != new_price;
            if needs_new_level && st.len(Ordering::SeqCst) >= st.capacity() {
                // New level does not fit: restore the order at its old
                // price so the book stays consistent, then reject typed.
                let level = self.restore_level(st, is_bid, old_price);
                node.price.store(old_price as u64, Ordering::SeqCst);
                link_tail(st, &self.arena, level, idx);
                st.qty[level].fetch_add(old_qty, Ordering::SeqCst);
                return Err(Reject::ArenaFull);
            }
            if needs_new_level {
                st.insert_level(pos, new_price);
            }
            node.price.store(new_price as u64, Ordering::SeqCst);
            node.qty.store(new_qty, Ordering::SeqCst);
            link_tail(st, &self.arena, pos, idx);
            st.qty[pos].fetch_add(new_qty, Ordering::SeqCst);
        }
        Ok(Applied {
            id,
            executed: 0,
            remaining: new_qty,
            version: 0,
        })
    }

    fn execute(&self, id: OrderId, qty: Qty) -> Result<Applied, Reject> {
        let idx = self.arena.id_find(id);
        if idx.is_nil() {
            return Err(Reject::UnknownOrder);
        }
        let node = self.arena.node(idx.slot);
        let price = node.price.load(Ordering::SeqCst) as isize as crate::Price;
        let resting = node.qty.load(Ordering::SeqCst);
        let is_bid = node.is_bid(Ordering::SeqCst);
        let Some(level) = self.level_on(is_bid, price) else {
            return Err(Reject::UnknownOrder);
        };
        let st = self.side(is_bid);
        let executed = qty.min(resting);
        let remaining = resting - executed;
        if remaining == 0 {
            self.arena.id_remove(id);
            unlink(st, &self.arena, level, idx);
            st.qty[level].fetch_sub(resting, Ordering::SeqCst);
            if st.count_at(level, Ordering::SeqCst) == 0 {
                st.remove_level(level);
            }
            self.arena.free_slot(idx);
        } else {
            node.qty.store(remaining, Ordering::SeqCst);
            st.qty[level].fetch_sub(executed, Ordering::SeqCst);
        }
        Ok(Applied {
            id,
            executed,
            remaining,
            version: 0,
        })
    }

    /// Re-insert (or find) the level for `old_price` during a failed
    /// replace-restore. Returns the level index.
    fn restore_level(&self, st: &SideStorage, is_bid: bool, old_price: crate::Price) -> usize {
        let pos = st.find_slot(is_bid, old_price);
        if pos == st.len(Ordering::SeqCst) || st.price_at(pos, Ordering::SeqCst) != old_price {
            st.insert_level(pos, old_price);
        }
        pos
    }

    /// Level index where `price` rests **on this side** (`None` when
    /// absent). Side-exact: a crossed book may rest the same price on the
    /// other side simultaneously.
    fn level_on(&self, is_bid: bool, price: crate::Price) -> Option<usize> {
        let st = self.side(is_bid);
        let pos = st.find_slot(is_bid, price);
        (pos < st.len(Ordering::SeqCst) && st.price_at(pos, Ordering::SeqCst) == price)
            .then_some(pos)
    }

    fn side(&self, is_bid: bool) -> &SideStorage {
        if is_bid {
            &self.bids
        } else {
            &self.asks
        }
    }

    /// Unlink `idx` from its level FIFO (via its own side and price) and
    /// free the slot back to the arena with a generation bump.
    fn unlink_and_free(&self, idx: GenIndex) {
        let node = self.arena.node(idx.slot);
        let price = node.price.load(Ordering::SeqCst) as isize as crate::Price;
        let qty = node.qty.load(Ordering::SeqCst);
        let is_bid = node.is_bid(Ordering::SeqCst);
        if let Some(level) = self.level_on(is_bid, price) {
            let st = self.side(is_bid);
            unlink(st, &self.arena, level, idx);
            st.qty[level].fetch_sub(qty, Ordering::SeqCst);
            if st.count_at(level, Ordering::SeqCst) == 0 {
                st.remove_level(level);
            }
        }
        self.arena.free_slot(idx);
    }

    // ---- reader path ----

    fn try_read(&self, out: &mut BookBuf) -> Result<u64, Torn> {
        out.reset();
        // R1: reject odd.
        let v1 = self.version.load(Ordering::SeqCst);
        if v1 % 2 == 1 {
            return Err(Torn::WriteInProgress);
        }
        // Explicit capacity check *before* copying: truncation is a typed
        // error, never silent partial state (spec §Risk register).
        let bl = self.bids.len(Ordering::SeqCst);
        let al = self.asks.len(Ordering::SeqCst);
        let (level_cap, order_cap) = out.capacity();
        if bl > level_cap || al > level_cap || self.orders_needed(bl, al) > order_cap {
            return Err(Torn::BufferTooSmall);
        }
        // R2: copy (Relaxed reads; a window-polluted copy is discarded at
        // R3 below and never exposed — `set_side`/`set_version` happen
        // only on success).
        let b = self.copy_side(true, out);
        let a = self.copy_side(false, out);
        // R3: reject changed.
        let v2 = self.version.load(Ordering::SeqCst);
        if v2 != v1 {
            return Err(Torn::VersionChanged);
        }
        out.set_side(true, b);
        out.set_side(false, a);
        out.set_version(v1);
        debug_assert!(
            crate::buf::depth_ordered(true, out.level_slice(b.off, b.len))
                && crate::buf::depth_ordered(false, out.level_slice(a.off, a.len)),
            "captured depth must be best→worst"
        );
        Ok(v1)
    }

    fn read(&self, out: &mut BookBuf, max_spins: u32) -> Result<u64, Torn> {
        let mut last = Torn::WriteInProgress;
        for _ in 0..max_spins {
            core::hint::spin_loop();
            match self.try_read(out) {
                Ok(v) => return Ok(v),
                Err(t) => {
                    if !t.is_retryable() {
                        return Err(t);
                    }
                    last = t;
                }
            }
        }
        Err(last)
    }

    fn orders_needed(&self, bl: usize, al: usize) -> usize {
        let mut n = 0u64;
        for i in 0..bl {
            n += self.bids.count_at(i, Ordering::SeqCst);
        }
        for i in 0..al {
            n += self.asks.count_at(i, Ordering::SeqCst);
        }
        n as usize
    }

    fn copy_side(&self, is_bid: bool, out: &mut BookBuf) -> SideSnap {
        let st = self.side(is_bid);
        let n = st.len(Ordering::SeqCst).min(out.levels_cap());
        let mut snap = SideSnap {
            off: out.levels_offset(),
            len: 0,
            total_qty: 0,
        };
        for i in 0..n {
            let head = st.head_at(i, Ordering::SeqCst);
            let orders_off = out.orders_offset() as u32;
            // Walk the FIFO (L3). Bounded by the arena capacity: during a
            // concurrent write window the links may be mid-mutation; such
            // copies are discarded at R3, so we only need termination.
            let mut steps = 0usize;
            let mut cur = head;
            let mut walked = 0u64;
            while !cur.is_nil() && steps < self.arena.capacity() {
                steps += 1;
                let Some(rec) = self.arena.get(cur) else {
                    break; // stale generation mid-walk: stop this level
                };
                out.push_order(OrderSnap {
                    id: rec.id,
                    qty: rec.qty,
                });
                walked += 1;
                cur = GenIndex::unpack(self.arena.node(cur.slot).next.load(Ordering::SeqCst));
            }
            let qty = st.qty_at(i, Ordering::SeqCst);
            out.push_level(LevelSnap {
                price: st.price_at(i, Ordering::SeqCst),
                qty,
                orders_len: walked as u32,
                orders_off,
            });
            snap.len += 1;
            snap.total_qty = snap.total_qty.saturating_add(qty);
        }
        snap
    }

    /// REQ-BOOK-010: FNV-1a-64 over version + per-side levels + L3 FIFOs.
    fn checksum(&self) -> u64 {
        let mut h = Fnv1a::new();
        h.write_u64(self.version.load(Ordering::SeqCst));
        for (_is_bid, st) in [(true, &self.bids), (false, &self.asks)] {
            let n = st.len(Ordering::SeqCst);
            h.write_u64(n as u64);
            for i in 0..n {
                h.write_u64(st.price_at(i, Ordering::SeqCst) as u64);
                h.write_u64(st.qty_at(i, Ordering::SeqCst));
                // FIFO in order: id then qty per resting order.
                let mut cur = st.head_at(i, Ordering::SeqCst);
                let mut steps = 0usize;
                while !cur.is_nil() && steps < self.arena.capacity() {
                    steps += 1;
                    match self.arena.get(cur) {
                        Some(rec) => {
                            h.write_u64(rec.id.raw());
                            h.write_u64(rec.qty);
                        }
                        // Mid-apply tear (writer called checksum inside its
                        // own window): deterministic marker, hash stays a
                        // pure function of observed state.
                        None => {
                            h.write_u64(u64::MAX);
                            break;
                        }
                    }
                    cur = GenIndex::unpack(self.arena.node(cur.slot).next.load(Ordering::SeqCst));
                }
            }
        }
        h.finish()
    }
}

/// Append `idx` to the FIFO tail of `level` (price-time priority: new
/// orders arrive behind resting ones at the same price).
fn link_tail(st: &SideStorage, arena: &crate::arena::OrderArena, level: usize, idx: GenIndex) {
    let node = arena.node(idx.slot);
    node.next.store(NIL_PACKED, Ordering::SeqCst);
    let tail = st.tail_at(level, Ordering::SeqCst);
    if tail.is_nil() {
        node.prev.store(NIL_PACKED, Ordering::SeqCst);
        st.head[level].store(idx.pack(), Ordering::SeqCst);
    } else {
        node.prev.store(tail.pack(), Ordering::SeqCst);
        arena
            .node(tail.slot)
            .next
            .store(idx.pack(), Ordering::SeqCst);
    }
    st.tail[level].store(idx.pack(), Ordering::SeqCst);
    st.count[level].fetch_add(1, Ordering::SeqCst);
}

/// O(1) unlink of `idx` from its level FIFO using prev/next.
fn unlink(st: &SideStorage, arena: &crate::arena::OrderArena, level: usize, idx: GenIndex) {
    let node = arena.node(idx.slot);
    let prev = GenIndex::unpack(node.prev.load(Ordering::SeqCst));
    let next = GenIndex::unpack(node.next.load(Ordering::SeqCst));
    if prev.is_nil() {
        st.head[level].store(next.pack(), Ordering::SeqCst);
    } else {
        arena
            .node(prev.slot)
            .next
            .store(next.pack(), Ordering::SeqCst);
    }
    if next.is_nil() {
        st.tail[level].store(prev.pack(), Ordering::SeqCst);
    } else {
        arena
            .node(next.slot)
            .prev
            .store(prev.pack(), Ordering::SeqCst);
    }
    st.count[level].fetch_sub(1, Ordering::SeqCst);
}
