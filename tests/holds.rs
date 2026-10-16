//! Holds that resolve once, on `Journal`: post and void once with derived
//! legs, expiry by caller dates, limits, and holds as evidence (no reversal,
//! seals, year-end closing and audit). Concurrent resolution is covered by the
//! conformance suite (`just conformance`) and `tests/properties.rs`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

mod common;

use doubleentry::store::{Cell, ProjectionPart, StoreProjection};
use doubleentry::{
    AccountKind, AccountRef, AccountSpec, Amount, BalanceLimit, BalanceQuery, Entry, EntryId,
    Error, HoldState, HoldStatus, Journal, Layer, LedgerId, MemoSalt, PageRead, PostAmount,
    Recorded, Rejection, ResolutionKind, Side, amount,
    currency::EUR,
    date,
    error::{AuditFailureKind, ScaledAmount},
    record::{HoldAction, Record},
};

const WALLET: &str = "Liabilities:Wallets:alice";
const CLEARING: &str = "Liabilities:Card:clearing";

fn ledger() -> LedgerId {
    LedgerId::new("example.com/payments").unwrap()
}

/// EUR (exponent 2), the wallet, the card clearing account and a bank
/// account, all open from 2026-01-01, and the periods 2026-11 and 2026-12.
fn wallet_books() -> Journal {
    let mut books = Journal::new(ledger())
        .with_salt_source(|| MemoSalt([7; 16]))
        .as_actor("setup");
    books.declare_currency(EUR, 2, date!("2026-01-01")).unwrap();
    for path in [WALLET, CLEARING] {
        books
            .open(path, AccountKind::Liability, date!("2026-01-01"))
            .unwrap();
    }
    books
        .open("Assets:Bank", AccountKind::Asset, date!("2026-01-01"))
        .unwrap();
    books.define_months(2026, 11..=12).unwrap();
    books
}

/// A hold of `amount` from the wallet to the card clearing account.
fn hold(key: &str, on: doubleentry::Date, amount: Amount) -> Entry {
    Entry::open_hold(key, on)
        .debit(WALLET, amount, EUR)
        .credit(CLEARING, amount, EUR)
}

fn eur(text: &str) -> Amount {
    text.parse().unwrap()
}

fn rejected<T: core::fmt::Debug>(result: Result<T, Error>) -> Rejection {
    match result {
        Err(Error::Rejected(rejection)) => rejection,
        other => panic!("expected a rejection, got {other:?}"),
    }
}

/// The postings of a recorded entry as (layer, side, account, amount).
fn legs(books: &Journal, id: EntryId) -> Vec<(Layer, Side, String, Amount)> {
    books
        .entry(id)
        .unwrap()
        .parts()
        .postings
        .iter()
        .map(|p| {
            let path = books.account(p.handle).unwrap().path.to_string();
            (p.layer, p.side, path, p.amount)
        })
        .collect()
}

/// The net (debit − credit) of an account on a layer.
fn net(books: &Journal, account: &str, layer: Layer) -> Amount {
    let query = match layer {
        Layer::Settled => BalanceQuery::all(),
        Layer::Pending => BalanceQuery::all().pending(),
    };
    books.balance(account, EUR, &query).unwrap().net()
}

/// The wallet's headroom under `NoDebitBalance`: −(settled net + what open
/// holds reserve), the Pending net being exactly the reserved debits.
fn headroom(books: &Journal) -> Amount {
    let exposure = net(books, WALLET, Layer::Settled)
        .checked_add(net(books, WALLET, Layer::Pending))
        .unwrap();
    exposure.negate()
}

// ── T004: the drafts ────────────────────────────────────────────────────────

