//! Error types — every failure mode is a typed value (REQ-BOOK-003):
//! no panics, no silent drops on the writer path; torn reads are reported,
//! never observed (REQ-BOOK-002).

/// Why a [`Command`](crate::Command) was rejected.
///
/// Typed and exhaustive: the writer returns `Err(Reject)` instead of
/// panicking or dropping (REQ-BOOK-003).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Reject {
    /// `Cancel`/`Replace`/`Execute` named an order id that is not resting.
    UnknownOrder,
    /// `AddBid`/`AddAsk` reused an id that is currently resting.
    DuplicateId,
    /// A quantity argument was zero (`Add*`, `Replace`, `Execute`).
    ZeroQty,
    /// A price argument was not a positive tick count (`Add*`, `Replace`).
    NonPositivePrice,
    /// The order arena or the price-level table is at capacity.
    ArenaFull,
    /// A generational index referred to a slot whose generation has moved
    /// on — the classic ABA reuse case, rejected instead of resurrected
    /// (REQ-BOOK-008).
    StaleGeneration,
}

impl core::fmt::Display for Reject {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let s = match self {
            Reject::UnknownOrder => "unknown order id",
            Reject::DuplicateId => "duplicate order id",
            Reject::ZeroQty => "zero quantity",
            Reject::NonPositivePrice => "non-positive price",
            Reject::ArenaFull => "arena/level capacity exhausted",
            Reject::StaleGeneration => "stale generational index (ABA)",
        };
        f.write_str(s)
    }
}

/// Why a snapshot read did not produce a usable snapshot.
///
/// Every variant means "you do not have a snapshot" — the [`BookBuf`]
/// contents after an `Err` are unspecified and must not be viewed. All three
/// are retryable (possibly after resizing the buffer).
///
/// [`BookBuf`]: crate::BookBuf
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Torn {
    /// The seqlock counter was odd: the writer was mid-apply. Single
    /// `try_read` attempts hit this under a hot writer; `read` retries.
    WriteInProgress,
    /// The counter changed between the reader's two checks: the writer
    /// lapped the copy. The partially copied data was discarded.
    VersionChanged,
    /// The live book is deeper than the buffer's explicit capacity.
    /// Nothing was copied — truncation is a typed error, never silent
    /// partial state (spec §Risk register).
    BufferTooSmall,
}

impl Torn {
    /// `true` when retrying the same read (same buffer) can succeed.
    pub fn is_retryable(self) -> bool {
        !matches!(self, Torn::BufferTooSmall)
    }
}

impl core::fmt::Display for Torn {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let s = match self {
            Torn::WriteInProgress => "writer mid-apply (seqlock odd)",
            Torn::VersionChanged => "writer lapped the read (version changed)",
            Torn::BufferTooSmall => "snapshot buffer too small for live depth",
        };
        f.write_str(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// REQ-BOOK-002 — `BufferTooSmall` is the one non-retryable torn reason.
    #[test]
    fn torn_retryability_matrix() {
        assert!(Torn::WriteInProgress.is_retryable());
        assert!(Torn::VersionChanged.is_retryable());
        assert!(!Torn::BufferTooSmall.is_retryable());
    }

    /// Display impls render the typed reason (human-facing consumers).
    #[test]
    fn display_impls_render_reasons() {
        assert_eq!(Reject::UnknownOrder.to_string(), "unknown order id");
        assert_eq!(Reject::DuplicateId.to_string(), "duplicate order id");
        assert_eq!(Reject::ZeroQty.to_string(), "zero quantity");
        assert_eq!(Reject::NonPositivePrice.to_string(), "non-positive price");
        assert_eq!(
            Reject::ArenaFull.to_string(),
            "arena/level capacity exhausted"
        );
        assert_eq!(
            Reject::StaleGeneration.to_string(),
            "stale generational index (ABA)"
        );
        assert_eq!(
            Torn::WriteInProgress.to_string(),
            "writer mid-apply (seqlock odd)"
        );
        assert_eq!(
            Torn::VersionChanged.to_string(),
            "writer lapped the read (version changed)"
        );
        assert_eq!(
            Torn::BufferTooSmall.to_string(),
            "snapshot buffer too small for live depth"
        );
        assert_eq!(
            crate::ReplayError::Gap {
                expected: 7,
                found: 9
            }
            .to_string(),
            "sequence gap: expected 7, found 9"
        );
        assert_eq!(
            crate::ReplayError::Rejected {
                seq: 3,
                reject: Reject::UnknownOrder
            }
            .to_string(),
            "command at seq 3 rejected: unknown order id"
        );
    }
}
