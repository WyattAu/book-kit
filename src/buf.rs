//! Reader-owned snapshot buffer and the L2/L3 views over it
//! (REQ-BOOK-001, REQ-BOOK-002, REQ-BOOK-005, REQ-BOOK-006).
//!
//! A [`BookBuf`] is plain (non-atomic) memory owned by the reader. A
//! successful `try_read`/`read` fills it with a version-consistent copy of
//! the book; every view (`bids`/`asks` → `SideView` → `LevelView`) then
//! reads the buffer, never the book — so views are race-free by
//! construction and allocate nothing.

use core::marker::PhantomData;

use crate::command::OrderId;
use crate::side::Side;
use crate::{Price, Qty};

/// One captured price level: the L2 aggregate plus the bounds of its L3
/// FIFO inside the buffer's order scratch. Public because
/// [`BookBuf::from_static`] takes caller-owned static scratch (core-only
/// builds); treat the fields as opaque.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LevelSnap {
    /// Price in ticks.
    pub(crate) price: Price,
    /// Aggregated resting quantity (L2).
    pub(crate) qty: Qty,
    /// Order count captured for this level.
    pub(crate) orders_len: u32,
    /// Offset of this level's orders in the buffer's order scratch.
    pub(crate) orders_off: u32,
}

/// One captured L3 order (see [`LevelSnap`] visibility note).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OrderSnap {
    pub(crate) id: OrderId,
    pub(crate) qty: Qty,
}

/// Per-side captured slice.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct SideSnap {
    /// Offset of this side's levels in the buffer's level scratch.
    pub(crate) off: usize,
    /// Captured level count (best→worst).
    pub(crate) len: usize,
    /// Sum of all captured level quantities.
    pub(crate) total_qty: Qty,
}

/// Level scratch: fixed-capacity, explicitly counted (never reallocates —
/// a read is allocation-free, REQ-BOOK-008 discipline extended to reads).
struct LevelScratch {
    cap_per_side: usize,
    len: usize,
    #[cfg(feature = "alloc")]
    inner: alloc::boxed::Box<[LevelSnap]>,
    #[cfg(not(feature = "alloc"))]
    inner: &'static mut [LevelSnap],
}

impl LevelScratch {
    #[cfg(feature = "alloc")]
    fn new(cap_per_side: usize) -> Self {
        Self {
            cap_per_side,
            len: 0,
            inner: alloc::vec![LevelSnap::default(); 2 * cap_per_side].into_boxed_slice(),
        }
    }

    #[cfg(not(feature = "alloc"))]
    fn with_static(inner: &'static mut [LevelSnap], cap_per_side: usize) -> Self {
        assert!(
            inner.len() >= 2 * cap_per_side,
            "static level scratch smaller than 2 * levels_per_side"
        );
        Self {
            cap_per_side,
            len: 0,
            inner,
        }
    }

    fn cap_per_side(&self) -> usize {
        self.cap_per_side
    }

    fn clear(&mut self) {
        self.len = 0;
    }

    fn push(&mut self, l: LevelSnap) {
        assert!(
            self.len < self.inner.len(),
            "level scratch overflow (capacity is explicit)"
        );
        self.inner[self.len] = l;
        self.len += 1;
    }

    fn slice(&self, off: usize, len: usize) -> &[LevelSnap] {
        &self.inner[off..off + len]
    }
}

/// Order scratch: same shape.
struct OrderScratch {
    cap_total: usize,
    len: usize,
    #[cfg(feature = "alloc")]
    inner: alloc::boxed::Box<[OrderSnap]>,
    #[cfg(not(feature = "alloc"))]
    inner: &'static mut [OrderSnap],
}

impl OrderScratch {
    #[cfg(feature = "alloc")]
    fn new(cap_total: usize) -> Self {
        Self {
            cap_total,
            len: 0,
            inner: alloc::vec![OrderSnap {
                id: OrderId::new(0),
                qty: 0
            }; cap_total]
            .into_boxed_slice(),
        }
    }

    #[cfg(not(feature = "alloc"))]
    fn with_static(inner: &'static mut [OrderSnap], cap_total: usize) -> Self {
        assert!(
            inner.len() >= cap_total,
            "static order scratch smaller than orders_total"
        );
        Self {
            cap_total,
            len: 0,
            inner,
        }
    }

    fn cap_total(&self) -> usize {
        self.cap_total
    }

