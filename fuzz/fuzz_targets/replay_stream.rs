//! Fuzz target: replay input streams (spec §Gate plan — feed-facing
//! ingress deserves fuzz discipline; full wire-matrix fuzzing remains
//! wire-kit's burden).
//!
//! The stream encodes (seq, Command) events with adversarial sequence
//! numbers (gaps, duplicates, wraps) and adversarial command fields
//! (zero quantities, non-positive prices, unknown/duplicate ids). The
//! invariants: replay never panics, every failure is a typed
//! `ReplayError`, and — on a clean run — identical input replays to an
//! identical checksum (determinism).

#![no_main]

use book_kit::{Book, Command, FeedEvent, GapPolicy, OrderId, ReplayError};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Decode 9-byte frames: 1 op byte + 8 id/price/qty bytes (LE u64).
    let events: Vec<FeedEvent> = data
        .chunks(9)
        .enumerate()
        .filter(|(_, c)| c.len() == 9)
        .map(|(i, c)| {
            let x = u64::from_le_bytes(c[1..9].try_into().expect("9-byte chunk"));
            let seq = x.wrapping_add(i as u64);
            let cmd = match c[0] % 5 {
                0 => Command::AddBid { id: OrderId::new(x), price: x as i64, qty: x, ts_mono: x },
                1 => Command::AddAsk { id: OrderId::new(x), price: x as i64, qty: x, ts_mono: x },
                2 => Command::Cancel { id: OrderId::new(x) },
                3 => Command::Replace { id: OrderId::new(x), new_price: x as i64, new_qty: x },
                _ => Command::Execute { id: OrderId::new(x), qty: x },
            };
            FeedEvent { seq, command: cmd }
        })
        .collect();

    for policy in [GapPolicy::Fail, GapPolicy::Skip] {
        let mut book = Book::with_capacity(1024, 256);
        match book_kit::replay(&mut book, events.iter().copied(), policy) {
            Ok(report) => {
                // Determinism: a second replay must land on the same state.
                let mut again = Book::with_capacity(1024, 256);
                let report2 = book_kit::replay(&mut again, events.iter().copied(), policy)
                    .expect("first replay succeeded");
                assert_eq!(report.checksum, report2.checksum, "replay is deterministic");
            }
            Err(ReplayError::Gap { .. } | ReplayError::Rejected { .. }) => {
                // Typed failures are the contract.
            }
        }
    }
});
