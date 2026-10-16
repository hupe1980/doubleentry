//! Master data: account registrations and changes, currencies and the
//! policy as dated, append-only records the rule core enforces.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

mod common;

use common::{books, eur, journal, ledger, rejected, sale};
use doubleentry::store::{
    AccountView, ClearingView, EntryMeta, EntryView, Header, HoldView, Movement, Op, PeriodView,
    Slice, StateView, Terms, Versioned, execute,
};
use doubleentry::{
    AccountHandle, AccountKind, AccountPath, AccountRef, AccountSpec, Amount, Balance,
    BalanceLimit, Currency, Entry, EntryId, IdempotencyKey, Journal, Label, LedgerPolicy, PeriodId,
    PostingRef, Rejection,
    currency::{EUR, USD},
    date,
    error::{LabelField, ValueError},
    record::{AccountChange, Record},
    store::BalanceKey,
};

fn path(text: &str) -> AccountPath {
    AccountPath::parse(text).unwrap()
}

fn size<const P: u8>(books: &Journal<P>) -> u64 {
    books.head().size
}

/// The record at a position of the log.
fn record_at(books: &Journal, position: usize) -> Record<2> {
    let leaf = books.leaves().nth(position).unwrap();
    Record::<2>::decode(leaf.bytes).unwrap()
}

/// The US1 setup: EUR, `Assets:Cash`, January.
fn setup(books: &mut Journal) -> AccountHandle {
    books.declare_currency(EUR, 2, date!("2026-01-01")).unwrap();
    let handle = books
        .open("Assets:Cash", AccountKind::Asset, date!("2026-01-01"))
        .unwrap();
    books
        .define_period("2026-01", date!("2026-01-01"), date!("2026-01-31"))
        .unwrap();
    handle
}

fn policy() -> LedgerPolicy {
    LedgerPolicy::new([Label::new("project").unwrap()], Some(30)).unwrap()
}

// ── US1 acceptance scenarios ─────────────────────────────────────────────────

#[test]
fn us1_s1_setup_appends_currency_account_and_period_records() {
    let mut books = journal();
    assert_eq!(size(&books), 1);
    let handle = setup(&mut books);
    assert_eq!(handle.get(), 0);
    assert_eq!(size(&books), 4);
    assert!(matches!(record_at(&books, 0), Record::Genesis(_)));
    match record_at(&books, 1) {
        Record::Currency(declaration) => {
            assert_eq!(declaration.code, EUR);
            assert_eq!(declaration.exponent, 2);
            assert_eq!(declaration.effective, date!("2026-01-01"));
            assert_eq!(declaration.actor.as_str(), "setup");
        }
        other => panic!("expected a currency record, got {other:?}"),
    }
    match record_at(&books, 2) {
        Record::Account(event) => {
            assert_eq!(event.effective, date!("2026-01-01"));
            assert_eq!(event.actor.as_str(), "setup");
            match event.action {
                doubleentry::record::AccountAction::Register(registration) => {
                    assert_eq!(registration.handle.get(), 0);
                    assert_eq!(registration.path, path("Assets:Cash"));
                    assert_eq!(registration.kind, AccountKind::Asset);
                }
                other => panic!("expected a registration, got {other:?}"),
            }
        }
        other => panic!("expected an account record, got {other:?}"),
    }
    assert!(matches!(record_at(&books, 3), Record::Period(_)));
}

#[test]
fn us1_s2_repeating_the_setup_appends_nothing() {
    let mut books = journal();
    setup(&mut books);
    let head = books.head();
    let handle = setup(&mut books);
    assert_eq!(handle.get(), 0);
    assert_eq!(books.head(), head);
}

