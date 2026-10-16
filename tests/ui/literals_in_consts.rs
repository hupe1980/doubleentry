//! The literal macros in `const` items and in code generic over the scale.
use doubleentry::{Amount, Date, amount, date};

const FEE: Amount = amount!("2.50");
const D: Date = date!("2026-03-15");
const FINE: Amount<4> = amount!("0.0001");

fn twice<const P: u8>() -> Amount<P> {
    let one: Amount<P> = amount!("1");
    match one.checked_add(one) {
        Ok(two) => two,
        Err(_) => Amount::ZERO,
    }
}

fn main() {
    assert_eq!(FEE.to_string(), "2.50");
    assert_eq!(D.to_string(), "2026-03-15");
    assert_eq!(FINE.to_string(), "0.0001");
    assert_eq!(twice::<2>().to_string(), "2.00");
    assert_eq!(twice::<0>().to_string(), "2");
}
