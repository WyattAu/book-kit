//! Replay / journal rebuild — deterministic state reconstruction from an
//! ordered command feed (REQ-BOOK-009).
//!
//! The feed is a total-ordered `(seq, Command)` stream. Determinism
//! contract: identical feed (same seqs, same commands, same order) ⇒
//! checksum-identical book state — the property test
//! `replay_is_deterministic_and_gap_policy_honored` is the contract
//! (spec §Risk register: no undocumented ordering assumptions).

use crate::command::{Command, OrderId};
use crate::{Book, Reject};

/// One journal/feed event: a sequence number and the command it carried.
///
/// `replay` accepts anything that converts into this — `(u64, Command)`
/// tuples (the natural journal shape) or explicit `FeedEvent`s.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FeedEvent {
    /// Producer sequence number.
    pub seq: u64,
    /// The lifecycle command.
    pub command: Command,
}

impl From<(u64, Command)> for FeedEvent {
    fn from((seq, command): (u64, Command)) -> Self {
        Self { seq, command }
    }
}

/// Sequence-gap handling — explicit, never guessed (REQ-BOOK-009).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GapPolicy {
    /// A sequence gap aborts the replay with [`ReplayError::Gap`].
    Fail,
    /// A sequence gap skips forward: the first observed event re-anchors
    /// the expected sequence, and the skip is counted in the report.
    Skip,
}

/// Replay success report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplayReport {
    /// Commands applied successfully.
    pub applied: u64,
    /// Sequence gaps skipped (`GapPolicy::Skip` only).
    pub gaps_skipped: u64,
    /// First sequence number seen (`None` for an empty feed).
    pub first_seq: Option<u64>,
    /// Last sequence number seen (`None` for an empty feed).
    pub last_seq: Option<u64>,
    /// The book's checksum after the replay (REQ-BOOK-010).
    pub checksum: u64,
}

/// Replay failure (REQ-BOOK-009).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReplayError {
    /// `GapPolicy::Fail` observed a gap.
    Gap {
        /// Sequence the feed was expected to continue at.
        expected: u64,
        /// Sequence the feed actually showed.
        found: u64,
    },
    /// The book rejected a command mid-replay. Replay is strict: a
    /// journal that contains unplayable commands is surfaced, not
    /// swallowed (callers that want skip-semantics filter beforehand).
    Rejected {
        /// Sequence of the rejected event.
        seq: u64,
        /// Why the book rejected it.
        reject: Reject,
    },
}

impl core::fmt::Display for ReplayError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self {
            ReplayError::Gap { expected, found } => {
                write!(f, "sequence gap: expected {expected}, found {found}")
            }
            ReplayError::Rejected { seq, reject } => {
                write!(f, "command at seq {seq} rejected: {reject}")
            }
        }
    }
}

/// Rebuild a book from an ordered `(seq, Command)` feed (REQ-BOOK-009).
///
/// The expected sequence **re-anchors at the first event**, so a journal
/// segment may start at any seq. After that, every event must be exactly
/// `expected + 1`:
///
/// * gap + [`GapPolicy::Fail`] → `Err(ReplayError::Gap)` (the book is
///   left in its partially rebuilt state — do not use it for
///   verification).
/// * gap + [`GapPolicy::Skip`] → the event stream re-anchors at the
///   observed seq and the skip is counted.
///
/// Deterministic: identical feed → checksum-identical state, because the
/// same applies happen in the same order on the same starting book.
pub fn replay<I, E>(
    book: &mut Book,
    feed: I,
    policy: GapPolicy,
) -> Result<ReplayReport, ReplayError>
where
    I: IntoIterator<Item = E>,
    E: Into<FeedEvent>,
{
    let mut expected: Option<u64> = None;
    let mut applied = 0u64;
    let mut gaps_skipped = 0u64;
    let mut first_seq = None;
    let mut last_seq = None;

    for event in feed {
        let FeedEvent { seq, command } = event.into();
        if first_seq.is_none() {
            first_seq = Some(seq);
            expected = Some(seq);
        }
        if let Some(exp) = expected {
            if seq != exp {
                match policy {
                    GapPolicy::Fail => {
                        return Err(ReplayError::Gap {
                            expected: exp,
                            found: seq,
                        });
                    }
                    GapPolicy::Skip => gaps_skipped += 1,
                }
            }
        }
        expected = Some(seq.wrapping_add(1));
        match book.apply(command) {
            Ok(_) => applied += 1,
            Err(reject) => return Err(ReplayError::Rejected { seq, reject }),
        }
        last_seq = Some(seq);
    }

    Ok(ReplayReport {
        applied,
        gaps_skipped,
        first_seq,
        last_seq,
        checksum: book.checksum(),
    })
}

/// Convenience: the id a replayed add ended up resting under is the
/// command's own id (book-kit keeps caller-chosen ids stable); journals
/// that need generational indices re-derive them with
/// [`Book::gen_index_of`].
#[allow(dead_code)]
fn _id_of(cmd: &Command) -> OrderId {
    cmd.id()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::Command;
    use crate::OrderId;

    /// Empty feed: report with no sequences, checksum of the empty book.
    #[test]
    fn replay_empty_feed_reports_checksum() {
        let mut book = Book::with_capacity(8, 8);
        let report = replay(&mut book, std::iter::empty::<FeedEvent>(), GapPolicy::Fail)
            .expect("empty feed replays");
        assert_eq!(report.applied, 0);
        assert_eq!(report.first_seq, None);
        assert_eq!(report.last_seq, None);
        assert_eq!(report.checksum, book.checksum());
    }

    /// `From<(u64, Command)>` covers the natural journal tuple shape.
    #[test]
    fn feed_event_from_tuple() {
        let cmd = Command::Cancel {
            id: OrderId::new(1),
        };
        let ev: FeedEvent = (9u64, cmd).into();
        assert_eq!(ev.seq, 9);
        assert_eq!(ev.command, cmd);
    }

    /// Rejected-command replay surfaces the typed error with the seq.
    #[test]
    fn replay_surfaces_typed_reject() {
        let mut book = Book::with_capacity(8, 8);
        let feed = [
            (
                10u64,
                Command::AddBid {
                    id: OrderId::new(1),
                    price: 5,
                    qty: 1,
                    ts_mono: 0,
                },
            ),
            (
                11u64,
                Command::Cancel {
                    id: OrderId::new(99),
                },
            ),
        ];
        match replay(&mut book, feed, GapPolicy::Fail) {
            Err(ReplayError::Rejected { seq, reject }) => {
                assert_eq!(seq, 11);
                assert_eq!(reject, crate::Reject::UnknownOrder);
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
    }
}