    fn clear(&mut self) {
        self.len = 0;
    }

    fn push(&mut self, o: OrderSnap) {
        assert!(
            self.len < self.inner.len(),
            "order scratch overflow (capacity is explicit)"
        );
        self.inner[self.len] = o;
        self.len += 1;
    }

    fn slice(&self, off: usize, len: usize) -> &[OrderSnap] {
        &self.inner[off..off + len]
    }
}

/// Reader-owned scratch for snapshots (REQ-BOOK-002).
///
/// Capacity is explicit: a snapshot of a book deeper than the buffer fails
/// with [`Torn::BufferTooSmall`] rather than truncating silently (spec
/// §Risk register). The buffer is reused across reads — a read performs no
/// allocation.
pub struct BookBuf {
    levels: LevelScratch,
    orders: OrderScratch,
    bids: SideSnap,
    asks: SideSnap,
    /// Seqlock version this buffer holds, `None` until a read succeeds.
    version: Option<u64>,
}

impl BookBuf {
    /// A buffer holding `levels_per_side` price levels per side and
    /// `orders_total` L3 orders across all captured levels (alloc feature).
    #[cfg(feature = "alloc")]
    pub fn new(levels_per_side: usize, orders_total: usize) -> Self {
        Self {
            levels: LevelScratch::new(levels_per_side),
            orders: OrderScratch::new(orders_total),
            bids: SideSnap::default(),
            asks: SideSnap::default(),
            version: None,
        }
    }

    /// Core-only constructor over caller-owned static scratch (zero heap).
    /// `levels` must hold ≥ `2 * levels_per_side` slots; `orders` ≥
    /// `orders_total`.
    ///
    /// # Panics
    /// If the static scratch is smaller than the requested capacity.
    #[cfg(not(feature = "alloc"))]
    pub fn from_static(
        levels: &'static mut [LevelSnap],
        orders: &'static mut [OrderSnap],
        levels_per_side: usize,
        orders_total: usize,
    ) -> Self {
        Self {
            levels: LevelScratch::with_static(levels, levels_per_side),
            orders: OrderScratch::with_static(orders, orders_total),
            bids: SideSnap::default(),
            asks: SideSnap::default(),
            version: None,
        }
    }

    /// Explicit capacity: `(levels per side, orders total)`.
    pub fn capacity(&self) -> (usize, usize) {
        (self.levels.cap_per_side(), self.orders.cap_total())
    }

    /// The seqlock version this buffer holds — `None` until a read
    /// succeeds, and `None` again after a failed read: partial copies are
    /// never exposed as valid (REQ-BOOK-002).
    pub fn version(&self) -> Option<u64> {
        self.version
    }

    pub(crate) fn reset(&mut self) {
        self.levels.clear();
        self.orders.clear();
        self.bids = SideSnap::default();
        self.asks = SideSnap::default();
        self.version = None;
    }

    pub(crate) fn levels_cap(&self) -> usize {
        self.levels.cap_per_side()
    }

    pub(crate) fn levels_offset(&self) -> usize {
        self.levels.len
    }

    pub(crate) fn orders_offset(&self) -> usize {
        self.orders.len
    }

    pub(crate) fn push_level(&mut self, l: LevelSnap) {
        self.levels.push(l);
    }

    pub(crate) fn push_order(&mut self, o: OrderSnap) {
        self.orders.push(o);
    }

    pub(crate) fn set_side(&mut self, is_bid: bool, snap: SideSnap) {
        if is_bid {
            self.bids = snap;
        } else {
            self.asks = snap;
        }
    }

    pub(crate) fn set_version(&mut self, v: u64) {
        self.version = Some(v);
    }

    pub(crate) fn side_snap(&self, is_bid: bool) -> SideSnap {
        if is_bid {
            self.bids
        } else {
            self.asks
        }
    }

    pub(crate) fn level_slice(&self, off: usize, len: usize) -> &[LevelSnap] {
        self.levels.slice(off, len)
    }

    pub(crate) fn order_slice(&self, off: usize, len: usize) -> &[OrderSnap] {
        self.orders.slice(off, len)
    }

    pub(crate) fn orders_used(&self) -> usize {
        self.orders.len
    }
}

/// A fresh buffer with the same explicit capacities. Snapshot contents
/// are *not* copied — this clones the buffer, not the snapshot
/// (alloc feature only; the core constructor takes exclusive static
/// scratch, which cannot be duplicated).
#[cfg(feature = "alloc")]
impl Clone for BookBuf {
    fn clone(&self) -> Self {
        Self::new(self.levels.cap_per_side(), self.orders.cap_total())
    }
}