#[test]
fn us1_s3_reopening_a_path_with_another_kind_is_account_exists() {
    let mut books = journal();
    setup(&mut books);
    let before = size(&books);
    assert_eq!(
        rejected(books.open("Assets:Cash", AccountKind::Liability, date!("2026-01-01"))),
        Rejection::AccountExists {
            path: path("Assets:Cash"),
            handle: AccountHandle::new(0).unwrap(),
        }
    );
    assert_eq!(size(&books), before);
}

#[test]
fn us1_s4_exponent_is_never_lowered_may_be_raised_and_is_bounded_by_p() {
    let mut books = journal();
    setup(&mut books);
    let before = size(&books);
    assert_eq!(
        rejected(books.declare_currency(EUR, 1, date!("2026-01-01"))),
        Rejection::ExponentLowered {
            currency: EUR,
            current: 2,
            requested: 1,
        }
    );
    assert_eq!(size(&books), before);

    // Raising needs room for it: at P = 18 an exponent of 3 is recorded.
    let mut wide = Journal::<18>::with_precision(ledger()).as_actor("setup");
    wide.declare_currency(EUR, 2, date!("2026-01-01")).unwrap();
    let before = size(&wide);
    wide.declare_currency(EUR, 3, date!("2026-01-01")).unwrap();
    assert_eq!(size(&wide), before + 1);
    let leaf = wide.leaves().nth(usize::try_from(before).unwrap()).unwrap();
    match Record::<18>::decode(leaf.bytes).unwrap() {
        Record::Currency(declaration) => assert_eq!(declaration.exponent, 3),
        other => panic!("expected a currency record, got {other:?}"),
    }
    assert_eq!(
        rejected(wide.declare_currency(EUR, 19, date!("2026-01-01"))),
        Rejection::ExponentTooLarge {
            exponent: 19,
            max: 18,
        }
    );
    wide.declare_currency(USD, 18, date!("2026-01-01")).unwrap();
}

#[test]
fn us1_s5_class_is_frozen_after_the_first_posting() {
    let mut books = books();
    let before = size(&books);
    books
        .set_class("Expenses:Rent", Some("opex"), date!("2026-02-01"))
        .unwrap();
    assert_eq!(size(&books), before + 1);

    books
        .record(sale("s-1", date!("2026-01-15"), "10.00"))
        .unwrap();
    let before = size(&books);
    assert_eq!(
        rejected(books.set_class("Assets:Cash", Some("cash"), date!("2026-02-01"))),
        Rejection::ClassFrozen {
            account: path("Assets:Cash"),
        }
    );
    assert_eq!(size(&books), before);
}

#[test]
fn us1_s6_register_at_or_below_the_watermark_is_refused() {
    let mut books = books();
    books.seal_period("2026-01").unwrap();
    assert_eq!(
        rejected(books.open("Assets:Bank", AccountKind::Asset, date!("2026-01-15"))),
        Rejection::AtOrBelowWatermark {
            effective: date!("2026-01-15"),
            watermark: date!("2026-01-31"),
        }
    );
    books
        .open("Assets:Bank", AccountKind::Asset, date!("2026-02-01"))
        .unwrap();
}

#[test]
fn us1_s6_change_at_or_below_the_watermark_is_refused() {
    let mut books = books();
    books.seal_period("2026-01").unwrap();
    assert_eq!(
        rejected(books.set_limit(
            "Assets:Cash",
            BalanceLimit::NoCreditBalance,
            date!("2026-01-31")
        )),
        Rejection::AtOrBelowWatermark {
            effective: date!("2026-01-31"),
            watermark: date!("2026-01-31"),
        }
    );
    books
        .set_limit(
            "Assets:Cash",
            BalanceLimit::NoCreditBalance,
            date!("2026-02-01"),
        )
        .unwrap();
}

#[test]
fn us1_s6_currency_at_or_below_the_watermark_is_refused() {
    let mut books = books();
    books.seal_period("2026-01").unwrap();
    assert_eq!(
        rejected(books.declare_currency(USD, 2, date!("2026-01-31"))),
        Rejection::AtOrBelowWatermark {
            effective: date!("2026-01-31"),
            watermark: date!("2026-01-31"),
        }
    );
    books.declare_currency(USD, 2, date!("2026-02-01")).unwrap();
}

