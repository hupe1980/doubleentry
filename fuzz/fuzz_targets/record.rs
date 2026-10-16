//! Arbitrary bytes as a log leaf, and as a seal balance-tree leaf.
//!
//! A leaf is `kind ‖ canonical(record)` for every record kind (genesis,
//! account, currency, policy, period, entry, clearing, seal); the decoder is
//! strict, so an accepted leaf re-encodes to exactly the input bytes. The
//! ledger's scale `P` changes how amounts decode, so the bytes are read at
//! the scales 0, 2 and 18. The same bytes are then read as a balance leaf
//! (`0x81 ‖ i128 debit ‖ i128 credit ‖ u64 postings`), which a
//! sealed-balance bundle carries; it, too, re-encodes to the input.
#![no_main]

use doubleentry::fuzz_api::decode_balance_leaf;
use doubleentry::log::balance_leaf_bytes;
use doubleentry::record::Record;
use libfuzzer_sys::fuzz_target;

fn leaf<const P: u8>(data: &[u8]) {
    if let Ok(record) = Record::<P>::decode(data) {
        assert_eq!(
            record.to_leaf_bytes(),
            data,
            "Record<{P}> {:?} does not re-encode to its leaf",
            record.kind()
        );
        // The hash is total over a decoded record.
        let _ = record.leaf_hash();
    }
}

fn balance<const P: u8>(data: &[u8]) {
    if let Ok(balance) = decode_balance_leaf::<P>(data) {
        assert_eq!(
            balance_leaf_bytes(&balance).as_slice(),
            data,
            "balance leaf at scale {P} does not re-encode"
        );
    }
}

fuzz_target!(|data: &[u8]| {
    leaf::<0>(data);
    leaf::<2>(data);
    leaf::<18>(data);
    balance::<0>(data);
    balance::<2>(data);
    balance::<18>(data);
});