#[test]
fn drafts_carry_their_hold_action() {
    let target = EntryId::from_bytes([1; 16]);
    let open: Entry =
        hold("h", date!("2026-11-20"), amount!("100.00")).expires(date!("2026-11-30"));
    assert_eq!(
        open.get_hold(),
        Some(&HoldAction::Open {
            expires: Some(date!("2026-11-30"))
        })
    );
    assert!(
        open.postings()
            .iter()
            .all(|p| p.get_layer() == Layer::Pending)
    );
    let post: Entry = Entry::post_hold("p", date!("2026-11-21"), target, PostAmount::Full);
    assert_eq!(
        post.get_hold(),
        Some(&HoldAction::Post {
            hold: target,
            amount: PostAmount::Full
        })
    );
    let void: Entry = Entry::void_hold("v", date!("2026-11-21"), target);
    assert_eq!(void.get_hold(), Some(&HoldAction::Void { hold: target }));
    let expire: Entry = Entry::expire_hold("x", date!("2026-11-30"), target);
    assert_eq!(
        expire.get_hold(),
        Some(&HoldAction::Expire { hold: target })
    );
    // Every other draft writes the Settled layer and no hold action, and an
    // expiry date on it changes nothing.
    let plain: Entry = Entry::new("e", date!("2026-11-20"))
        .debit(WALLET, amount!("1.00"), EUR)
        .credit(CLEARING, amount!("1.00"), EUR)
        .expires(date!("2026-11-30"));
    assert_eq!(plain.get_hold(), None);
    assert!(
        plain
            .postings()
            .iter()
            .all(|p| p.get_layer() == Layer::Settled)
    );
}

// ── US1: authorise, then capture part ───────────────────────────────────────

#[test]
fn us1_partial_post_derives_four_postings() {
    let mut books = wallet_books();
    let held = books
        .record(hold("auth", date!("2026-11-20"), amount!("100.00")))
        .unwrap();
    let capture = books
        .record(Entry::post_hold(
            "capture",
            date!("2026-11-22"),
            held.id,
            PostAmount::Partial(amount!("60.00")),
        ))
        .unwrap();
    assert_eq!(
        legs(&books, capture.id),
        [
            (
                Layer::Pending,
                Side::Credit,
                WALLET.into(),
                amount!("100.00")
            ),
            (
                Layer::Pending,
                Side::Debit,
                CLEARING.into(),
                amount!("100.00")
            ),
            (Layer::Settled, Side::Debit, WALLET.into(), amount!("60.00")),
            (
                Layer::Settled,
                Side::Credit,
                CLEARING.into(),
                amount!("60.00")
            ),
        ]
    );
    let state = books.hold(held.id).unwrap();
    assert_eq!(
        state.status,
        HoldStatus::Posted {
            by: capture.id,
            amount: amount!("60.00")
        }
    );
    assert_eq!(state.remaining(), Amount::ZERO);
    assert!(!state.is_open());
    // The Pending layer nets to zero; the Settled one shows the capture.
    assert_eq!(net(&books, WALLET, Layer::Pending), Amount::ZERO);
    assert_eq!(net(&books, WALLET, Layer::Settled), amount!("60.00"));
    books.audit().unwrap();
}

#[test]
fn us1_a_resolved_hold_refuses_every_further_resolution() {
    let mut books = wallet_books();
    let held = books
        .record(hold("auth", date!("2026-11-20"), amount!("100.00")).expires(date!("2026-11-25")))
        .unwrap();
    let capture = books
        .record(Entry::post_hold(
            "capture",
            date!("2026-11-22"),
            held.id,
            PostAmount::Partial(amount!("60.00")),
        ))
        .unwrap();
    let head = books.head();
    let resolved = Rejection::HoldResolved {
        hold: held.id,
        by: capture.id,
        how: ResolutionKind::Posted,
    };
    let again = [
        Entry::post_hold("post-2", date!("2026-11-23"), held.id, PostAmount::Full),
        Entry::void_hold("void", date!("2026-11-23"), held.id),
        Entry::expire_hold("expire", date!("2026-11-26"), held.id),
    ];
    for draft in again {
        assert_eq!(rejected(books.record(draft)), resolved);
    }
    assert_eq!(books.head(), head, "nothing was written");
    assert_eq!(resolved.code(), 442);
    assert_eq!(
        resolved.to_string(),
        format!(
            "[442] the hold {} was already posted by {}",
            held.id, capture.id
        )
    );
}

#[test]
fn us1_full_post_settles_the_whole_amount() {
    let mut books = wallet_books();
    let held = books
        .record(hold("auth", date!("2026-11-20"), amount!("100.00")))
        .unwrap();
    let capture = books
        .record(Entry::post_hold(
            "capture",
            date!("2026-11-22"),
            held.id,
            PostAmount::Full,
        ))
        .unwrap();
    let legs = legs(&books, capture.id);
    assert!(
        legs.iter()
            .all(|(_, _, _, amount)| *amount == amount!("100.00"))
    );
    assert_eq!(
        books.hold(held.id).unwrap().status,
        HoldStatus::Posted {
            by: capture.id,
            amount: amount!("100.00")
        }
    );
}

