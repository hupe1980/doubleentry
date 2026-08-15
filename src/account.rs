//! Accounts and the account tree.
//!
//! An account is a path in a user-defined hierarchy, such as
//! `Assets:Current:Bank:Main`. The engine imposes no chart of accounts: it does
//! not know what `Assets` means, does not require that name, and does not
//! validate against any national or corporate scheme.
//!
//! Two rules are enforced, because both are structural rather than
//! jurisdictional:
//!
//! - **Only leaves are postable.** An account with children is an aggregation
//!   path. Posting to both a node and its descendants makes rollups
//!   double-count, and no reporting layer can repair it afterwards.
//! - **Postings fall inside the account's open window.** An account has an
//!   opening date and an optional closing date.
//!
//! A third is offered rather than imposed. A [`BalanceLimit`] forbids an
//! account's net from crossing zero in one direction — a cash box that cannot be
//! overdrawn, a wallet that cannot be drawn beyond what was funded. It is off by
//! default, because most accounts legitimately sit on either side; where it is
//! set it is checked when an entry is recorded, against the balance that entry
//! would leave behind.

use std::collections::BTreeMap;

use time::Date;

use crate::balance::Balance;
use crate::canonical::{Canonical, CanonicalWriter};
use crate::hash::{Hash, tag, tagged};
use crate::merkle::{InclusionProof, MerkleLog, TreeHead};

/// Maximum number of characters in one path segment.
pub const MAX_SEGMENT_LEN: usize = 64;

/// Maximum depth of an account path.
pub const MAX_DEPTH: usize = 16;

/// The separator between path segments.
pub const SEPARATOR: char = ':';

/// Maximum number of accounts one registry may hold.
///
/// A handle is a `u32` position, so this is how many distinct ones exist. The
/// bound is stated rather than left implicit because passing it must be an
/// error: a registry that wrapped, saturated, or reused a handle would silently
/// repoint every posting and every sealed balance that names it.
pub const MAX_ACCOUNTS: usize = u32::MAX as usize;

/// Failure constructing or registering an account.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum AccountError {
    /// The path had no segments.
    #[error("account path is empty")]
    Empty,
    /// A segment was empty, e.g. from a doubled or trailing separator.
    #[error("account path has an empty segment at position {position}")]
    EmptySegment {
        /// Zero-based segment position.
        position: usize,
    },
    /// A segment exceeded [`MAX_SEGMENT_LEN`].
    #[error("account path segment {position} exceeds {MAX_SEGMENT_LEN} characters")]
    SegmentTooLong {
        /// Zero-based segment position.
        position: usize,
    },
    /// The path exceeded [`MAX_DEPTH`].
    #[error("account path is deeper than {MAX_DEPTH} segments")]
    TooDeep,
    /// A segment contained a control character.
    #[error("account path segment {position} contains a control character")]
    ControlCharacter {
        /// Zero-based segment position.
        position: usize,
    },
    /// The account was already registered.
    #[error("account {path} is already registered")]
    AlreadyRegistered {
        /// The duplicate path.
        path: AccountPath,
    },
    /// The account handle does not belong to this registry.
    #[error("account {id} is not registered")]
    UnknownAccount {
        /// The offending handle.
        id: AccountId,
    },
    /// Stored bindings were not a dense range starting at zero.
    ///
    /// Handles are positions, so a gap or a duplicate means the stored set is
    /// not the set the handles were issued against.
    #[error("stored account bindings are not dense: expected index {expected}, found {found}")]
    NotDense {
        /// The index the sequence required.
        expected: u32,
        /// The index actually present.
        found: u32,
    },
    /// The registry has issued every handle its index space allows.
    ///
    /// Handles are `u32` positions written into every posting row and into every
    /// trial-balance leaf a seal commits to, so reusing one would repoint
    /// history. Refusing the registration is the only safe answer.
    #[error("account registry is full: all {MAX_ACCOUNTS} handles have been issued")]
    RegistryFull,
    /// The closing date preceded the opening date.
    #[error("account {path} closes on {closed} before it opens on {opened}")]
    ClosedBeforeOpened {
        /// The offending path.
        path: AccountPath,
        /// Opening date.
        opened: Date,
        /// Closing date.
        closed: Date,
    },
}

/// A validated account path.
///
/// Paths are compared and ordered segment-wise, so sorting a set of paths yields
/// a stable, tree-like order regardless of platform.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AccountPath {
    segments: Vec<String>,
}

#[cfg(feature = "serde")]
impl serde::Serialize for AccountPath {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for AccountPath {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = <std::borrow::Cow<'_, str> as serde::Deserialize>::deserialize(d)?;
        Self::parse(&s).map_err(serde::de::Error::custom)
    }
}

impl AccountPath {
    /// Parses a separator-delimited path such as `"Assets:Bank"`.
    pub fn parse(s: &str) -> Result<Self, AccountError> {
        if s.is_empty() {
            return Err(AccountError::Empty);
        }
        let segments: Vec<&str> = s.split(SEPARATOR).collect();
        Self::from_segments(segments)
    }

