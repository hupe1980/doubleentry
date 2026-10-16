//! The reporting and export fixture, built through a `Ledger`
//! on any store, and its `Journal` replica.
//!
//! EUR and USD (exponent 2) and JPY (exponent 0); monthly periods
//! `2026-01` … `2026-12`, January and February sealed; a backdated entry;
//! value dates after booking dates; the dimension `cost_centre` with `A`,
//! `B` and absent; a reversal; a document and a rate; a memo kept and one
//! erased; an account whose class changes from 2026-06-01; an invoice, a
//! payment, a clearing between them and its reset; in August a cash hold
//! posted in part and one voided.
// Each test file uses a different subset of the fixture.
#![allow(dead_code)]

use doubleentry::store::Store;
use doubleentry::{
    AccountKind, AccountSpec, Amount, Currency, Document, Entry, EntryId, Journal, Ledger,
    LedgerId, MemoSalt, PostAmount, Posting, PostingRef, Rate, Recorded,
    currency::{EUR, JPY, USD},
    date,
};

/// What the tests need to know about the fixture.
#[derive(Debug, Clone)]
pub struct Fixture {
    /// The March sale booked 2026-03-31, value-dated 2026-04-02.
    pub month_end_sale: Recorded,
    /// The sale booked 2026-03-20.
    pub late_sale: Recorded,
    /// The sale booked 2026-03-15, recorded after `late_sale`.
    pub backdated_sale: Recorded,
    /// The reversal of `late_sale`, booked 2026-03-25.
    pub reversal: Recorded,
    /// The invoice: receivables D 119.00 / sales C 100.00 / VAT C 19.00.
    pub invoice: Recorded,
    /// The payment of 100.00 against the receivables, 2026-04-01.
    pub payment: Recorded,
    /// The clearing of 100.00 of the payment against the invoice.
    pub clearing: u64,
    /// Its reset, dated 2026-04-10.
    pub reset: u64,
    /// The entry whose memo was erased.
    pub erased: EntryId,
    /// The travel expenses booked in March and July.
    pub travel: [Recorded; 2],
    /// The cash hold of 80.00 (expiring 2026-08-31), its partial post of
    /// 30.00, the hold of 20.00 and its void.
    pub holds: [Recorded; 4],
}

fn eur(text: &str) -> Amount {
    text.parse().unwrap()
}

/// A sale of `amount` in `currency` from the bank to sales.
fn sale(key: &str, booked: doubleentry::Date, amount: Amount, currency: Currency) -> Entry {
    Entry::new(key, booked)
        .debit("Assets:Bank", amount, currency)
        .credit("Income:Sales", amount, currency)
}