#[test]
fn us1_a_partial_post_equal_to_the_hold_is_a_full_post_in_effect() {
    let posted = |amount: PostAmount| {
        let mut books = wallet_books();
        let held = books
            .record(hold("auth", date!("2026-11-20"), amount!("100.00")))
            .unwrap();
        let capture = books
            .record(Entry::post_hold(
                "capture",
                date!("2026-11-22"),
                held.id,
                amount,
            ))
            .unwrap();
        (
            legs(&books, capture.id),
            books.hold(held.id).unwrap().status,
        )
    };
    let (full, _) = posted(PostAmount::Full);
    let (partial, status) = posted(PostAmount::Partial(amount!("100.00")));
    assert_eq!(full, partial);
    assert!(matches!(status, HoldStatus::Posted { amount, .. } if amount == amount!("100.00")));
}

#[test]
fn us1_a_post_beyond_the_hold_is_refused() {
    let mut books = wallet_books();
    let held = books
        .record(hold("auth", date!("2026-11-20"), amount!("100.00")))
        .unwrap();
    let refusal = rejected(books.record(Entry::post_hold(
        "capture",
        date!("2026-11-22"),
        held.id,
        PostAmount::Partial(amount!("150.00")),
    )));
    assert_eq!(
        refusal,
        Rejection::PostExceedsHold {
            hold: held.id,
            held: ScaledAmount::from(eur("100.00")),
            requested: ScaledAmount::from(eur("150.00")),
        }
    );
    assert_eq!(refusal.code(), 448);
}

#[test]
fn us1_a_void_releases_the_reservation() {
    let mut books = wallet_books();
    let held = books
        .record(hold("auth", date!("2026-11-20"), amount!("100.00")))
        .unwrap();
    let void = books
        .record(Entry::void_hold("void", date!("2026-11-21"), held.id))
        .unwrap();
    assert_eq!(
        legs(&books, void.id),
        [
            (
                Layer::Pending,
                Side::Credit,
                WALLET.into(),
                amount!("100.00")
            ),
            (
                Layer::Pending,
                Side::Debit,
                CLEARING.into(),
                amount!("100.00")
            ),
        ]
    );
    assert_eq!(
        books.hold(held.id).unwrap().status,
        HoldStatus::Voided { by: void.id }
    );
    assert_eq!(net(&books, WALLET, Layer::Settled), Amount::ZERO);
}

#[test]
fn us1_unknown_entries_and_entries_that_are_no_hold_are_refused() {
    let mut books = wallet_books();
    let ghost = EntryId::from_bytes([9; 16]);
    assert_eq!(
        rejected(books.record(Entry::void_hold("void", date!("2026-11-21"), ghost))),
        Rejection::HoldNotFound { hold: ghost }
    );
    let sale = books
        .record(
            Entry::new("sale", date!("2026-11-20"))
                .debit("Assets:Bank", amount!("5.00"), EUR)
                .credit(WALLET, amount!("5.00"), EUR),
        )
        .unwrap();
    assert_eq!(
        rejected(books.record(Entry::void_hold("void", date!("2026-11-21"), sale.id))),
        Rejection::NotAHold { entry: sale.id }
    );
    // A resolution is no hold either.
    let held = books
        .record(hold("auth", date!("2026-11-20"), amount!("1.00")))
        .unwrap();
    let void = books
        .record(Entry::void_hold("void-auth", date!("2026-11-21"), held.id))
        .unwrap();
    assert_eq!(
        rejected(books.record(Entry::void_hold("void-void", date!("2026-11-21"), void.id))),
        Rejection::NotAHold { entry: void.id }
    );
    assert!(books.hold(void.id).is_none() && books.hold(sale.id).is_none());
}