#[test]
fn us1_s6_policy_at_or_below_the_watermark_is_refused() {
    let mut books = books();
    books.seal_period("2026-01").unwrap();
    assert_eq!(
        rejected(books.set_policy(policy(), date!("2026-01-01"))),
        Rejection::AtOrBelowWatermark {
            effective: date!("2026-01-01"),
            watermark: date!("2026-01-31"),
        }
    );
    books.set_policy(policy(), date!("2026-02-01")).unwrap();
}

#[test]
fn us1_s7_a_limit_change_before_the_latest_is_effective_date_regresses() {
    let mut books = books();
    books
        .set_limit(
            "Assets:Cash",
            BalanceLimit::NoCreditBalance,
            date!("2026-03-01"),
        )
        .unwrap();
    let before = size(&books);
    assert_eq!(
        rejected(books.set_limit("Assets:Cash", BalanceLimit::Unlimited, date!("2026-02-15"))),
        Rejection::EffectiveDateRegresses {
            effective: date!("2026-02-15"),
            latest: date!("2026-03-01"),
        }
    );
    assert_eq!(size(&books), before);
}

#[test]
fn us1_s8_close_before_the_last_posting_is_refused_then_close_bounds_postings() {
    let mut books = books();
    books
        .record(sale("s-1", date!("2026-03-20"), "10.00"))
        .unwrap();
    assert_eq!(
        rejected(books.close_account("Assets:Cash", date!("2026-03-10"))),
        Rejection::PostingsAfterClose {
            account: path("Assets:Cash"),
            last_booked: date!("2026-03-20"),
        }
    );
    let before = size(&books);
    books
        .close_account("Assets:Cash", date!("2026-03-31"))
        .unwrap();
    assert_eq!(size(&books), before + 1);
    assert_eq!(
        rejected(books.record(sale("s-2", date!("2026-04-01"), "10.00"))),
        Rejection::AccountNotOpen {
            account: path("Assets:Cash"),
            date: date!("2026-04-01"),
        }
    );
    books
        .record(sale("s-3", date!("2026-03-31"), "10.00"))
        .unwrap();
}

// ── Unchanged repeats (A16) ──────────────────────────────────────────────────

#[test]
fn register_identical_spec_under_another_actor_is_unchanged() {
    let books = books();
    let head = books.head();
    let mut books = books.as_actor("someone-else");
    let handle = books
        .open("Assets:Cash", AccountKind::Asset, date!("2026-01-01"))
        .unwrap();
    assert_eq!(handle.get(), 0);
    assert_eq!(books.head(), head);
}

#[test]
fn repeated_set_limit_with_same_or_later_date_is_unchanged() {
    let mut books = books();
    books
        .set_limit(
            "Assets:Cash",
            BalanceLimit::NoCreditBalance,
            date!("2026-07-01"),
        )
        .unwrap();
    let head = books.head();
    books
        .set_limit(
            "Assets:Cash",
            BalanceLimit::NoCreditBalance,
            date!("2026-07-01"),
        )
        .unwrap();
    books
        .set_limit(
            "Assets:Cash",
            BalanceLimit::NoCreditBalance,
            date!("2026-08-01"),
        )
        .unwrap();
    assert_eq!(books.head(), head);
}

#[test]
fn interleaved_retry_of_an_older_limit_change_is_effective_date_regresses() {
    let mut books = books();
    books
        .set_limit(
            "Assets:Cash",
            BalanceLimit::NoCreditBalance,
            date!("2026-07-01"),
        )
        .unwrap();
    books
        .set_limit("Assets:Cash", BalanceLimit::Unlimited, date!("2026-08-01"))
        .unwrap();
    let head = books.head();
    assert_eq!(
        rejected(books.set_limit(
            "Assets:Cash",
            BalanceLimit::NoCreditBalance,
            date!("2026-07-01")
        )),
        Rejection::EffectiveDateRegresses {
            effective: date!("2026-07-01"),
            latest: date!("2026-08-01"),
        }
    );
    assert_eq!(books.head(), head);
}

