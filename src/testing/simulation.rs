//! The seeded model-based simulation: a swarm of
//! operations — valid and mutated — run against the reference [`Journal`]
//! and a [`Ledger`] over the store under test, every outcome compared
//! exactly, reads compared with an independent oracle at sampled steps, and
//! the store invariants and an audit at the end.

use std::collections::BTreeSet;

use time::{Date, Month};

use super::model::{Check, Oracle, SplitMix64, check_invariants, proofs_verify, store_failed};
use super::reports::{export_checks, report_checks};
use super::transparency::transparency_checks;
use crate::PageRead;
use crate::core::{BalanceKey, BatchItem, BatchReceipt, ChangeInput, Clearing, Done, Op};
use crate::error::{Error, Rejection};
use crate::model::{
    AccountKind, AccountRef, Amount, BalanceLimit, CHF, Currency, EUR, Entry, EntryId, GBP,
    HoldAction, JPY, Label, Layer, LedgerId, LedgerPolicy, MemoSalt, PostAmount, Posting,
    PostingRef, Side, USD,
};
use crate::store::{
    BalanceRead, BalanceScope, EntryRef, Erasure, HoldRead, Memo, PostingCursor, PostingOrder,
    PostingRead, Store,
};
use crate::{AccountSpec, Journal, Ledger};

/// What a step changed or returned, comparable across the reference and the
/// store.
type Outcome<const P: u8> = Result<Done<P>, Rejection>;

/// The kinds of operation, in weight order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Entry,
    Account,
    Change,
    Currency,
    Period,
    Seal,
    CloseYear,
    Reverse,
    Clear,
    Reset,
    Erase,
    Retry,
    Policy,
    Hold,
    Resolve,
    Batch,
}

const KINDS: [Kind; 16] = [
    Kind::Entry,
    Kind::Account,
    Kind::Change,
    Kind::Currency,
    Kind::Period,
    Kind::Seal,
    Kind::CloseYear,
    Kind::Reverse,
    Kind::Clear,
    Kind::Reset,
    Kind::Erase,
    Kind::Retry,
    Kind::Policy,
    Kind::Hold,
    Kind::Resolve,
    Kind::Batch,
];

/// The options of one seed (swarm testing: each seed exercises a different
/// mix).
#[derive(Debug, Clone)]
struct Swarm {
    weights: [u64; 16],
    mutate: u64,
    retry: u64,
    read: u64,
    memo: u64,
    dimensions: u64,
    limited: u64,
    accounts: usize,
}

impl Swarm {
    fn of(random: &mut SplitMix64) -> Self {
        let mut weights = [0u64; 16];
        for weight in &mut weights {
            *weight = random.below(11);
        }
        if let Some(entry) = weights.first_mut() {
            *entry = (*entry).max(1).saturating_add(10);
        }
        Self {
            weights,
            mutate: random.below(31),
            retry: random.below(21),
            read: random.between(5, 20),
            memo: random.below(60),
            dimensions: random.below(50),
            limited: random.below(50),
            accounts: usize::try_from(random.between(4, 40)).unwrap_or(4),
        }
    }

    fn kind(&self, random: &mut SplitMix64) -> Kind {
        let total: u64 = self.weights.iter().copied().fold(0, u64::saturating_add);
        let mut roll = random.below(total);
        for (kind, weight) in KINDS.iter().zip(self.weights) {
            if roll < weight {
                return *kind;
            }
            roll = roll.saturating_sub(weight);
        }
        Kind::Entry
    }
}

/// What the generator knows of the books, from the reference's outcomes.
#[derive(Debug, Default)]
struct World<const P: u8> {
    accounts: Vec<(String, AccountKind)>,
    currencies: Vec<(Currency, u8)>,
    months: u32,
    sealed: u32,
    entries: Vec<(EntryId, Date)>,
    retries: Vec<Entry<P>>,
    applies: Vec<u64>,
    pending: Vec<Entry<P>>,
    keys: u64,
    /// The recorded holds: id, booking date, expiry, amount, and the
    /// exponent of their currency.
    holds: Vec<(EntryId, Date, Option<Date>, Amount<P>, u8)>,
}

/// The first day of month `index` (0 = January 2026).
pub(super) fn month_start(index: u32) -> Option<Date> {
    let year = 2026i32.checked_add(i32::try_from(index.checked_div(12)?).ok()?)?;
    let month = Month::try_from(u8::try_from(index.checked_rem(12)?.checked_add(1)?).ok()?).ok()?;
    Date::from_calendar_date(year, month, 1).ok()
}

/// The last day of month `index`.
fn month_end(index: u32) -> Option<Date> {
    month_start(index.checked_add(1)?)?.previous_day()
}

/// The id `YYYY-MM` of month `index`.
pub(super) fn period_id(index: u32) -> Option<String> {
    let start = month_start(index)?;
    Some(format!(
        "{:04}-{:02}",
        start.year(),
        u8::from(start.month())
    ))
}

/// `date` plus `days`.
pub(super) fn plus(date: Date, days: i64) -> Date {
    i32::try_from(i64::from(date.to_julian_day()).saturating_add(days))
        .ok()
        .and_then(|day| Date::from_julian_day(day).ok())
        .unwrap_or(date)
}

const TOPS: [(&str, AccountKind); 5] = [
    ("Assets", AccountKind::Asset),
    ("Liabilities", AccountKind::Liability),
    ("Equity", AccountKind::Equity),
    ("Income", AccountKind::Income),
    ("Expenses", AccountKind::Expense),
];
const SEGMENTS: [&str; 4] = ["A", "B", "C", "D"];
const LEAVES: [&str; 2] = ["X", "Y"];
pub(super) const AXES: [&str; 2] = ["region", "team"];
pub(super) const VALUES: [&str; 4] = ["eu", "us", "Zebra", "apple"];