#[test]
fn us1_partial_amounts_must_be_positive_and_never_rounded() {
    let mut books = wallet_books();
    let held = books
        .record(hold("auth", date!("2026-11-20"), amount!("100.00")))
        .unwrap();
    let zero = Entry::post_hold(
        "zero",
        date!("2026-11-22"),
        held.id,
        PostAmount::Partial(Amount::ZERO),
    );
    assert_eq!(
        rejected(books.record(zero)),
        Rejection::PostAmountNotPositive
    );
    // At scale 4, EUR keeps exponent 2: 0.001 is refused, not rounded.
    let mut fine = Journal::<4>::with_precision(ledger()).as_actor("setup");
    fine.declare_currency(EUR, 2, date!("2026-01-01")).unwrap();
    fine.open(WALLET, AccountKind::Liability, date!("2026-01-01"))
        .unwrap();
    fine.open(CLEARING, AccountKind::Liability, date!("2026-01-01"))
        .unwrap();
    fine.define_months(2026, 11..=11).unwrap();
    let held = fine
        .record(
            Entry::open_hold("auth", date!("2026-11-20"))
                .debit(WALLET, amount!("100.00"), EUR)
                .credit(CLEARING, amount!("100.00"), EUR),
        )
        .unwrap();
    let refusal = rejected(fine.record(Entry::post_hold(
        "capture",
        date!("2026-11-22"),
        held.id,
        PostAmount::Partial(amount!("0.001")),
    )));
    assert_eq!(
        refusal,
        Rejection::PostAmountPrecision {
            hold: held.id,
            currency: EUR,
            exponent: 2
        }
    );
    assert_eq!(refusal.code(), 447);
}

/// `holds_double_void_on_unlimited_account_refused` (research R9; the 0.7
/// `posting.rs` claim "only a limit stops a doubled release").
#[test]
fn holds_double_void_on_unlimited_account_refused() {
    let mut books = wallet_books();
    let held = books
        .record(hold("auth", date!("2026-11-20"), amount!("100.00")))
        .unwrap();
    let void = books
        .record(Entry::void_hold("void-1", date!("2026-11-21"), held.id))
        .unwrap();
    assert_eq!(
        rejected(books.record(Entry::void_hold("void-2", date!("2026-11-21"), held.id))),
        Rejection::HoldResolved {
            hold: held.id,
            by: void.id,
            how: ResolutionKind::Voided
        }
    );
    assert_eq!(net(&books, WALLET, Layer::Pending), Amount::ZERO);
}

/// FR-009: a void (or expiry) releases a hold on an account that has
/// closed since; a post is refused there.
#[test]
fn us1_closed_account_void_ok_post_refused() {
    let mut books = wallet_books();
    let held = books
        .record(hold("auth", date!("2026-11-20"), amount!("100.00")))
        .unwrap();
    books.close_account(CLEARING, date!("2026-11-25")).unwrap();
    let refusal = rejected(books.record(Entry::post_hold(
        "capture",
        date!("2026-11-26"),
        held.id,
        PostAmount::Full,
    )));
    assert_eq!(
        refusal,
        Rejection::PostToClosedAccount {
            hold: held.id,
            account: AccountRef::Path(CLEARING.parse().unwrap()),
            date: date!("2026-11-26"),
        }
    );
    assert_eq!(refusal.code(), 449);
    books
        .record(Entry::void_hold("void", date!("2026-11-26"), held.id))
        .unwrap();
    books.audit().unwrap();
}

/// `holds_resolution_link_is_committed` (redesign A1 / Q-E1): the hold a
/// resolution names is in its committed bytes, so pointing it at another
/// hold changes its leaf hash and fails the audit.
#[test]
fn holds_resolution_link_is_committed() {
    let mut books = wallet_books();
    let first = books
        .record(hold("auth-1", date!("2026-11-20"), amount!("10.00")))
        .unwrap();
    let second = books
        .record(hold("auth-2", date!("2026-11-20"), amount!("10.00")))
        .unwrap();
    let void = books
        .record(Entry::void_hold("void", date!("2026-11-21"), first.id))
        .unwrap();
    let claims = common::Claims::of(&books);
    let mut leaves = common::leaf_bytes(&books);
    let at = usize::try_from(void.position).unwrap();
    let Record::Entry(record) = Record::<2>::decode(&leaves[at]).unwrap() else {
        panic!("the void is an entry");
    };
    let mut parts = record.into_parts();
    parts.hold = Some(HoldAction::Void { hold: second.id });
    let tampered = Record::Entry(doubleentry::EntryRecord::new(parts).unwrap()).to_leaf_bytes();
    assert_ne!(
        doubleentry::log::leaf_hash(&tampered),
        doubleentry::log::leaf_hash(&leaves[at])
    );
    leaves[at] = tampered;
    // The rewritten void is a valid record on its own: the changed leaf
    // hash is what gives it away.
    let failure = common::audit_leaves(&books, &leaves, &claims).unwrap_err();
    assert_eq!(failure.kind, AuditFailureKind::HeadMismatch);
}

// ── US2: expiry by caller dates ─────────────────────────────────────────────

