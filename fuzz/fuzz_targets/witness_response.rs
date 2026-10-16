//! An arbitrary witness reply — status, content type, body — read against a
//! fixed submitted checkpoint and witness key, and the body as an
//! `add-checkpoint` request.
//!
//! The checkpoint is `example.com/ledgers/acme` at size 13, signed by the
//! log key of the interop vectors (seed `[7; 32]`); the witness is
//! `witness.example/w1` (`tests/vectors/c2sp/witness/replies/200.txt`).
//! `WitnessReply::read` never panics and gives either the witness's verified
//! cosignature, whose line is one of the body's lines, or one typed error. A
//! body that parses as a request renders back to exactly itself.
//!
//! Input layout (`arbitrary::Unstructured`): a `u16` status, an
//! `Option<String>` content type (its length byte comes from the end of the
//! input), then the body: every remaining byte, as is.
#![no_main]

use std::str::FromStr;
use std::sync::OnceLock;

use arbitrary::{Result, Unstructured};
use doubleentry::transparency::{AddCheckpoint, SignedCheckpoint, Vkey, WitnessReply};
use libfuzzer_sys::fuzz_target;

/// `tests/vectors/c2sp/notes/checkpoint-size-13.txt` (`note_b64`).
const CHECKPOINT: &str = "example.com/ledgers/acme\n13\nqO9IRMjh1bpJyBHNuG6VeR9dMsp9lwmv2ij99l6UmlM=\n\n\
\u{2014} example.com/ledgers/acme w7bZlC5mfA06DpTi62mP0qMuD1FJW9mMCKfYVVpv/OlJLW9mUhkt2ZRdTgmGbyZqDPglkLsNrAPqKjiyB+w+8Jkh1gA=\n";

/// `tests/vectors/c2sp/keys/witness-cosignature.txt`.
const WITNESS: &str = "witness.example/w1+24b5d9fe+BP0XJDhaoMdbZPt4zWAvodmR/ev3axPFjtcC6sg16fYY";

fn fixed() -> &'static (SignedCheckpoint, Vkey) {
    static FIXED: OnceLock<(SignedCheckpoint, Vkey)> = OnceLock::new();
    FIXED.get_or_init(|| {
        (
            SignedCheckpoint::parse(CHECKPOINT.as_bytes()).expect("the vector checkpoint parses"),
            Vkey::from_str(WITNESS).expect("the vector witness key parses"),
        )
    })
}

#[derive(Debug)]
struct Reply<'a> {
    status: u16,
    content_type: Option<String>,
    body: &'a [u8],
}

impl<'a> Reply<'a> {
    fn read(data: &'a [u8]) -> Result<Self> {
        let mut u = Unstructured::new(data);
        Ok(Self {
            status: u.arbitrary()?,
            content_type: u.arbitrary()?,
            body: u.take_rest(),
        })
    }
}

fuzz_target!(|data: &[u8]| {
    let Ok(reply) = Reply::read(data) else {
        return;
    };
    let (checkpoint, witness) = fixed();
    let read = WitnessReply::read(
        reply.status,
        reply.content_type.as_deref(),
        reply.body,
        checkpoint,
        witness,
    );
    if let Ok(cosignature) = read {
        assert_eq!(reply.status, 200, "a cosignature from a non-200 reply");
        assert_eq!(cosignature.name(), witness.name());
        let line = cosignature.to_line();
        let body = String::from_utf8_lossy(reply.body);
        assert!(
            body.split_inclusive('\n').any(|l| l == line),
            "the cosignature line {line:?} is not in the body"
        );
        // A verified cosignature attaches to the checkpoint it signs.
        checkpoint
            .with_cosignature(&cosignature)
            .expect("a verified cosignature attaches");
    }
    if let Ok(request) = AddCheckpoint::parse(reply.body) {
        assert_eq!(
            request.to_bytes(),
            reply.body,
            "a request does not render back"
        );
    }
});