/// A generator of operations for one seed.
struct Generator<const P: u8> {
    random: SplitMix64,
    swarm: Swarm,
    world: World<P>,
}

/// One step: an operation for both façades, or a memo erasure.
#[derive(Debug, Clone)]
enum Step<const P: u8> {
    Op(Op<P>),
    Erase(EntryId),
}

impl<const P: u8> Generator<P> {
    fn new(seed: u64) -> Self {
        let mut random = SplitMix64::new(seed);
        let swarm = Swarm::of(&mut random);
        Self {
            random,
            swarm,
            world: World::default(),
        }
    }

    /// The ops every run starts with: a currency, a few accounts, a quarter.
    fn setup(&mut self) -> Vec<Op<P>> {
        let mut ops = vec![self.currency_op(EUR)];
        for _ in 0..6 {
            ops.push(self.account_op());
        }
        for month in 0..3 {
            if let Some(op) = self.period_op(month) {
                ops.push(op);
            }
        }
        ops
    }

    fn next_key(&mut self, prefix: &str) -> String {
        self.world.keys = self.world.keys.saturating_add(1);
        format!("{prefix}-{}", self.world.keys)
    }

    fn currency_op(&mut self, code: Currency) -> Op<P> {
        let exponent =
            u8::try_from(self.random.below(u64::from(P.min(4)).saturating_add(1))).unwrap_or(0);
        let exponent = if code == EUR { P.min(2) } else { exponent };
        Op::Currency {
            code,
            exponent,
            effective: plus(month_start(0).unwrap_or(Date::MIN), 0),
            actor: String::from("sim"),
        }
    }

    fn account_op(&mut self) -> Op<P> {
        let (top, kind) = *self
            .random
            .pick(&TOPS)
            .unwrap_or(&("Assets", AccountKind::Asset));
        let segment = self.random.pick(&SEGMENTS).copied().unwrap_or("A");
        let path = if self.random.chance(40) {
            let leaf = self.random.pick(&LEAVES).copied().unwrap_or("X");
            format!("{top}:{segment}:{leaf}")
        } else {
            format!("{top}:{segment}")
        };
        let mut spec = AccountSpec::new(&path, kind, month_start(0).unwrap_or(Date::MIN));
        if kind == AccountKind::Liability && self.random.chance(self.swarm.limited) {
            spec = spec.limit(BalanceLimit::NoDebitBalance);
        }
        Op::Register {
            spec,
            actor: String::from("sim"),
        }
    }

    fn period_op(&self, index: u32) -> Option<Op<P>> {
        Some(Op::Period {
            id: period_id(index)?,
            start: month_start(index)?,
            end: month_end(index)?,
        })
    }

    /// A random account path (sometimes one that does not exist).
    fn path(&mut self) -> String {
        match self.random.pick(&self.world.accounts) {
            Some((path, _)) => path.clone(),
            None => String::from("Assets:A"),
        }
    }

    /// A booking date in a defined period, after the sealed ones (mostly).
    fn date(&mut self) -> Date {
        let first = self.world.sealed;
        let months = self.world.months.max(1);
        let month = if first < months {
            first.saturating_add(
                u32::try_from(self.random.below(u64::from(months.saturating_sub(first))))
                    .unwrap_or(0),
            )
        } else {
            months.saturating_sub(1)
        };
        let day = i64::try_from(self.random.below(28)).unwrap_or(0);
        plus(month_start(month).unwrap_or(Date::MIN), day)
    }

    fn amount(&mut self, exponent: u8) -> Amount<P> {
        let unit = 10i128
            .checked_pow(u32::from(P.saturating_sub(exponent)))
            .unwrap_or(1);
        let units = i128::from(self.random.between(1, 50_000));
        Amount::from_minor(units.saturating_mul(unit)).unwrap_or(Amount::ZERO)
    }

    fn posting(
        &mut self,
        side: Side,
        path: String,
        amount: Amount<P>,
        currency: Currency,
    ) -> Posting<P> {
        let mut posting = match side {
            Side::Debit => Posting::debit(path, amount, currency),
            Side::Credit => Posting::credit(path, amount, currency),
        };
        if self.random.chance(self.swarm.dimensions) {
            let axis = self.random.pick(&AXES).copied().unwrap_or("region");
            let value = self.random.pick(&VALUES).copied().unwrap_or("eu");
            posting = posting.dimension(axis, value);
        }
        posting
    }

    /// A valid-by-construction entry draft (unless mutated).
    fn entry(&mut self) -> Entry<P> {
        let key = self.next_key("e");
        let (currency, exponent) = self
            .random
            .pick(&self.world.currencies)
            .copied()
            .unwrap_or((EUR, 2));
        let amount = self.amount(exponent);
        let (debit, credit) = (self.path(), self.path());
        let date = self.date();
        let mut draft = Entry::new(key.as_str(), date)
            .posting(self.posting(Side::Debit, debit, amount, currency))
            .posting(self.posting(Side::Credit, credit, amount, currency));
        if self.random.chance(self.swarm.memo) {
            let mut salt = [0u8; 16];
            for byte in &mut salt {
                *byte = u8::try_from(self.random.below(256)).unwrap_or(0);
            }
            draft = draft.memo_salted(format!("memo {key}"), MemoSalt(salt));
        }
        if self.random.chance(10) {
            draft = draft.value_date(plus(date, i64::try_from(self.random.below(9)).unwrap_or(0)));
        }
        if self.random.chance(self.swarm.mutate) {
            draft = self.mutate(draft, key, date, currency, amount);
        }
        draft
    }