#[test]
fn us2_expiry_dates_decide_post_and_expire() {
    let mut books = wallet_books();
    let held = books
        .record(hold("auth", date!("2026-11-20"), amount!("100.00")).expires(date!("2026-11-30")))
        .unwrap();
    assert_eq!(
        rejected(books.record(Entry::expire_hold("early", date!("2026-11-29"), held.id))),
        Rejection::HoldNotExpired {
            hold: held.id,
            expires: date!("2026-11-30")
        }
    );
    assert_eq!(
        rejected(books.record(Entry::post_hold(
            "late",
            date!("2026-11-30"),
            held.id,
            PostAmount::Full
        ))),
        Rejection::HoldExpired {
            hold: held.id,
            expires: date!("2026-11-30")
        }
    );
    let expired = books
        .record(Entry::expire_hold("expire", date!("2026-11-30"), held.id))
        .unwrap();
    assert_eq!(
        books.hold(held.id).unwrap().status,
        HoldStatus::Expired { by: expired.id }
    );
}

#[test]
fn us2_a_void_on_the_expiry_date_is_recorded() {
    let mut books = wallet_books();
    let held = books
        .record(hold("auth", date!("2026-11-20"), amount!("100.00")).expires(date!("2026-11-30")))
        .unwrap();
    books
        .record(Entry::void_hold("void", date!("2026-11-30"), held.id))
        .unwrap();
}

#[test]
fn us2_a_hold_without_expiry_never_expires() {
    let mut books = wallet_books();
    let held = books
        .record(hold("auth", date!("2026-11-20"), amount!("100.00")))
        .unwrap();
    assert_eq!(
        rejected(books.record(Entry::expire_hold("expire", date!("2026-12-31"), held.id))),
        Rejection::HoldNeverExpires { hold: held.id }
    );
    books
        .record(Entry::post_hold(
            "capture",
            date!("2026-12-31"),
            held.id,
            PostAmount::Full,
        ))
        .unwrap();
}

#[test]
fn us2_holds_due_lists_open_holds_by_expiry_then_position() {
    let mut books = wallet_books();
    let late = books
        .record(hold("late", date!("2026-11-02"), amount!("1.00")).expires(date!("2026-11-30")))
        .unwrap();
    let early = books
        .record(hold("early", date!("2026-11-03"), amount!("2.00")).expires(date!("2026-11-15")))
        .unwrap();
    books
        .record(hold("never", date!("2026-11-04"), amount!("3.00")))
        .unwrap();
    let voided = books
        .record(hold("voided", date!("2026-11-01"), amount!("4.00")).expires(date!("2026-11-10")))
        .unwrap();
    books
        .record(Entry::void_hold("void", date!("2026-11-05"), voided.id))
        .unwrap();
    let due = books.holds_due(date!("2026-11-30"), PageRead::first(50));
    assert_eq!(
        due.items.iter().map(|hold| hold.id).collect::<Vec<_>>(),
        [early.id, late.id]
    );
    assert_eq!(due.items[0].amount, amount!("2.00"));
    assert!(due.next.is_none());
    // One per page, by cursor.
    let first = books.holds_due(date!("2026-11-30"), PageRead::first(1));
    assert_eq!(first.items[0].id, early.id);
    let next = first.next.unwrap();
    let second = books.holds_due(date!("2026-11-30"), PageRead::first(1).after(next));
    assert_eq!(second.items[0].id, late.id);
    assert!(second.next.is_none());
    assert!(
        books
            .holds_due(date!("2026-11-14"), PageRead::first(50))
            .items
            .is_empty()
    );
}

#[test]
fn us2_a_hold_cannot_be_born_expired() {
    let mut books = wallet_books();
    let refusal =
        rejected(books.record(
            hold("auth", date!("2026-11-20"), amount!("1.00")).expires(date!("2026-11-20")),
        ));
    assert_eq!(
        refusal,
        Rejection::HoldExpiry {
            booking: date!("2026-11-20"),
            expires: date!("2026-11-20")
        }
    );
    assert_eq!(refusal.code(), 141);
}

#[test]
fn us2_a_resolution_before_its_hold_is_refused() {
    let mut books = wallet_books();
    let held = books
        .record(hold("auth", date!("2026-11-20"), amount!("1.00")))
        .unwrap();
    assert_eq!(
        rejected(books.record(Entry::void_hold("void", date!("2026-11-19"), held.id))),
        Rejection::ResolutionBeforeHold {
            hold: held.id,
            held_on: date!("2026-11-20")
        }
    );
}

