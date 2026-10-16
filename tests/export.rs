//! The GoBD export view: verifiable flat rows, the Konto/Gegenkonto
//! pairing, minor units, master data in force on the booking date, scopes,
//! identical rows on every store and page size, and test-only sketches of
//! three adapters built from the rows alone.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

#[path = "common/fixture.rs"]
mod fixture;

use std::collections::BTreeMap;

use doubleentry::export::{
    DocumentRow, EntryRow, ExportRow, ExportScope, HoldKind, PairRow, PostingRow, RateRow,
    VerifyError, rebuild_entry, verify,
};
use doubleentry::store::{MemoryStore, Store};
use doubleentry::{
    AccountKind, Amount, Entry, Error, Journal, Layer, LedgerId, Side, TreeHead,
    currency::{EUR, JPY, USD},
    date,
    error::QueryError,
    log::{Hash, leaf_hash},
    record::RecordKind,
};
use fixture::{fixture, journal};

fn eur(text: &str) -> Amount {
    text.parse().unwrap()
}

fn rows(books: &Journal, scope: ExportScope) -> Vec<ExportRow> {
    books
        .export(scope, None)
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

/// An entry row and its child rows, by the entry's position.
#[derive(Default)]
struct Entries {
    entries: BTreeMap<u64, EntryRow>,
    postings: BTreeMap<u64, Vec<PostingRow>>,
    pairs: BTreeMap<u64, Vec<PairRow>>,
    documents: BTreeMap<u64, Vec<DocumentRow>>,
    rates: BTreeMap<u64, Vec<RateRow>>,
}

fn entries(rows: &[ExportRow]) -> Entries {
    let mut found = Entries::default();
    for row in rows {
        match row {
            ExportRow::Entry(row) => {
                found.entries.insert(row.position, row.clone());
            }
            ExportRow::Posting(row) => found
                .postings
                .entry(row.position)
                .or_default()
                .push(row.clone()),
            ExportRow::Pair(row) => found
                .pairs
                .entry(row.position)
                .or_default()
                .push(row.clone()),
            ExportRow::Document(row) => found
                .documents
                .entry(row.position)
                .or_default()
                .push(row.clone()),
            ExportRow::Rate(row) => found
                .rates
                .entry(row.position)
                .or_default()
                .push(row.clone()),
            _ => {}
        }
    }
    found
}

fn pairs_of(entries: &Entries, position: u64) -> Vec<(String, String, i128)> {
    entries.pairs[&position]
        .iter()
        .map(|pair| {
            (
                pair.debit_path.to_string(),
                pair.credit_path.to_string(),
                pair.minor_units,
            )
        })
        .collect()
}

// ── US6 ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn us6_every_entry_and_the_root_rebuild_from_the_rows() {
    let (books, _) = journal().await;
    let head = books.head();
    let all = rows(&books, ExportScope::All);
    let leaves: Vec<Hash> = all
        .iter()
        .filter_map(|row| match row {
            ExportRow::Leaf(leaf) => Some(leaf.leaf_hash),
            _ => None,
        })
        .collect();
    assert_eq!(leaves.len() as u64, head.size);
    let found = entries(&all);
    assert!(found.entries.len() > 10);
    for (position, entry) in &found.entries {
        let none = Vec::new();
        let bytes = rebuild_entry::<2>(
            entry,
            found.postings.get(position).unwrap_or(&none),
            found.documents.get(position).map_or(&[][..], |d| d),
            found.rates.get(position).map_or(&[][..], |r| r),
        )
        .unwrap();
        assert_eq!(bytes.first(), Some(&(RecordKind::Entry as u8)));
        assert_eq!(leaf_hash(&bytes), entry.leaf_hash);
        assert_eq!(leaves[*position as usize], entry.leaf_hash);
    }
    verify(all.clone(), &head).unwrap();

    // Tampering with any row is caught.
    let mut forged = all.clone();
    for row in &mut forged {
        if let ExportRow::Posting(posting) = row {
            posting.minor_units += 1;
            break;
        }
    }
    assert!(matches!(
        verify(forged, &head),
        Err(VerifyError::EntryMismatch { .. })
    ));
    let mut forged = all.clone();
    for row in &mut forged {
        if let ExportRow::Entry(entry) = row
            && entry.memo_text.is_some()
        {
            entry.memo_text = Some(String::from("another memo"));
        }
    }
    assert!(matches!(
        verify(forged, &head),
        Err(VerifyError::MemoMismatch { .. })
    ));
    let mut forged = all.clone();
    for row in &mut forged {
        if let ExportRow::Pair(pair) = row {
            pair.credit_index += 1;
            break;
        }
    }
    assert!(matches!(
        verify(forged, &head),
        Err(VerifyError::PairMismatch { .. })
    ));
    let other = TreeHead {
        size: head.size,
        root: Hash::from_bytes([1; 32]),
    };
    assert_eq!(
        verify(all, &other),
        Err(VerifyError::RootMismatch { size: head.size })
    );
}