    /// A hold draft between two random accounts, mostly with an expiry date
    /// (sometimes on the booking date itself, which is refused).
    fn hold(&mut self) -> Entry<P> {
        let key = self.next_key("h");
        let (currency, exponent) = self
            .random
            .pick(&self.world.currencies)
            .copied()
            .unwrap_or((EUR, 2));
        let amount = self.amount(exponent);
        let (debit, credit) = (self.path(), self.path());
        let date = self.date();
        let draft = Entry::open_hold(key.as_str(), date);
        let draft = if self.random.chance(60) {
            draft.expires(plus(
                date,
                i64::try_from(self.random.below(25)).unwrap_or(0),
            ))
        } else {
            draft
        };
        draft
            .posting(self.posting(Side::Debit, debit, amount, currency))
            .posting(self.posting(Side::Credit, credit, amount, currency))
    }

    /// A resolution of a recorded hold: a full or partial post (sometimes
    /// beyond the hold, or finer than its currency), a void or an expiry,
    /// dated around the hold's booking or expiry date.
    fn resolve(&mut self) -> Option<Op<P>> {
        let (hold, booked, expires, amount, exponent) = *self.random.pick(&self.world.holds)?;
        let key = self.next_key("r");
        let base = match expires {
            Some(expires) if self.random.chance(50) => expires,
            _ => booked,
        };
        let date = plus(
            base,
            i64::try_from(self.random.below(6))
                .unwrap_or(0)
                .saturating_sub(1),
        );
        let draft = match self.random.below(4) {
            0 => Entry::post_hold(key.as_str(), date, hold, PostAmount::Full),
            1 => {
                let unit = 10i128
                    .checked_pow(u32::from(P.saturating_sub(exponent)))
                    .unwrap_or(1);
                let units = amount.to_minor().checked_div(unit).unwrap_or(1);
                let part = if self.random.chance(10) {
                    1
                } else {
                    i128::from(
                        self.random
                            .between(1, u64::try_from(units).unwrap_or(1).saturating_add(1)),
                    )
                    .saturating_mul(unit)
                };
                let part = Amount::from_minor(part).unwrap_or(amount);
                Entry::post_hold(key.as_str(), date, hold, PostAmount::Partial(part))
            }
            2 => Entry::void_hold(key.as_str(), date, hold),
            _ => Entry::expire_hold(key.as_str(), date, hold),
        };
        Some(Op::Record(Box::new(draft)))
    }

    /// A draft broken in one way the core must refuse (or, by chance, not).
    fn mutate(
        &mut self,
        draft: Entry<P>,
        key: String,
        date: Date,
        currency: Currency,
        amount: Amount<P>,
    ) -> Entry<P> {
        let one = Amount::from_minor(1).unwrap_or(Amount::ZERO);
        match self.random.below(8) {
            0 => draft.credit(self.path(), one, currency),
            1 => Entry::new(key.as_str(), date)
                .debit("Assets:Nope", amount, currency)
                .credit(self.path(), amount, currency),
            2 => {
                let path = self.path();
                let parent = path
                    .rsplit_once(':')
                    .map_or(path.clone(), |(parent, _)| parent.to_owned());
                Entry::new(key.as_str(), date)
                    .debit(parent, amount, currency)
                    .credit(path, amount, currency)
            }
            3 => Entry::new(key.as_str(), date)
                .debit(self.path(), amount, GBP)
                .credit(self.path(), amount, GBP),
            4 => Entry::new(key.as_str(), date)
                .debit(self.path(), one, currency)
                .credit(self.path(), one, currency),
            5 => {
                let early = plus(date, -400);
                Entry::new(key.as_str(), early)
                    .debit(self.path(), amount, currency)
                    .credit(self.path(), amount, currency)
            }
            6 => {
                let huge = Amount::from_minor(
                    i128::from(self.random.between(1, 1_000_000)).saturating_mul(1_000_000),
                )
                .unwrap_or(amount);
                let wallet = self
                    .world
                    .accounts
                    .iter()
                    .find(|(_, kind)| *kind == AccountKind::Liability)
                    .map(|(path, _)| path.clone());
                let wallet = wallet.unwrap_or_else(|| self.path());
                Entry::new(key.as_str(), date)
                    .debit(wallet, huge, currency)
                    .credit(self.path(), huge, currency)
            }
            _ => Entry::new(key.as_str(), date),
        }
    }

    /// The next step.
    fn step(&mut self, journal: &Journal<P>, oracle: &Oracle<P>) -> Step<P> {
        if let Some(draft) = self.world.pending.pop() {
            return Step::Op(Op::Record(Box::new(draft)));
        }
        let kind = if !self.world.retries.is_empty() && self.random.chance(self.swarm.retry) {
            Kind::Retry
        } else {
            self.swarm.kind(&mut self.random)
        };
        if kind == Kind::Erase {
            let ids: Vec<EntryId> = oracle
                .entries
                .iter()
                .filter(|(_, known)| matches!(known.memo, Memo::Kept { .. }))
                .map(|(id, _)| *id)
                .collect();
            if let Some(id) = self.random.pick(&ids) {
                return Step::Erase(*id);
            }
        }
        match self.op(kind, journal, oracle) {
            Some(op) => Step::Op(op),
            None => Step::Op(Op::Record(Box::new(self.entry()))),
        }
    }