// ── US3: limits ─────────────────────────────────────────────────────────────

/// The wallet sequence of SC-005: headroom 70.00 → 20.00 → refusal → 80.00.
#[test]
fn us3_holds_consume_headroom_and_releases_return_it() {
    let mut books = wallet_books();
    books
        .set_limit(WALLET, BalanceLimit::NoDebitBalance, date!("2026-01-01"))
        .unwrap();
    books
        .record(
            Entry::new("fund", date!("2026-11-01"))
                .debit("Assets:Bank", amount!("100.00"), EUR)
                .credit(WALLET, amount!("100.00"), EUR),
        )
        .unwrap();
    assert_eq!(headroom(&books), amount!("100.00"));
    let thirty = books
        .record(hold("h30", date!("2026-11-02"), amount!("30.00")))
        .unwrap();
    assert_eq!(headroom(&books), amount!("70.00"));
    let fifty = books
        .record(hold("h50", date!("2026-11-03"), amount!("50.00")))
        .unwrap();
    assert_eq!(headroom(&books), amount!("20.00"));
    let refusal = rejected(books.record(hold("h30b", date!("2026-11-04"), amount!("30.00"))));
    assert_eq!(
        refusal,
        Rejection::LimitExceeded {
            account: WALLET.parse().unwrap(),
            currency: EUR,
            limit: BalanceLimit::NoDebitBalance,
            shortfall: ScaledAmount::from(eur("10.00")),
        }
    );
    assert_eq!(headroom(&books), amount!("20.00"));
    books
        .record(Entry::post_hold(
            "capture",
            date!("2026-11-05"),
            thirty.id,
            PostAmount::Partial(amount!("20.00")),
        ))
        .unwrap();
    books
        .record(Entry::void_hold("void", date!("2026-11-05"), fifty.id))
        .unwrap();
    assert_eq!(headroom(&books), amount!("80.00"));
    assert_eq!(net(&books, WALLET, Layer::Settled), amount!("-80.00"));
    assert_eq!(net(&books, WALLET, Layer::Pending), Amount::ZERO);
    books.audit().unwrap();
}

#[test]
fn us3_a_release_is_never_refused_by_a_limit() {
    let mut books = wallet_books();
    let held = books
        .record(hold("auth", date!("2026-11-02"), amount!("50.00")))
        .unwrap();
    let expiring = books
        .record(hold("auth-2", date!("2026-11-02"), amount!("20.00")).expires(date!("2026-11-10")))
        .unwrap();
    // The limit tightened below what the wallet already reserves.
    books
        .set_limit(WALLET, BalanceLimit::NoDebitBalance, date!("2026-11-03"))
        .unwrap();
    assert!(headroom(&books).is_negative());
    books
        .record(Entry::void_hold("void", date!("2026-11-04"), held.id))
        .unwrap();
    books
        .record(Entry::expire_hold(
            "expire",
            date!("2026-11-10"),
            expiring.id,
        ))
        .unwrap();
    assert_eq!(headroom(&books), Amount::ZERO);
}

#[test]
fn us3_a_credit_side_limit_refuses_the_hold_not_its_post() {
    let mut books = wallet_books();
    books
        .register(
            AccountSpec::new("Assets:Card:limit", AccountKind::Asset, date!("2026-01-01"))
                .limit(BalanceLimit::NoCreditBalance),
        )
        .unwrap();
    books
        .record(
            Entry::new("fund", date!("2026-11-01"))
                .debit("Assets:Card:limit", amount!("100.00"), EUR)
                .credit("Assets:Bank", amount!("100.00"), EUR),
        )
        .unwrap();
    let reserve = |key: &str, amount: Amount| {
        Entry::open_hold(key, date!("2026-11-02"))
            .debit("Assets:Bank", amount, EUR)
            .credit("Assets:Card:limit", amount, EUR)
    };
    // Settled credit 0 + held credits 120 − settled debit 100 > 0.
    let refusal = rejected(books.record(reserve("too-much", amount!("120.00"))));
    assert_eq!(refusal.code(), 501);
    let held = books.record(reserve("ok", amount!("80.00"))).unwrap();
    books
        .record(Entry::post_hold(
            "capture",
            date!("2026-11-03"),
            held.id,
            PostAmount::Full,
        ))
        .unwrap();
    books.audit().unwrap();
}

// ── US5: evidence ───────────────────────────────────────────────────────────

