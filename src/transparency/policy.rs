//! C2SP `tlog-policy`: which logs and witnesses an auditor trusts, and
//! which witnesses must have cosigned ([`Policy`]).
//!
//! Evaluation ([`Policy::verify`], feature `witness`) follows
//! transparency-dev/formats `TLogPolicy.Verify`: the checkpoint must be
//! signed by a policy log whose key name is its origin; every signature from
//! a key the policy knows must verify — even one the quorum could do
//! without; signatures from unknown keys are ignored; the quorum is
//! evaluated over the verified witness cosignatures.
//!
//! Where the crate decides differently from formats — always by refusing
//! what formats accepts, never the reverse (each recorded with both verdicts
//! in `tests/vectors/c2sp/policies/`):
//!
//! - ML-DSA-44 (`0x06`) witnesses are parsed and kept but never verified and
//!   never counted (fail closed): a quorum that needs one is not met;
//!   formats verifies them.
//! - A log key other than `0x01`/`0x04` is refused ([`PolicyRule::KeyType`]);
//!   formats accepts every note signature type it knows.
//! - Two different keys with the same (key name, key ID) are refused when
//!   the policy is parsed ([`PolicyRule::AmbiguousKey`]); formats accepts
//!   the policy and refuses a checkpoint only once a signature line names
//!   that pair (`VerifierList`'s ambiguity error).
//! - A witness whose Ed25519 public key is also a log's key is refused
//!   ([`PolicyRule::WitnessIsLog`]): signed-note says a key SHOULD NOT be
//!   reused across signature types, and such a witness would let the log
//!   meet its own quorum. A log signature never counts towards the quorum
//!   in either implementation.
//! - A checkpoint with two signature lines of one (key name, key ID) is
//!   refused when it is parsed ([`Error::DuplicateSignature`]); formats
//!   keeps the first.

use core::fmt;
use core::str::FromStr;

use super::Error;
use super::error::PolicyRule;
use super::note::KeyName;
use super::signing::{KeyType, Vkey, key_id};

/// The most logs, witnesses and groups a policy may define (each).
pub const MAX_POLICY_ITEMS: usize = 256;

/// The reserved quorum name that requires no cosignature.
const NONE: &str = "none";

/// The signature type of an ML-DSA-44 cosignature key.
const ML_DSA_44: u8 = 0x06;

/// The length of an ML-DSA-44 public key.
const ML_DSA_44_KEY_BYTES: usize = 1312;

/// A group's threshold as written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Threshold {
    /// Every member.
    All,
    /// At least one member.
    Any,
    /// At least `k` members, `1 ≤ k ≤ n`.
    AtLeast(u16),
}

/// A witness key in a policy.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum WitnessKey {
    /// An Ed25519 cosignature key (`0x04`): verified and counted.
    Ed25519(Vkey),
    /// An ML-DSA-44 key (`0x06`): kept verbatim, never counted.
    Unsupported {
        name: KeyName,
        key_id: [u8; 4],
        text: String,
        key: Vec<u8>,
    },
}

impl WitnessKey {
    /// The bytes two keys are duplicates by: the type and the key.
    fn identity(&self) -> (u8, &[u8]) {
        match self {
            Self::Ed25519(vkey) => (vkey.key_type().byte(), vkey.key_bytes()),
            Self::Unsupported { key, .. } => (ML_DSA_44, key),
        }
    }

    /// The Ed25519 public key, for comparison with the log keys.
    fn ed25519_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Ed25519(vkey) => Some(vkey.key_bytes()),
            Self::Unsupported { .. } => None,
        }
    }

    /// The (key name, key ID) a signature line names this key by.
    fn name_and_id(&self) -> (&KeyName, [u8; 4]) {
        match self {
            Self::Ed25519(vkey) => (vkey.name(), vkey.key_id()),
            Self::Unsupported { name, key_id, .. } => (name, *key_id),
        }
    }
}