/// 025 T034: hold rows carry the hold action, its target, the expiry and
/// a partial post's amount; the legs pair within the Pending layer, the
/// settled capture within the Settled one.
#[tokio::test]
async fn us6_hold_rows_carry_their_action() {
    let (books, fixture) = journal().await;
    let [hold, capture, second, void] = fixture.holds;
    let found = entries(&rows(&books, ExportScope::All));
    let row = |recorded: &doubleentry::Recorded| found.entries[&recorded.position].clone();
    let opened = row(&hold);
    assert_eq!(opened.hold_action, Some(HoldKind::Open));
    assert_eq!(opened.hold_target, None);
    assert_eq!(opened.hold_expires, Some(date!("2026-08-31")));
    let posted = row(&capture);
    assert_eq!(posted.hold_action, Some(HoldKind::Post));
    assert_eq!(posted.hold_target, Some(hold.id));
    assert_eq!(posted.post_amount_minor, Some(3_000));
    assert_eq!(posted.posting_count, 4);
    let voided = row(&void);
    assert_eq!(voided.hold_action, Some(HoldKind::Void));
    assert_eq!(voided.hold_target, Some(second.id));
    assert_eq!(voided.post_amount_minor, None);
    let layers = |recorded: &doubleentry::Recorded| {
        found.pairs[&recorded.position]
            .iter()
            .map(|pair| {
                (
                    pair.layer,
                    pair.debit_path.to_string(),
                    pair.credit_path.to_string(),
                    pair.minor_units,
                )
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        layers(&hold),
        [(
            Layer::Pending,
            "Assets:Cash".into(),
            "Assets:Bank".into(),
            8_000
        )]
    );
    assert_eq!(
        layers(&capture),
        [
            (
                Layer::Pending,
                "Assets:Bank".into(),
                "Assets:Cash".into(),
                8_000
            ),
            (
                Layer::Settled,
                "Assets:Cash".into(),
                "Assets:Bank".into(),
                3_000
            ),
        ]
    );
}

#[tokio::test]
async fn us6_an_erased_memo_still_rebuilds() {
    let (books, fixture) = journal().await;
    let all = rows(&books, ExportScope::All);
    let found = entries(&all);
    let erased = found
        .entries
        .values()
        .find(|entry| entry.entry == fixture.erased)
        .unwrap();
    assert!(erased.memo_commitment.is_some());
    assert_eq!((&erased.memo_text, erased.memo_salt), (&None, None));
    let kept = found
        .entries
        .values()
        .find(|entry| entry.key.as_str() == "jan-sale")
        .unwrap();
    assert_eq!(kept.memo_text.as_deref(), Some("january"));
    verify(all, &books.head()).unwrap();
}

#[tokio::test]
async fn us6_a_sales_invoice_pairs_against_revenue_and_vat() {
    let (books, fixture) = journal().await;
    let found = entries(&rows(&books, ExportScope::All));
    assert_eq!(
        pairs_of(&found, fixture.invoice.position),
        [
            ("Assets:Receivables".into(), "Income:Sales".into(), 10_000),
            ("Assets:Receivables".into(), "Liabilities:VAT".into(), 1_900)
        ]
    );
    let invoice = &found.entries[&fixture.invoice.position];
    assert_eq!(invoice.kind.as_ref().map(|k| k.as_str()), Some("invoice"));
    assert_eq!(invoice.period.as_str(), "2026-03");
    let document = &found.documents[&fixture.invoice.position][0];
    assert_eq!(
        (document.reference.as_str(), document.date, document.sha256),
        ("RE-2026-0001", Some(date!("2026-03-01")), Some([0x5a; 32]))
    );
}

/// The worked examples of FR-014 and a two-currency entry, as rows.
fn pairing() -> Journal {
    let mut books = Journal::new(LedgerId::new("example.com/pairs").unwrap());
    let on = date!("2026-01-01");
    books.declare_currency(EUR, 2, on).unwrap();
    books.declare_currency(USD, 2, on).unwrap();
    for path in ["Assets:Bank", "Assets:A", "Assets:B"] {
        books.open(path, AccountKind::Asset, on).unwrap();
    }
    for path in ["Income:InvoiceA", "Income:InvoiceB", "Income:X", "Income:Y"] {
        books.open(path, AccountKind::Income, on).unwrap();
    }
    books.define_months(2026, 3..=3).unwrap();
    let day = date!("2026-03-02");
    books
        .record(
            Entry::new("two-payments", day)
                .debit("Assets:Bank", eur("50.00"), EUR)
                .debit("Assets:Bank", eur("100.00"), EUR)
                .credit("Income:InvoiceA", eur("100.00"), EUR)
                .credit("Income:InvoiceB", eur("50.00"), EUR),
        )
        .unwrap();
    books
        .record(
            Entry::new("greedy", day)
                .debit("Assets:A", eur("70.00"), EUR)
                .debit("Assets:B", eur("30.00"), EUR)
                .credit("Income:X", eur("50.00"), EUR)
                .credit("Income:Y", eur("50.00"), EUR),
        )
        .unwrap();
    books
        .record(
            Entry::new("two-currencies", day)
                .debit("Assets:A", eur("10.00"), EUR)
                .debit("Assets:B", eur("5.00"), USD)
                .credit("Income:X", eur("5.00"), USD)
                .credit("Income:Y", eur("10.00"), EUR),
        )
        .unwrap();
    books
}

#[test]
fn us6_exact_matches_then_the_greedy_merge() {
    let books = pairing();
    let all = rows(&books, ExportScope::All);
    let found = entries(&all);
    let positions: Vec<u64> = found.entries.keys().copied().collect();
    let indexes = |position| -> Vec<(u16, u16, i128)> {
        found.pairs[&position]
            .iter()
            .map(|pair| (pair.debit_index, pair.credit_index, pair.minor_units))
            .collect()
    };
    assert_eq!(indexes(positions[0]), [(0, 3, 5_000), (1, 2, 10_000)]);
    assert_eq!(
        pairs_of(&found, positions[1]),
        [
            ("Assets:A".into(), "Income:X".into(), 5_000),
            ("Assets:A".into(), "Income:Y".into(), 2_000),
            ("Assets:B".into(), "Income:Y".into(), 3_000)
        ]
    );
    // No pair joins two currencies.
    for pair in &found.pairs[&positions[2]] {
        let postings = &found.postings[&positions[2]];
        let debit = &postings[usize::from(pair.debit_index)];
        let credit = &postings[usize::from(pair.credit_index)];
        assert_eq!(
            (debit.currency, credit.currency),
            (pair.currency, pair.currency)
        );
        assert_eq!(
            (debit.layer, credit.layer),
            (Layer::Settled, Layer::Settled)
        );
        assert_eq!((debit.side, credit.side), (Side::Debit, Side::Credit));
    }
    assert_eq!(found.pairs[&positions[2]].len(), 2);
    verify(all, &books.head()).unwrap();
}

#[tokio::test]
async fn us6_postings_carry_the_master_data_in_force_on_their_booking_date() {
    let (books, fixture) = journal().await;
    let found = entries(&rows(&books, ExportScope::All));
    let class = |position: u64| {
        found.postings[&position]
            .iter()
            .find(|posting| posting.path.to_string() == "Expenses:Travel")
            .and_then(|posting| posting.class.clone())
            .map(|class| class.as_str().to_owned())
    };
    assert_eq!(
        class(fixture.travel[0].position).as_deref(),
        Some("operating")
    );
    assert_eq!(
        class(fixture.travel[1].position).as_deref(),
        Some("capital")
    );
    let travel = &found.postings[&fixture.travel[0].position][0];
    assert_eq!(travel.kind, AccountKind::Expense);
}

#[tokio::test]
async fn us6_amounts_are_minor_units_of_the_currency_exponent() {
    let (books, _) = journal().await;
    let all = rows(&books, ExportScope::All);
    let yen: Vec<(i128, u8)> = all
        .iter()
        .filter_map(|row| match row {
            ExportRow::Posting(posting) if posting.currency == JPY => {
                Some((posting.minor_units, posting.exponent))
            }
            _ => None,
        })
        .collect();
    assert_eq!(yen, [(500, 0), (500, 0)]);
    let rate = all
        .iter()
        .find_map(|row| match row {
            ExportRow::Rate(rate) => Some((rate.base, rate.quote, rate.mantissa, rate.exponent)),
            _ => None,
        })
        .unwrap();
    assert_eq!(rate, (EUR, USD, 10_873, -4));
    // Seals total in minor units too.
    let totals: Vec<(i128, i128, u8)> = all
        .iter()
        .filter_map(|row| match row {
            ExportRow::SealTotal(total) => {
                Some((total.debit_minor, total.credit_minor, total.exponent))
            }
            _ => None,
        })
        .collect();
    assert_eq!(totals, [(100_000, 100_000, 2), (130_000, 130_000, 2)]);
}

async fn pages<S: Store<2>>(books: &doubleentry::Ledger<S>, limit: u32) -> Vec<ExportRow> {
    let head = books.head().await.unwrap();
    let mut export = books.export(ExportScope::All, Some(head)).await.unwrap();
    let mut found = Vec::new();
    while let Some(page) = export.next_page(limit).await.unwrap() {
        assert!(!page.is_empty() && page.len() <= limit as usize);
        found.extend(page);
    }
    found
}

async fn same_rows<S: Store<2>>(store: S) {
    let (books, _) = fixture(store).await;
    let (journal, _) = journal().await;
    let expected = rows(&journal, ExportScope::All);
    for limit in [1, 7, 1_000] {
        assert_eq!(pages(&books, limit).await, expected, "page size {limit}");
    }
}

#[tokio::test]
async fn us6_every_store_and_page_size_exports_the_same_rows() {
    same_rows(MemoryStore::new()).await;
    #[cfg(feature = "sqlite")]
    same_rows(
        doubleentry::store::SqlStore::sqlite("sqlite::memory:")
            .await
            .unwrap(),
    )
    .await;
}

#[tokio::test]
async fn us6_scopes_hold_their_entries_and_every_leaf() {
    let (books, fixture) = journal().await;
    let all = rows(&books, ExportScope::All);
    let count = |rows: &[ExportRow], kind: fn(&ExportRow) -> bool| {
        rows.iter().filter(|row| kind(row)).count()
    };
    let is_leaf = |row: &ExportRow| matches!(row, ExportRow::Leaf(_));
    let is_master = |row: &ExportRow| matches!(row, ExportRow::Account(_) | ExportRow::Currency(_));
    let is_seal = |row: &ExportRow| matches!(row, ExportRow::Seal(_));

    let march = rows(&books, ExportScope::period("2026-03"));
    let booked: Vec<_> = entries(&march)
        .entries
        .values()
        .map(|entry| entry.booking_date)
        .collect();
    assert!(!booked.is_empty());
    assert!(
        booked
            .iter()
            .all(|day| (date!("2026-03-01")..=date!("2026-03-31")).contains(day))
    );
    assert_eq!(count(&march, is_leaf), count(&all, is_leaf));
    assert_eq!(count(&march, is_master), count(&all, is_master));
    assert_eq!(count(&march, is_seal), 0);
    verify(march, &books.head()).unwrap();

    let january = rows(&books, ExportScope::period("2026-01"));
    assert_eq!(entries(&january).entries.len(), 1);
    assert_eq!(count(&january, is_seal), 1);

    let range = fixture.invoice.position..fixture.reversal.position;
    let slice = rows(&books, ExportScope::Positions(range.clone()));
    let positions: Vec<u64> = entries(&slice).entries.keys().copied().collect();
    assert!(positions.iter().all(|position| range.contains(position)));
    assert!(positions.contains(&fixture.invoice.position));
    assert_eq!(count(&slice, is_leaf), count(&all, is_leaf));

    assert!(matches!(
        books.export(ExportScope::period("2027-13"), None).err(),
        Some(Error::Query(QueryError::UnknownPeriod { .. }))
    ));
    assert!(matches!(
        books.export(ExportScope::period(" "), None).err(),
        Some(Error::Query(QueryError::InvalidLabel { .. }))
    ));
    let ahead = TreeHead {
        size: books.head().size + 1,
        root: books.head().root,
    };
    assert!(matches!(
        books.export(ExportScope::All, Some(ahead)).err(),
        Some(Error::Query(QueryError::PositionBeyondHead { .. }))
    ));
}

#[test]
fn us6_a_genesis_only_ledger_exports_one_leaf_row() {
    let books = Journal::new(LedgerId::new("example.com/empty").unwrap());
    let all = rows(&books, ExportScope::All);
    assert!(matches!(all.as_slice(), [ExportRow::Leaf(leaf)] if leaf.kind == RecordKind::Genesis));
    verify(all, &books.head()).unwrap();
}

#[tokio::test]
async fn us6_a_pinned_export_reads_its_prefix_only() {
    let (books, fixture) = journal().await;
    let head = books.head_at(fixture.invoice.position).unwrap();
    let prefix: Vec<ExportRow> = books
        .export(ExportScope::All, Some(head))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(
        entries(&prefix)
            .entries
            .keys()
            .all(|position| *position < head.size)
    );
    verify(prefix, &head).unwrap();
}

// ── US7: the view is enough for DATEV, GDPdU and SAF-T ─────────────────────

/// What a DATEV adapter is configured with: account numbers and the axis
/// its BU-Schlüssel comes from.
struct DatevConfig {
    accounts: BTreeMap<&'static str, u32>,
    tax_key_axis: &'static str,
}

/// One DATEV EXTF Buchungsstapel line, as an adapter would write it.
#[derive(Debug, PartialEq, Eq)]
struct DatevLine {
    umsatz: String,
    soll_haben: char,
    wkz: String,
    konto: u32,
    gegenkonto: u32,
    bu_schluessel: Option<String>,
    belegdatum: String,
    belegfeld1: Option<String>,
    buchungstext: String,
    festschreibung: bool,
}

/// An amount in minor units of `exponent`, with a decimal comma.
fn decimal(minor: i128, exponent: u8, separator: char) -> String {
    let digits = format!(
        "{:0>width$}",
        minor.unsigned_abs(),
        width = usize::from(exponent) + 1
    );
    let (int, frac) = digits.split_at(digits.len() - usize::from(exponent));
    let sign = if minor < 0 { "-" } else { "" };
    if frac.is_empty() {
        format!("{sign}{int}")
    } else {
        format!("{sign}{int}{separator}{frac}")
    }
}

fn adapter_sketch_datev(entries: &Entries, position: u64, config: &DatevConfig) -> Vec<DatevLine> {
    let entry = &entries.entries[&position];
    let postings = &entries.postings[&position];
    let document = entries
        .documents
        .get(&position)
        .and_then(|documents| documents.iter().find(|d| d.doc_type.as_str() == "invoice"));
    entries.pairs[&position]
        .iter()
        .map(|pair| {
            let credit = &postings[usize::from(pair.credit_index)];
            DatevLine {
                umsatz: decimal(pair.minor_units, pair.exponent, ','),
                soll_haben: 'S',
                wkz: pair.currency.to_string(),
                konto: config.accounts[pair.debit_path.to_string().as_str()],
                gegenkonto: config.accounts[pair.credit_path.to_string().as_str()],
                bu_schluessel: credit
                    .dimensions
                    .iter()
                    .find(|(axis, _)| axis.as_str() == config.tax_key_axis)
                    .map(|(_, value)| value.to_string()),
                belegdatum: {
                    let day = document.and_then(|d| d.date).unwrap_or(entry.booking_date);
                    format!("{:02}{:02}", day.day(), u8::from(day.month()))
                },
                belegfeld1: document.map(|d| d.reference.clone()),
                buchungstext: entry
                    .memo_text
                    .clone()
                    .or_else(|| entry.kind.as_ref().map(ToString::to_string))
                    .unwrap_or_default(),
                festschreibung: true,
            }
        })
        .collect()
}

#[tokio::test]
async fn adapter_sketch_datev_builds_a_buchungsstapel_line() {
    let (books, fixture) = journal().await;
    let found = entries(&rows(&books, ExportScope::All));
    let config = DatevConfig {
        accounts: BTreeMap::from([
            ("Assets:Receivables", 10_000),
            ("Income:Sales", 8_400),
            ("Liabilities:VAT", 1_776),
        ]),
        tax_key_axis: "tax_key",
    };
    let lines = adapter_sketch_datev(&found, fixture.invoice.position, &config);
    assert_eq!(
        lines[0],
        DatevLine {
            umsatz: String::from("100,00"),
            soll_haben: 'S',
            wkz: String::from("EUR"),
            konto: 10_000,
            gegenkonto: 8_400,
            bu_schluessel: None,
            belegdatum: String::from("0103"),
            belegfeld1: Some(String::from("RE-2026-0001")),
            buchungstext: String::from("invoice"),
            festschreibung: true,
        }
    );
    assert_eq!(
        (lines[1].umsatz.as_str(), lines[1].gegenkonto),
        ("19,00", 1_776)
    );
}

/// A GDPdU table: column names and types from the documented schema, one
/// row per posting, numbers with the column's accuracy.
fn adapter_sketch_gdpdu(
    posting: &PostingRow,
    axes: &[&str],
) -> (Vec<(String, String)>, Vec<String>) {
    let mut columns = vec![
        ("Position".to_owned(), "Numeric".to_owned()),
        ("Buchung".to_owned(), "AlphaNumeric".to_owned()),
        ("Konto".to_owned(), "AlphaNumeric".to_owned()),
        ("Kontoart".to_owned(), "AlphaNumeric".to_owned()),
        ("Kontotyp".to_owned(), "AlphaNumeric".to_owned()),
        ("SollHaben".to_owned(), "AlphaNumeric".to_owned()),
        (
            "Betrag".to_owned(),
            format!("Numeric Accuracy={}", posting.exponent),
        ),
        ("Waehrung".to_owned(), "AlphaNumeric".to_owned()),
        ("Ebene".to_owned(), "AlphaNumeric".to_owned()),
        ("Buchungsdatum".to_owned(), "Date".to_owned()),
    ];
    columns.extend(
        axes.iter()
            .map(|axis| ((*axis).to_owned(), "AlphaNumeric".to_owned())),
    );
    let mut row = vec![
        posting.position.to_string(),
        posting.entry.to_string(),
        posting.path.to_string(),
        posting.kind.to_string(),
        posting
            .class
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_default(),
        posting.side.to_string(),
        decimal(posting.minor_units, posting.exponent, ','),
        posting.currency.to_string(),
        posting.layer.to_string(),
        posting.booking_date.to_string(),
    ];
    row.extend(axes.iter().map(|axis| {
        posting
            .dimensions
            .iter()
            .find(|(name, _)| name.as_str() == *axis)
            .map(|(_, value)| value.to_string())
            .unwrap_or_default()
    }));
    (columns, row)
}

#[tokio::test]
async fn adapter_sketch_gdpdu_builds_a_table_row() {
    let (books, _) = journal().await;
    let found = entries(&rows(&books, ExportScope::All));
    let january = found
        .entries
        .values()
        .find(|entry| entry.key.as_str() == "jan-sale")
        .unwrap();
    let sale = &found.postings[&january.position][1];
    let (columns, row) = adapter_sketch_gdpdu(sale, &["cost_centre"]);
    assert_eq!(columns.len(), row.len());
    assert_eq!(columns[6].1, "Numeric Accuracy=2");
    assert_eq!(
        row[2..],
        [
            "Income:Sales",
            "income",
            "",
            "credit",
            "1000,00",
            "EUR",
            "settled",
            "2026-01-10",
            "A"
        ]
    );
}

/// One SAF-T general-ledger line (OECD 2.0), with an account-code map.
#[derive(Debug, PartialEq, Eq)]
struct SaftLine {
    transaction_id: String,
    record_id: String,
    account_id: String,
    debit_amount: Option<String>,
    credit_amount: Option<String>,
    currency_code: String,
    transaction_date: String,
    system_entry_date: Option<i64>,
    description: String,
}

fn adapter_sketch_saft(
    entry: &EntryRow,
    posting: &PostingRow,
    codes: &BTreeMap<&str, &str>,
) -> SaftLine {
    let amount = decimal(posting.minor_units, posting.exponent, '.');
    let (debit_amount, credit_amount) = match posting.side {
        Side::Debit => (Some(amount), None),
        Side::Credit => (None, Some(amount)),
    };
    SaftLine {
        transaction_id: entry.entry.to_string(),
        record_id: format!("{}-{}", entry.position, posting.index),
        account_id: codes
            .get(posting.path.to_string().as_str())
            .map_or_else(|| posting.path.to_string(), |code| (*code).to_owned()),
        debit_amount,
        credit_amount,
        currency_code: posting.currency.to_string(),
        transaction_date: entry.booking_date.to_string(),
        system_entry_date: entry.captured_at.map(|at| at.unix_seconds()),
        description: entry.memo_text.clone().unwrap_or_default(),
    }
}

#[tokio::test]
async fn adapter_sketch_saft_builds_a_general_ledger_line() {
    let (books, fixture) = journal().await;
    let found = entries(&rows(&books, ExportScope::All));
    let entry = &found.entries[&fixture.invoice.position];
    let codes = BTreeMap::from([("Assets:Receivables", "1400")]);
    let line = adapter_sketch_saft(entry, &found.postings[&fixture.invoice.position][0], &codes);
    assert_eq!(line.transaction_id, fixture.invoice.id.to_string());
    assert_eq!(line.account_id, "1400");
    assert_eq!(
        (line.debit_amount.as_deref(), line.credit_amount.as_deref()),
        (Some("119.00"), None)
    );
    assert_eq!(
        (line.currency_code.as_str(), line.transaction_date.as_str()),
        ("EUR", "2026-03-01")
    );
    let vat = adapter_sketch_saft(entry, &found.postings[&fixture.invoice.position][2], &codes);
    assert_eq!(
        (vat.account_id.as_str(), vat.credit_amount.as_deref()),
        ("Liabilities:VAT", Some("19.00"))
    );
}