#[test]
fn us5_holds_and_resolutions_are_not_reversible() {
    let mut books = wallet_books();
    let held = books
        .record(hold("auth", date!("2026-11-20"), amount!("100.00")))
        .unwrap();
    let mirror = books
        .reversal(held.id, "undo-hold", date!("2026-11-21"))
        .unwrap();
    assert_eq!(
        rejected(books.record(mirror)),
        Rejection::HoldNotReversible { target: held.id }
    );
    let capture = books
        .record(Entry::post_hold(
            "capture",
            date!("2026-11-22"),
            held.id,
            PostAmount::Full,
        ))
        .unwrap();
    let mirror = books
        .reversal(capture.id, "undo-capture", date!("2026-11-23"))
        .unwrap();
    let refusal = rejected(books.record(mirror));
    assert_eq!(refusal, Rejection::HoldNotReversible { target: capture.id });
    assert_eq!(refusal.code(), 450);
}

#[test]
fn us5_a_reversal_cannot_act_on_a_hold() {
    let mut books = wallet_books();
    let refusal = rejected(books.record(
        hold("auth", date!("2026-11-20"), amount!("1.00")).reverses(EntryId::from_bytes([3; 16])),
    ));
    assert_eq!(refusal, Rejection::ReversalWithHoldAction);
    assert_eq!(refusal.code(), 144);
}

#[test]
fn us5_a_hold_stays_open_across_a_seal() {
    let mut books = wallet_books();
    let held = books
        .record(hold("auth", date!("2026-11-20"), amount!("100.00")))
        .unwrap();
    let november = books.seal_period("2026-11").unwrap();
    let pending = november
        .seal
        .parts()
        .trees
        .iter()
        .find(|tree| tree.currency == EUR && tree.layer == Layer::Pending)
        .expect("November's seal has a Pending tree");
    assert_eq!(pending.totals.debit(), amount!("100.00"));
    books
        .record(Entry::post_hold(
            "capture",
            date!("2026-12-02"),
            held.id,
            PostAmount::Partial(amount!("60.00")),
        ))
        .unwrap();
    let december = books.seal_period("2026-12").unwrap();
    let pending = december
        .seal
        .parts()
        .trees
        .iter()
        .find(|tree| tree.currency == EUR && tree.layer == Layer::Pending)
        .unwrap();
    // The release legs: the cumulative Pending totals now net to zero.
    assert_eq!(pending.totals.debit(), amount!("200.00"));
    assert_eq!(pending.totals.debit(), pending.totals.credit());
    books.audit().unwrap();
}

#[test]
fn us5_year_end_closing_leaves_the_pending_layer_alone() {
    let mut books = wallet_books();
    books
        .open("Income:Fees", AccountKind::Income, date!("2026-01-01"))
        .unwrap();
    books
        .open("Equity:Retained", AccountKind::Equity, date!("2026-01-01"))
        .unwrap();
    books
        .record(
            Entry::open_hold("auth", date!("2026-12-01"))
                .debit("Assets:Bank", amount!("9.00"), EUR)
                .credit("Income:Fees", amount!("9.00"), EUR),
        )
        .unwrap();
    books
        .record(
            Entry::new("fee", date!("2026-12-02"))
                .debit("Assets:Bank", amount!("5.00"), EUR)
                .credit("Income:Fees", amount!("5.00"), EUR),
        )
        .unwrap();
    let drafts = books
        .close_year("close-2026", date!("2026-12-31"), "Equity:Retained")
        .unwrap();
    assert!(!drafts.is_empty());
    for draft in drafts {
        assert!(
            draft
                .postings()
                .iter()
                .all(|p| p.get_layer() == Layer::Settled)
        );
        let closed = draft
            .postings()
            .iter()
            .find(|p| p.side() == Side::Debit)
            .map(|p| p.amount());
        assert_eq!(closed, Some(amount!("5.00")));
        books.record(draft).unwrap();
    }
    assert_eq!(net(&books, "Income:Fees", Layer::Pending), amount!("-9.00"));
}