    /// Builds a path from already-split segments.
    pub fn from_segments<S: AsRef<str>>(
        segments: impl IntoIterator<Item = S>,
    ) -> Result<Self, AccountError> {
        let raw: Vec<String> = segments
            .into_iter()
            .map(|s| s.as_ref().to_owned())
            .collect();

        if raw.is_empty() {
            return Err(AccountError::Empty);
        }
        if raw.len() > MAX_DEPTH {
            return Err(AccountError::TooDeep);
        }
        for (position, seg) in raw.iter().enumerate() {
            if seg.is_empty() {
                return Err(AccountError::EmptySegment { position });
            }
            if seg.chars().count() > MAX_SEGMENT_LEN {
                return Err(AccountError::SegmentTooLong { position });
            }
            if seg.chars().any(char::is_control) {
                return Err(AccountError::ControlCharacter { position });
            }
        }
        Ok(Self { segments: raw })
    }

    /// The path segments, outermost first.
    #[must_use]
    pub fn segments(&self) -> &[String] {
        &self.segments
    }

    /// Number of segments.
    #[must_use]
    pub fn depth(&self) -> usize {
        self.segments.len()
    }

    /// The parent path, or `None` for a top-level account.
    #[must_use]
    pub fn parent(&self) -> Option<Self> {
        if self.segments.len() <= 1 {
            return None;
        }
        let mut segments = self.segments.clone();
        segments.pop();
        Some(Self { segments })
    }

    /// Every ancestor, from the top-level account down to the direct parent.
    #[must_use]
    pub fn ancestors(&self) -> Vec<Self> {
        let mut out = Vec::new();
        let mut current = self.parent();
        while let Some(p) = current {
            current = p.parent();
            out.push(p);
        }
        out.reverse();
        out
    }

    /// True when `self` is `other` or a descendant of it.
    #[must_use]
    pub fn is_under(&self, other: &Self) -> bool {
        if self.segments.len() < other.segments.len() {
            return false;
        }
        self.segments
            .iter()
            .zip(other.segments.iter())
            .all(|(a, b)| a == b)
    }

    /// A stable content hash of this path.
    #[must_use]
    pub fn content_hash(&self) -> Hash {
        tagged(tag::ACCOUNT_V1, &self.to_canonical_bytes())
    }
}

impl Canonical for AccountPath {
    fn encode(&self, w: &mut CanonicalWriter) {
        w.seq(self.segments.iter(), |w, s| {
            w.str(s);
        });
    }
}

impl std::fmt::Display for AccountPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut first = true;
        for seg in &self.segments {
            if !first {
                f.write_str(":")?;
            }
            f.write_str(seg)?;
            first = false;
        }
        Ok(())
    }
}

impl std::str::FromStr for AccountPath {
    type Err = AccountError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

/// The classification of an account for reporting purposes.
///
/// This is metadata only. The engine records it, exposes it, and never uses it
/// to constrain a posting — a chart of accounts is the caller's concern.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum AccountKind {
    /// Resources controlled by the entity.
    Asset,
    /// Obligations owed by the entity.
    Liability,
    /// Residual interest in the assets.
    Equity,
    /// Inflows increasing equity.
    Income,
    /// Outflows decreasing equity.
    Expense,
}

impl AccountKind {
    /// The side on which this kind of account normally carries a balance.
    #[must_use]
    pub const fn normal_side(self) -> crate::posting::Direction {
        use crate::posting::Direction;
        match self {
            Self::Asset | Self::Expense => Direction::Debit,
            Self::Liability | Self::Equity | Self::Income => Direction::Credit,
        }
    }

    fn discriminant(self) -> u8 {
        match self {
            Self::Asset => 0,
            Self::Liability => 1,
            Self::Equity => 2,
            Self::Income => 3,
            Self::Expense => 4,
        }
    }
}

/// Which side an account's net balance is allowed to fall on.
///
/// Unconstrained by default. Set it where the books would be wrong rather than
/// merely surprising if the balance crossed zero — a cash account that cannot be
/// overdrawn, a customer prepayment that cannot go into debit, a control account
/// that must never carry the wrong sign.
///
/// The limit is checked when an entry is recorded, against the balance the entry
/// would leave behind, **per currency and per layer independently**. It is a
/// rule about what may be written next, exactly like an account's open window:
/// it never invalidates what is already recorded, and an entry that breaches it
/// is refused whole rather than partly applied.
///
/// Gross totals are irrelevant to it — only the net crosses zero — so an account
/// with heavy offsetting turnover is unaffected as long as the net stays on the
/// permitted side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum BalanceLimit {
    /// The balance may fall on either side. The default.
    #[default]
    Unlimited,
    /// Credits may never exceed debits: the net stays on the debit side or zero.
    ///
    /// The rule for an asset that cannot go negative — a cash box, a bank
    /// account without an overdraft facility, an inventory quantity.
    NoCreditBalance,
    /// Debits may never exceed credits: the net stays on the credit side or zero.
    ///
    /// The rule for a liability that cannot be drawn beyond what was funded — a
    /// customer wallet, a prepayment, a reserve.
    NoDebitBalance,
}

impl BalanceLimit {
    /// True when `balance` satisfies this limit.
    ///
    /// An overflow computing the net counts as a breach: an account whose totals
    /// cannot be netted is not one this limit can vouch for.
    #[must_use]
    pub fn permits<const P: u8>(self, balance: &Balance<P>) -> bool {
        match self {
            Self::Unlimited => true,
            Self::NoCreditBalance => balance.debits >= balance.credits,
            Self::NoDebitBalance => balance.credits >= balance.debits,
        }
    }

