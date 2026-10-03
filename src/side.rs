//! Side typing — `Bid`/`Ask` are distinct types behind a sealed trait
//! (REQ-BOOK-005). No public API accepts `bool` for side.

/// Sealed side trait: implemented only by [`Bid`] and [`Ask`] in this crate.
///
/// Downstream crates cannot add sides (the [`sealed::Sealed`] supertrait is
/// unnameable), so `SideView<Bid>` and `SideView<Ask>` are the only
/// instantiations — the compiler, not convention, keeps the two views
/// unswappable.
pub trait Side: sealed::Sealed + Copy + 'static {
    /// The other side of the book.
    type Opposite: Side;

    /// The other side of the book.
    fn opposite() -> Self::Opposite;

    /// `true` for [`Bid`] (buy side), `false` for [`Ask`] (sell side).
    fn is_bid() -> bool;
}

/// The buy side: price-time priority favors the **highest** price.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Bid;

/// The sell side: price-time priority favors the **lowest** price.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Ask;

impl sealed::Sealed for Bid {}
impl sealed::Sealed for Ask {}

impl Side for Bid {
    type Opposite = Ask;

    fn opposite() -> Self::Opposite {
        Ask
    }
    fn is_bid() -> bool {
        true
    }
}

impl Side for Ask {
    type Opposite = Bid;

    fn opposite() -> Self::Opposite {
        Bid
    }
    fn is_bid() -> bool {
        false
    }
}

mod sealed {
    /// Unnameable outside the crate: makes [`super::Side`] sealed.
    pub trait Sealed {}
}

#[cfg(test)]
mod tests {
    use super::*;

    /// REQ-BOOK-005 — sides are distinct types, never `bool`: the trait
    /// dispatch differs per type and `opposite()` is an involution.
    #[test]
    fn sides_are_distinct_types_not_bool() {
        assert!(<Bid as Side>::is_bid());
        assert!(!<Ask as Side>::is_bid());
        assert_eq!(<Bid as Side>::opposite(), Ask);
        assert_eq!(<Ask as Side>::opposite(), Bid);
        // Type-level distinctness: SideView<Bid> and SideView<Ask> are
        // distinct instantiations — enforced structurally by the phantom
        // type in `buf.rs` (compile-fail is covered by the doc statement
        // here; the public API has no bool-taking constructor to abuse).
    }
}