/// View of one side of a snapshot (REQ-BOOK-005: lifetime-bound,
/// side-typed — `SideView<Bid>` and `SideView<Ask>` are distinct types and
/// neither can be built from the other side's data).
#[derive(Clone, Copy)]
pub struct SideView<'a, S: Side> {
    levels: &'a [LevelSnap],
    orders: &'a [OrderSnap],
    total_qty: Qty,
    _side: PhantomData<S>,
}

impl<'a, S: Side> SideView<'a, S> {
    pub(crate) fn new(buf: &'a BookBuf, is_bid: bool) -> Self {
        let snap = buf.side_snap(is_bid);
        Self {
            levels: buf.level_slice(snap.off, snap.len),
            orders: buf.order_slice(0, buf.orders_used()),
            total_qty: snap.total_qty,
            _side: PhantomData,
        }
    }

    /// Best price level — O(1) (REQ-BOOK-006: index 0 of the captured,
    /// sorted depth). `None` on an empty side.
    pub fn best(&self) -> Option<LevelView<'a, S>> {
        self.levels.first().map(|l| LevelView {
            level: l,
            orders: self.orders,
            _side: PhantomData,
        })
    }

    /// All captured price levels, best→worst (REQ-BOOK-006). Allocation-
    /// free: the iterator borrows the buffer.
    pub fn depth(&self) -> DepthIter<'a, S> {
        DepthIter {
            levels: self.levels,
            orders: self.orders,
            pos: 0,
            _side: PhantomData,
        }
    }

    /// Total resting quantity across the captured depth.
    pub fn total_qty(&self) -> Qty {
        self.total_qty
    }

    /// Captured level count.
    pub fn level_count(&self) -> usize {
        self.levels.len()
    }
}

/// One price level of a snapshot: the L2 aggregate plus the L3 FIFO
/// (REQ-BOOK-001: both views come from the same captured version).
#[derive(Clone, Copy)]
pub struct LevelView<'a, S: Side> {
    level: &'a LevelSnap,
    orders: &'a [OrderSnap],
    _side: PhantomData<S>,
}

impl<'a, S: Side> LevelView<'a, S> {
    /// Price in ticks.
    pub fn price(&self) -> Price {
        self.level.price
    }

    /// Aggregated resting quantity (L2).
    pub fn qty(&self) -> Qty {
        self.level.qty
    }

    /// The L3 FIFO, first-in-first-out (REQ-BOOK-001).
    pub fn orders(&self) -> impl Iterator<Item = OrderRef> + '_ {
        let start = self.level.orders_off as usize;
        let end = start + self.level.orders_len as usize;
        self.orders[start..end].iter().map(|o| OrderRef {
            id: o.id,
            qty: o.qty,
        })
    }

    /// Captured order count for this level.
    pub fn order_count(&self) -> usize {
        self.level.orders_len as usize
    }
}

/// A borrowed L3 order reference.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OrderRef {
    /// Resting order id.
    pub id: OrderId,
    /// Resting quantity.
    pub qty: Qty,
}

/// Best→worst iterator over captured price levels (REQ-BOOK-006).
pub struct DepthIter<'a, S: Side> {
    levels: &'a [LevelSnap],
    orders: &'a [OrderSnap],
    pos: usize,
    _side: PhantomData<S>,
}

impl<'a, S: Side> Iterator for DepthIter<'a, S> {
    type Item = LevelView<'a, S>;

    fn next(&mut self) -> Option<Self::Item> {
        let l = self.levels.get(self.pos)?;
        self.pos += 1;
        Some(LevelView {
            level: l,
            orders: self.orders,
            _side: PhantomData,
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let rem = self.levels.len() - self.pos;
        (rem, Some(rem))
    }
}

impl<'a, S: Side> ExactSizeIterator for DepthIter<'a, S> {}

/// Defense-in-depth behind the writer's sorted-array invariant: captured
/// levels must be ordered best→worst for the side (asserted in debug
/// builds after every successful read).
pub(crate) fn depth_ordered(is_bid: bool, levels: &[LevelSnap]) -> bool {
    levels.windows(2).all(|w| {
        if is_bid {
            w[0].price >= w[1].price
        } else {
            w[0].price <= w[1].price
        }
    })
}