impl fmt::Display for WitnessKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ed25519(vkey) => fmt::Display::fmt(vkey, f),
            Self::Unsupported { text, .. } => f.write_str(text),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Log {
    key: Vkey,
    url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Witness {
    name: String,
    key: WitnessKey,
    url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Group {
    name: String,
    threshold: u16,
    members: Vec<String>,
}

/// A C2SP `tlog-policy`: known logs, known witnesses, groups with
/// thresholds, and the quorum.
///
/// `Display` writes the canonical form transparency-dev/formats writes
/// (logs, witnesses, groups with numeric thresholds, the quorum; no
/// comments), which parses back to an equal policy.
///
/// ```
/// use doubleentry::transparency::{Policy, Threshold, Vkey};
///
/// let log: Vkey = "example.com/ledgers/acme+c3b6d994+AepKbGPinFIKvvVQexMuxfmVR3auvr57kkIe6mkURtIs".parse()?;
/// let w1: Vkey = "witness.example/w1+24b5d9fe+BP0XJDhaoMdbZPt4zWAvodmR/ev3axPFjtcC6sg16fYY".parse()?;
/// let text = format!("log {log}\nwitness w1 {w1}\ngroup witnesses any w1\nquorum witnesses\n");
/// let policy: Policy = text.parse()?;
/// let built = Policy::builder()
///     .log(log.clone())
///     .witness("w1", w1.clone())
///     .group("witnesses", Threshold::Any, &["w1"])
///     .quorum("witnesses")
///     .build()?;
/// assert_eq!(policy, built);
/// assert_eq!(policy.to_string(), text.replace(" any ", " 1 "));
/// assert_eq!(Policy::k_of_n(log, 1, [(String::from("w1"), w1)])?.logs().count(), 1);
/// # Ok::<(), doubleentry::transparency::Error>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Policy {
    logs: Vec<Log>,
    witnesses: Vec<Witness>,
    groups: Vec<Group>,
    /// `None`: `quorum none`.
    quorum: Option<String>,
}

impl Policy {
    /// Parses policy text.
    ///
    /// # Errors
    ///
    /// [`Error::PolicyRule`] naming the line and rule, or a vkey's error
    /// ([`Error::Malformed`] naming the line, [`Error::KeyIdMismatch`],
    /// [`Error::WeakKey`], [`Error::BadName`]).
    pub fn parse(text: &str) -> Result<Self, Error> {
        Parser::default().parse(text)
    }

    /// A policy with nothing in it yet (not a valid policy: no quorum).
    const fn empty() -> Self {
        Self {
            logs: Vec::new(),
            witnesses: Vec::new(),
            groups: Vec::new(),
            quorum: None,
        }
    }

    /// The policy that trusts one log and needs no witness (`quorum none`).
    #[must_use]
    pub fn log_only(log: Vkey) -> Self {
        Self {
            logs: vec![Log {
                key: log,
                url: None,
            }],
            witnesses: Vec::new(),
            groups: Vec::new(),
            quorum: None,
        }
    }

    /// The policy that trusts one log and needs `k` of the named
    /// witnesses (a group `witnesses` that is the quorum).
    ///
    /// # Errors
    ///
    /// As [`PolicyBuilder::build`]: for example a `k` outside `1..=n`.
    pub fn k_of_n(
        log: Vkey,
        k: u16,
        witnesses: impl IntoIterator<Item = (String, Vkey)>,
    ) -> Result<Self, Error> {
        let mut builder = Self::builder().log(log);
        let mut members = Vec::new();
        for (name, key) in witnesses {
            builder = builder.witness(&name, key);
            members.push(name);
        }
        let members: Vec<&str> = members.iter().map(String::as_str).collect();
        builder
            .group("witnesses", Threshold::AtLeast(k), &members)
            .quorum("witnesses")
            .build()
    }

    /// A builder with the same rules as the text.
    pub fn builder() -> PolicyBuilder {
        PolicyBuilder::default()
    }

    /// The log keys, in order.
    pub fn logs(&self) -> impl Iterator<Item = &Vkey> {
        self.logs.iter().map(|log| &log.key)
    }
}

impl FromStr for Policy {
    type Err = Error;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::parse(text)
    }
}

