//! Arbitrary bytes as a `c2sp.org/tlog-proof@v1` bundle (record and
//! sealed-balance kinds) and as a tile path.
//!
//! The bundle is read at the scales 2 (`parse`), 0 and 18
//! (`parse_with_precision`); an accepted bundle renders back to exactly the
//! input. Verifying it never panics: by hashes (`verify_inclusion`), against
//! a policy of the fixed log key, and by a `Verifier` that follows the log
//! with a fixed trusted head — the head of the ledger the seeds come from
//! (`fuzz/corpus/bundle/`). A proven record is the bundle's `extra`. A tile
//! path that parses renders back to itself.
#![no_main]

use std::str::FromStr;
use std::sync::OnceLock;

use doubleentry::TreeHead;
use doubleentry::log::Hash;
use doubleentry::transparency::{KeyName, Policy, ProofBundle, Proven, TileId, Verifier, Vkey};
use libfuzzer_sys::fuzz_target;

/// The log key of `example.com/ledgers/acme` from seed `[7; 32]`
/// (`tests/vectors/c2sp/keys/log-ed25519.txt`).
const LOG_VKEY: &str =
    "example.com/ledgers/acme+c3b6d994+AepKbGPinFIKvvVQexMuxfmVR3auvr57kkIe6mkURtIs";

/// The head the seed bundles were made at: a ledger with EUR, two accounts,
/// 2026's months, four January sales, January sealed and one February sale
/// (the `sealed()` ledger of `tests/transparency.rs`).
const TRUSTED_SIZE: u64 = 22;
const TRUSTED_ROOT: &str = "9ea91f504ddeeb7b21324684e40802dd00e563d26a36552d9f13147e6e8a9f14";

struct Fixed {
    policy: Policy,
    verifier: Verifier,
}

fn fixed() -> &'static Fixed {
    static FIXED: OnceLock<Fixed> = OnceLock::new();
    FIXED.get_or_init(|| {
        let key = Vkey::from_str(LOG_VKEY).expect("the vector key parses");
        let origin = KeyName::from_str("example.com/ledgers/acme").expect("a key name");
        let head = TreeHead {
            size: TRUSTED_SIZE,
            root: Hash::from_str(TRUSTED_ROOT).expect("a hash"),
        };
        Fixed {
            policy: Policy::log_only(key.clone()),
            verifier: Verifier::new()
                .log(key)
                .expect("one key is unambiguous")
                .pin(origin, head),
        }
    })
}

fn bundle<const P: u8>(data: &[u8], parsed: Option<ProofBundle<P>>) {
    let Some(bundle) = parsed else {
        return;
    };
    assert_eq!(bundle.to_bytes(), data, "a bundle does not render back");
    if let Ok(text) = core::str::from_utf8(data) {
        assert_eq!(bundle.to_string(), text, "Display is not the bytes");
    }
    let inclusion = bundle.verify_inclusion();
    if let Ok(Proven::Record(proven)) = &inclusion {
        assert_eq!(
            Some(proven.record.to_leaf_bytes().as_slice()),
            bundle.extra(),
            "the proven record is not the bundle's extra"
        );
        assert_eq!(proven.position, bundle.index());
    }
    let fixed = fixed();
    if let Ok(proven) = bundle.verify(&fixed.policy) {
        assert_eq!(
            Ok(&proven),
            inclusion.as_ref(),
            "verify and verify_inclusion disagree"
        );
    }
    if let Ok(proven) = fixed.verifier.verify(&bundle) {
        assert_eq!(
            Ok(&proven),
            inclusion.as_ref(),
            "Verifier and verify_inclusion disagree"
        );
    }
    // Following the bundle's checkpoint without a consistency proof: an
    // outcome, never a panic.
    let _ = fixed.verifier.clone().update(bundle.checkpoint(), None);
}

fuzz_target!(|data: &[u8]| {
    bundle::<2>(data, ProofBundle::parse(data).ok());
    bundle::<0>(data, ProofBundle::<0>::parse_with_precision(data).ok());
    bundle::<18>(data, ProofBundle::<18>::parse_with_precision(data).ok());
    if let Ok(text) = core::str::from_utf8(data)
        && let Ok(tile) = TileId::parse(text)
    {
        assert_eq!(tile.path(), text, "a tile path does not render back");
        assert_eq!(tile.to_string(), text, "Display is not the path");
        let _ = tile.entry_bundle();
        let _ = tile.needs_size();
        let _ = tile.leaf_range(TRUSTED_SIZE);
        let _ = tile.node_ids(TRUSTED_SIZE);
        let _ = tile.check_within(TRUSTED_SIZE);
    }
});