    /// True for [`BalanceLimit::Unlimited`].
    #[must_use]
    pub const fn is_unlimited(self) -> bool {
        matches!(self, Self::Unlimited)
    }

    const fn discriminant(self) -> u8 {
        match self {
            Self::Unlimited => 0,
            Self::NoCreditBalance => 1,
            Self::NoDebitBalance => 2,
        }
    }
}

impl std::fmt::Display for BalanceLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Unlimited => "unlimited",
            Self::NoCreditBalance => "no credit balance",
            Self::NoDebitBalance => "no debit balance",
        })
    }
}

/// A handle to a registered account.
///
/// Handles are dense and assigned in registration order, which makes them cheap
/// to compare and to use as indices. They are meaningful only relative to the
/// [`AccountRegistry`] that issued them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct AccountId(u32);

impl AccountId {
    /// The underlying index.
    #[must_use]
    pub const fn index(self) -> u32 {
        self.0
    }

    /// Wraps a raw index.
    ///
    /// Only meaningful against the registry that issued it. Provided so a
    /// persistence layer can rehydrate handles; a handle built this way is
    /// validated when the entry referencing it is sealed.
    #[must_use]
    pub const fn from_index(index: u32) -> Self {
        Self(index)
    }
}

impl std::fmt::Display for AccountId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// Registration details for one account.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Account {
    /// The account's path.
    pub path: AccountPath,
    /// Optional reporting classification.
    pub kind: Option<AccountKind>,
    /// First date on which the account may be posted to.
    pub opened_on: Date,
    /// Last date on which the account may be posted to, if it has closed.
    pub closed_on: Option<Date>,
    /// Which side the account's net balance may fall on.
    pub limit: BalanceLimit,
}

impl Account {
    /// Creates an account open from `opened_on`, with no closing date and no
    /// balance limit.
    #[must_use]
    pub fn new(path: AccountPath, opened_on: Date) -> Self {
        Self {
            path,
            kind: None,
            opened_on,
            closed_on: None,
            limit: BalanceLimit::Unlimited,
        }
    }

    /// Sets the reporting classification.
    #[must_use]
    pub fn with_kind(mut self, kind: AccountKind) -> Self {
        self.kind = Some(kind);
        self
    }

    /// Sets the closing date.
    #[must_use]
    pub fn closing_on(mut self, date: Date) -> Self {
        self.closed_on = Some(date);
        self
    }

    /// Constrains which side the account's net balance may fall on.
    #[must_use]
    pub fn limited_to(mut self, limit: BalanceLimit) -> Self {
        self.limit = limit;
        self
    }

    /// True when the account accepts postings on `date`.
    #[must_use]
    pub fn is_open_on(&self, date: Date) -> bool {
        date >= self.opened_on && self.closed_on.is_none_or(|c| date <= c)
    }
}

impl Canonical for Account {
    fn encode(&self, w: &mut CanonicalWriter) {
        self.path.encode(w);
        w.option(self.kind.as_ref(), |w, k| {
            w.u8(k.discriminant());
        });
        encode_date(w, self.opened_on);
        w.option(self.closed_on.as_ref(), |w, d| encode_date(w, *d));
        // Encoded unconditionally rather than as an option: the limit is part of
        // what the account *is*, so a registry that quietly relaxed one must
        // produce a different binding commitment.
        w.u8(self.limit.discriminant());
    }
}

/// Encodes a date as year, month, day.
///
/// The year is a signed `i32`, which is the type [`Date::year`] returns and
/// covers every date `time` can represent under any feature set. An earlier
/// revision split it into a magnitude and a sign bit, which needed a clamp — and
/// a clamp inside a hash preimage is a silent collision waiting for someone to
/// enable a wider date range.
pub(crate) fn encode_date(w: &mut CanonicalWriter, d: Date) {
    w.i32(d.year());
    w.u8(u8::from(d.month()));
    w.u8(d.day());
}

/// One account together with the handle it was issued.
///
/// The handle is the account's position in registration order, and that
/// position is written into every posting row and into the trial balance leaves
/// a seal commits to. It is therefore part of the ledger's persistent state,
/// not a runtime detail: a registry rebuilt in a different order would repoint
/// history. Records carry the binding explicitly so it survives a restart.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct AccountRecord {
    /// The handle this account was issued.
    pub id: AccountId,
    /// The account itself.
    pub account: Account,
}

impl Canonical for AccountRecord {
    fn encode(&self, w: &mut CanonicalWriter) {
        w.u32(self.id.index());
        self.account.encode(w);
    }
}

/// The leaf one handle-to-account binding hashes to.
///
/// Covers the handle and the whole account — path, kind, and open window — so a
/// registry that reopened a closed account, or moved a path to a different
/// handle, produces a different commitment.
#[must_use]
pub fn account_binding_leaf(record: &AccountRecord) -> Hash {
    let mut w = CanonicalWriter::new();
    record.encode(&mut w);
    tagged(tag::ACCOUNT_BINDING_V1, &w.finish())
}