    /// An operation of `kind`; `None` when the books offer no target for it.
    fn op(&mut self, kind: Kind, journal: &Journal<P>, oracle: &Oracle<P>) -> Option<Op<P>> {
        match kind {
            Kind::Entry | Kind::Erase => Some(Op::Record(Box::new(self.entry()))),
            Kind::Account => {
                (self.world.accounts.len() < self.swarm.accounts).then(|| self.account_op())
            }
            Kind::Change => {
                let account = AccountRef::Path(self.path().parse().ok()?);
                let change = match self.random.below(4) {
                    0 => ChangeInput::Limit(BalanceLimit::NoDebitBalance),
                    1 => ChangeInput::Limit(BalanceLimit::Unlimited),
                    2 => ChangeInput::OpenTo(Some(self.date())),
                    _ => ChangeInput::OpenTo(None),
                };
                Some(Op::Change {
                    account,
                    change,
                    effective: self.date(),
                    actor: String::from("sim"),
                })
            }
            Kind::Currency => {
                let code = self
                    .random
                    .pick(&[USD, JPY, CHF, EUR])
                    .copied()
                    .unwrap_or(USD);
                Some(self.currency_op(code))
            }
            Kind::Period => self.period_op(self.world.months),
            Kind::Seal => (self.world.sealed < self.world.months).then(|| Op::Seal {
                period: period_id(self.world.sealed).unwrap_or_default(),
            }),
            Kind::CloseYear => Some(Op::CloseYear {
                key: self.next_key("close"),
                on: self.date(),
                equity: AccountRef::Path(self.path().parse().ok()?),
            }),
            Kind::Reverse => {
                let (id, _) = *self.random.pick(&self.world.entries)?;
                let key = self.next_key("rev");
                let date = self.date();
                journal
                    .reversal(id, &key, date)
                    .ok()
                    .map(|draft| Op::Record(Box::new(draft)))
            }
            Kind::Clear => self.clear(oracle),
            Kind::Reset => {
                let clearing = *self.random.pick(&self.world.applies)?;
                Some(Op::ResetClearing {
                    key: self.next_key("reset"),
                    clearing,
                    date: self.date(),
                })
            }
            Kind::Retry => {
                let draft = self.random.pick(&self.world.retries)?.clone();
                if self.random.chance(50) {
                    return Some(Op::Record(Box::new(draft)));
                }
                let key = draft.key()?.as_str().to_owned();
                let (debit, credit, amount) = (self.path(), self.path(), self.amount(0));
                Some(Op::Record(Box::new(
                    Entry::new(key.as_str(), draft.booking_date())
                        .debit(debit, amount, EUR)
                        .credit(credit, amount, EUR),
                )))
            }
            Kind::Hold => Some(Op::Record(Box::new(self.hold()))),
            Kind::Resolve => self.resolve(),
            Kind::Batch => Some(self.batch(journal, oracle)),
            Kind::Policy => {
                let required: Vec<Label> = if self.random.chance(30) {
                    Label::new("region").ok().into_iter().collect()
                } else {
                    Vec::new()
                };
                let drift = self
                    .random
                    .chance(50)
                    .then(|| u32::try_from(self.random.below(10)).unwrap_or(0));
                let policy = LedgerPolicy::new(required, drift).ok()?;
                Some(Op::Policy {
                    policy,
                    effective: self.date(),
                    actor: String::from("sim"),
                })
            }
        }
    }

    /// An atomic batch of one to six drafts: entries (some mutated), holds
    /// and their resolutions — sometimes of a hold opened earlier in the same
    /// batch — retries, and repeats of a draft of the batch itself.
    fn batch(&mut self, journal: &Journal<P>, oracle: &Oracle<P>) -> Op<P> {
        let mut items: Vec<BatchItem<P>> = Vec::new();
        for _ in 0..self.random.between(1, 7) {
            // Clearings and resets of committed postings, mixed in; a
            // repeat of an earlier clearing under a new key over-applies
            // across the batch's own items.
            match self.random.below(14) {
                10 | 11 => {
                    if let Some(item) = self.clear(oracle).and_then(as_item) {
                        items.push(item);
                        continue;
                    }
                }
                12 => {
                    let earlier = items.iter().rev().find_map(|item| match item {
                        BatchItem::Clear(clearing) => Some(clearing.clone()),
                        _ => None,
                    });
                    if let Some(mut clearing) = earlier {
                        clearing.key = self.next_key("clr");
                        items.push(BatchItem::Clear(clearing));
                        continue;
                    }
                }
                13 => {
                    if let Some(item) = self.op(Kind::Reset, journal, oracle).and_then(as_item) {
                        items.push(item);
                        continue;
                    }
                }
                _ => {}
            }
            let drafts: Vec<Entry<P>> = items
                .iter()
                .filter_map(|item| match item {
                    BatchItem::Record(draft) => Some(draft.clone()),
                    _ => None,
                })
                .collect();
            let draft = match self.random.below(10) {
                0 => self.hold(),
                1 => match self.op(Kind::Resolve, journal, oracle) {
                    Some(Op::Record(draft)) => *draft,
                    _ => self.entry(),
                },
                2 => match self.op(Kind::Retry, journal, oracle) {
                    Some(Op::Record(draft)) => *draft,
                    _ => self.entry(),
                },
                3 => match self.random.pick(&drafts) {
                    Some(earlier) => earlier.clone(),
                    None => self.entry(),
                },
                _ => self.entry(),
            };
            let opened = draft
                .get_hold()
                .filter(|action| matches!(action, HoldAction::Open { .. }))
                .and(draft.key().cloned())
                .map(|key| (EntryId::derive(journal.id(), &key), draft.booking_date()));
            items.push(BatchItem::Record(draft));
            if let Some((hold, booked)) = opened
                && self.random.chance(50)
            {
                let key = self.next_key("r");
                items.push(BatchItem::Record(Entry::void_hold(
                    key.as_str(),
                    plus(booked, 1),
                    hold,
                )));
            }
        }
        Op::Batch(items)
    }

    /// A clearing of a debit against a credit posting of one balance.
    fn clear(&mut self, oracle: &Oracle<P>) -> Option<Op<P>> {
        let keys: Vec<BalanceKey> = oracle.keys().into_iter().collect();
        let key = *self.random.pick(&keys)?;
        let debits: Vec<_> = oracle
            .postings
            .iter()
            .filter(|line| line.key == key && line.side == Side::Debit)
            .collect();
        let credits: Vec<_> = oracle
            .postings
            .iter()
            .filter(|line| line.key == key && line.side == Side::Credit)
            .collect();
        let debit = *self.random.pick(&debits)?;
        let credit = *self.random.pick(&credits)?;
        let cap = debit.amount.to_minor().min(credit.amount.to_minor());
        let amount = Amount::from_minor(i128::from(
            self.random.between(1, u64::try_from(cap).unwrap_or(1)),
        ))
        .ok()?;
        Some(Op::Clear {
            key: self.next_key("clr"),
            debit: PostingRef::at(debit.entry, debit.index),
            credit: PostingRef::at(credit.entry, credit.index),
            amount,
            date: plus(
                debit.booking.max(credit.booking),
                i64::try_from(self.random.below(5)).unwrap_or(0),
            ),
        })
    }

