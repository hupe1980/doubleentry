//! [`TestWitness`]: a C2SP `tlog-witness` cosigner in memory, for tests and
//! examples (features `testing` and `witness`).

use std::collections::BTreeMap;

use ed25519_dalek::SigningKey;

use crate::log::{ConsistencyProof, TreeHead, empty_root};
use crate::transparency::{
    AddCheckpoint, Error, KeyName, KeyType, Verified, Vkey, cosign, verify_line, vkey,
};

/// A witness's answer to one `add-checkpoint` request.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Reply {
    /// The HTTP status.
    pub status: u16,
    /// The `Content-Type`, when the answer has one.
    pub content_type: Option<&'static str>,
    /// The body.
    pub body: Vec<u8>,
}

impl Reply {
    fn status(status: u16) -> Self {
        Self {
            status,
            content_type: None,
            body: Vec::new(),
        }
    }
}

/// A witness that answers `add-checkpoint` request bodies the way
/// c2sp.org/tlog-witness says, keeping the size it holds per log in memory.
///
/// **Not a production witness.** It forgets everything when dropped, its
/// "timestamps" are a counter (1, 2, 3, … — this crate reads no clock), and
/// a witness is worth having only if it is somebody else's. It exists so
/// tests and examples can drive a [`WitnessSession`] without a network.
///
/// It answers 200 with one `cosignature/v1` line; 400 for a body that does
/// not parse or an `old` above the checkpoint's size; 404 for a log it was
/// not told about ([`log`](Self::log)); 403 when no known log key verifies,
/// one fails, or two known keys share a (key name, key ID); 409 `text/x.tlog.size` with the size it holds when `old`
/// differs (0 for a log it never cosigned); 422 for a bad consistency proof,
/// a proof from size 0, another root at the same size, or a size-0
/// checkpoint without the empty-tree root.
///
/// [`WitnessSession`]: crate::transparency::WitnessSession
///
/// ```
/// use doubleentry::testing::TestWitness;
/// use doubleentry::transparency::{AddCheckpoint, LogKey};
/// use doubleentry::{Journal, LedgerId};
///
/// let ledger = LedgerId::new("example.com/ledgers/acme")?;
/// let key = LogKey::from_seed(&ledger, [7; 32]);
/// let mut witness = TestWitness::new("witness.example/w1", [9; 32])?.log(key.vkey());
/// let signed = Journal::new(ledger).checkpoint(&key)?;
/// let reply = witness.post(&AddCheckpoint::new(&signed, 0, None)?.to_bytes());
/// assert_eq!(reply.status, 200);
/// let stale = witness.post(&AddCheckpoint::new(&signed, 0, None)?.to_bytes());
/// assert_eq!((stale.status, stale.body.as_slice()), (409, b"1\n".as_slice()));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Clone)]
pub struct TestWitness {
    name: KeyName,
    key: SigningKey,
    logs: Vec<Vkey>,
    heads: BTreeMap<KeyName, TreeHead>,
    clock: u64,
}

impl core::fmt::Debug for TestWitness {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TestWitness")
            .field("vkey", &self.vkey().to_string())
            .field("logs", &self.logs)
            .field("heads", &self.heads)
            .finish_non_exhaustive()
    }
}

impl TestWitness {
    /// A witness named `name` with the Ed25519 key from `seed`.
    ///
    /// # Errors
    ///
    /// [`Error::BadName`] for a name that is no key name.
    pub fn new(name: &str, seed: [u8; 32]) -> Result<Self, Error> {
        Ok(Self {
            name: KeyName::new(name)?,
            key: SigningKey::from_bytes(&seed),
            logs: Vec::new(),
            heads: BTreeMap::new(),
            clock: 0,
        })
    }

    /// Trusts a log key for the origin it names.
    #[must_use]
    pub fn log(mut self, key: Vkey) -> Self {
        self.logs.push(key);
        self
    }

    /// The witness's cosignature verifier key (type `0x04`).
    #[must_use]
    pub fn vkey(&self) -> Vkey {
        vkey(
            &self.name,
            KeyType::CosignatureEd25519,
            &self.key.verifying_key(),
        )
    }

    /// The head the witness holds for an origin.
    #[must_use]
    pub fn head(&self, origin: &KeyName) -> Option<TreeHead> {
        self.heads.get(origin).copied()
    }

    /// Answers one `add-checkpoint` request body.
    pub fn post(&mut self, body: &[u8]) -> Reply {
        let Ok(request) = AddCheckpoint::parse(body) else {
            return Reply::status(400);
        };
        let signed = request.checkpoint();
        let checkpoint = signed.checkpoint();
        let origin = checkpoint.origin();
        let keys: Vec<&Vkey> = self
            .logs
            .iter()
            .filter(|key| key.name() == origin)
            .collect();
        if keys.is_empty() {
            return Reply::status(404);
        }
        let text = signed.note().text();
        let mut verified = 0usize;
        for line in signed.signatures() {
            let mut matching = keys.iter().filter(|key| key.signed(line));
            if let Some(key) = matching.next() {
                if matching.any(|other| other != key) {
                    // Two trusted keys of one (name, key ID): ambiguous,
                    // never the first one taken.
                    return Reply::status(403);
                }
                match verify_line(key, line, text) {
                    Ok(Verified::Log { .. } | Verified::Cosigner(_)) => {
                        verified = verified.saturating_add(1)
                    }
                    Err(_) => return Reply::status(403),
                }
            }
        }
        if verified == 0 {
            return Reply::status(403);
        }
        let head = checkpoint.head();
        if head.size == 0 && head.root != empty_root() {
            return Reply::status(422);
        }
        if request.old() > head.size {
            return Reply::status(400);
        }
        let held = self.heads.get(origin).copied();
        let held_size = held.map_or(0, |held| held.size);
        if request.old() != held_size {
            return Reply {
                status: 409,
                content_type: Some("text/x.tlog.size"),
                body: format!("{held_size}\n").into_bytes(),
            };
        }
        let consistent = match held {
            None => request.proof().is_empty(),
            Some(held) if held.size == 0 => request.proof().is_empty(),
            Some(held) if held.size == head.size => {
                request.proof().is_empty() && held.root == head.root
            }
            Some(held) => ConsistencyProof {
                old_size: held.size,
                new_size: head.size,
                path: request.proof().to_vec(),
            }
            .verify(&held, &head)
            .is_ok(),
        };
        if !consistent {
            return Reply::status(422);
        }
        self.clock = self.clock.saturating_add(1);
        self.heads.insert(origin.clone(), head);
        let cosignature = cosign(&self.name, &self.key, self.clock, checkpoint);
        Reply {
            status: 200,
            content_type: None,
            body: cosignature.to_line().into_bytes(),
        }
    }
}
