//! Property tests — the spec's mandated invariants over random command
//! sequences (1000 cases):
//!
//! * price-time priority: a reference model (BTreeMap of per-price FIFOs)
//!   and the book agree on L2 aggregation and L3 ordering after every
//!   step (REQ-BOOK-001, REQ-BOOK-006);
//! * add/cancel/replace/execute never panic, whatever the ids/qty/prices
//!   (REQ-BOOK-003);
//! * depth monotonicity: bids strictly descending, asks strictly
//!   ascending;
//! * replay determinism + gap policy (REQ-BOOK-009) and checksum
//!   sensitivity (REQ-BOOK-010).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
#![allow(clippy::if_same_then_else)]

use book_kit::{Book, BookBuf, Command, GapPolicy, OrderId, ReplayError, Side};
use proptest::prelude::*;

const MAX_CASES: u32 = 1000;

/// Random command sequence over a small id/price/qty universe (so adds
/// collide, cancels miss, and the book stays small).
fn command_strategy() -> impl Strategy<Value = Command> {
    prop_oneof![
        4 => (0u64..8, -4i64..8, 1u64..4).prop_map(|(id, price, qty)| Command::AddBid {
            id: OrderId::new(id),
            price,
            qty,
            ts_mono: id,
        }),
        4 => (0u64..8, -4i64..8, 1u64..4).prop_map(|(id, price, qty)| Command::AddAsk {
            id: OrderId::new(id),
            price,
            qty,
            ts_mono: id,
        }),
        2 => (0u64..10).prop_map(|id| Command::Cancel { id: OrderId::new(id) }),
        1 => (0u64..10, -4i64..8, 1u64..4).prop_map(|(id, price, qty)| Command::Replace {
            id: OrderId::new(id),
            new_price: price,
            new_qty: qty,
        }),
        1 => (0u64..10, 1u64..5).prop_map(|(id, qty)| Command::Execute {
            id: OrderId::new(id),
            qty,
        }),
    ]
}

fn commands_strategy() -> impl Strategy<Value = Vec<Command>> {
    prop::collection::vec(command_strategy(), 0..48)
}

/// The reference model: price → FIFO of (id, qty), best-side-aware.
#[derive(Default)]
struct Model {
    bids: std::collections::BTreeMap<i64, Vec<(u64, u64)>>,
    asks: std::collections::BTreeMap<i64, Vec<(u64, u64)>>,
}

impl Model {
    fn side(&mut self, is_bid: bool) -> &mut std::collections::BTreeMap<i64, Vec<(u64, u64)>> {
        if is_bid {
            &mut self.bids
        } else {
            &mut self.asks
        }
    }

    fn find(&self, is_bid: bool, id: u64) -> Option<i64> {
        let map = if is_bid { &self.bids } else { &self.asks };
        map.iter()
            .find_map(|(price, fifo)| fifo.iter().any(|(i, _)| *i == id).then_some(*price))
    }
}