/// SC-006 on `Journal`: an audit replays the hold index and the amounts
/// open holds reserve, and refuses a resolution whose postings are not the
/// ones the core derives.
#[test]
fn us5_audit_catches_hold_tampering() {
    let mut books = wallet_books();
    let posted = books
        .record(hold("auth-1", date!("2026-11-20"), amount!("30.00")))
        .unwrap();
    books
        .record(hold("auth-2", date!("2026-11-20"), amount!("20.00")))
        .unwrap();
    let capture = books
        .record(Entry::post_hold(
            "capture",
            date!("2026-11-21"),
            posted.id,
            PostAmount::Partial(amount!("10.00")),
        ))
        .unwrap();
    let honest = common::Claims::of(&books);
    let leaves = common::leaf_bytes(&books);
    common::audit_leaves(&books, &leaves, &honest).unwrap();
    let fails = |claims: &StoreProjection| match common::audit_leaves(&books, &leaves, claims)
        .unwrap_err()
        .kind
    {
        AuditFailureKind::ProjectionMismatch { part, .. } => part,
        other => panic!("not a projection mismatch: {other:?}"),
    };
    let holds = honest.rows(ProjectionPart::Holds).len();
    // A resolved hold marked open.
    let mut claims = honest.clone();
    for row in 0..holds {
        common::tamper_cell(&mut claims, ProjectionPart::Holds, row, 8, Cell::Null);
        common::tamper_cell(&mut claims, ProjectionPart::Holds, row, 9, Cell::Null);
    }
    assert_eq!(fails(&claims), ProjectionPart::Holds);
    // An open hold marked resolved.
    let mut claims = honest.clone();
    for row in 0..holds {
        common::tamper_cell(
            &mut claims,
            ProjectionPart::Holds,
            row,
            8,
            Cell::Bytes(capture.id.as_bytes().to_vec()),
        );
        common::tamper_cell(&mut claims, ProjectionPart::Holds, row, 9, Cell::Int(0));
    }
    assert_eq!(fails(&claims), ProjectionPart::Holds);
    // An edited reserved amount.
    let mut claims = honest.clone();
    common::tamper_cell(
        &mut claims,
        ProjectionPart::Outstanding,
        0,
        2,
        Cell::Bytes(100i128.to_be_bytes().to_vec()),
    );
    assert_eq!(fails(&claims), ProjectionPart::Outstanding);
    // A resolution whose stored postings are not the derived ones.
    let at = usize::try_from(capture.position).unwrap();
    let Record::Entry(record) = Record::<2>::decode(&leaves[at]).unwrap() else {
        panic!("the capture is an entry");
    };
    let mut parts = record.into_parts();
    for posting in parts
        .postings
        .iter_mut()
        .filter(|p| p.layer == Layer::Settled)
    {
        posting.amount = amount!("11.00");
    }
    let mut forged = leaves.clone();
    forged[at] = Record::Entry(doubleentry::EntryRecord::new(parts).unwrap()).to_leaf_bytes();
    let failure = common::audit_leaves(&books, &forged, &honest).unwrap_err();
    assert_eq!(failure.position, Some(capture.position));
    assert_eq!(failure.kind, AuditFailureKind::NotProduced);
}

#[test]
fn hold_state_derives_from_the_records_alone() {
    let mut books = wallet_books();
    let held = books
        .record(hold("auth", date!("2026-11-20"), amount!("100.00")))
        .unwrap();
    let open = books.entry(held.id).unwrap();
    let state = HoldState::derive(books.id(), held.position, &open, None).unwrap();
    assert_eq!(Some(&state), books.hold(held.id).as_ref());
    let capture: Recorded = books
        .record(Entry::post_hold(
            "capture",
            date!("2026-11-21"),
            held.id,
            PostAmount::Partial(amount!("25.00")),
        ))
        .unwrap();
    let resolution = books.entry(capture.id).unwrap();
    let state = HoldState::derive(books.id(), held.position, &open, Some(&resolution)).unwrap();
    assert_eq!(Some(&state), books.hold(held.id).as_ref());
    assert_eq!(state.status.resolved_by(), Some(capture.id));
    // A record that is no hold, or a resolution of another hold, derives
    // nothing.
    assert!(HoldState::derive(books.id(), capture.position, &resolution, None).is_none());
    let other = books
        .record(hold("auth-2", date!("2026-11-20"), amount!("1.00")))
        .unwrap();
    let other = books.entry(other.id).unwrap();
    assert!(HoldState::derive(books.id(), 0, &other, Some(&resolution)).is_none());
    // One line, honouring width.
    let line = state.to_string();
    assert!(
        line.starts_with(&format!("hold {} 100.00 EUR #0 → #1", held.id)),
        "{line}"
    );
    assert!(
        line.ends_with(&format!("posted 25.00 by {}", capture.id)),
        "{line}"
    );
    assert_eq!(format!("{:>8}", HoldStatus::<2>::Open), "    open");
}