impl fmt::Display for Policy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let url = |url: &Option<String>| {
            url.as_ref()
                .map(|url| format!(" {url}"))
                .unwrap_or_default()
        };
        for log in &self.logs {
            writeln!(f, "log {}{}", log.key, url(&log.url))?;
        }
        for witness in &self.witnesses {
            writeln!(
                f,
                "witness {} {}{}",
                witness.name,
                witness.key,
                url(&witness.url)
            )?;
        }
        for group in &self.groups {
            writeln!(
                f,
                "group {} {} {}",
                group.name,
                group.threshold,
                group.members.join(" ")
            )?;
        }
        writeln!(f, "quorum {}", self.quorum.as_deref().unwrap_or(NONE))
    }
}

/// The state of a parse: what is defined so far.
#[derive(Default)]
struct Parser {
    defined: Vec<String>,
    members: Vec<String>,
    quorum: bool,
    line: u32,
}

impl Parser {
    fn rule(&self, rule: PolicyRule) -> Error {
        Error::PolicyRule {
            line: self.line,
            rule,
        }
    }

    /// A vkey's refusal, naming this line.
    fn on_line<T>(&self, result: Result<T, Error>) -> Result<T, Error> {
        result.map_err(|error| match error {
            Error::Malformed { what, .. } => Error::Malformed {
                what,
                line: self.line,
            },
            other => other,
        })
    }

    fn parse(mut self, text: &str) -> Result<Policy, Error> {
        let mut policy = Policy::empty();
        if let Some(at) = text
            .bytes()
            .position(|byte| byte != b'\t' && byte != b'\n' && (byte < 0x20 || byte == 0x7f))
        {
            self.line = super::note::line_of(text.as_bytes(), at);
            return Err(self.rule(PolicyRule::Charset));
        }
        for line in text.lines() {
            self.line = self.line.saturating_add(1);
            let fields: Vec<&str> = line
                .split([' ', '\t'])
                .filter(|field| !field.is_empty())
                .collect();
            match fields.as_slice() {
                [] => {}
                [first, ..] if first.starts_with('#') => {}
                ["log", vkey, url @ ..] if url.len() <= 1 => {
                    self.log(&mut policy, vkey, url.first())?;
                }
                ["witness", name, vkey, url @ ..] if url.len() <= 1 => {
                    self.witness(&mut policy, name, vkey, url.first())?;
                }
                ["group", name, threshold, members @ ..] if !members.is_empty() => {
                    self.group(&mut policy, name, threshold, members)?;
                }
                ["quorum", name] => self.quorum(&mut policy, name)?,
                _ => return Err(self.rule(PolicyRule::Line)),
            }
        }
        self.finish(policy)
    }

    /// The policy, once it has its quorum.
    fn finish(&self, policy: Policy) -> Result<Policy, Error> {
        if self.quorum {
            Ok(policy)
        } else {
            Err(self.rule(PolicyRule::QuorumCount))
        }
    }

    fn log(&self, policy: &mut Policy, vkey: &str, url: Option<&&str>) -> Result<(), Error> {
        let (_, _, key_type, _) = self.on_line(Vkey::split(vkey))?;
        if KeyType::from_byte(key_type).is_err() {
            return Err(self.rule(PolicyRule::KeyType));
        }
        let key: Vkey = self.on_line(vkey.parse())?;
        self.add_log(policy, key, url.map(|url| (*url).to_owned()))
    }

    /// Adds a parsed (or built) log key.
    fn add_log(&self, policy: &mut Policy, key: Vkey, url: Option<String>) -> Result<(), Error> {
        if policy.logs.iter().any(|log| {
            (log.key.key_type(), log.key.key_bytes()) == (key.key_type(), key.key_bytes())
        }) {
            return Err(self.rule(PolicyRule::DuplicateKey));
        }
        if policy
            .witnesses
            .iter()
            .any(|witness| witness.key.ed25519_bytes() == Some(key.key_bytes()))
        {
            return Err(self.rule(PolicyRule::WitnessIsLog));
        }
        self.unambiguous(policy, key.name(), key.key_id())?;
        if policy.logs.len() == MAX_POLICY_ITEMS {
            return Err(self.rule(PolicyRule::TooMany));
        }
        policy.logs.push(Log { key, url });
        Ok(())
    }

