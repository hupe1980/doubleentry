//! Reports that line up: every single-line value honours width, fill and
//! alignment, and no precision ever cuts a value short; money also takes `+`
//! and sign-aware `0` padding and is never rounded. The composite single-line
//! types (`Balance`, `TreeHead`, `Recorded`, …) render once and pad the same
//! way.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use doubleentry::{
    AccountHandle, AccountKind, AccountPath, Amount, Currency, EntryId, IdempotencyKey, Label,
    Layer, LedgerId, Side, amount, currency::EUR, date, log::Hash,
};

/// Asserts the five formats of the table for `value`, whose plain text is
/// `plain`.
fn padded(value: &dyn std::fmt::Display, plain: &str) {
    let width = plain.chars().count() + 4;
    let left = format!("{plain}    ");
    let right = format!("    {plain}");
    let center = format!("  {plain}  ");
    let starred = format!("**{plain}**");
    assert_eq!(format!("{value:<width$}"), left, "{plain} left");
    assert_eq!(format!("{value:>width$}"), right, "{plain} right");
    assert_eq!(format!("{value:^width$}"), center, "{plain} center");
    assert_eq!(format!("{value:*^width$}"), starred, "{plain} fill");
    assert_eq!(format!("{value:.1}"), plain, "{plain} precision");
    assert_eq!(format!("{value:>2.1}"), plain, "{plain} narrow");
}

#[test]
fn every_single_line_value_pads_and_never_truncates() {
    let ledger = LedgerId::new("example.com/acme").unwrap();
    let key = IdempotencyKey::new("invoice-2026-0001").unwrap();
    let id = EntryId::derive(&ledger, &key);
    let hash = Hash::from_bytes([0xab; 32]);
    let cases: Vec<(Box<dyn std::fmt::Display>, String)> = vec![
        (Box::new(amount!("-70.00") as Amount), "-70.00".into()),
        (Box::new(EUR), "EUR".into()),
        (Box::new(Currency::new("XAU").unwrap()), "XAU".into()),
        (
            Box::new(AccountPath::parse("Assets:Cash").unwrap()),
            "Assets:Cash".into(),
        ),
        (Box::new(AccountHandle::new(7).unwrap()), "#7".into()),
        (Box::new(Label::new("region").unwrap()), "region".into()),
        (Box::new(ledger.clone()), "example.com/acme".into()),
        (Box::new(id), id.to_string()),
        (Box::new(key.clone()), "invoice-2026-0001".into()),
        (Box::new(hash), "ab".repeat(32)),
        (Box::new(Layer::Settled), "settled".into()),
        (Box::new(Layer::Pending), "pending".into()),
        (Box::new(Side::Debit), "debit".into()),
        (Box::new(Side::Credit), "credit".into()),
        (Box::new(AccountKind::Liability), "liability".into()),
        (Box::new(date!("2026-03-15")), "2026-03-15".into()),
    ];
    for (value, plain) in &cases {
        assert_eq!(value.to_string(), *plain);
        if plain != "2026-03-15" {
            padded(value.as_ref(), plain);
        }
    }
    assert_eq!(id.to_string().len(), 32);
}

#[test]
fn money_pads_like_an_integer_and_is_never_rounded() {
    let minus: Amount = amount!("-70.00");
    assert_eq!(format!("{minus:010}"), "-000070.00");
    assert_eq!(format!("{minus:>10}"), "    -70.00");
    assert_eq!(format!("{minus:<10}|"), "-70.00    |");
    assert_eq!(format!("{minus:.1}"), "-70.00");
    let plus: Amount = amount!("1190.05");
    assert_eq!(format!("{plus:+}"), "+1190.05");
    assert_eq!(format!("{plus:+012}"), "+00001190.05");
    assert_eq!(format!("{plus:.0}"), "1190.05");
    let yen: Amount<0> = amount!("1190");
    assert_eq!(format!("{yen:>6}"), "  1190");
    let fine: Amount<4> = amount!("0.0001");
    assert_eq!(format!("{fine:*>8}"), "**0.0001");
}