    /// Learns from the reference's outcome of a step.
    fn learn(&mut self, op: &Op<P>, outcome: &Outcome<P>) {
        let Ok(done) = outcome else {
            return;
        };
        match (op, done) {
            (Op::Register { spec, .. }, Done::Account(_)) => {
                let path = spec.path().to_owned();
                if !self.world.accounts.iter().any(|(known, _)| *known == path) {
                    self.world.accounts.push((path, spec.kind()));
                }
            }
            (Op::Currency { code, exponent, .. }, Done::Changed) => {
                self.world.currencies.retain(|(known, _)| known != code);
                self.world.currencies.push((*code, *exponent));
            }
            (Op::Period { start, .. }, Done::Period(_)) => {
                let index = u32::try_from(start.year().saturating_sub(2026))
                    .unwrap_or(0)
                    .saturating_mul(12)
                    .saturating_add(u32::from(u8::from(start.month())).saturating_sub(1));
                self.world.months = self.world.months.max(index.saturating_add(1));
            }
            (Op::Seal { .. }, Done::Sealed(_)) => {
                self.world.sealed = self.world.sealed.saturating_add(1)
            }
            (Op::Record(draft), Done::Entry(recorded)) if !recorded.replayed => {
                self.world.entries.push((recorded.id, draft.booking_date()));
                if let (Some(HoldAction::Open { expires }), Some(leg)) =
                    (draft.get_hold(), draft.postings().first())
                {
                    let exponent = self
                        .world
                        .currencies
                        .iter()
                        .find(|(code, _)| *code == leg.currency())
                        .map_or(P, |(_, exponent)| *exponent);
                    self.world.holds.push((
                        recorded.id,
                        draft.booking_date(),
                        *expires,
                        leg.amount(),
                        exponent,
                    ));
                }
                if self.world.retries.len() < 128 {
                    self.world.retries.push((**draft).clone());
                }
            }
            (Op::Batch(items), Done::Batch(receipts)) => {
                for (item, receipt) in items.iter().zip(receipts) {
                    match (item, receipt) {
                        (BatchItem::Record(draft), BatchReceipt::Recorded(recorded)) => self.learn(
                            &Op::Record(Box::new(draft.clone())),
                            &Ok(Done::Entry(*recorded)),
                        ),
                        (BatchItem::Clear(_), BatchReceipt::Cleared(cleared))
                            if !cleared.replayed =>
                        {
                            self.world.applies.push(cleared.position);
                        }
                        _ => {}
                    }
                }
            }
            (Op::Clear { .. }, Done::Cleared(cleared)) if !cleared.replayed => {
                self.world.applies.push(cleared.position);
            }
            (Op::CloseYear { .. }, Done::Closing(drafts)) => {
                self.world.pending = drafts.iter().rev().cloned().collect();
            }
            _ => {}
        }
    }
}

/// A single clearing or reset as a batch item.
fn as_item<const P: u8>(op: Op<P>) -> Option<BatchItem<P>> {
    match op {
        Op::Clear {
            key,
            debit,
            credit,
            amount,
            date,
        } => Some(BatchItem::Clear(Clearing::new(
            key, debit, credit, amount, date,
        ))),
        Op::ResetClearing {
            key,
            clearing,
            date,
        } => Some(BatchItem::reset(key, clearing, date)),
        _ => None,
    }
}

/// The comparable outcome of a façade call.
fn comparable<const P: u8>(result: Result<Done<P>, Error>) -> Result<Outcome<P>, String> {
    match result {
        Ok(done) => Ok(Ok(done)),
        Err(Error::Rejected(rejection)) => Ok(Err(rejection)),
        Err(error) => Err(error.to_string()),
    }
}

/// Runs `op` on the store, retrying transient failures (up to five times);
/// returns the outcome and whether an earlier attempt may have applied.
async fn on_store<S: Store<P>, const P: u8>(
    ledger: &Ledger<S, P>,
    op: &Op<P>,
) -> Result<(Outcome<P>, bool), String> {
    let mut maybe = false;
    for _ in 0..6 {
        match ledger.run(op.clone()).await {
            Err(error) if error.is_transient() => maybe |= error.may_have_applied(),
            result => return comparable(result).map(|outcome| (outcome, maybe)),
        }
    }
    Err(String::from(
        "the store failed transiently six times in a row",
    ))
}

/// Whether the store's outcome equals the reference's: exactly, or — after
/// an attempt that may have applied — as a replay of the same receipt.
fn equal<const P: u8>(expected: &Outcome<P>, actual: &Outcome<P>, maybe: bool) -> bool {
    match (expected, actual) {
        (Ok(Done::Entry(a)), Ok(Done::Entry(b))) if maybe => {
            (a.id, a.position) == (b.id, b.position)
        }
        (Ok(Done::Batch(a)), Ok(Done::Batch(b))) if maybe => {
            let same = |receipt: &BatchReceipt| {
                (
                    receipt.position(),
                    receipt.as_recorded().map(|recorded| recorded.id),
                )
            };
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| same(a) == same(b))
        }
        (Ok(Done::Cleared(a)), Ok(Done::Cleared(b))) if maybe => a.position == b.position,
        _ => expected == actual,
    }
}

