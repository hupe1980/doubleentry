//! Arbitrary text as every value the crate parses from a string.
//!
//! `Amount<0>`, `Amount<2>` and `Amount<18>` (`parse` and `FromStr`),
//! `Currency`, `AccountPath`, `Label`, `LedgerId`, `IdempotencyKey`,
//! `EntryId`, `Hash` and `Rate` (between two fixed currencies). Asserts no
//! panic and, on accepted text, that `Display` parses back to an equal value
//! and is itself a fixed point; an accepted account path or label is in
//! Unicode NFC (the constructors normalise, so another spelling of the same
//! text is accepted as its NFC form).
#![no_main]

use core::fmt::{Debug, Display};
use core::str::FromStr;

use doubleentry::currency::{EUR, USD};
use doubleentry::log::Hash;
use doubleentry::{AccountPath, Amount, Currency, EntryId, IdempotencyKey, Label, LedgerId, Rate};
use libfuzzer_sys::fuzz_target;
use unicode_normalization::is_nfc;

/// `Display` parses back to `value` and renders identically.
fn displays<T, E>(value: &T, parse: impl Fn(&str) -> Result<T, E>)
where
    T: Display + PartialEq + Debug,
    E: Debug,
{
    let name = core::any::type_name::<T>();
    let shown = value.to_string();
    let back = parse(&shown)
        .unwrap_or_else(|e| panic!("{name}: Display {shown:?} does not parse back: {e:?}"));
    assert_eq!(
        &back, value,
        "{name}: Display {shown:?} parses to another value"
    );
    assert_eq!(
        back.to_string(),
        shown,
        "{name}: Display is not a fixed point"
    );
}

/// `FromStr` accepts what `parse` accepts, as the same value; on success the
/// value round-trips through `Display`.
fn from_str<T>(text: &str) -> Option<T>
where
    T: FromStr + Display + PartialEq + Debug,
    T::Err: Debug,
{
    let value = T::from_str(text).ok()?;
    displays(&value, T::from_str);
    Some(value)
}

fn amount<const P: u8>(text: &str) {
    let parsed = Amount::<P>::parse(text);
    assert_eq!(
        parsed.as_ref().ok(),
        Amount::<P>::from_str(text).as_ref().ok(),
        "Amount<{P}>: parse and FromStr disagree on {text:?}"
    );
    if let Ok(value) = parsed {
        displays(&value, Amount::<P>::parse);
    }
}

fuzz_target!(|data: &[u8]| {
    let Ok(text) = core::str::from_utf8(data) else {
        return;
    };
    amount::<0>(text);
    amount::<2>(text);
    amount::<18>(text);
    from_str::<Currency>(text);
    if let Some(path) = from_str::<AccountPath>(text) {
        let shown = path.to_string();
        assert!(
            is_nfc(&shown),
            "an accepted account path is not NFC: {shown:?}"
        );
        assert_eq!(AccountPath::parse(text).as_ref(), Ok(&path));
    }
    if let Some(label) = from_str::<Label>(text) {
        let shown = label.to_string();
        assert!(is_nfc(&shown), "an accepted label is not NFC: {shown:?}");
        assert_eq!(Label::new(text).as_ref(), Ok(&label));
    }
    from_str::<LedgerId>(text);
    from_str::<IdempotencyKey>(text);
    from_str::<EntryId>(text);
    from_str::<Hash>(text);
    if let Ok(rate) = Rate::parse(EUR, USD, text) {
        displays(&rate, |shown| Rate::parse(EUR, USD, shown));
    }
});
