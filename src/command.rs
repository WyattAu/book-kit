//! Writer commands and their success report.
//!
//! Commands carry the side in **distinct typed variants** — `AddBid` and
//! `AddAsk`, never a `bool` (REQ-BOOK-003, REQ-BOOK-005). Timestamps are raw
//! `u64` nanos carried opaquely (`ts_mono`); book-kit keeps zero estate
//! deps and assigns timestamp *semantics* to the producer (spec §Cross-
//! references, decision 1).

/// A resting-order identifier. Opaque; `0` is a legal id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct OrderId(u64);

impl OrderId {
    /// Wrap a raw id.
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// The raw id.
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// A lifecycle command for the single writer thread (REQ-BOOK-003).
///
/// `Execute` *applies a fill to resting quantity* — deciding which orders
/// fill is the matching engine's job, explicitly out of scope (spec
/// §Out of scope).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// Add a resting bid.
    AddBid {
        /// Caller-chosen order id.
        id: OrderId,
        /// Price in ticks (`i64`, positive — REQ-BOOK-004).
        price: crate::Price,
        /// Quantity in fixed-point units (`u64`, non-zero).
        qty: crate::Qty,
        /// Raw `u64` nanos (monotonic producer clock); stored, not
        /// interpreted.
        ts_mono: u64,
    },
    /// Add a resting ask.
    AddAsk {
        /// Caller-chosen order id.
        id: OrderId,
        /// Price in ticks (`i64`, positive — REQ-BOOK-004).
        price: crate::Price,
        /// Quantity in fixed-point units (`u64`, non-zero).
        qty: crate::Qty,
        /// Raw `u64` nanos (monotonic producer clock); stored, not
        /// interpreted.
        ts_mono: u64,
    },
    /// Remove a resting order.
    Cancel {
        /// The order to remove.
        id: OrderId,
    },
    /// Amend a resting order's price and/or quantity.
    ///
    /// A price change moves the order to the **tail** of the new price
    /// level's FIFO (it loses time priority, matching common exchange
    /// semantics); a quantity-only change keeps its FIFO position. The
    /// original id and `ts_mono` are kept.
    Replace {
        /// The order to amend.
        id: OrderId,
        /// New price in ticks (positive).
        new_price: crate::Price,
        /// New quantity (non-zero).
        new_qty: crate::Qty,
    },
    /// Apply a fill of `qty` against a resting order. When the fill covers
    /// the resting quantity the order is removed; otherwise its quantity is
    /// reduced in place (FIFO position kept).
    Execute {
        /// The order to fill against.
        id: OrderId,
        /// Fill quantity (non-zero).
        qty: crate::Qty,
    },
}

impl Command {
    /// The command's target order id (all five variants name one).
    pub fn id(&self) -> OrderId {
        match *self {
            Command::AddBid { id, .. }
            | Command::AddAsk { id, .. }
            | Command::Cancel { id }
            | Command::Replace { id, .. }
            | Command::Execute { id, .. } => id,
        }
    }
}

/// Success report for [`Book::apply`](crate::Book::apply)
/// (REQ-BOOK-003: success is as typed as failure).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Applied {
    /// The command's target order id.
    pub id: OrderId,
    /// Quantity actually filled (`Execute` only; `0` otherwise).
    pub executed: crate::Qty,
    /// Resting quantity remaining after the command (`0` after `Cancel`
    /// or a fully-consuming `Execute`).
    pub remaining: crate::Qty,
    /// The book version published by this apply (even/odd protocol value;
    /// readers that complete a snapshot see exactly this number).
    pub version: u64,
}