    /// Refuses a key whose (name, key ID) another key of the policy has.
    /// Identical keys are refused earlier, as duplicates.
    fn unambiguous(&self, policy: &Policy, name: &KeyName, id: [u8; 4]) -> Result<(), Error> {
        let taken = policy
            .logs
            .iter()
            .map(|log| (log.key.name(), log.key.key_id()))
            .chain(
                policy
                    .witnesses
                    .iter()
                    .map(|witness| witness.key.name_and_id()),
            )
            .any(|pair| pair == (name, id));
        if taken {
            Err(self.rule(PolicyRule::AmbiguousKey))
        } else {
            Ok(())
        }
    }

    fn name(&self, name: &str) -> Result<(), Error> {
        if name == NONE {
            Err(self.rule(PolicyRule::ReservedName))
        } else if self.defined.iter().any(|seen| seen == name) {
            Err(self.rule(PolicyRule::DuplicateName))
        } else {
            Ok(())
        }
    }

    fn witness(
        &mut self,
        policy: &mut Policy,
        name: &str,
        vkey: &str,
        url: Option<&&str>,
    ) -> Result<(), Error> {
        self.name(name)?;
        let (key_name, id, key_type, key) = self.on_line(Vkey::split(vkey))?;
        let key = match key_type {
            0x04 => WitnessKey::Ed25519(self.on_line(vkey.parse())?),
            ML_DSA_44 if key.len() == ML_DSA_44_KEY_BYTES => {
                if key_id(key_name.as_str(), ML_DSA_44, &key) != id {
                    return Err(Error::KeyIdMismatch);
                }
                WitnessKey::Unsupported {
                    name: key_name,
                    key_id: id,
                    text: vkey.to_owned(),
                    key,
                }
            }
            ML_DSA_44 => {
                return Err(Error::Malformed {
                    what: super::error::Syntax::Vkey,
                    line: self.line,
                });
            }
            _ => return Err(self.rule(PolicyRule::KeyType)),
        };
        self.add_witness(policy, name, key, url.map(|url| (*url).to_owned()))
    }

    /// Adds a parsed (or built) witness under a name already checked by
    /// [`name`](Self::name).
    fn add_witness(
        &mut self,
        policy: &mut Policy,
        name: &str,
        key: WitnessKey,
        url: Option<String>,
    ) -> Result<(), Error> {
        if policy
            .witnesses
            .iter()
            .any(|witness| witness.key.identity() == key.identity())
        {
            return Err(self.rule(PolicyRule::DuplicateKey));
        }
        if let Some(bytes) = key.ed25519_bytes()
            && policy.logs.iter().any(|log| log.key.key_bytes() == bytes)
        {
            return Err(self.rule(PolicyRule::WitnessIsLog));
        }
        let (key_name, id) = key.name_and_id();
        self.unambiguous(policy, key_name, id)?;
        if policy.witnesses.len() == MAX_POLICY_ITEMS {
            return Err(self.rule(PolicyRule::TooMany));
        }
        self.defined.push(name.to_owned());
        policy.witnesses.push(Witness {
            name: name.to_owned(),
            key,
            url,
        });
        Ok(())
    }

