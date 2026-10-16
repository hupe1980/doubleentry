//! Arbitrary text as a `tlog-policy`.
//!
//! `Policy`'s `Display` is the canonical form (comments and blank lines
//! dropped, `any`/`all` thresholds written as numbers, items in kind order),
//! so on accepted input the invariant is the canonical round trip: the
//! rendering parses back to an equal policy and renders to itself. `FromStr`
//! agrees with `parse`, and judging a fixed signed checkpoint never panics.
#![no_main]

use std::str::FromStr;
use std::sync::OnceLock;

use doubleentry::transparency::{Policy, SignedCheckpoint};
use libfuzzer_sys::fuzz_target;

/// `tests/vectors/c2sp/witness/cosignatures/w1-plain.txt` (`cosigned_b64`):
/// a checkpoint of `example.com/ledgers/acme` at size 13, signed by its log
/// and cosigned by `witness.example/w1`.
const COSIGNED: &str = "example.com/ledgers/acme\n13\nqO9IRMjh1bpJyBHNuG6VeR9dMsp9lwmv2ij99l6UmlM=\n\n\
\u{2014} example.com/ledgers/acme w7bZlC5mfA06DpTi62mP0qMuD1FJW9mMCKfYVVpv/OlJLW9mUhkt2ZRdTgmGbyZqDPglkLsNrAPqKjiyB+w+8Jkh1gA=\n\
\u{2014} witness.example/w1 JLXZ/gAAAABq0WkA4rwWFMzyd7STXAxOKu4DRomOjV0W7l4rc0Cr3E5WCjdBVc0rkG+GZo6vVe/1wNBJ1PfdAleTW7+mPZ+P73ouCQ==\n";

fn checkpoint() -> &'static SignedCheckpoint {
    static CHECKPOINT: OnceLock<SignedCheckpoint> = OnceLock::new();
    CHECKPOINT.get_or_init(|| {
        SignedCheckpoint::parse(COSIGNED.as_bytes()).expect("the vector checkpoint parses")
    })
}

fuzz_target!(|data: &[u8]| {
    let Ok(text) = core::str::from_utf8(data) else {
        return;
    };
    let parsed = Policy::parse(text);
    assert_eq!(
        parsed.is_ok(),
        Policy::from_str(text).is_ok(),
        "FromStr and parse disagree"
    );
    let Ok(policy) = parsed else {
        return;
    };
    let rendered = policy.to_string();
    let again = Policy::parse(&rendered)
        .unwrap_or_else(|e| panic!("the rendering {rendered:?} does not parse: {e}"));
    assert_eq!(again, policy, "the rendering parses to another policy");
    assert_eq!(
        again.to_string(),
        rendered,
        "the rendering is not a fixed point"
    );
    let _ = policy.verify(checkpoint());
});