#[test]
fn columns_of_paths_and_amounts_line_up() {
    let rows = [
        ("Assets:Cash", amount!("1190.00")),
        ("Income:Sales:Consulting", amount!("-1190.00")),
        ("Expenses:Rent", amount!("5.5")),
    ];
    let lines: Vec<String> = rows
        .iter()
        .map(|(path, amount): &(&str, Amount)| {
            let path = AccountPath::parse(path).unwrap();
            format!("{path:<24}{amount:>10}")
        })
        .collect();
    assert_eq!(
        lines,
        [
            "Assets:Cash                1190.00",
            "Income:Sales:Consulting   -1190.00",
            "Expenses:Rent                 5.50",
        ]
    );
    assert!(lines.iter().all(|line| line.chars().count() == 34));
}

/// `report_display_honours_width` (025 T032; dx-audit F12/F13): the
/// single-line report values pad like every value; the multi-line reports
/// align their own columns and ignore width.
#[test]
fn report_display_honours_width() {
    use doubleentry::{
        BalanceQuery, Entry, Journal, PostingRef,
        report::{AssertAt, AssertionOutcome, BalanceAssertion, DateBasis},
    };

    let mut books = Journal::new(LedgerId::new("example.com/acme").unwrap());
    let on = date!("2026-01-01");
    books.declare_currency(EUR, 2, on).unwrap();
    books
        .open("Assets:Receivables", AccountKind::Asset, on)
        .unwrap();
    books.open("Income:Sales", AccountKind::Income, on).unwrap();
    books.define_months(2026, 3..=3).unwrap();
    let invoice = books
        .record(
            Entry::new("invoice", date!("2026-03-01"))
                .debit("Assets:Receivables", amount!("119.00"), EUR)
                .credit("Income:Sales", amount!("119.00"), EUR),
        )
        .unwrap();
    let refund = books
        .record(
            Entry::new("refund", date!("2026-03-02"))
                .debit("Income:Sales", amount!("100.00"), EUR)
                .credit("Assets:Receivables", amount!("100.00"), EUR),
        )
        .unwrap();
    books
        .clear(
            "clear",
            PostingRef::at(invoice.id, 0),
            PostingRef::at(refund.id, 1),
            amount!("100.00"),
            date!("2026-03-02"),
        )
        .unwrap();
    let all = BalanceQuery::all();
    let statement = books.statement("Assets:Receivables", EUR, &all).unwrap();
    padded(
        &statement.lines[0],
        "2026-03-01 5/0 debit 119.00, balance 119.00",
    );
    let open = books.open_items("Assets:Receivables", EUR, &all).unwrap();
    padded(
        &open[0],
        "2026-03-01 5/0 debit 119.00, applied 100.00, open 19.00",
    );
    let at = AssertAt::EndOfDay {
        date: date!("2026-03-31"),
        basis: DateBasis::Booking,
    };
    let held = books
        .check_assertion(&BalanceAssertion::new(
            "Assets:Receivables",
            EUR,
            at,
            amount!("19.00"),
        ))
        .unwrap();
    padded(&held, "held at 19.00");
    let failed = books
        .check_assertion(&BalanceAssertion::new(
            "Assets:Receivables",
            EUR,
            at,
            amount!("20.00"),
        ))
        .unwrap();
    let AssertionOutcome::Failed(failure) = &failed else {
        panic!()
    };
    let text = "Assets:Receivables EUR settled at the end of 2026-03-31 by booking date: expected 20.00, actual 19.00, shortfall 1.00";
    padded(failure, text);
    padded(&failed, text);

    // Multi-line reports: aligned columns, width ignored.
    let trial = books.trial_balance(&all).unwrap();
    assert_eq!(
        trial.to_string(),
        "trial balance, settled, by booking date\n\
         account             currency   debit  credit\n\
         Assets:Receivables  EUR       119.00  100.00\n\
         Income:Sales        EUR       100.00  119.00\n\
         total               EUR       219.00  219.00\n"
    );
    assert_eq!(format!("{trial:>200}"), trial.to_string());
    assert_eq!(
        statement.to_string(),
        "Assets:Receivables EUR, settled, by booking date, the beginning to the head\n\
         opening                            0.00\n\
         2026-03-01  5/0  debit   119.00  119.00\n\
         2026-03-02  6/1  credit  100.00   19.00\n\
         closing                           19.00\n"
    );
    let sheet = books.rollup(EUR, &all).unwrap();
    assert_eq!(
        sheet.to_string(),
        "Assets          19.00\n  Receivables   19.00\nIncome         -19.00\n  Sales        -19.00\n"
    );
    assert_eq!(format!("{sheet:^99}"), sheet.to_string());
}