/// Apply one command to both the model and the book; assert agreement of
/// every reject/accept decision, then the full L2/L3 view equality.
fn step(model: &mut Model, book: &mut Book, cmd: &Command) {
    let id = cmd.id().raw();
    // Model-side decision: does the reference model accept this command?
    let model_result: Result<(), ()> = match *cmd {
        Command::AddBid { price, qty, .. } | Command::AddAsk { price, qty, .. } => {
            if qty == 0 || price <= 0 {
                Err(())
            } else if model.find(true, id).is_some() || model.find(false, id).is_some() {
                Err(())
            } else {
                Ok(())
            }
        }
        Command::Cancel { .. } => {
            if model.find(true, id).is_none() && model.find(false, id).is_none() {
                Err(())
            } else {
                Ok(())
            }
        }
        Command::Replace {
            new_price, new_qty, ..
        } => {
            if new_qty == 0 || new_price <= 0 {
                Err(())
            } else if model.find(true, id).is_none() && model.find(false, id).is_none() {
                Err(())
            } else {
                Ok(())
            }
        }
        Command::Execute { qty, .. } => {
            if qty == 0 {
                Err(())
            } else if model.find(true, id).is_none() && model.find(false, id).is_none() {
                Err(())
            } else {
                Ok(())
            }
        }
    };

    let book_result = book.apply(*cmd);
    assert_eq!(
        book_result.is_ok(),
        model_result.is_ok(),
        "decision divergence on {cmd:?}"
    );

    // Mirror into the model on success.
    if model_result.is_ok() {
        match *cmd {
            Command::AddBid { price, qty, .. } => {
                model.bids.entry(price).or_default().push((id, qty));
            }
            Command::AddAsk { price, qty, .. } => {
                model.asks.entry(price).or_default().push((id, qty));
            }
            Command::Cancel { .. } => {
                for is_bid in [true, false] {
                    if let Some(price) = model.find(is_bid, id) {
                        let fifo = model.side(is_bid).get_mut(&price).expect("level");
                        fifo.retain(|(i, _)| *i != id);
                        if fifo.is_empty() {
                            model.side(is_bid).remove(&price);
                        }
                    }
                }
            }
            Command::Replace {
                new_price, new_qty, ..
            } => {
                for is_bid in [true, false] {
                    if let Some(old) = model.find(is_bid, id) {
                        let fifo = model.side(is_bid).get_mut(&old).expect("level");
                        let pos = fifo.iter().position(|(i, _)| *i == id).expect("in fifo");
                        let (_, old_qty) = fifo.remove(pos);
                        if fifo.is_empty() {
                            model.side(is_bid).remove(&old);
                        }
                        if new_price == old {
                            // Qty-only: keeps FIFO position.
                            let fifo = model.side(is_bid).entry(old).or_default();
                            fifo.insert(pos, (id, new_qty));
                        } else {
                            // Price change: tail of the new level's FIFO.
                            let new = if is_bid == (new_price > old) {
                                new_price
                            } else {
                                new_price
                            };
                            let _ = new;
                            model
                                .side(is_bid)
                                .entry(new_price)
                                .or_default()
                                .push((id, new_qty));
                        }
                        let _ = old_qty;
                        return;
                    }
                }
            }
            Command::Execute { qty, .. } => {
                for is_bid in [true, false] {
                    if let Some(price) = model.find(is_bid, id) {
                        let fifo = model.side(is_bid).get_mut(&price).expect("level");
                        let pos = fifo.iter().position(|(i, _)| *i == id).expect("in fifo");
                        let resting = fifo[pos].1;
                        if qty >= resting {
                            fifo.remove(pos);
                            if fifo.is_empty() {
                                model.side(is_bid).remove(&price);
                            }
                        } else {
                            fifo[pos].1 = resting - qty;
                        }
                        return;
                    }
                }
            }
        }
    }

    // View equality: every level's price/qty/FIFO must match the model.
    let mut buf = BookBuf::new(64, 64);
    let v = book.try_read(&mut buf).expect("read after apply");
    let _ = v;

    // Side-generic view-vs-model equality + depth monotonicity.
    fn check_side<S: Side>(
        view: book_kit::SideView<'_, S>,
        map: &std::collections::BTreeMap<i64, Vec<(u64, u64)>>,
        label: &str,
        bid: bool,
    ) {
        let mut depth: Vec<(i64, u64)> = Vec::new();
        for level in view.depth() {
            let fifo: Vec<(u64, u64)> = level.orders().map(|o| (o.id.raw(), o.qty)).collect();
            let expected: Vec<(u64, u64)> = map.get(&level.price()).cloned().unwrap_or_default();
            assert_eq!(&fifo, &expected, "L3 divergence at price {}", level.price());
            assert_eq!(level.qty(), expected.iter().map(|(_, q)| q).sum::<u64>());
            depth.push((level.price(), level.qty()));
        }
        assert_eq!(
            depth.len(),
            map.len(),
            "L2 level-count divergence ({label})"
        );
        // Depth monotonicity (strict).
        if bid {
            assert!(depth.windows(2).all(|w| w[0] > w[1]), "bids {depth:?}");
        } else {
            assert!(depth.windows(2).all(|w| w[0] < w[1]), "asks {depth:?}");
        }
    }
    check_side(book.bids(&buf), &model.bids, "bids", true);
    check_side(book.asks(&buf), &model.asks, "asks", false);
}

