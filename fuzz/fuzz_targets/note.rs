//! Arbitrary bytes as a C2SP signed note, a checkpoint, a verifier key and a
//! key name.
//!
//! Every one of these formats is canonical, so an accepted input renders back
//! to exactly the input: `Note::to_bytes`, `SignedCheckpoint::to_bytes`,
//! `Checkpoint::text`, and `Display` for `Vkey` and `KeyName`. Verifying an
//! accepted note or checkpoint against a fixed key never panics.
#![no_main]

use std::str::FromStr;
use std::sync::OnceLock;

use doubleentry::transparency::{Checkpoint, KeyName, Note, SignedCheckpoint, Vkey};
use libfuzzer_sys::fuzz_target;

/// The log key of the interop vectors (`tests/vectors/c2sp/keys/log-ed25519.txt`).
const LOG_VKEY: &str =
    "example.com/ledgers/acme+c3b6d994+AepKbGPinFIKvvVQexMuxfmVR3auvr57kkIe6mkURtIs";

fn keys() -> &'static [Vkey] {
    static KEYS: OnceLock<Vec<Vkey>> = OnceLock::new();
    KEYS.get_or_init(|| vec![Vkey::from_str(LOG_VKEY).expect("the vector key parses")])
}

fuzz_target!(|data: &[u8]| {
    if let Ok(note) = Note::parse(data) {
        assert_eq!(note.to_bytes(), data, "a note does not render back");
        for signature in note.signatures() {
            let _ = signature.to_line();
        }
        let _ = note.verify(keys());
    }
    if let Ok(signed) = SignedCheckpoint::parse(data) {
        assert_eq!(
            signed.to_bytes(),
            data,
            "a signed checkpoint does not render back"
        );
        assert_eq!(
            signed.checkpoint().text(),
            signed.note().text(),
            "a checkpoint's text is not its note's text"
        );
        let _ = signed.verify(keys());
    }
    let Ok(text) = core::str::from_utf8(data) else {
        return;
    };
    if let Ok(checkpoint) = Checkpoint::parse_text(text) {
        assert_eq!(checkpoint.text(), text, "a checkpoint does not render back");
    }
    if let Ok(vkey) = Vkey::from_str(text) {
        assert_eq!(vkey.to_string(), text, "a vkey does not render back");
        assert_eq!(Vkey::from_str(&vkey.to_string()).as_ref(), Ok(&vkey));
    }
    if let Ok(name) = KeyName::from_str(text) {
        assert_eq!(name.to_string(), text, "a key name does not render back");
        assert_eq!(name.as_str(), text);
    }
});