/// 022 T030: the composite single-line types render once, then pad like
/// every value; the multi-line `AuditReport` aligns its own columns.
#[test]
fn composite_values_pad_and_never_truncate() {
    use doubleentry::{BalanceQuery, Entry, Error, Journal, Rejection};

    let mut books = Journal::new(LedgerId::new("example.com/acme").unwrap());
    let on = date!("2026-01-01");
    books.declare_currency(EUR, 2, on).unwrap();
    books.open("Assets:Cash", AccountKind::Asset, on).unwrap();
    books.open("Income:Sales", AccountKind::Income, on).unwrap();
    books.define_months(2026, 3..=3).unwrap();
    let sale = Entry::new("sale-1", date!("2026-03-15"))
        .debit("Assets:Cash", amount!("420.00"), EUR)
        .credit("Income:Sales", amount!("420.00"), EUR);
    let recorded = books.record(sale.clone()).unwrap();
    let replayed = books.record(sale).unwrap();
    let balance = books
        .balance("Assets:Cash", EUR, &BalanceQuery::all())
        .unwrap();
    let receipt = books.seal_period("2026-03").unwrap();
    let head = books.head();
    let rejection = Rejection::TooFewPostings { count: 1 };
    let error = Error::from(rejection.clone());

    let cases: Vec<(Box<dyn std::fmt::Display>, String)> = vec![
        (
            Box::new(balance),
            "dr 420.00 cr 0.00 net 420.00 (1 posting)".into(),
        ),
        (
            Box::new(head),
            format!("size {} root {}", head.size, head.root),
        ),
        (
            Box::new(recorded),
            format!("entry {} at position {}", recorded.id, recorded.position),
        ),
        (
            Box::new(replayed),
            format!(
                "entry {} at position {}, replayed",
                recorded.id, recorded.position
            ),
        ),
        (
            Box::new(receipt.clone()),
            format!("2026-03 sealed at position {}", receipt.position),
        ),
        (
            Box::new(receipt.seal.clone()),
            format!(
                "seal of 2026-03 (2026-03-01 to 2026-03-31) through position {}: 2 accounts, 1 balance tree",
                receipt.position - 1
            ),
        ),
        (
            Box::new(rejection),
            "[101] an entry needs at least two postings, found 1".into(),
        ),
        (
            Box::new(error),
            "rejected: [101] an entry needs at least two postings, found 1".into(),
        ),
    ];
    for (value, plain) in &cases {
        assert_eq!(value.to_string(), *plain);
        padded(value.as_ref(), plain);
    }

    let audited = books.audit().unwrap();
    let report = audited.to_string();
    let lines: Vec<&str> = report.lines().collect();
    assert_eq!(lines[0], "audit passed");
    assert_eq!(head.size, 7);
    assert_eq!(lines[1], "  records  7");
    assert_eq!(lines[2], "  entries  1");
    assert_eq!(lines[5], format!("  root     {}", head.root));
    assert_eq!(format!("{audited:>200}"), report, "a report ignores width");
}