/// One seed of the simulation.
pub(crate) async fn simulate<S: Store<P>, const P: u8>(
    store: S,
    seed: u64,
    steps: u32,
    ledger: &LedgerId,
) -> Result<(), (Option<u32>, String)> {
    let mut generator = Generator::<P>::new(seed);
    let mut journal = Journal::<P>::with_precision(ledger.clone()).as_actor("sim");
    let books = Ledger::<S, P>::with_precision(store, ledger.clone())
        .await
        .map_err(|error| {
            (
                None,
                format!("the store did not create the ledger: {error}"),
            )
        })?
        .as_actor("sim");
    let setup = generator.setup();
    let mut steps_run = 0u32;
    let mut oracle = Oracle::of(&journal).map_err(|message| (None, message))?;
    let mut planned: Vec<Step<P>> = setup.into_iter().rev().map(Step::Op).collect();
    while steps_run < steps.saturating_add(10) {
        let step = match planned.pop() {
            Some(step) => step,
            None if steps_run < steps => generator.step(&journal, &oracle),
            None => break,
        };
        let at = Some(steps_run);
        match &step {
            Step::Op(op) => {
                let expected = comparable(journal.run(op))
                    .map_err(|message| (at, format!("the reference failed: {message}")))?;
                let (actual, maybe) = on_store(&books, op)
                    .await
                    .map_err(|message| (at, format!("op={op:?}: {message}")))?;
                if !equal(&expected, &actual, maybe) {
                    return Err((
                        at,
                        format!("op={op:?} expected={expected:?} actual={actual:?}"),
                    ));
                }
                generator.learn(op, &expected);
            }
            Step::Erase(id) => {
                // The generator erases only kept memos.
                journal
                    .erase_memo(*id)
                    .map_err(|error| (at, format!("the reference failed: {error}")))?;
                let erased = books
                    .store()
                    .erase_memo(*id)
                    .await
                    .map_err(|error| (at, format!("erase {id}: {error}")))?;
                if erased != Erasure::Erased {
                    return Err((
                        at,
                        format!("erasing the kept memo of {id}: expected Erased, got {erased:?}"),
                    ));
                }
            }
        }
        oracle = Oracle::of(&journal).map_err(|message| (at, message))?;
        oracle.hold_invariants().map_err(|message| (at, message))?;
        if generator.random.chance(generator.swarm.read) {
            read_checks(&books, &oracle, &mut generator.random)
                .await
                .map_err(|message| (at, message))?;
            hold_checks(&books, &journal, &oracle, &mut generator.random)
                .await
                .map_err(|message| (at, message))?;
            report_checks(&books, &journal, &mut generator.random)
                .await
                .map_err(|message| (at, message))?;
        }
        steps_run = steps_run.saturating_add(1);
    }
    let head = books
        .head()
        .await
        .map_err(|error| (None, error.to_string()))?;
    if head != journal.head() {
        return Err((
            None,
            format!(
                "the final head differs: expected {:?} actual {:?}",
                journal.head(),
                head
            ),
        ));
    }
    report_checks(&books, &journal, &mut generator.random)
        .await
        .map_err(|message| (None, message))?;
    hold_checks(&books, &journal, &oracle, &mut generator.random)
        .await
        .map_err(|message| (None, message))?;
    export_checks(&books, &journal)
        .await
        .map_err(|message| (None, message))?;
    transparency_checks(&books, &journal, &mut generator.random)
        .await
        .map_err(|message| (None, message))?;
    check_invariants(&books, &oracle, &mut generator.random)
        .await
        .map_err(|message| (None, message))
}

