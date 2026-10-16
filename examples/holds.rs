//! A card authorisation held on a wallet, captured in part: the capture
//! settles 60.00 and releases the whole reservation in one entry, and the
//! hold refuses any second resolution.
//!
//! `cargo run --example holds`

use doubleentry::prelude::*;
use doubleentry::{HoldStatus, PostAmount};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut books = Journal::new(LedgerId::new("example.com/payments")?).as_actor("setup");
    books.declare_currency(EUR, 2, date!("2026-01-01"))?;
    books.open("Assets:Bank", AccountKind::Asset, date!("2026-01-01"))?;
    for wallet in ["Liabilities:Wallets:alice", "Liabilities:Card:clearing"] {
        books.open(wallet, AccountKind::Liability, date!("2026-01-01"))?;
    }
    books.set_limit(
        "Liabilities:Wallets:alice",
        BalanceLimit::NoDebitBalance,
        date!("2026-01-01"),
    )?;
    books.define_months(2026, 11..=11)?;
    books.record(
        Entry::new("top-up", date!("2026-11-02"))
            .debit("Assets:Bank", amount!("250.00"), EUR)
            .credit("Liabilities:Wallets:alice", amount!("250.00"), EUR),
    )?;

    // Authorise 100.00 until the end of the month.
    let hold = books.record(
        Entry::open_hold("auth-4711", date!("2026-11-20"))
            .expires(date!("2026-11-30"))
            .debit("Liabilities:Wallets:alice", amount!("100.00"), EUR)
            .credit("Liabilities:Card:clearing", amount!("100.00"), EUR),
    )?;
    println!("authorised: {}", books.hold(hold.id).ok_or("no hold")?);

    // Capture 60.00 of it: no postings to write, the core derives them.
    let capture = books.record(Entry::post_hold(
        "capture-4711",
        date!("2026-11-22"),
        hold.id,
        PostAmount::Partial(amount!("60.00")),
    ))?;
    let state = books.hold(hold.id).ok_or("no hold")?;
    println!("captured:   {state}");
    assert_eq!(
        state.status,
        HoldStatus::Posted {
            by: capture.id,
            amount: amount!("60.00")
        }
    );

    let wallet = "Liabilities:Wallets:alice";
    let settled = books.balance(wallet, EUR, &BalanceQuery::all())?;
    let pending = books.balance(wallet, EUR, &BalanceQuery::all().pending())?;
    println!(
        "wallet settled net {}, pending net {}",
        settled.net(),
        pending.net()
    );
    assert_eq!(settled.net(), amount!("-190.00"));
    assert!(pending.net().is_zero());

    // Resolved once: a void now names the capture that ended the hold.
    match books.record(Entry::void_hold("void-4711", date!("2026-11-23"), hold.id)) {
        Err(Error::Rejected(refusal)) => println!("refused:    {refusal}"),
        other => return Err(format!("a second resolution was not refused: {other:?}").into()),
    }
    books.audit()?;
    Ok(())
}