#[test]
fn repeated_declare_currency_is_unchanged() {
    let mut books = books();
    let head = books.head();
    books.declare_currency(EUR, 2, date!("2026-01-01")).unwrap();
    books.declare_currency(EUR, 2, date!("2026-05-01")).unwrap();
    assert_eq!(books.head(), head);
}

#[test]
fn repeated_set_policy_is_unchanged() {
    let mut books = books();
    books.set_policy(policy(), date!("2026-02-01")).unwrap();
    let head = books.head();
    books.set_policy(policy(), date!("2026-02-01")).unwrap();
    books.set_policy(policy(), date!("2026-03-01")).unwrap();
    assert_eq!(books.head(), head);
}

#[test]
fn repeated_set_class_is_unchanged() {
    let mut books = books();
    books
        .set_class("Expenses:Rent", Some("opex"), date!("2026-02-01"))
        .unwrap();
    let head = books.head();
    books
        .set_class("Expenses:Rent", Some("opex"), date!("2026-02-01"))
        .unwrap();
    assert_eq!(books.head(), head);
}

#[test]
fn repeated_close_account_is_unchanged() {
    let mut books = books();
    books
        .close_account("Expenses:Rent", date!("2026-03-31"))
        .unwrap();
    let head = books.head();
    books
        .close_account("Expenses:Rent", date!("2026-03-31"))
        .unwrap();
    assert_eq!(books.head(), head);
}

// ── Rejection rows ───────────────────────────────────────────────────────────

#[test]
fn register_with_an_invalid_path_is_invalid_path() {
    let mut books = books();
    let head = books.head();
    let refusal = rejected(books.open("Assets::Cash", AccountKind::Asset, date!("2026-01-01")));
    assert!(
        matches!(
            refusal,
            Rejection::InvalidPath {
                posting: None,
                reason: ValueError::Empty
            }
        ),
        "{refusal:?}"
    );
    assert_eq!(books.head(), head);
}

#[test]
fn register_with_an_empty_class_is_invalid_label() {
    let mut books = books();
    let refusal = rejected(books.register(
        AccountSpec::new("Assets:Petty", AccountKind::Asset, date!("2026-01-01")).class(""),
    ));
    assert!(
        matches!(
            refusal,
            Rejection::InvalidLabel {
                field: LabelField::Class,
                reason: ValueError::Empty
            }
        ),
        "{refusal:?}"
    );
}

#[test]
fn an_invalid_actor_is_refused_by_the_next_master_data_write() {
    let mut books = journal().as_actor("");
    let refusal = rejected(books.declare_currency(EUR, 2, date!("2026-01-01")));
    assert!(
        matches!(
            refusal,
            Rejection::InvalidLabel {
                field: LabelField::Actor,
                reason: ValueError::Empty
            }
        ),
        "{refusal:?}"
    );
    let mut books = books.as_actor(" x");
    let refusal = rejected(books.open("Assets:Cash", AccountKind::Asset, date!("2026-01-01")));
    assert!(
        matches!(
            refusal,
            Rejection::InvalidLabel {
                field: LabelField::Actor,
                reason: ValueError::EdgeWhitespace
            }
        ),
        "{refusal:?}"
    );
    assert_eq!(size(&books), 1);
}

#[test]
fn register_with_open_to_before_open_from_is_invalid_open_window() {
    let mut books = books();
    assert_eq!(
        rejected(
            books.register(
                AccountSpec::new("Assets:Petty", AccountKind::Asset, date!("2026-03-01"))
                    .open_to(date!("2026-02-01")),
            )
        ),
        Rejection::InvalidOpenWindow {
            open_from: date!("2026-03-01"),
            open_to: date!("2026-02-01"),
        }
    );
}