/// Proof that a handle was issued to a particular account.
///
/// Self-contained: it carries the binding as well as the path, so a verifier
/// needs nothing but this and an [`AccountRegistry::commitment`] — which a
/// [`Seal`](crate::Seal) publishes as
/// [`accounts`](crate::Seal::accounts).
///
/// This is what makes a sealed balance legible. A
/// [`BalanceProof`](crate::BalanceProof) proves that handle `#7` held a
/// balance; on its own that is a statement about an integer. Pairing it with
/// this proof, against the same seal, turns it into a statement about
/// `Assets:Cash` — without disclosing any other account, balance, or entry.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct AccountBindingProof {
    /// The handle and the account it names.
    pub record: AccountRecord,
    /// Path from the binding's leaf up to the registry commitment.
    pub proof: InclusionProof,
}

impl AccountBindingProof {
    /// The handle being proven.
    #[must_use]
    pub fn id(&self) -> AccountId {
        self.record.id
    }

    /// The account the handle names.
    #[must_use]
    pub fn account(&self) -> &Account {
        &self.record.account
    }

    /// Verifies the binding against an [`AccountRegistry::commitment`].
    ///
    /// Returns `false` on any inconsistency rather than distinguishing failure
    /// modes: a verifier cannot act differently on a malformed proof than on a
    /// forged one.
    #[must_use]
    pub fn verify(&self, accounts: &TreeHead) -> bool {
        // The handle *is* the leaf index, so a proof for one binding cannot be
        // replayed at another position without failing here.
        self.proof.leaf_index == u64::from(self.record.id.index())
            && self
                .proof
                .verify(&account_binding_leaf(&self.record), accounts)
    }
}

/// The set of accounts a ledger may post to.
///
/// The registry owns the account tree and answers the two questions validation
/// needs: does this account exist, and may it be posted to on this date.
#[derive(Debug, Clone, Default)]
pub struct AccountRegistry {
    accounts: Vec<Account>,
    /// Ordered so that iteration and any derived output is deterministic.
    index: BTreeMap<AccountPath, AccountId>,
    child_count: Vec<u32>,
}

