//! The quick-start `Journal` example, run as a test.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use doubleentry::{AccountKind, Entry, Error, Journal, LedgerId, amount, currency::EUR, date};

#[test]
fn the_design_example_records_and_proves() -> Result<(), Error> {
    let books = Journal::new(LedgerId::new("example.com/acme")?); // P = 2
    let mut books = books.as_actor("setup"); // actor for master data
    books.declare_currency(EUR, 2, date!("2026-01-01"))?;
    books.open("Assets:Cash", AccountKind::Asset, date!("2026-01-01"))?;
    books.open("Income:Sales", AccountKind::Income, date!("2026-01-01"))?;
    books.define_period("2026-03", date!("2026-03-01"), date!("2026-03-31"))?;

    let rec = books.record(
        Entry::new("invoice-2026-0001", date!("2026-03-15"))
            .debit("Assets:Cash", amount!("1190.00"), EUR)
            .credit("Income:Sales", amount!("1190.00"), EUR),
    )?; // Ok(Recorded) | Err(Error::Rejected(r))
    let proof = books.prove_entry(rec.id)?; // verifies against books.head()

    let position = usize::try_from(rec.position).unwrap();
    let leaf = books.leaves().nth(position).unwrap();
    proof.verify_record::<2>(&books.head(), leaf.bytes).unwrap();
    assert_eq!(books.actor(), "setup");
    assert!(!rec.replayed);
    books.audit()?;
    Ok(())
}