/// REQ-BOOK-001 + REQ-BOOK-003 + REQ-BOOK-006 — price-time priority over
/// random command sequences: the book equals the reference model at every
/// step, decisions agree, depth is monotone, nothing panics.
#[test]
fn price_time_priority_matches_reference_model() {
    let cfg = proptest::test_runner::Config {
        cases: MAX_CASES,
        ..proptest::test_runner::Config::default()
    };
    proptest::test_runner::TestRunner::new(cfg)
        .run(&commands_strategy(), |cmds| {
            let mut book = Book::with_capacity(64, 64);
            let mut model = Model::default();
            for cmd in &cmds {
                step(&mut model, &mut book, cmd);
            }
            Ok(())
        })
        .unwrap_or_else(|e| panic!("{e}"));
}

/// REQ-BOOK-010 — the checksum changes whenever observable state changes,
/// and is stable when nothing changes (mutation-style probes).
#[test]
fn checksum_detects_any_state_delta() {
    let cfg = proptest::test_runner::Config {
        cases: 300,
        ..proptest::test_runner::Config::default()
    };
    proptest::test_runner::TestRunner::new(cfg)
        .run(&commands_strategy(), |cmds| {
            let mut book = Book::with_capacity(64, 64);
            for cmd in &cmds {
                let before = book.checksum();
                let res = book.apply(*cmd);
                let after = book.checksum();
                if res.is_ok() {
                    // A successful apply always moves the version word —
                    // and the checksum hashes the version — so it always
                    // changes. (An add→empty→add cycle that restores
                    // *content* still differs via the version+gen words.)
                    assert_ne!(before, after, "checksum blind to apply: {cmd:?}");
                } else {
                    assert_eq!(before, after, "checksum moved on a rejected apply: {cmd:?}");
                }
            }
            // Stability: repeated checksumming is pure.
            let c = book.checksum();
            assert_eq!(book.checksum(), c);
            Ok(())
        })
        .unwrap_or_else(|e| panic!("{e}"));
}