/// The reads of the store at a sampled step, against the oracle.
pub(crate) async fn read_checks<S: Store<P>, const P: u8>(
    books: &Ledger<S, P>,
    oracle: &Oracle<P>,
    random: &mut SplitMix64,
) -> Check {
    let store = books.store();
    // Size, root and proofs at an earlier size.
    let head = books.head().await.map_err(|error| error.to_string())?;
    if head != oracle.head_at(oracle.size)? {
        return Err(String::from("the store's head is not the reference's"));
    }
    let old = random.between(1, oracle.size.max(1));
    proofs_verify(store, oracle, random.below(old), old).await?;
    // Balances in all three scopes for a key.
    let keys: Vec<BalanceKey> = oracle.keys().into_iter().collect();
    if let Some(key) = random.pick(&keys).copied() {
        let at = random.between(1, oracle.size);
        for (scope, expected) in [
            (BalanceScope::Current, oracle.balance_at(key, oracle.size)),
            (BalanceScope::At(at), oracle.balance_at(key, at)),
        ] {
            let read = store
                .balances(&BalanceRead {
                    scope: scope.clone(),
                    keys: Some(vec![key]),
                    page: PageRead::first(1),
                })
                .await
                .map_err(|error| store_failed(&error))?;
            if read.items.first().map(|(_, balance)| *balance) != Some(expected) {
                return Err(format!(
                    "balances {scope:?} of {key}: expected {expected:?}, got {:?}",
                    read.items
                ));
            }
        }
        if let Some((period, _)) = random.pick(&oracle.seals).cloned() {
            let end = oracle
                .periods
                .get(&period)
                .map(|(_, end)| *end)
                .ok_or("a seal names no period")?;
            let expected = oracle.balance_through(key, end)?;
            let read = store
                .balances(&BalanceRead {
                    scope: BalanceScope::Sealed(period.clone()),
                    keys: Some(vec![key]),
                    page: PageRead::first(1),
                })
                .await
                .map_err(|error| store_failed(&error))?;
            if read.items.first().map(|(_, balance)| *balance) != Some(expected) {
                return Err(format!(
                    "the sealed balance of {key} in {period}: expected {expected:?}, got {:?}",
                    read.items
                ));
            }
        }
        postings_page(store, oracle, key, random).await?;
        let refs: BTreeSet<u64> = oracle.clearing_refs.get(&key).cloned().unwrap_or_default();
        let found = store
            .clearings(key, PageRead::first(PageRead::<()>::MAX_LIMIT))
            .await
            .map_err(|error| store_failed(&error))?;
        let found: BTreeSet<u64> = found.items.iter().map(|leaf| leaf.position).collect();
        if found != refs {
            return Err(format!(
                "the clearings of {key}: expected {refs:?}, got {found:?}"
            ));
        }
    }
    // Entries by id and key, and memos.
    let ids: Vec<EntryId> = oracle.entries.keys().copied().collect();
    if let Some(id) = random.pick(&ids).copied() {
        let known = oracle.entries.get(&id).ok_or("an unknown entry")?;
        let stored = store
            .entry(EntryRef::Key(&known.key), None)
            .await
            .map_err(|error| store_failed(&error))?;
        if stored.as_ref().map(|stored| (stored.id, stored.position)) != Some((id, known.position))
        {
            return Err(format!("the entry under key {}: got {stored:?}", known.key));
        }
        let memo = store.memo(id).await.map_err(|error| store_failed(&error))?;
        if memo.as_ref() != Some(&known.memo) {
            return Err(format!(
                "the memo of {id}: expected {:?}, got {memo:?}",
                known.memo
            ));
        }
        let hold = store
            .holds(&HoldRead::ById { id, through: None })
            .await
            .map_err(|error| store_failed(&error))?;
        let expected = oracle.holds.get(&id).map(|(row, _)| row);
        if hold.items.first() != expected {
            return Err(format!(
                "the hold index of entry {id}: expected {expected:?}, got {:?}",
                hold.items
            ));
        }
    }
    // Seals.
    let seals = store
        .seals(None)
        .await
        .map_err(|error| store_failed(&error))?;
    let found: Vec<_> = seals
        .into_iter()
        .map(|row| (row.period, row.position))
        .collect();
    if found != oracle.seals {
        return Err(format!(
            "the seals: expected {:?}, got {found:?}",
            oracle.seals
        ));
    }
    // Account records.
    let handles: Vec<_> = oracle.account_records.keys().copied().collect();
    if let Some(handle) = random.pick(&handles).copied() {
        let records = store
            .account_records(handle, PageRead::first(PageRead::<()>::MAX_LIMIT))
            .await
            .map_err(|error| store_failed(&error))?;
        let found: Vec<u64> = records.items.iter().map(|leaf| leaf.position).collect();
        if Some(&found) != oracle.account_records.get(&handle) {
            return Err(format!("the records of account {handle}: got {found:?}"));
        }
    }
    Ok(())
}

/// The holds at a sampled step: a random hold's state on both façades, and
/// the due holds on a random date read in pages of 1 and 50 — the store,
/// the reference and the oracle agree.
async fn hold_checks<S: Store<P>, const P: u8>(
    books: &Ledger<S, P>,
    journal: &Journal<P>,
    oracle: &Oracle<P>,
    random: &mut SplitMix64,
) -> Check {
    let ids: Vec<EntryId> = oracle.holds.keys().copied().collect();
    if let Some(id) = random.pick(&ids).copied() {
        let expected = journal.hold(id);
        let found = books.hold(id).await.map_err(|error| error.to_string())?;
        if found != expected || expected.is_none() {
            return Err(format!(
                "the hold {id}: expected {expected:?}, got {found:?}"
            ));
        }
    }
    let dates: Vec<Date> = oracle
        .holds
        .values()
        .filter_map(|(row, _)| row.expires)
        .chain([Date::MAX])
        .collect();
    let as_of = random.pick(&dates).copied().unwrap_or(Date::MAX);
    let expected: Vec<EntryId> = oracle.due(as_of).iter().map(|row| row.id).collect();
    for limit in [1u32, 50] {
        let mut page = PageRead::first(limit);
        let (mut reference, mut stored) = (Vec::new(), Vec::new());
        loop {
            let ours = journal.holds_due(as_of, page.clone());
            let theirs = books
                .holds_due(as_of, page.clone())
                .await
                .map_err(|error| error.to_string())?;
            if theirs.items != ours.items || theirs.next != ours.next {
                return Err(format!(
                    "the due holds on {as_of} (pages of {limit}): expected {:?}, got {:?}",
                    ours.items, theirs.items
                ));
            }
            reference.extend(ours.items.iter().map(|hold| hold.id));
            stored.extend(theirs.items.iter().map(|hold| hold.id));
            match ours.next {
                Some(next) => page = page.after(next),
                None => break,
            }
        }
        if reference != expected || stored != expected {
            return Err(format!(
                "the due holds on {as_of}: expected {expected:?}, got {stored:?}"
            ));
        }
    }
    Ok(())
}

