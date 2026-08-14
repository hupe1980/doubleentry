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

use std::collections::BTreeMap;

use time::Date;

use crate::canonical::{Canonical, CanonicalWriter};
use crate::hash::{Hash, tag, tagged};
use crate::merkle::MerkleLog;

/// Maximum number of characters in one path segment.
pub const MAX_SEGMENT_LEN: usize = 64;

/// Maximum depth of an account path.
pub const MAX_DEPTH: usize = 16;

/// The separator between path segments.
pub const SEPARATOR: char = ':';

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
}

impl Account {
    /// Creates an account open from `opened_on` with no closing date.
    #[must_use]
    pub fn new(path: AccountPath, opened_on: Date) -> Self {
        Self {
            path,
            kind: None,
            opened_on,
            closed_on: None,
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
    }
}

pub(crate) fn encode_date(w: &mut CanonicalWriter, d: Date) {
    w.u16(d.year().unsigned_abs().min(u32::from(u16::MAX)) as u16);
    w.bool(d.year() < 0);
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

        let id = AccountId(u32::try_from(self.accounts.len()).unwrap_or(u32::MAX));
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

    /// Every account with its handle, in handle order.
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
    /// For a backend rehydrating a registry one record at a time. Unlike
    /// [`AccountRegistry::register`], which issues the next free handle, this
    /// insists the record land where it says it belongs — a binding restored at
    /// a different position would repoint every posting row that names it.
    ///
    /// Restoring a record that is already present, unchanged, is a no-op, so a
    /// backend may replay its whole account table on start-up.
    ///
    /// # Errors
    ///
    /// Returns [`AccountError::NotDense`] when the handle is not the next one,
    /// and [`AccountError::AlreadyRegistered`] when it is already held by a
    /// different account.
    pub fn restore(&mut self, record: AccountRecord) -> Result<(), AccountError> {
        let next = u32::try_from(self.accounts.len()).unwrap_or(u32::MAX);
        if let Some(existing) = self.get(record.id) {
            if *existing == record.account {
                return Ok(());
            }
            return Err(AccountError::AlreadyRegistered {
                path: existing.path.clone(),
            });
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

    /// A hash over every handle-to-account binding.
    ///
    /// Two registries agree on this only if they agree on every account *and*
    /// on the handle each was issued. Comparing a locally built registry
    /// against the stored one turns a silent repointing into a caught mismatch.
    #[must_use]
    pub fn commitment(&self) -> Hash {
        let leaves: Vec<Hash> = self
            .records()
            .iter()
            .map(|record| {
                let mut w = CanonicalWriter::new();
                record.encode(&mut w);
                tagged(tag::ACCOUNT_BINDING_V1, &w.finish())
            })
            .collect();
        MerkleLog::from_leaves(leaves).root()
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
    fn path_hash_is_stable_and_distinguishing() {
        let a = AccountPath::parse("A:B").expect("valid");
        let b = AccountPath::parse("A:B").expect("valid");
        let c = AccountPath::parse("AB").expect("valid");
        assert_eq!(a.content_hash(), b.content_hash());
        assert_ne!(a.content_hash(), c.content_hash());
    }
}