#[test]
fn close_account_before_its_open_from_is_invalid_open_window() {
    let mut books = books();
    books
        .open("Assets:Bank", AccountKind::Asset, date!("2026-03-01"))
        .unwrap();
    assert_eq!(
        rejected(books.close_account("Assets:Bank", date!("2026-02-15"))),
        Rejection::InvalidOpenWindow {
            open_from: date!("2026-03-01"),
            open_to: date!("2026-02-15"),
        }
    );
}

#[test]
fn change_of_an_unknown_account_is_unknown_account() {
    let mut books = books();
    assert_eq!(
        rejected(books.set_limit(
            "Assets:Unknown",
            BalanceLimit::NoCreditBalance,
            date!("2026-02-01")
        )),
        Rejection::UnknownAccount {
            account: AccountRef::Path(path("Assets:Unknown")),
        }
    );
    let ghost = AccountHandle::new(99).unwrap();
    assert_eq!(
        rejected(books.set_limit(ghost, BalanceLimit::NoCreditBalance, date!("2026-02-01"))),
        Rejection::UnknownAccount {
            account: AccountRef::Handle(ghost),
        }
    );
}

#[test]
fn register_under_a_posted_to_parent_is_parent_has_postings() {
    let mut books = books();
    books
        .record(sale("s-1", date!("2026-01-15"), "10.00"))
        .unwrap();
    assert_eq!(
        rejected(books.open("Assets:Cash:Petty", AccountKind::Asset, date!("2026-02-01"))),
        Rejection::ParentHasPostings {
            parent: path("Assets:Cash"),
        }
    );
}

#[test]
fn registering_a_parent_of_an_unposted_account_makes_it_a_non_leaf() {
    let mut books = books();
    books
        .open(
            "Expenses:Travel:Hotel",
            AccountKind::Expense,
            date!("2026-01-01"),
        )
        .unwrap();
    books
        .open("Expenses:Travel", AccountKind::Expense, date!("2026-01-01"))
        .unwrap();
    assert_eq!(
        rejected(
            books.record(
                Entry::new("trip-1", date!("2026-02-10"))
                    .debit("Expenses:Travel", eur("80.00"), EUR)
                    .credit("Assets:Cash", eur("80.00"), EUR),
            )
        ),
        Rejection::NotALeaf {
            account: path("Expenses:Travel"),
        }
    );
    books
        .record(
            Entry::new("trip-2", date!("2026-02-10"))
                .debit("Expenses:Travel:Hotel", eur("80.00"), EUR)
                .credit("Assets:Cash", eur("80.00"), EUR),
        )
        .unwrap();
}

// ── 428 through a stub view ──────────────────────────────────────────────────

/// A view of a ledger whose every account handle is in use, and nothing else.
struct Exhausted;

