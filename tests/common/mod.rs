//! Deterministic fixtures shared by the rule-core tests.
// Each test file uses a different subset of these fixtures.
#![allow(dead_code)]

use doubleentry::{
    AccountKind, Amount, BalanceLimit, Date, Entry, Error, Journal, LedgerId, MemoSalt, Rejection,
    currency::EUR, date,
};

/// The ledger every fixture books into.
pub const LEDGER: &str = "example.com/acme";

/// The fixtures' ledger id.
pub fn ledger() -> LedgerId {
    LedgerId::new(LEDGER).unwrap()
}

/// An empty journal (genesis only) with a fixed memo salt and actor `setup`.
pub fn journal() -> Journal {
    Journal::new(ledger())
        .with_salt_source(|| MemoSalt([7; 16]))
        .as_actor("setup")
}

/// The first day of a month of 2026.
pub fn month_start(month: u8) -> Date {
    Date::from_calendar_date(2026, time::Month::try_from(month).unwrap(), 1).unwrap()
}

/// The last day of a month of 2026.
pub fn month_end(month: u8) -> Date {
    let month = time::Month::try_from(month).unwrap();
    Date::from_calendar_date(2026, month, month.length(2026)).unwrap()
}

/// A journal with EUR (exponent 2), the accounts `Assets:Cash`,
/// `Income:Sales`, `Liabilities:Wallet` (limited `NoDebitBalance`),
/// `Expenses:Rent` and `Equity:Retained`, all open from 2026-01-01, and the
/// monthly periods `2026-01` … `2026-12`.
pub fn books() -> Journal {
    let mut books = journal();
    books.declare_currency(EUR, 2, date!("2026-01-01")).unwrap();
    books
        .open("Assets:Cash", AccountKind::Asset, date!("2026-01-01"))
        .unwrap();
    books
        .open("Income:Sales", AccountKind::Income, date!("2026-01-01"))
        .unwrap();
    books
        .register(
            doubleentry::AccountSpec::new(
                "Liabilities:Wallet",
                AccountKind::Liability,
                date!("2026-01-01"),
            )
            .limit(BalanceLimit::NoDebitBalance),
        )
        .unwrap();
    books
        .open("Expenses:Rent", AccountKind::Expense, date!("2026-01-01"))
        .unwrap();
    books
        .open("Equity:Retained", AccountKind::Equity, date!("2026-01-01"))
        .unwrap();
    for month in 1..=12 {
        books
            .define_period(
                format!("2026-{month:02}"),
                month_start(month),
                month_end(month),
            )
            .unwrap();
    }
    books
}

/// An amount at scale 2.
pub fn eur(text: &str) -> Amount {
    text.parse().unwrap()
}

/// A sale: debit `Assets:Cash`, credit `Income:Sales`.
pub fn sale(key: &str, on: Date, amount: &str) -> Entry {
    Entry::new(key, on)
        .debit("Assets:Cash", eur(amount), EUR)
        .credit("Income:Sales", eur(amount), EUR)
}

/// The rejection of a refused operation; panics on anything else.
#[track_caller]
pub fn rejected<T: core::fmt::Debug>(result: Result<T, Error>) -> Rejection {
    match result {
        Err(Error::Rejected(rejection)) => rejection,
        other => panic!("expected a rejection, got {other:?}"),
    }
}

/// The code of a refused operation's rejection.
#[track_caller]
pub fn code<T: core::fmt::Debug>(result: Result<T, Error>) -> u16 {
    rejected(result).code()
}

/// What an honest store holding a journal holds: the projection of its
/// replayed log, every table, so audits can be run against honest and
/// tampered claims.
pub struct Claims;

impl Claims {
    /// The projection of a journal's log.
    pub fn of(journal: &Journal) -> doubleentry::store::StoreProjection {
        let mut replay = doubleentry::store::Replayer::<2>::new(journal.id());
        for leaf in journal.leaves() {
            replay.feed(leaf).unwrap();
        }
        replay.projection()
    }
}

/// Replaces cell `cell` of row `row` of `part` in `claims`; returns the row
/// as it was.
pub fn tamper_cell(
    claims: &mut doubleentry::store::StoreProjection,
    part: doubleentry::store::ProjectionPart,
    row: usize,
    cell: usize,
    value: doubleentry::store::Cell,
) -> doubleentry::store::ProjectionRow {
    let rows = claims.rows_mut(part);
    let before = rows[row].clone();
    let mut cells = before.cells().to_vec();
    cells[cell] = value;
    rows[row] = doubleentry::store::ProjectionRow::new(cells);
    before
}

/// Replays owned leaves (with side data taken from `journal` where the
/// position still holds the same bytes) and compares the result with
/// `claims`.
pub fn audit_leaves(
    journal: &Journal,
    leaves: &[Vec<u8>],
    claims: &doubleentry::store::StoreProjection,
) -> Result<doubleentry::AuditReport, doubleentry::error::AuditFailure> {
    let side: Vec<_> = journal
        .leaves()
        .map(|leaf| (leaf.bytes.to_vec(), leaf.memo.cloned(), leaf.fingerprint))
        .collect();
    let mut replay = doubleentry::store::Replayer::<2>::new(journal.id());
    for (position, bytes) in leaves.iter().enumerate() {
        let (memo, fingerprint) = side
            .get(position)
            .filter(|(original, _, _)| original == bytes)
            .map_or((None, None), |(_, memo, fingerprint)| {
                (memo.clone(), *fingerprint)
            });
        replay.feed(doubleentry::log::ReplayLeaf {
            position: position as u64,
            bytes,
            memo: memo.as_ref(),
            fingerprint,
        })?;
    }
    replay.finish(claims)
}

/// A journal's leaf bytes.
pub fn leaf_bytes(journal: &Journal) -> Vec<Vec<u8>> {
    journal.leaves().map(|leaf| leaf.bytes.to_vec()).collect()
}