/// REQ-BOOK-009 — replay is deterministic (identical feed →
/// checksum-identical state) and the gap policy is honored exactly.
#[test]
fn replay_is_deterministic_and_gap_policy_honored() {
    let cfg = proptest::test_runner::Config {
        cases: 300,
        ..proptest::test_runner::Config::default()
    };
    let seed_strategy = prop::collection::vec(((0u64..12, 1i64..6, 1u64..3), 0u8..4), 1..32);
    proptest::test_runner::TestRunner::new(cfg)
        .run(&seed_strategy, |raw| {
            // Build a gapless, *playable* feed: track a reference resting
            // map (id → resting qty) so cancels/executes always land on
            // live orders and full consumes retire the id.
            let mut feed: Vec<(u64, Command)> = Vec::new();
            let mut resting: std::collections::HashMap<u64, u64> = Default::default();
            let mut seq = 100u64;
            for ((id, price, qty), kind) in raw {
                let cmd = match kind {
                    0 | 1 if !resting.contains_key(&id) => {
                        let cmd = if kind == 0 {
                            Command::AddBid {
                                id: OrderId::new(id),
                                price,
                                qty,
                                ts_mono: seq,
                            }
                        } else {
                            Command::AddAsk {
                                id: OrderId::new(id),
                                price,
                                qty,
                                ts_mono: seq,
                            }
                        };
                        resting.insert(id, qty);
                        cmd
                    }
                    2 if resting.contains_key(&id) => {
                        resting.remove(&id);
                        Command::Cancel {
                            id: OrderId::new(id),
                        }
                    }
                    3 if resting.contains_key(&id) => {
                        let held = resting[&id];
                        if qty >= held {
                            resting.remove(&id);
                        } else {
                            resting.insert(id, held - qty);
                        }
                        Command::Execute {
                            id: OrderId::new(id),
                            qty,
                        }
                    }
                    _ => continue,
                };
                feed.push((seq, cmd));
                seq += 1;
            }
            prop_assume!(feed.len() >= 2);

            // Determinism: two fresh books + same feed → same report.
            let mut a = Book::with_capacity(64, 64);
            let mut b = Book::with_capacity(64, 64);
            let ra = book_kit::replay(&mut a, feed.iter().copied(), GapPolicy::Fail);
            let rb = book_kit::replay(&mut b, feed.iter().copied(), GapPolicy::Fail);
            assert_eq!(ra, rb, "replay is deterministic");
            let report = ra.expect("gapless feed replays");
            assert_eq!(report.applied as usize, feed.len());
            assert_eq!(report.gaps_skipped, 0);
            assert_eq!(report.first_seq, Some(100));
            assert_eq!(a.checksum(), b.checksum());
            assert_eq!(a.checksum(), report.checksum);

            // GapPolicy::Fail: punch a hole into a compacted pure-add
            // feed (drop one event, keep seq continuity elsewhere) —
            // playability preserved, exactly one gap guaranteed. Only
            // ids that appear exactly once qualify for removal: removing
            // a first add whose id is re-added later would create a
            // DuplicateId, not a clean gap.
            use std::collections::HashSet;
            let mut counts: HashSet<u64> = HashSet::new();
            let mut dup: HashSet<u64> = HashSet::new();
            let mut adds: Vec<(u64, Command)> = Vec::new();
            for (s, c) in feed.iter() {
                if matches!(c, Command::AddBid { .. } | Command::AddAsk { .. }) {
                    let id = c.id().raw();
                    if !counts.insert(id) {
                        dup.insert(id);
                    }
                    adds.push((*s, *c));
                }
            }
            adds.retain(|(_, c)| !dup.contains(&c.id().raw()));
            prop_assume!(adds.len() >= 3);
            let hole = adds.len() / 2;
            let expected_gap = 100u64 + hole as u64;
            let mut holed: Vec<(u64, Command)> = Vec::new();
            let mut s = 100u64;
            for (i, (_, c)) in adds.iter().enumerate() {
                if i == hole {
                    s += 1; // the skipped sequence number — the gap
                    continue;
                }
                holed.push((s, *c));
                s += 1;
            }
            let mut c = Book::with_capacity(64, 64);
            match book_kit::replay(&mut c, holed.iter().copied(), GapPolicy::Fail) {
                Err(ReplayError::Gap { expected, found }) => {
                    assert_eq!(expected, expected_gap);
                    assert_eq!(found, expected_gap + 1);
                }
                other => panic!("expected Gap, got {other:?}"),
            }

            // GapPolicy::Skip: the same holed feed replays, counting one
            // skip, deterministically.
            let mut d = Book::with_capacity(64, 64);
            let mut e = Book::with_capacity(64, 64);
            let rd = book_kit::replay(&mut d, holed.iter().copied(), GapPolicy::Skip);
            let re = book_kit::replay(&mut e, holed.iter().copied(), GapPolicy::Skip);
            assert_eq!(rd, re, "skip-replay is deterministic too");
            assert_eq!(rd.expect("skip replays").gaps_skipped, 1);
            assert_eq!(d.checksum(), e.checksum());
            Ok(())
        })
        .unwrap_or_else(|e| panic!("{e}"));
}

/// REQ-BOOK-003 — garbage in, typed rejects out: random ids/qty/prices
/// never panic the writer.
#[test]
fn add_cancel_execute_never_panics() {
    let cfg = proptest::test_runner::Config {
        cases: MAX_CASES,
        ..proptest::test_runner::Config::default()
    };
    proptest::test_runner::TestRunner::new(cfg)
        .run(&commands_strategy(), |cmds| {
            let mut book = Book::with_capacity(32, 32);
            for cmd in &cmds {
                let _ = book.apply(*cmd); // typed result; must not panic
            }
            // Whatever happened, the book is still readable and
            // self-consistent.
            let mut buf = BookBuf::new(64, 64);
            book.try_read(&mut buf).expect("readable after chaos");
            Ok(())
        })
        .unwrap_or_else(|e| panic!("{e}"));
}