impl AccountRegistry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers an account and returns its handle.
    ///
    /// Ancestors are not created implicitly: a path may be registered without
    /// its parents existing, and registering a parent later reclassifies it as a
    /// non-postable node.
    pub fn register(&mut self, account: Account) -> Result<AccountId, AccountError> {
        if let Some(closed) = account.closed_on
            && closed < account.opened_on
        {
            return Err(AccountError::ClosedBeforeOpened {
                path: account.path.clone(),
                opened: account.opened_on,
                closed,
            });
        }
        if self.index.contains_key(&account.path) {
            return Err(AccountError::AlreadyRegistered { path: account.path });
        }

        // Refused rather than saturated: a reused handle repoints history.
        let id =
            AccountId(u32::try_from(self.accounts.len()).map_err(|_| AccountError::RegistryFull)?);
        let path = account.path.clone();

        // Registering a child turns its registered ancestors into path nodes.
        for ancestor in path.ancestors() {
            if let Some(parent_id) = self.index.get(&ancestor)
                && let Some(count) = self.child_count.get_mut(parent_id.0 as usize)
            {
                *count = count.saturating_add(1);
            }
        }
        // A newly registered account may already have descendants. Paths sort
        // segment-wise, so every descendant is contiguous with `path` in the
        // index and the scan stops at the first non-descendant — rather than
        // walking the whole registry once per registration.
        let existing_children = self.descendant_range(&path).count();

        self.accounts.push(account);
        self.child_count
            .push(u32::try_from(existing_children).unwrap_or(u32::MAX));
        self.index.insert(path, id);
        Ok(id)
    }

    /// Every strict descendant of `prefix` already in the index, in path order.
    ///
    /// Relies on the segment-wise `Ord` on [`AccountPath`]: a path under
    /// `prefix` sorts after `prefix` and before the first path that is not under
    /// it, so the descendants form one contiguous run.
    fn descendant_range<'a>(
        &'a self,
        prefix: &'a AccountPath,
    ) -> impl Iterator<Item = (&'a AccountPath, &'a AccountId)> {
        self.index
            .range(prefix.clone()..)
            .skip_while(move |(path, _)| *path == prefix)
            .take_while(move |(path, _)| path.is_under(prefix))
    }

    /// Convenience registration from a path string.
    pub fn register_path(
        &mut self,
        path: &str,
        opened_on: Date,
    ) -> Result<AccountId, AccountError> {
        self.register(Account::new(AccountPath::parse(path)?, opened_on))
    }

    /// Looks up an account by path.
    #[must_use]
    pub fn id_of(&self, path: &AccountPath) -> Option<AccountId> {
        self.index.get(path).copied()
    }

    /// Returns the account behind a handle.
    #[must_use]
    pub fn get(&self, id: AccountId) -> Option<&Account> {
        self.accounts.get(id.0 as usize)
    }

    /// True when the account has no registered descendants and may be posted to.
    #[must_use]
    pub fn is_leaf(&self, id: AccountId) -> bool {
        self.child_count.get(id.0 as usize).copied().unwrap_or(0) == 0
    }

    /// Number of registered accounts.
    #[must_use]
    pub fn len(&self) -> usize {
        self.accounts.len()
    }

    /// True when nothing is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.accounts.is_empty()
    }

    /// Every registered account with its handle, in path order.
    pub fn iter(&self) -> impl Iterator<Item = (AccountId, &Account)> {
        self.index
            .values()
            .filter_map(|id| self.get(*id).map(|a| (*id, a)))
    }

    /// Closes an account from `on`, so postings after that date are refused.
    ///
    /// Closing is master data, not a journal event: it changes what may be
    /// booked next, and never what was booked already.
    pub fn close(&mut self, id: AccountId, on: Date) -> Result<(), AccountError> {
        let Some(account) = self.accounts.get_mut(id.index() as usize) else {
            return Err(AccountError::UnknownAccount { id });
        };
        if on < account.opened_on {
            return Err(AccountError::ClosedBeforeOpened {
                path: account.path.clone(),
                opened: account.opened_on,
                closed: on,
            });
        }
        account.closed_on = Some(on);
        Ok(())
    }

    /// Reopens a closed account.
    pub fn reopen(&mut self, id: AccountId) -> Result<(), AccountError> {
        let Some(account) = self.accounts.get_mut(id.index() as usize) else {
            return Err(AccountError::UnknownAccount { id });
        };
        account.closed_on = None;
        Ok(())
    }

    /// Sets which side an account's net balance may fall on.
    ///
    /// Master data, like closing: it governs what may be booked next and never
    /// what was booked already. Tightening a limit on an account that already
    /// breaches it is therefore permitted, and nothing recorded is invalidated.
    ///
    /// What it does mean is that the account is then frozen against any further
    /// movement the wrong way, *including a partial repair*: the rule is that
    /// the balance an accepted entry leaves behind satisfies the limit, and a
    /// half-repaired balance does not. One entry that brings it back over the
    /// line is accepted; two that each get halfway are not. Lift the limit if
    /// that is what you need, so the exception is a deliberate act on the
    /// record rather than a rule that quietly bends.
    pub fn set_limit(&mut self, id: AccountId, limit: BalanceLimit) -> Result<(), AccountError> {
        let Some(account) = self.accounts.get_mut(id.index() as usize) else {
            return Err(AccountError::UnknownAccount { id });
        };
        account.limit = limit;
        Ok(())
    }

    /// The limit on an account, or [`BalanceLimit::Unlimited`] if it is not
    /// registered.
    #[must_use]
    pub fn limit_of(&self, id: AccountId) -> BalanceLimit {
        self.get(id).map_or(BalanceLimit::Unlimited, |a| a.limit)
    }

    /// Every account with its handle, in handle order.
    ///
    /// The index cast cannot saturate: [`AccountRegistry::register`] refuses to
    /// issue a handle beyond [`MAX_ACCOUNTS`], so every position here fits.
    #[must_use]
    pub fn records(&self) -> Vec<AccountRecord> {
        self.accounts
            .iter()
            .enumerate()
            .map(|(i, account)| AccountRecord {
                id: AccountId(u32::try_from(i).unwrap_or(u32::MAX)),
                account: account.clone(),
            })
            .collect()
    }

    /// Restores one stored binding, at the handle it was issued.
    ///
    /// For a backend rehydrating a registry one record at a time, and for
    /// pushing a master-data change back to one. Unlike
    /// [`AccountRegistry::register`], which issues the next free handle, this
    /// insists the record land where it says it belongs — a binding restored at
    /// a different position would repoint every posting row that names it.
    ///
    /// At a handle already held, the **path is immutable** and everything else
    /// is master data: the classification, the open window and the balance limit
    /// are taken from the record. That is what makes closing an account or
    /// tightening a limit durable, and it is safe for the same reason those are
    /// mutable in the first place — they govern what may be booked next, never
    /// what was booked already. Restoring an unchanged record is therefore a
    /// no-op, so a backend may replay its whole account table on start-up.
    ///
    /// # Errors
    ///
    /// Returns [`AccountError::NotDense`] when the handle is neither held nor
    /// the next one, and [`AccountError::AlreadyRegistered`] when the handle is
    /// held by a *different path* — which would repoint history.
    pub fn restore(&mut self, record: AccountRecord) -> Result<(), AccountError> {
        let next = u32::try_from(self.accounts.len()).unwrap_or(u32::MAX);
        if let Some(existing) = self.get(record.id) {
            if existing.path != record.account.path {
                return Err(AccountError::AlreadyRegistered {
                    path: existing.path.clone(),
                });
            }
            if let Some(closed) = record.account.closed_on
                && closed < record.account.opened_on
            {
                return Err(AccountError::ClosedBeforeOpened {
                    path: record.account.path,
                    opened: record.account.opened_on,
                    closed,
                });
            }
            // The path is unchanged, so the index, the child counts and every
            // handle stay exactly as they were; only the mutable fields move.
            if let Some(slot) = self.accounts.get_mut(record.id.index() as usize) {
                *slot = record.account;
            }
            return Ok(());
        }
        if record.id.index() != next {
            return Err(AccountError::NotDense {
                expected: next,
                found: record.id.index(),
            });
        }
        self.register(record.account).map(|_| ())
    }

    /// Rebuilds a registry from stored bindings.
    ///
    /// Restores each account at the handle it was issued rather than reissuing
    /// handles in iteration order, which is what makes a restart safe.
    ///
    /// # Errors
    ///
    /// Returns [`AccountError::NotDense`] if the handles are not `0..n` exactly
    /// once each, and [`AccountError::AlreadyRegistered`] if two records share a
    /// path. Either means the stored set is not the one the handles were issued
    /// against, and posting against it would corrupt history.
    pub fn from_records(
        records: impl IntoIterator<Item = AccountRecord>,
    ) -> Result<Self, AccountError> {
        let mut records: Vec<_> = records.into_iter().collect();
        records.sort_by_key(|r| r.id.index());

        let mut registry = Self::new();
        for record in records {
            registry.restore(record)?;
        }
        Ok(registry)
    }

    /// A Merkle head over every handle-to-account binding, in handle order.
    ///
    /// Two registries agree on this only if they agree on every account *and*
    /// on the handle each was issued. Comparing a locally built registry
    /// against the stored one turns a silent repointing into a caught mismatch.
    ///
    /// A [`Seal`](crate::Seal) records this alongside its trial-balance head, so
    /// the handles that head is keyed on are pinned to the paths they meant at
    /// the moment of sealing. Without it, renumbering the registry afterwards
    /// would leave every seal and every balance proof verifying while every
    /// balance referred to a different account.
    ///
    /// A head and not a bare root, because the size is half of what an
    /// [`AccountBindingProof`] is checked against — see
    /// [`InclusionProof::verify`](crate::merkle::InclusionProof::verify). It
    /// also states how many handles the registry had issued, which is the number
    /// a verifier needs to know that a handle it was shown is one the registry
    /// had actually reached.
    #[must_use]
    pub fn commitment(&self) -> TreeHead {
        self.binding_log().head()
    }

    /// The Merkle log the commitment is the root of.
    ///
    /// Leaves are in handle order, so a handle is its own leaf index — which is
    /// what makes [`AccountRegistry::prove_binding`] a direct lookup.
    fn binding_log(&self) -> MerkleLog {
        MerkleLog::from_leaves(self.records().iter().map(account_binding_leaf).collect())
    }

    /// Proves which account a handle was issued to.
    ///
    /// The companion to a [`BalanceProof`](crate::BalanceProof). That proof
    /// establishes what handle `#7` held; this one establishes that `#7` is
    /// `Assets:Cash`. Together with a [`Seal`](crate::Seal) they make a closing
    /// balance a self-describing claim, checkable by someone who holds neither
    /// the trial balance nor the chart of accounts.
    ///
    /// Returns `None` when the handle is not registered.
    #[must_use]
    pub fn prove_binding(&self, id: AccountId) -> Option<AccountBindingProof> {
        let record = AccountRecord {
            id,
            account: self.get(id)?.clone(),
        };
        let proof = self
            .binding_log()
            .inclusion_proof(u64::from(id.index()))
            .ok()?;
        Some(AccountBindingProof { record, proof })
    }

    /// Handles of every account at or below `prefix`, in path order.
    ///
    /// Includes `prefix` itself when it is registered, so this is the set a
    /// rollup over that node covers.
    #[must_use]
    pub fn descendants_of(&self, prefix: &AccountPath) -> Vec<AccountId> {
        self.index
            .range(prefix.clone()..)
            .take_while(|(path, _)| path.is_under(prefix))
            .map(|(_, id)| *id)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::date;

    fn reg() -> AccountRegistry {
        AccountRegistry::new()
    }

    #[test]
    fn parses_and_displays_a_path() {
        let p = AccountPath::parse("Assets:Bank:Main").expect("valid");
        assert_eq!(p.depth(), 3);
        assert_eq!(p.to_string(), "Assets:Bank:Main");
    }

    #[test]
    fn rejects_malformed_paths() {
        assert_eq!(AccountPath::parse(""), Err(AccountError::Empty));
        assert!(matches!(
            AccountPath::parse("Assets::Main"),
            Err(AccountError::EmptySegment { position: 1 })
        ));
        assert!(matches!(
            AccountPath::parse("Assets:"),
            Err(AccountError::EmptySegment { position: 1 })
        ));
        assert!(matches!(
            AccountPath::parse(&"x".repeat(MAX_SEGMENT_LEN + 1)),
            Err(AccountError::SegmentTooLong { position: 0 })
        ));
        assert!(matches!(
            AccountPath::parse("Assets:Ba\nk"),
            Err(AccountError::ControlCharacter { position: 1 })
        ));
    }

    #[test]
    fn rejects_excessive_depth() {
        let deep = (0..=MAX_DEPTH).map(|i| i.to_string()).collect::<Vec<_>>();
        assert_eq!(AccountPath::from_segments(deep), Err(AccountError::TooDeep));
    }

    #[test]
    fn computes_ancestry() {
        let p = AccountPath::parse("A:B:C").expect("valid");
        assert_eq!(p.parent().expect("has parent").to_string(), "A:B");
        let ancestors: Vec<String> = p.ancestors().iter().map(ToString::to_string).collect();
        assert_eq!(ancestors, vec!["A", "A:B"]);
        assert!(AccountPath::parse("A").expect("valid").parent().is_none());
    }

    #[test]
    fn is_under_matches_whole_segments_only() {
        let child = AccountPath::parse("Assets:Bank").expect("valid");
        let prefix = AccountPath::parse("Assets").expect("valid");
        let decoy = AccountPath::parse("Asset").expect("valid");
        assert!(child.is_under(&prefix));
        assert!(child.is_under(&child));
        assert!(!child.is_under(&decoy));
    }

    #[test]
    fn registers_and_looks_up() {
        let mut r = reg();
        let id = r
            .register_path("Assets:Bank", date!(2026 - 01 - 01))
            .expect("registers");
        let path = AccountPath::parse("Assets:Bank").expect("valid");
        assert_eq!(r.id_of(&path), Some(id));
        assert_eq!(r.get(id).expect("exists").path, path);
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn rejects_duplicate_registration() {
        let mut r = reg();
        r.register_path("Assets", date!(2026 - 01 - 01))
            .expect("registers");
        assert!(matches!(
            r.register_path("Assets", date!(2026 - 01 - 01)),
            Err(AccountError::AlreadyRegistered { .. })
        ));
    }

    #[test]
    fn rejects_closing_before_opening() {
        let mut r = reg();
        let acct = Account::new(
            AccountPath::parse("A").expect("valid"),
            date!(2026 - 06 - 01),
        )
        .closing_on(date!(2026 - 01 - 01));
        assert!(matches!(
            r.register(acct),
            Err(AccountError::ClosedBeforeOpened { .. })
        ));
    }

    #[test]
    fn a_parent_registered_first_stops_being_a_leaf() {
        let mut r = reg();
        let parent = r
            .register_path("Assets", date!(2026 - 01 - 01))
            .expect("registers");
        assert!(r.is_leaf(parent));
        r.register_path("Assets:Bank", date!(2026 - 01 - 01))
            .expect("registers");
        assert!(!r.is_leaf(parent));
    }

    #[test]
    fn a_parent_registered_later_is_not_a_leaf() {
        let mut r = reg();
        let child = r
            .register_path("Assets:Bank", date!(2026 - 01 - 01))
            .expect("registers");
        let parent = r
            .register_path("Assets", date!(2026 - 01 - 01))
            .expect("registers");
        assert!(!r.is_leaf(parent));
        assert!(r.is_leaf(child));
    }

    #[test]
    fn open_window_is_inclusive() {
        let a = Account::new(
            AccountPath::parse("A").expect("valid"),
            date!(2026 - 01 - 01),
        )
        .closing_on(date!(2026 - 12 - 31));
        assert!(!a.is_open_on(date!(2025 - 12 - 31)));
        assert!(a.is_open_on(date!(2026 - 01 - 01)));
        assert!(a.is_open_on(date!(2026 - 12 - 31)));
        assert!(!a.is_open_on(date!(2027 - 01 - 01)));
    }

    #[test]
    fn account_without_closing_date_stays_open() {
        let a = Account::new(
            AccountPath::parse("A").expect("valid"),
            date!(2026 - 01 - 01),
        );
        assert!(a.is_open_on(date!(2999 - 01 - 01)));
    }

    #[test]
    fn descendants_are_returned_in_path_order() {
        let mut r = reg();
        r.register_path("Assets:Cash", date!(2026 - 01 - 01))
            .expect("registers");
        r.register_path("Assets:Bank", date!(2026 - 01 - 01))
            .expect("registers");
        r.register_path("Income:Sales", date!(2026 - 01 - 01))
            .expect("registers");
        let prefix = AccountPath::parse("Assets").expect("valid");
        let found: Vec<String> = r
            .descendants_of(&prefix)
            .into_iter()
            .filter_map(|id| r.get(id).map(|a| a.path.to_string()))
            .collect();
        assert_eq!(found, vec!["Assets:Bank", "Assets:Cash"]);
    }

    #[test]
    fn normal_sides_follow_the_accounting_equation() {
        use crate::posting::Direction;
        assert_eq!(AccountKind::Asset.normal_side(), Direction::Debit);
        assert_eq!(AccountKind::Expense.normal_side(), Direction::Debit);
        assert_eq!(AccountKind::Liability.normal_side(), Direction::Credit);
        assert_eq!(AccountKind::Equity.normal_side(), Direction::Credit);
        assert_eq!(AccountKind::Income.normal_side(), Direction::Credit);
    }

    #[test]
    fn the_commitment_changes_when_a_handle_moves() {
        // The property the whole binding commitment exists for: two registries
        // holding the same paths, issued in different orders, are not the same
        // registry — because every posting row and every sealed balance names
        // an account by its handle.
        let build = |paths: [&str; 2]| {
            let mut r = reg();
            for p in paths {
                r.register_path(p, date!(2026 - 01 - 01))
                    .expect("registers");
            }
            r
        };
        let forward = build(["Assets:Cash", "Income:Sales"]);
        let swapped = build(["Income:Sales", "Assets:Cash"]);

        assert_ne!(forward.commitment(), swapped.commitment());
        // … and rebuilding the same order reproduces it exactly.
        assert_eq!(
            forward.commitment(),
            build(["Assets:Cash", "Income:Sales"]).commitment()
        );
    }

    #[test]
    fn every_binding_proves_against_the_commitment() {
        let mut r = reg();
        for i in 0..17 {
            r.register_path(&format!("Assets:A{i}"), date!(2026 - 01 - 01))
                .expect("registers");
        }
        let root = r.commitment();
        for (id, account) in r.iter() {
            let proof = r.prove_binding(id).expect("registered");
            assert!(proof.verify(&root), "binding for {id} must prove");
            assert_eq!(proof.account().path, account.path);
        }
        assert!(r.prove_binding(AccountId::from_index(99)).is_none());
    }

    #[test]
    fn a_binding_proof_does_not_survive_an_edit_to_the_account() {
        let mut r = reg();
        let id = r
            .register_path("Assets:Cash", date!(2026 - 01 - 01))
            .expect("registers");
        r.register_path("Income:Sales", date!(2026 - 01 - 01))
            .expect("registers");
        let root = r.commitment();
        let proof = r.prove_binding(id).expect("registered");
        assert!(proof.verify(&root));

        // Claiming a different path under the same handle.
        let mut renamed = proof.clone();
        renamed.record.account.path = AccountPath::parse("Assets:Slush").expect("valid");
        assert!(!renamed.verify(&root));

        // Claiming the account was open longer than it was.
        let mut reopened = proof;
        reopened.record.account.closed_on = None;
        reopened.record.account.opened_on = date!(2020 - 01 - 01);
        assert!(!reopened.verify(&root));
    }

    #[test]
    fn a_limit_constrains_the_net_and_ignores_the_gross() {
        use crate::money::Amount;
        use crate::posting::Direction;

        let mut heavy = Balance::<2>::ZERO;
        heavy
            .add(Direction::Debit, Amount::<2>::from_minor(1_000))
            .expect("ok");
        heavy
            .add(Direction::Credit, Amount::<2>::from_minor(1_000))
            .expect("ok");

        // Turnover in both directions, net zero: every limit tolerates it.
        for limit in [
            BalanceLimit::Unlimited,
            BalanceLimit::NoCreditBalance,
            BalanceLimit::NoDebitBalance,
        ] {
            assert!(limit.permits(&heavy), "{limit} should permit a zero net");
        }

        let mut overdrawn = Balance::<2>::ZERO;
        overdrawn
            .add(Direction::Credit, Amount::<2>::from_minor(1))
            .expect("ok");
        assert!(BalanceLimit::Unlimited.permits(&overdrawn));
        assert!(!BalanceLimit::NoCreditBalance.permits(&overdrawn));
        assert!(BalanceLimit::NoDebitBalance.permits(&overdrawn));
    }

    #[test]
    fn a_limit_is_part_of_what_the_account_is() {
        // The limit is inside the binding commitment, so relaxing one after the
        // fact cannot pass unnoticed by a seal that named the account.
        let mut r = reg();
        let id = r
            .register_path("Assets:Cash", date!(2026 - 01 - 01))
            .expect("registers");
        let before = r.commitment();
        r.set_limit(id, BalanceLimit::NoCreditBalance)
            .expect("registered");
        assert_eq!(r.limit_of(id), BalanceLimit::NoCreditBalance);
        assert_ne!(r.commitment(), before);

        assert!(matches!(
            r.set_limit(AccountId::from_index(9), BalanceLimit::Unlimited),
            Err(AccountError::UnknownAccount { .. })
        ));
        assert_eq!(
            r.limit_of(AccountId::from_index(9)),
            BalanceLimit::Unlimited
        );
    }

    #[test]
    fn restoring_a_binding_updates_its_master_data_but_never_its_path() {
        // The write direction of a restart. A store that could only ever insert
        // could not close an account or tighten a limit — the registry's own
        // mutators would have nowhere to go the moment a ledger became durable.
        let mut r = reg();
        let id = r
            .register_path("Assets:Cash", date!(2026 - 01 - 01))
            .expect("registers");

        let mut record = AccountRecord {
            id,
            account: Account::new(
                AccountPath::parse("Assets:Cash").expect("valid"),
                date!(2026 - 01 - 01),
            ),
        };
        // Restoring it unchanged is a no-op, so a backend may replay its table.
        r.restore(record.clone()).expect("unchanged");
        assert_eq!(r.len(), 1);

        record.account = record
            .account
            .clone()
            .with_kind(AccountKind::Asset)
            .closing_on(date!(2026 - 12 - 31))
            .limited_to(BalanceLimit::NoCreditBalance);
        r.restore(record.clone()).expect("master data moves");
        let stored = r.get(id).expect("registered");
        assert_eq!(stored.kind, Some(AccountKind::Asset));
        assert_eq!(stored.closed_on, Some(date!(2026 - 12 - 31)));
        assert_eq!(stored.limit, BalanceLimit::NoCreditBalance);
        assert_eq!(r.len(), 1, "an update must not issue a second handle");

        // The path is not master data: rebinding a handle would repoint every
        // posting row and every sealed balance that names it.
        let mut rebound = record.clone();
        rebound.account.path = AccountPath::parse("Assets:Slush").expect("valid");
        assert!(matches!(
            r.restore(rebound),
            Err(AccountError::AlreadyRegistered { .. })
        ));
        assert_eq!(
            r.get(id).expect("registered").path.to_string(),
            "Assets:Cash"
        );

        // An update is still refused if it would invert the open window.
        let mut inverted = record;
        inverted.account.closed_on = Some(date!(2020 - 01 - 01));
        assert!(matches!(
            r.restore(inverted),
            Err(AccountError::ClosedBeforeOpened { .. })
        ));
    }

    #[test]
    fn path_hash_is_stable_and_distinguishing() {
        let a = AccountPath::parse("A:B").expect("valid");
        let b = AccountPath::parse("A:B").expect("valid");
        let c = AccountPath::parse("AB").expect("valid");
        assert_eq!(a.content_hash(), b.content_hash());
        assert_ne!(a.content_hash(), c.content_hash());
    }
}