/// Builds the fixture in `store`.
pub async fn fixture<S: Store<2>>(store: S) -> (Ledger<S>, Fixture) {
    let books = Ledger::new(store, LedgerId::new("example.com/acme").unwrap())
        .await
        .unwrap()
        .as_actor("setup")
        .with_salt_source(|| MemoSalt([7; 16]));
    let on = date!("2026-01-01");
    for (code, exponent) in [(EUR, 2), (USD, 2), (JPY, 0)] {
        books.declare_currency(code, exponent, on).await.unwrap();
    }
    for (path, kind) in [
        ("Assets:Bank", AccountKind::Asset),
        ("Assets:Cash", AccountKind::Asset),
        ("Assets:Receivables", AccountKind::Asset),
        ("Income:Sales", AccountKind::Income),
        ("Liabilities:VAT", AccountKind::Liability),
        ("Expenses:Rent", AccountKind::Expense),
        ("Equity:Retained", AccountKind::Equity),
    ] {
        books.open(path, kind, on).await.unwrap();
    }
    books
        .register(AccountSpec::new("Expenses:Travel", AccountKind::Expense, on).class("operating"))
        .await
        .unwrap();
    books
        .set_class("Expenses:Travel", Some("capital"), date!("2026-06-01"))
        .await
        .unwrap();
    books.define_months(2026, 1..=12).await.unwrap();
    let record = |entry: Entry| async { books.record(entry).await.unwrap() };

    record(
        Entry::new("jan-sale", date!("2026-01-10"))
            .debit("Assets:Bank", eur("1000.00"), EUR)
            .posting(
                Posting::credit("Income:Sales", eur("1000.00"), EUR).dimension("cost_centre", "A"),
            )
            .memo("january"),
    )
    .await;
    record(
        Entry::new("feb-rent", date!("2026-02-03"))
            .value_date(date!("2026-02-05"))
            .posting(
                Posting::debit("Expenses:Rent", eur("300.00"), EUR).dimension("cost_centre", "B"),
            )
            .credit("Assets:Bank", eur("300.00"), EUR),
    )
    .await;
    books.seal_period("2026-01").await.unwrap();
    books.seal_period("2026-02").await.unwrap();

    let month_end_sale = record(
        sale("mar-31", date!("2026-03-31"), eur("100.00"), EUR).value_date(date!("2026-04-02")),
    )
    .await;
    let late_sale = record(
        Entry::new("mar-20", date!("2026-03-20"))
            .debit("Assets:Bank", eur("50.00"), EUR)
            .posting(
                Posting::credit("Income:Sales", eur("50.00"), EUR).dimension("cost_centre", "A"),
            ),
    )
    .await;
    let backdated_sale = record(
        Entry::new("mar-15", date!("2026-03-15"))
            .debit("Assets:Bank", eur("20.00"), EUR)
            .posting(
                Posting::credit("Income:Sales", eur("20.00"), EUR).dimension("cost_centre", "B"),
            ),
    )
    .await;
    let invoice = record(
        Entry::new("invoice-1", date!("2026-03-01"))
            .kind("invoice")
            .debit("Assets:Receivables", eur("119.00"), EUR)
            .credit("Income:Sales", eur("100.00"), EUR)
            .credit("Liabilities:VAT", eur("19.00"), EUR)
            .document(
                Document::new("invoice", "RE-2026-0001")
                    .unwrap()
                    .dated(date!("2026-03-01"))
                    .with_sha256([0x5a; 32]),
            ),
    )
    .await;
    record(
        sale("usd-sale", date!("2026-03-05"), eur("50.00"), USD)
            .rate(Rate::parse(EUR, USD, "1.0873").unwrap()),
    )
    .await;
    record(sale(
        "jpy-sale",
        date!("2026-03-06"),
        "500".parse().unwrap(),
        JPY,
    ))
    .await;
    let erased = record(
        Entry::new("erase-me", date!("2026-03-07"))
            .debit("Assets:Cash", eur("1.00"), EUR)
            .credit("Income:Sales", eur("1.00"), EUR)
            .memo("a customer's name"),
    )
    .await;
    books.erase_memo(erased.id).await.unwrap();
    let travel_march = record(
        Entry::new("travel-mar", date!("2026-03-10"))
            .debit("Expenses:Travel", eur("40.00"), EUR)
            .credit("Assets:Cash", eur("40.00"), EUR),
    )
    .await;
    let reversal = books
        .reversal(late_sale.id, "mar-20-reversal", date!("2026-03-25"))
        .await
        .unwrap();
    let reversal = record(reversal).await;
    let payment = record(
        Entry::new("payment-1", date!("2026-04-01"))
            .debit("Assets:Bank", eur("100.00"), EUR)
            .credit("Assets:Receivables", eur("100.00"), EUR),
    )
    .await;
    let clearing = books
        .clear(
            "clear-1",
            PostingRef::at(invoice.id, 0),
            PostingRef::at(payment.id, 1),
            eur("100.00"),
            date!("2026-04-01"),
        )
        .await
        .unwrap();
    let reset = books
        .reset_clearing("reset-1", clearing, date!("2026-04-10"))
        .await
        .unwrap();
    let travel_july = record(
        Entry::new("travel-jul", date!("2026-07-10"))
            .debit("Expenses:Travel", eur("60.00"), EUR)
            .credit("Assets:Cash", eur("60.00"), EUR),
    )
    .await;
    let withdrawal = record(
        Entry::open_hold("hold-1", date!("2026-08-03"))
            .expires(date!("2026-08-31"))
            .debit("Assets:Cash", eur("80.00"), EUR)
            .credit("Assets:Bank", eur("80.00"), EUR),
    )
    .await;
    let capture = record(Entry::post_hold(
        "hold-1-post",
        date!("2026-08-05"),
        withdrawal.id,
        PostAmount::Partial(eur("30.00")),
    ))
    .await;
    let second = record(
        Entry::open_hold("hold-2", date!("2026-08-06"))
            .debit("Assets:Cash", eur("20.00"), EUR)
            .credit("Assets:Bank", eur("20.00"), EUR),
    )
    .await;
    let void = record(Entry::void_hold(
        "hold-2-void",
        date!("2026-08-07"),
        second.id,
    ))
    .await;
    let fixture = Fixture {
        month_end_sale,
        late_sale,
        backdated_sale,
        reversal,
        invoice,
        payment,
        clearing,
        reset,
        erased: erased.id,
        travel: [travel_march, travel_july],
        holds: [withdrawal, capture, second, void],
    };
    (books, fixture)
}

/// The fixture in memory, its `Journal` replica, and what to know.
pub async fn journal() -> (Journal, Fixture) {
    let (books, fixture) = fixture(doubleentry::store::MemoryStore::new()).await;
    (books.to_journal().await.unwrap(), fixture)
}
