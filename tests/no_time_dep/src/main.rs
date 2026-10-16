//! `date!` and `amount!` without a `time` dependency of this crate's own.
use doubleentry::{Amount, Date, amount, date};

const OPENED: Date = date!("2026-01-01");
const FEE: Amount = amount!("2.50");

fn main() {
    println!("{OPENED} {FEE}");
}