    fn group(
        &mut self,
        policy: &mut Policy,
        name: &str,
        threshold: &str,
        members: &[&str],
    ) -> Result<(), Error> {
        self.name(name)?;
        let threshold = match threshold {
            "any" => Threshold::Any,
            "all" => Threshold::All,
            digits if !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()) => {
                // Leading zeros are accepted, as formats accepts them; a
                // value beyond `u16` is above any `n` the item limit allows.
                Threshold::AtLeast(digits.parse::<u16>().unwrap_or(u16::MAX))
            }
            _ => return Err(self.rule(PolicyRule::Threshold)),
        };
        self.add_group(policy, name, threshold, members)
    }

    /// Adds a parsed (or built) group.
    fn add_group(
        &mut self,
        policy: &mut Policy,
        name: &str,
        threshold: Threshold,
        members: &[&str],
    ) -> Result<(), Error> {
        self.name(name)?;
        let n = members.len();
        let k = match threshold {
            Threshold::Any => 1,
            Threshold::All => n,
            Threshold::AtLeast(k) => usize::from(k),
        };
        let k = Some(k)
            .filter(|k| (1..=n).contains(k))
            .and_then(|k| u16::try_from(k).ok())
            .ok_or(self.rule(PolicyRule::Threshold))?;
        for member in members {
            if *member == NONE {
                return Err(self.rule(PolicyRule::ReservedName));
            }
            if !self.defined.iter().any(|seen| seen == member) {
                return Err(self.rule(PolicyRule::Undefined));
            }
            if self.members.iter().any(|seen| seen == member) {
                return Err(self.rule(PolicyRule::MemberTwice));
            }
            self.members.push((*member).to_owned());
        }
        if policy.groups.len() == MAX_POLICY_ITEMS {
            return Err(self.rule(PolicyRule::TooMany));
        }
        self.defined.push(name.to_owned());
        policy.groups.push(Group {
            name: name.to_owned(),
            threshold: k,
            members: members.iter().map(|member| (*member).to_owned()).collect(),
        });
        Ok(())
    }

    /// Sets the quorum: once, to `none` or a name defined before.
    fn quorum(&mut self, policy: &mut Policy, name: &str) -> Result<(), Error> {
        if self.quorum {
            return Err(self.rule(PolicyRule::QuorumCount));
        }
        self.quorum = true;
        if name != NONE && !self.defined.iter().any(|seen| seen == name) {
            return Err(self.rule(PolicyRule::Undefined));
        }
        policy.quorum = (name != NONE).then(|| name.to_owned());
        Ok(())
    }
}

/// Builds a [`Policy`] in code, with the rules of the text.
///
/// Names are policy-local (one namespace for witnesses and groups), defined
/// before use; `none` is reserved. Every name — of a witness, a group, a
/// member or the quorum — must be one `tlog-policy` item: non-empty, without
/// a space, tab, newline or other control character (`0x00–0x1F`, `0x7F`);
/// any other byte is opaque, as formats reads it. Nothing is turned into
/// text and parsed again: [`build`](Self::build) applies the parser's rules
/// to the items as given, so no name can add or change a policy line.
///
/// ```
/// use doubleentry::transparency::{Error, Policy, PolicyRule, Vkey};
///
/// let log: Vkey = "example.com/ledgers/acme+c3b6d994+AepKbGPinFIKvvVQexMuxfmVR3auvr57kkIe6mkURtIs".parse()?;
/// let refused = Policy::builder().log(log).quorum("none\nlog evil").build();
/// assert_eq!(refused, Err(Error::PolicyRule { line: 0, rule: PolicyRule::Name }));
/// # Ok::<(), doubleentry::transparency::Error>(())
/// ```
#[derive(Debug, Clone, Default)]
#[must_use]
pub struct PolicyBuilder {
    items: Vec<Item>,
}

/// One definition given to a [`PolicyBuilder`].
#[derive(Debug, Clone)]
enum Item {
    Log(Vkey),
    Witness(String, Vkey),
    Group(String, Threshold, Vec<String>),
    Quorum(String),
}

impl PolicyBuilder {
    /// Trusts a log.
    pub fn log(mut self, key: Vkey) -> Self {
        self.items.push(Item::Log(key));
        self
    }

    /// Knows a witness by a policy-local name.
    pub fn witness(mut self, name: &str, key: Vkey) -> Self {
        self.items.push(Item::Witness(name.to_owned(), key));
        self
    }

    /// Defines a group of earlier witnesses and groups.
    pub fn group(mut self, name: &str, threshold: Threshold, members: &[&str]) -> Self {
        let members = members.iter().map(|member| (*member).to_owned()).collect();
        self.items
            .push(Item::Group(name.to_owned(), threshold, members));
        self
    }

    /// Sets the quorum: a witness, a group, or `none`.
    pub fn quorum(mut self, name: &str) -> Self {
        self.items.push(Item::Quorum(name.to_owned()));
        self
    }

