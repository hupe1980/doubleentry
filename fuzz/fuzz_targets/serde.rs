//! A selector byte plus JSON, deserialised into every public serde type.
//!
//! Asserts, per type: no panic; an accepted value serialises, deserialises
//! back to an equal value, and re-serialises byte-identically (its JSON form
//! is a fixed point). Deserialising runs the same checks as the constructors,
//! so an accepted value is one the constructors could have built. Generic
//! types are read at the scale 2, and `Amount` at 0 and 18 too.
//!
//! The first byte picks the type (modulo the list's length), so libFuzzer
//! spends its time on one type per input; the rest is the JSON. The list
//! below must name every public type that implements `Deserialize`:
//! `tests/serde_fuzz_guard.rs` (`serde_fuzz_target_lists_every_deserialize_type`)
//! fails when one is missing.
#![no_main]

use core::fmt::Debug;

use doubleentry::error::{LabelField, ScaledAmount, Stage, ValueError};
use doubleentry::log::{BalanceProof, ConsistencyProof, Hash, InclusionProof, SealedBalanceProof};
use doubleentry::record::HoldAction;
use doubleentry::report::{
    AssertAt, DateBasis, Deviation, Difference, OpenItem, Rollup, RollupNode, Statement,
    StatementLine, TrialBalance, TrialBalanceRow,
};
use doubleentry::store::BalanceKey;
use doubleentry::{
    AccountHandle, AccountKind, AccountPath, AccountRef, Amount, Balance, BalanceLimit,
    BalanceQuery, CaptureTime, Currency, Dimensions, Document, Entry, EntryId, HoldState,
    HoldStatus, IdempotencyKey, Label, Layer, LedgerId, LedgerPolicy, MemoSalt, PostAmount,
    Posting, PostingRef, Rate, Rejection, ResolutionKind, Side, TreeHead,
};
use libfuzzer_sys::fuzz_target;
use serde::{Serialize, de::DeserializeOwned};

fn roundtrip<T: Serialize + DeserializeOwned + PartialEq + Debug>(data: &[u8]) {
    let Ok(value) = serde_json::from_slice::<T>(data) else {
        return;
    };
    let name = core::any::type_name::<T>();
    let once = serde_json::to_vec(&value)
        .unwrap_or_else(|e| panic!("{name}: an accepted value does not serialise: {e}"));
    let back: T = serde_json::from_slice(&once).unwrap_or_else(|e| {
        panic!(
            "{name} did not re-read its own output {:?}: {e}",
            String::from_utf8_lossy(&once)
        )
    });
    assert_eq!(back, value, "{name}: the round trip changed the value");
    let twice = serde_json::to_vec(&back).expect("serialises");
    assert_eq!(
        String::from_utf8_lossy(&once),
        String::from_utf8_lossy(&twice),
        "{name} does not re-serialise identically"
    );
}

/// Every public `Deserialize` type, in the order the selector picks them.
const TYPES: &[fn(&[u8])] = &[
    // Values.
    roundtrip::<Amount<0>>,
    roundtrip::<Amount<2>>,
    roundtrip::<Amount<18>>,
    roundtrip::<Balance<2>>,
    roundtrip::<Currency>,
    roundtrip::<Rate>,
    roundtrip::<Side>,
    roundtrip::<Layer>,
    roundtrip::<AccountKind>,
    roundtrip::<BalanceLimit>,
    roundtrip::<AccountPath>,
    roundtrip::<AccountHandle>,
    roundtrip::<AccountRef>,
    roundtrip::<Label>,
    roundtrip::<LedgerId>,
    roundtrip::<IdempotencyKey>,
    roundtrip::<EntryId>,
    roundtrip::<MemoSalt>,
    roundtrip::<CaptureTime>,
    roundtrip::<Dimensions>,
    roundtrip::<Document>,
    roundtrip::<LedgerPolicy>,
    // Entries and holds.
    roundtrip::<PostAmount<2>>,
    roundtrip::<Posting<2>>,
    roundtrip::<PostingRef>,
    roundtrip::<Entry<2>>,
    roundtrip::<HoldAction<2>>,
    roundtrip::<HoldState<2>>,
    roundtrip::<HoldStatus<2>>,
    roundtrip::<ResolutionKind>,
    // Errors.
    roundtrip::<Rejection>,
    roundtrip::<ValueError>,
    roundtrip::<LabelField>,
    roundtrip::<ScaledAmount>,
    roundtrip::<Stage>,
    // Log and proofs.
    roundtrip::<Hash>,
    roundtrip::<TreeHead>,
    roundtrip::<InclusionProof>,
    roundtrip::<ConsistencyProof>,
    roundtrip::<BalanceProof<2>>,
    roundtrip::<SealedBalanceProof<2>>,
    // Reports and queries.
    roundtrip::<BalanceQuery>,
    roundtrip::<BalanceKey>,
    roundtrip::<AssertAt>,
    roundtrip::<DateBasis>,
    roundtrip::<Deviation>,
    roundtrip::<Difference<2>>,
    roundtrip::<OpenItem<2>>,
    roundtrip::<Rollup<2>>,
    roundtrip::<RollupNode<2>>,
    roundtrip::<Statement<2>>,
    roundtrip::<StatementLine<2>>,
    roundtrip::<TrialBalance<2>>,
    roundtrip::<TrialBalanceRow<2>>,
];

fuzz_target!(|data: &[u8]| {
    let Some((&selector, json)) = data.split_first() else {
        return;
    };
    TYPES[usize::from(selector) % TYPES.len()](json);
});