/// One postings page with random order, ranges, filters and cursor, against
/// a naive filter over the oracle's postings.
async fn postings_page<S: Store<P>, const P: u8>(
    store: &S,
    oracle: &Oracle<P>,
    key: BalanceKey,
    random: &mut SplitMix64,
) -> Check {
    let order = if random.chance(50) {
        PostingOrder::Position
    } else {
        PostingOrder::Booking
    };
    let dates: Vec<Date> = oracle.postings.iter().map(|line| line.booking).collect();
    let booking = random.chance(30).then(|| {
        let a = random.pick(&dates).copied().unwrap_or(Date::MIN);
        let b = random.pick(&dates).copied().unwrap_or(Date::MAX);
        a.min(b)..=a.max(b)
    });
    let value = random.chance(20).then(|| {
        let a = random.pick(&dates).copied().unwrap_or(Date::MIN);
        a..=plus(a, 30)
    });
    let mut dimensions = Vec::new();
    for _ in 0..random.below(3) {
        let axis = random.pick(&AXES).copied().unwrap_or("region");
        let value = random.pick(&VALUES).copied().unwrap_or("eu");
        if let (Ok(axis), Ok(value)) = (Label::new(axis), Label::new(value)) {
            dimensions.push((axis, value));
        }
    }
    let mut expected: Vec<_> = oracle
        .postings
        .iter()
        .filter(|line| {
            line.key == key
                && booking
                    .as_ref()
                    .is_none_or(|range| range.contains(&line.booking))
                && value
                    .as_ref()
                    .is_none_or(|range| range.contains(&line.value))
                && dimensions
                    .iter()
                    .all(|(axis, value)| line.dimensions.get(axis) == Some(value))
        })
        .collect();
    if order == PostingOrder::Booking {
        expected.sort_by_key(|line| (line.booking, line.position, line.index));
    }
    let limit = u32::try_from(random.between(1, 50)).unwrap_or(1);
    let mut page: PageRead<PostingCursor> = PageRead::first(limit).through(oracle.size);
    let mut found = Vec::new();
    loop {
        let read = store
            .postings(&PostingRead {
                key,
                order,
                booking: booking.clone(),
                value: value.clone(),
                dimensions: dimensions.clone(),
                kinds: crate::store::KindFilter::Any,
                page: page.clone(),
            })
            .await
            .map_err(|error| store_failed(&error))?;
        if read.items.len() > usize::try_from(limit).unwrap_or(usize::MAX) {
            return Err(String::from("a postings page exceeds its limit"));
        }
        found.extend(read.items);
        match read.next {
            Some(next) => page = page.after(next),
            None => break,
        }
    }
    let expected: Vec<_> = expected
        .iter()
        .map(|line| {
            let cleared = oracle
                .cleared
                .get(&(line.position, line.index))
                .copied()
                .unwrap_or(Amount::ZERO);
            (
                line.position,
                line.index,
                line.entry,
                line.side,
                line.amount,
                line.booking,
                line.value,
                line.dimensions.clone(),
                line.balance_after,
                cleared,
            )
        })
        .collect();
    let found: Vec<_> = found
        .into_iter()
        .map(|line| {
            (
                line.position,
                line.index,
                line.entry,
                line.side,
                line.amount,
                line.booking,
                line.value,
                line.dimensions,
                line.balance_after,
                line.cleared,
            )
        })
        .collect();
    if found != expected {
        return Err(format!(
            "the postings of {key} ({order:?}, booking {booking:?}, value {value:?}, {dimensions:?}): expected {expected:?}, got {found:?}"
        ));
    }
    Ok(())
}

/// `regress_g7_seal_counts_late_booked_entries` (PoC: G7): an entry booked
/// into a period after later periods have entries is counted by that
/// period's seal — the seal reads every balance minus the postings booked
/// after its end, never only what was "sequenced" before it.
// PoC: G7 poc_g7_deferred_seal_ignores_unsequenced_entries_then_restated
pub(crate) async fn regress_g7_seal_counts_late_booked_entries<S: Store<P>, const P: u8>(
    store: S,
    ledger: &LedgerId,
) -> Check {
    let books = Ledger::<S, P>::with_precision(store, ledger.clone())
        .await
        .map_err(|error| error.to_string())?
        .as_actor("sim");
    let mut journal = Journal::<P>::with_precision(ledger.clone()).as_actor("sim");
    let day = |month: u32, day: i64| plus(month_start(month).unwrap_or(Date::MIN), day);
    let amount = Amount::<P>::from_minor(100).map_err(|error| error.to_string())?;
    let sale = |key: &str, on: Date| {
        Entry::new(key, on)
            .debit("Assets:Cash", amount, EUR)
            .credit("Income:Sales", amount, EUR)
    };
    let mut ops = vec![
        Op::Currency {
            code: EUR,
            exponent: P.min(2),
            effective: day(0, 0),
            actor: String::from("sim"),
        },
        Op::Register {
            spec: AccountSpec::new("Assets:Cash", AccountKind::Asset, day(0, 0)),
            actor: String::from("sim"),
        },
        Op::Register {
            spec: AccountSpec::new("Income:Sales", AccountKind::Income, day(0, 0)),
            actor: String::from("sim"),
        },
    ];
    for month in 0u32..2 {
        ops.push(Op::Period {
            id: format!("2026-{:02}", month.saturating_add(1)),
            start: day(month, 0),
            end: month_end(month).unwrap_or(Date::MAX),
        });
    }
    ops.push(Op::Record(Box::new(sale("jan-1", day(0, 4)))));
    ops.push(Op::Record(Box::new(sale("feb-1", day(1, 2)))));
    ops.push(Op::Record(Box::new(sale("jan-late", day(0, 30)))));
    ops.push(Op::Seal {
        period: String::from("2026-01"),
    });
    for op in &ops {
        let expected = comparable(journal.run(op))?;
        let (actual, _) = on_store(&books, op).await?;
        if expected != actual {
            return Err(format!("op={op:?} expected={expected:?} actual={actual:?}"));
        }
    }
    let oracle = Oracle::of(&journal)?;
    let key = BalanceKey {
        handle: crate::model::AccountHandle::new(0).ok_or("handle 0")?,
        currency: EUR,
        layer: Layer::Settled,
    };
    let expected = oracle.balance_through(key, day(0, 30))?;
    if expected.postings() != 2 {
        return Err(String::from(
            "the January seal must count both January entries",
        ));
    }
    let sealed = books
        .store()
        .balances(&BalanceRead {
            scope: BalanceScope::Sealed(Label::new("2026-01").map_err(|error| error.to_string())?),
            keys: Some(vec![key]),
            page: PageRead::first(1),
        })
        .await
        .map_err(|error| store_failed(&error))?;
    if sealed.items.first().map(|(_, balance)| *balance) != Some(expected) {
        return Err(format!(
            "the sealed balance counts {:?}, expected {expected:?}",
            sealed.items
        ));
    }
    Ok(())
}