    /// The policy.
    ///
    /// # Errors
    ///
    /// [`Error::PolicyRule`] with line 0: [`PolicyRule::Name`] for a name
    /// that is not one policy item (see the type's docs), otherwise as
    /// [`Policy::parse`] decides the same definitions — for example
    /// [`PolicyRule::Threshold`] for a `k` outside `1..=n`, or
    /// [`PolicyRule::WitnessIsLog`].
    pub fn build(self) -> Result<Policy, Error> {
        let mut parser = Parser::default();
        let mut policy = Policy::empty();
        for item in self.items {
            match item {
                Item::Log(key) => parser.add_log(&mut policy, key, None)?,
                Item::Witness(name, key) => {
                    parser.item(&name)?;
                    parser.name(&name)?;
                    parser.add_witness(&mut policy, &name, WitnessKey::Ed25519(key), None)?;
                }
                Item::Group(name, threshold, members) => {
                    parser.item(&name)?;
                    for member in &members {
                        parser.item(member)?;
                    }
                    let members: Vec<&str> = members.iter().map(String::as_str).collect();
                    parser.add_group(&mut policy, &name, threshold, &members)?;
                }
                Item::Quorum(name) => {
                    parser.item(&name)?;
                    parser.quorum(&mut policy, &name)?;
                }
            }
        }
        parser.finish(policy)
    }
}

impl Parser {
    /// Refuses a built name that is not one policy item.
    fn item(&self, name: &str) -> Result<(), Error> {
        if name.is_empty() || name.bytes().any(|byte| byte <= b' ' || byte == 0x7f) {
            Err(self.rule(PolicyRule::Name))
        } else {
            Ok(())
        }
    }
}

/// What a policy accepted: the log that signed and the counted witnesses.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PolicyVerdict {
    /// The log key's name (the checkpoint's origin).
    pub log: KeyName,
    /// Every witness whose cosignature verified, by its policy name, with
    /// the cosignature and its timestamp.
    pub witnesses: Vec<(String, super::signing::Cosignature)>,
}

#[cfg(feature = "witness")]
mod evaluate {
    use super::{Policy, PolicyVerdict, WitnessKey};
    use crate::transparency::Error;
    use crate::transparency::checkpoint::SignedCheckpoint;
    use crate::transparency::signing::{Verified, Vkey, verify_line};

    impl Policy {
        /// Applies the policy to a signed checkpoint (transparency-dev/formats
        /// `TLogPolicy.Verify` semantics).
        ///
        /// ```
        /// use doubleentry::transparency::{Checkpoint, KeyName, LogKey, Policy};
        /// use doubleentry::{LedgerId, TreeHead, log::empty_root};
        ///
        /// let ledger = LedgerId::new("example.com/ledgers/acme")?;
        /// let key = LogKey::from_seed(&ledger, [7; 32]);
        /// let head = TreeHead { size: 0, root: empty_root() };
        /// let signed = key.sign(&Checkpoint::new(KeyName::from(&ledger), head))?;
        /// let verdict = Policy::log_only(key.vkey()).verify(&signed)?;
        /// assert_eq!(verdict.log.as_str(), "example.com/ledgers/acme");
        /// # Ok::<(), Box<dyn std::error::Error>>(())
        /// ```
        ///
        /// # Errors
        ///
        /// [`Error::EmptyTreeRoot`], [`Error::BadSignature`] for any
        /// failing line from a known key, [`Error::NoTrustedSignature`]
        /// when no policy log signed, [`Error::OriginNotSigned`], or
        /// [`Error::QuorumNotMet`].
        pub fn verify(&self, checkpoint: &SignedCheckpoint) -> Result<PolicyVerdict, Error> {
            let head = checkpoint.checkpoint().head();
            if head.size == 0 && head.root != crate::log::empty_root() {
                return Err(Error::EmptyTreeRoot);
            }
            let text = checkpoint.note().text();
            let mut signers: Vec<&Vkey> = Vec::new();
            let mut cosigned = vec![None; self.witnesses.len()];
            // The policy has no two keys of one (name, key ID) and no
            // witness key that is a log key (both refused when it is built),
            // so each line names at most one key, and a log's signature is
            // never counted towards the quorum.
            for line in checkpoint.signatures() {
                if let Some(log) = self.logs.iter().find(|log| log.key.signed(line)) {
                    verify_line(&log.key, line, text)?;
                    signers.push(&log.key);
                    continue;
                }
                let found = self.witnesses.iter().enumerate().find_map(|(at, witness)| {
                    match &witness.key {
                        WitnessKey::Ed25519(key) if key.signed(line) => Some((at, key)),
                        _ => None,
                    }
                });
                if let Some((at, key)) = found
                    && let Verified::Cosigner(cosignature) = verify_line(key, line, text)?
                    && let Some(slot) = cosigned.get_mut(at)
                {
                    *slot = Some(cosignature);
                }
            }
            if signers.is_empty() {
                return Err(Error::NoTrustedSignature);
            }
            let origin = checkpoint.checkpoint().origin();
            let log = signers
                .iter()
                .find(|key| key.name() == origin)
                .ok_or(Error::OriginNotSigned)?;
            if !self.quorum_met(&cosigned) {
                return Err(Error::QuorumNotMet {
                    quorum: self.quorum.clone().unwrap_or_default(),
                });
            }
            Ok(PolicyVerdict {
                log: log.name().clone(),
                witnesses: self
                    .witnesses
                    .iter()
                    .zip(cosigned)
                    .filter_map(|(witness, cosignature)| Some((witness.name.clone(), cosignature?)))
                    .collect(),
            })
        }