impl StateView<2> for Exhausted {
    fn header(&self) -> Header<2> {
        Header {
            ledger: ledger(),
            size: 1,
            accounts: u32::MAX,
            entries: 0,
            watermark: None,
            last_booking: None,
            last_period: None,
        }
    }
    fn entry_by_key(&self, _key: &IdempotencyKey) -> Option<EntryMeta> {
        None
    }
    fn account_by_path(&self, _path: &AccountPath) -> Option<AccountView> {
        None
    }
    fn account(&self, _handle: AccountHandle) -> Option<AccountView> {
        None
    }
    fn account_terms(&self, _handle: AccountHandle, _on: doubleentry::Date) -> Option<Terms> {
        None
    }
    fn currency(&self, _code: Currency, _on: doubleentry::Date) -> Option<u8> {
        None
    }
    fn currency_latest(&self, _code: Currency) -> Option<Versioned<u8>> {
        None
    }
    fn policy(&self, _on: doubleentry::Date) -> LedgerPolicy {
        LedgerPolicy::default()
    }
    fn policy_latest(&self) -> Option<Versioned<LedgerPolicy>> {
        None
    }
    fn period_at(&self, _date: doubleentry::Date) -> Option<PeriodView<2>> {
        None
    }
    fn period(&self, _id: &PeriodId) -> Option<PeriodView<2>> {
        None
    }
    fn first_unsealed(&self) -> Option<PeriodView<2>> {
        None
    }
    fn entry(&self, _id: EntryId) -> Option<EntryView<2>> {
        None
    }
    fn reversed_by(&self, _id: EntryId) -> Option<EntryId> {
        None
    }
    fn hold(&self, _id: EntryId) -> Option<HoldView<2>> {
        None
    }
    fn outstanding_hold_debits(&self, _handle: AccountHandle, _currency: Currency) -> Amount<2> {
        Amount::ZERO
    }
    fn outstanding_hold_credits(&self, _handle: AccountHandle, _currency: Currency) -> Amount<2> {
        Amount::ZERO
    }
    fn balance(&self, _key: BalanceKey) -> Balance<2> {
        Balance::ZERO
    }
    fn clearing_by_key(&self, _key: &IdempotencyKey) -> Option<ClearingView<2>> {
        None
    }
    fn clearing_at(&self, _position: u64) -> Option<ClearingView<2>> {
        None
    }
    fn applied(&self, _posting: PostingRef) -> Amount<2> {
        Amount::ZERO
    }
    fn all_balances(&self) -> Vec<(BalanceKey, Balance<2>)> {
        Vec::new()
    }
    fn postings_after(&self, _date: doubleentry::Date) -> Vec<Movement<2>> {
        Vec::new()
    }
    fn pnl_slices(&self, _through: doubleentry::Date) -> Vec<Slice<2>> {
        Vec::new()
    }
}

#[test]
fn register_refuses_when_the_handle_space_is_exhausted() {
    let outcome = execute(
        &Exhausted,
        &Op::Register {
            spec: AccountSpec::new("Assets:New", AccountKind::Asset, date!("2026-01-01")),
            actor: "setup".into(),
        },
    );
    assert_eq!(outcome.err(), Some(Rejection::TooManyAccounts));
}

// ── Immutability and version selection ───────────────────────────────────────

/// FR-015: an account's path and kind never change. `AccountChange` has
/// exactly these three variants; adding one (say, for the kind or the path)
/// breaks this match, which has no wildcard on purpose.
#[test]
fn account_change_has_no_variant_for_path_or_kind() {
    let changes = [
        AccountChange::OpenTo(None),
        AccountChange::Limit(BalanceLimit::Unlimited),
        AccountChange::Class(None),
    ];
    for change in changes {
        let field = match change {
            AccountChange::OpenTo(_) => "open_to",
            AccountChange::Limit(_) => "limit",
            AccountChange::Class(_) => "class",
            other => panic!("an unknown change {other:?}"),
        };
        assert!(!field.is_empty());
    }
}

#[test]
fn the_window_in_force_on_the_booking_date_decides() {
    let mut books = books();
    books
        .close_account("Assets:Cash", date!("2026-03-31"))
        .unwrap();
    // Recorded after the close, dated before it: the window in force on
    // 03-15 is open.
    books
        .record(sale("s-1", date!("2026-03-15"), "10.00"))
        .unwrap();
    books
        .reopen_account("Assets:Cash", date!("2026-05-01"))
        .unwrap();
    assert_eq!(
        rejected(books.record(sale("s-2", date!("2026-04-15"), "10.00"))),
        Rejection::AccountNotOpen {
            account: path("Assets:Cash"),
            date: date!("2026-04-15"),
        }
    );
    books
        .record(sale("s-3", date!("2026-05-02"), "10.00"))
        .unwrap();
}