        /// Whether the verified witnesses (by index) meet the quorum.
        /// Groups only name earlier definitions, so one pass in definition
        /// order decides every name.
        fn quorum_met<T>(&self, cosigned: &[Option<T>]) -> bool {
            let Some(quorum) = &self.quorum else {
                return true;
            };
            let mut satisfied: Vec<(&str, bool)> = self
                .witnesses
                .iter()
                .zip(cosigned)
                .map(|(witness, cosignature)| (witness.name.as_str(), cosignature.is_some()))
                .collect();
            for group in &self.groups {
                let count = group
                    .members
                    .iter()
                    .filter(|member| {
                        satisfied
                            .iter()
                            .any(|(name, ok)| *ok && name == &member.as_str())
                    })
                    .count();
                satisfied.push((group.name.as_str(), count >= usize::from(group.threshold)));
            }
            satisfied
                .iter()
                .any(|(name, ok)| *ok && *name == quorum.as_str())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOG: &str =
        "example.com/ledgers/acme+c3b6d994+AepKbGPinFIKvvVQexMuxfmVR3auvr57kkIe6mkURtIs";
    const W1: &str = "witness.example/w1+24b5d9fe+BP0XJDhaoMdbZPt4zWAvodmR/ev3axPFjtcC6sg16fYY";
    const W2: &str = "witness.example/w2+0863ad1e+BEOnLnFEAXYt9mtowm373yaCquyfJHTspGE+QkoPuv08";

    fn rule(text: &str) -> Option<(u32, PolicyRule)> {
        match Policy::parse(text) {
            Err(Error::PolicyRule { line, rule }) => Some((line, rule)),
            _ => None,
        }
    }

    #[test]
    fn the_text_rules() {
        let head = format!("log {LOG}\nwitness w1 {W1}\nwitness w2 {W2}\n");
        assert!(
            Policy::parse(&format!("# c\n\n  \t\n{head}group g any w1 w2\nquorum g\n")).is_ok()
        );
        assert_eq!(
            rule(&format!("{head}quorum w1\r\n")),
            Some((4, PolicyRule::Charset))
        );
        assert_eq!(
            rule(&format!("{head}\x7fquorum w1\n")),
            Some((4, PolicyRule::Charset))
        );
        assert_eq!(rule(&head), Some((3, PolicyRule::QuorumCount)));
        assert_eq!(
            rule(&format!("{head}quorum w1\nquorum w2\n")),
            Some((5, PolicyRule::QuorumCount))
        );
        assert_eq!(
            rule(&format!("{head}quorum w3\n")),
            Some((4, PolicyRule::Undefined))
        );
        assert_eq!(
            rule(&format!("{head}group g any w1\ngroup h any w1\nquorum g\n")),
            Some((5, PolicyRule::MemberTwice))
        );
        assert_eq!(
            rule(&format!("{head}group none any w1\nquorum none\n")),
            Some((4, PolicyRule::ReservedName))
        );
        assert_eq!(
            rule(&format!("{head}group g any none\nquorum g\n")),
            Some((4, PolicyRule::ReservedName))
        );
        assert_eq!(
            rule(&format!("{head}group w1 any w2\nquorum w1\n")),
            Some((4, PolicyRule::DuplicateName))
        );
        assert_eq!(
            rule(&format!("{head}group g 0 w1\nquorum g\n")),
            Some((4, PolicyRule::Threshold))
        );
        assert_eq!(
            rule(&format!("{head}group g +1 w1\nquorum g\n")),
            Some((4, PolicyRule::Threshold))
        );
        assert!(Policy::parse(&format!("{head}group g 02 w1 w2\nquorum g\n")).is_ok());
        assert_eq!(
            rule(&format!("{head}witness w3 {LOG}\nquorum none\n")),
            Some((4, PolicyRule::KeyType))
        );
        assert_eq!(
            rule(&format!("log {W1}\nlog {W1}\nquorum none\n")),
            Some((2, PolicyRule::DuplicateKey))
        );
        assert_eq!(
            rule(&format!("{head}quorum\n")),
            Some((4, PolicyRule::Line))
        );
        assert_eq!(
            rule(&format!("{head}witness\n")),
            Some((4, PolicyRule::Line))
        );
        assert_eq!(
            rule(&format!("{head}verify all\nquorum none\n")),
            Some((4, PolicyRule::Line))
        );
        let mut many = String::new();
        for i in 0..=MAX_POLICY_ITEMS {
            many.push_str(&format!("group g{i} any w1\n"));
        }
        assert!(rule(&format!("{head}{many}quorum none\n")).is_some());
    }

    #[test]
    fn builders_equal_the_text() {
        let (log, w1, w2): (Vkey, Vkey, Vkey) = (
            LOG.parse().unwrap(),
            W1.parse().unwrap(),
            W2.parse().unwrap(),
        );
        let text: Policy = format!(
            "log {LOG}\nwitness a {W1}\nwitness b {W2}\ngroup witnesses 2 a b\nquorum witnesses\n"
        )
        .parse()
        .unwrap();
        let k_of_n = Policy::k_of_n(
            log.clone(),
            2,
            [("a".to_owned(), w1.clone()), ("b".to_owned(), w2)],
        )
        .unwrap();
        assert_eq!(k_of_n, text);
        assert_eq!(Policy::parse(&text.to_string()).unwrap(), text);
        assert_eq!(
            Policy::k_of_n(log.clone(), 3, [("a".to_owned(), w1.clone())]),
            Err(Error::PolicyRule {
                line: 0,
                rule: PolicyRule::Threshold
            })
        );
        for bad in ["has space", "tab\t", "new\nline", "", "del\u{7f}"] {
            assert_eq!(
                Policy::builder()
                    .log(log.clone())
                    .witness(bad, w1.clone())
                    .quorum("none")
                    .build(),
                Err(Error::PolicyRule {
                    line: 0,
                    rule: PolicyRule::Name
                }),
                "{bad:?}"
            );
        }
        // Any other byte is opaque, as formats reads it.
        let odd = Policy::builder()
            .log(log.clone())
            .witness("#w\u{a0}\u{e9}", w1)
            .quorum("#w\u{a0}\u{e9}")
            .build()
            .unwrap();
        assert_eq!(Policy::parse(&odd.to_string()).unwrap(), odd);
        assert_eq!(
            Policy::log_only(log.clone()).to_string(),
            format!("log {LOG}\nquorum none\n")
        );
        assert_eq!(Policy::log_only(log).logs().count(), 1);
    }
}
