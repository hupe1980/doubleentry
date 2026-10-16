//! Properties of the store protocol that need the in-memory state: a
//! [`Snapshot`] filled by [`plan`] decides exactly like the state itself,
//! and the facts an entry reads do not grow with history.

use std::cell::Cell;

use time::Date;

use super::{
    AccountSpec, AccountView, BalanceKey, ChangeInput, ClearingView, Done, EntryMeta, EntryView,
    Fact, Header, HoldView, MemState, Movement, Op, Outcome, PeriodView, Slice, Snapshot,
    StateView, Terms, Versioned, execute, plan,
};
use crate::model::{
    AccountHandle, AccountKind, AccountPath, AccountRef, Amount, Balance, BalanceLimit, Currency,
    EUR, Entry, EntryId, IdempotencyKey, LedgerId, LedgerPolicy, MemoSalt, PeriodId, PostAmount,
    PostingRef, USD,
};

const ACCOUNTS: [&str; 5] = [
    "Assets:Cash",
    "Income:Sales",
    "Liabilities:Wallet",
    "Expenses:Rent",
    "Equity:Retained",
];

fn day(month: u8, day: u8) -> Date {
    Date::from_calendar_date(2026, time::Month::try_from(month).unwrap(), day).unwrap()
}

fn apply(state: &mut MemState<2>, op: &Op<2>) -> Result<Outcome<2>, crate::Rejection> {
    let outcome = execute(state, op);
    match &outcome {
        Ok(Outcome::Append(write)) => state.apply((**write).clone()).unwrap(),
        Ok(Outcome::Batch { writes, .. }) => {
            for write in writes {
                state.apply(write.clone()).unwrap();
            }
        }
        _ => {}
    }
    outcome
}

/// A ledger with EUR, five accounts (the wallet limited) and twelve months.
fn base() -> MemState<2> {
    let mut state = MemState::new(LedgerId::new("example.com/acme").unwrap());
    let mut ops = vec![
        Op::Genesis,
        Op::Currency {
            code: EUR,
            exponent: 2,
            effective: day(1, 1),
            actor: "setup".into(),
        },
    ];
    let kinds = [
        AccountKind::Asset,
        AccountKind::Income,
        AccountKind::Liability,
        AccountKind::Expense,
        AccountKind::Equity,
    ];
    for (path, kind) in ACCOUNTS.iter().zip(kinds) {
        let mut spec = AccountSpec::new(path, kind, day(1, 1));
        if kind == AccountKind::Liability {
            spec = spec.limit(BalanceLimit::NoDebitBalance);
        }
        ops.push(Op::Register {
            spec,
            actor: "setup".into(),
        });
    }
    for month in 1..=12u8 {
        let month_of = time::Month::try_from(month).unwrap();
        ops.push(Op::Period {
            id: format!("2026-{month:02}"),
            start: day(month, 1),
            end: day(month, month_of.length(2026)),
        });
    }
    for op in &ops {
        apply(&mut state, op).unwrap();
    }
    state
}

/// A deterministic generator.
struct Lcg(u64);

impl Lcg {
    fn below(&mut self, bound: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % bound
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len() as u64) as usize]
    }
}

/// What a random sequence remembers to build retries and references.
#[derive(Default)]
struct Memory {
    drafts: Vec<Entry<2>>,
    entries: Vec<EntryId>,
    holds: Vec<EntryId>,
    clearings: Vec<u64>,
    sealed: u8,
}

fn random_op(rng: &mut Lcg, memory: &mut Memory, step: usize) -> Op<2> {
    let month = 1 + rng.below(12) as u8;
    let date = day(month, 1 + rng.below(28) as u8);
    let amount = Amount::from_minor(1 + i128::from(rng.below(10_000))).unwrap();
    match rng.below(16) {
        0 if !memory.drafts.is_empty() => Op::Record(Box::new(rng.pick(&memory.drafts).clone())),
        1 if !memory.entries.is_empty() => {
            let target = *rng.pick(&memory.entries);
            let draft = Entry::new(format!("rev-{step}"), date).reverses(target);
            Op::Record(Box::new(draft.debit("Assets:Cash", amount, EUR).credit(
                "Income:Sales",
                amount,
                EUR,
            )))
        }
        2 => Op::Register {
            spec: AccountSpec::new(
                format!("{}:Sub{}", rng.pick(&ACCOUNTS), rng.below(3)),
                AccountKind::Asset,
                date,
            ),
            actor: "setup".into(),
        },
        3 => Op::Change {
            account: AccountRef::Path(rng.pick(&ACCOUNTS).parse().unwrap()),
            change: match rng.below(3) {
                0 => ChangeInput::OpenTo(Some(date)),
                1 => ChangeInput::Limit(BalanceLimit::NoDebitBalance),
                _ => ChangeInput::Class(Some("x".into())),
            },
            effective: date,
            actor: "setup".into(),
        },
        4 => Op::Currency {
            code: *rng.pick(&[EUR, USD]),
            exponent: rng.below(3) as u8,
            effective: date,
            actor: "setup".into(),
        },
        5 => Op::Policy {
            policy: LedgerPolicy::new([], Some(rng.below(30) as u32)).unwrap(),
            effective: date,
            actor: "setup".into(),
        },
        6 => Op::Period {
            id: format!("2027-{:02}", 1 + rng.below(2)),
            start: Date::from_calendar_date(2027, time::Month::January, 1).unwrap(),
            end: Date::from_calendar_date(2027, time::Month::January, 31).unwrap(),
        },
        7 if memory.entries.len() >= 2 => Op::Clear {
            key: format!("clr-{}", rng.below(4)),
            debit: PostingRef::at(*rng.pick(&memory.entries), 0),
            credit: PostingRef::at(*rng.pick(&memory.entries), 1),
            amount,
            date,
        },
        8 if !memory.clearings.is_empty() => Op::ResetClearing {
            key: format!("reset-{}", rng.below(3)),
            clearing: *rng.pick(&memory.clearings),
            date,
        },
        9 => Op::Seal {
            period: format!("2026-{:02}", memory.sealed + 1 + rng.below(2) as u8),
        },
        // A hold: the deepest read chain a store plans.
        12 => {
            let debit = *rng.pick(&ACCOUNTS);
            let credit = *rng.pick(&ACCOUNTS);
            let mut draft = Entry::open_hold(format!("hold-{step}"), date)
                .debit(debit, amount, EUR)
                .credit(credit, amount, EUR);
            if rng.below(2) == 0 {
                draft = draft.expires(day(month, 28));
            }
            Op::Record(Box::new(draft))
        }
        13 if !memory.holds.is_empty() => {
            let hold = *rng.pick(&memory.holds);
            let key = format!("resolve-{step}");
            Op::Record(Box::new(match rng.below(4) {
                0 => Entry::post_hold(key, date, hold, PostAmount::Full),
                1 => Entry::post_hold(key, date, hold, PostAmount::Partial(amount)),
                2 => Entry::void_hold(key, date, hold),
                _ => Entry::expire_hold(key, date, hold),
            }))
        }
        10 | 11 => Op::Batch(
            (0..rng.below(6))
                .map(
                    |n| match random_op(rng, memory, step * 8 + n as usize + 1_000_000) {
                        Op::Record(draft) => *draft,
                        // A draw on the limited wallet: refused unless funded
                        // (perhaps earlier in this batch).
                        _ => Entry::new(format!("draw-{step}-{n}"), date)
                            .debit("Liabilities:Wallet", amount, EUR)
                            .credit("Assets:Cash", amount, EUR),
                    },
                )
                .collect(),
        ),
        _ => {
            let debit = *rng.pick(&ACCOUNTS);
            let credit = *rng.pick(&ACCOUNTS);
            let mut draft = Entry::new(format!("e-{step}"), date)
                .debit(debit, amount, EUR)
                .credit(credit, amount, EUR);
            if rng.below(4) == 0 {
                draft = draft.memo_salted("memo", MemoSalt([3; 16]));
            }
            memory.drafts.push(draft.clone());
            Op::Record(Box::new(draft))
        }
    }
}

/// Runs the store protocol against `source` and returns the decision and
/// the number of planning rounds.
fn planned(source: &MemState<2>, op: &Op<2>) -> (Result<Outcome<2>, crate::Rejection>, usize) {
    let mut snapshot = Snapshot::new(source.header());
    let mut rounds = 0;
    loop {
        rounds += 1;
        let reads = plan(&snapshot, op);
        if reads.is_empty() {
            break;
        }
        assert!(
            rounds <= op.max_rounds(),
            "plan does not converge for {op:?}"
        );
        for read in &reads {
            snapshot.insert(Fact::answer(source, read));
        }
    }
    (execute(&snapshot, op), rounds)
}

#[test]
fn snapshot_after_plan_decides_like_the_state() {
    let base = base();
    let mut rng = Lcg(2026);
    let mut appended = 0usize;
    let mut resolutions = 0usize;
    for sequence in 0..10_000 {
        let mut state = base.clone();
        let mut memory = Memory::default();
        for step in 0..8 {
            let op = random_op(&mut rng, &mut memory, sequence * 8 + step);
            let (decided, rounds) = planned(&state, &op);
            assert!(rounds <= op.max_rounds());
            let expected = apply(&mut state, &op);
            assert_eq!(
                decided, expected,
                "sequence {sequence}, step {step}: {op:?}"
            );
            match expected {
                Ok(Outcome::Append(write)) => {
                    appended += 1;
                    match &write.record {
                        crate::model::Record::Entry(entry) => {
                            let id = entry.id(state.ledger());
                            if super::holds::is_resolution(entry.parts().hold.as_ref()) {
                                resolutions += 1;
                            }
                            if matches!(
                                entry.parts().hold,
                                Some(crate::model::HoldAction::Open { .. })
                            ) {
                                memory.holds.push(id);
                            }
                            memory.entries.push(id);
                        }
                        crate::model::Record::Clearing(_) => memory.clearings.push(write.position),
                        crate::model::Record::Seal(_) => memory.sealed += 1,
                        _ => {}
                    }
                }
                Ok(Outcome::Unchanged(_) | Outcome::Batch { .. }) | Err(_) => {}
            }
        }
    }
    assert!(
        appended > 20_000,
        "the sequences write ({appended} records)"
    );
    assert!(
        resolutions > 100,
        "the sequences resolve holds ({resolutions} resolutions)"
    );
}

/// A batch decides exactly like its drafts recorded one by one: the same
/// receipts and records when every draft is accepted; otherwise the first
/// refused draft's index and rejection, and nothing written.
#[test]
fn batch_decides_like_its_drafts_in_sequence() {
    let base = base();
    let mut rng = Lcg(2107);
    let (mut accepted, mut refused) = (0usize, 0usize);
    for sequence in 0..3_000 {
        let mut state = base.clone();
        let mut memory = Memory::default();
        for step in 0..6 {
            let op = random_op(&mut rng, &mut memory, sequence * 8 + step);
            let Op::Batch(drafts) = &op else {
                let _ = apply(&mut state, &op);
                continue;
            };
            let mut sequential = state.clone();
            let mut receipts = Vec::new();
            let mut first_refusal = None;
            for (index, draft) in drafts.iter().enumerate() {
                match apply(&mut sequential, &Op::Record(Box::new(draft.clone()))) {
                    Ok(outcome) => receipts.push(outcome.done().clone()),
                    Err(rejection) => {
                        first_refusal = Some((index, rejection));
                        break;
                    }
                }
            }
            let before = state.head();
            let decided = apply(&mut state, &op).unwrap();
            match first_refusal {
                None => {
                    accepted += 1;
                    let Done::Batch(recorded) = decided.done() else {
                        panic!("sequence {sequence}: expected a batch, got {decided:?}");
                    };
                    let recorded: Vec<Done<2>> = recorded.iter().map(|r| Done::Entry(*r)).collect();
                    assert_eq!(recorded, receipts, "sequence {sequence}: {op:?}");
                    assert_eq!(state.head(), sequential.head(), "sequence {sequence}");
                }
                Some((index, rejection)) => {
                    refused += 1;
                    assert_eq!(
                        decided,
                        Outcome::Unchanged(Done::BatchRejected { index, rejection }),
                        "sequence {sequence}: {op:?}"
                    );
                    assert_eq!(state.head(), before, "a refused batch wrote");
                }
            }
        }
    }
    assert!(
        accepted > 300 && refused > 300,
        "{accepted} accepted, {refused} refused"
    );
}

/// A view that counts the facts it is asked for.
struct Counting<'a> {
    state: &'a MemState<2>,
    reads: Cell<u64>,
}

impl Counting<'_> {
    fn count(&self) {
        self.reads.set(self.reads.get() + 1);
    }
}

impl StateView<2> for Counting<'_> {
    fn header(&self) -> Header<2> {
        self.state.header()
    }
    fn entry_by_key(&self, key: &IdempotencyKey) -> Option<EntryMeta> {
        self.count();
        self.state.entry_by_key(key)
    }
    fn account_by_path(&self, path: &AccountPath) -> Option<AccountView> {
        self.count();
        self.state.account_by_path(path)
    }
    fn account(&self, handle: AccountHandle) -> Option<AccountView> {
        self.count();
        self.state.account(handle)
    }
    fn account_terms(&self, handle: AccountHandle, on: Date) -> Option<Terms> {
        self.count();
        self.state.account_terms(handle, on)
    }
    fn currency(&self, code: Currency, on: Date) -> Option<u8> {
        self.count();
        self.state.currency(code, on)
    }
    fn currency_latest(&self, code: Currency) -> Option<Versioned<u8>> {
        self.count();
        self.state.currency_latest(code)
    }
    fn policy(&self, on: Date) -> LedgerPolicy {
        self.count();
        self.state.policy(on)
    }
    fn policy_latest(&self) -> Option<Versioned<LedgerPolicy>> {
        self.count();
        self.state.policy_latest()
    }
    fn period_at(&self, date: Date) -> Option<PeriodView<2>> {
        self.count();
        self.state.period_at(date)
    }
    fn period(&self, id: &PeriodId) -> Option<PeriodView<2>> {
        self.count();
        self.state.period(id)
    }
    fn first_unsealed(&self) -> Option<PeriodView<2>> {
        self.count();
        self.state.first_unsealed()
    }
    fn entry(&self, id: EntryId) -> Option<EntryView<2>> {
        self.count();
        self.state.entry(id)
    }
    fn reversed_by(&self, id: EntryId) -> Option<EntryId> {
        self.count();
        self.state.reversed_by(id)
    }
    fn hold(&self, id: EntryId) -> Option<HoldView<2>> {
        self.count();
        self.state.hold(id)
    }
    fn outstanding_hold_debits(&self, handle: AccountHandle, currency: Currency) -> Amount<2> {
        self.count();
        self.state.outstanding_hold_debits(handle, currency)
    }
    fn outstanding_hold_credits(&self, handle: AccountHandle, currency: Currency) -> Amount<2> {
        self.count();
        self.state.outstanding_hold_credits(handle, currency)
    }
    fn balance(&self, key: BalanceKey) -> Balance<2> {
        self.count();
        self.state.balance(key)
    }
    fn clearing_by_key(&self, key: &IdempotencyKey) -> Option<ClearingView<2>> {
        self.count();
        self.state.clearing_by_key(key)
    }
    fn clearing_at(&self, position: u64) -> Option<ClearingView<2>> {
        self.count();
        self.state.clearing_at(position)
    }
    fn applied(&self, posting: PostingRef) -> Amount<2> {
        self.count();
        self.state.applied(posting)
    }
    fn all_balances(&self) -> Vec<(BalanceKey, Balance<2>)> {
        self.count();
        self.state.all_balances()
    }
    fn postings_after(&self, date: Date) -> Vec<Movement<2>> {
        self.count();
        self.state.postings_after(date)
    }
    fn pnl_slices(&self, through: Date) -> Vec<Slice<2>> {
        self.count();
        self.state.pnl_slices(through)
    }
}

fn transfer(key: &str, debit: &str, credit: &str, minor: i128) -> Op<2> {
    let amount = Amount::from_minor(minor).unwrap();
    Op::Record(Box::new(
        Entry::new(key, day(6, 15))
            .debit(debit, amount, EUR)
            .credit(credit, amount, EUR),
    ))
}

/// The facts read to decide `op`, and whether deciding it changed nothing.
fn reads(state: &MemState<2>, op: &Op<2>) -> u64 {
    let counting = Counting {
        state,
        reads: Cell::new(0),
    };
    let _decided = execute(&counting, op);
    counting.reads.get()
}

// PoC: qe7_batch_rollback_cost_scales_with_history
#[test]
fn qe7_rejection_cost_independent_of_history() {
    // Every entry posts once to the wallet: 200 000 postings of history in a
    // release build, 20 000 in a debug one (the count is what is compared).
    let history = if cfg!(debug_assertions) {
        20_000
    } else {
        200_000
    };
    let mut small = base();
    let mut large = base();
    let fund = transfer("fund", "Assets:Cash", "Liabilities:Wallet", 1_000_000_000);
    for state in [&mut small, &mut large] {
        apply(state, &fund).unwrap();
    }
    for n in 0..history {
        let op = transfer(&format!("h-{n}"), "Liabilities:Wallet", "Assets:Cash", 1);
        apply(&mut large, &op).unwrap();
    }
    let accepted = transfer("ok", "Liabilities:Wallet", "Assets:Cash", 1);
    let refused = transfer("over", "Liabilities:Wallet", "Assets:Cash", 9_000_000_000);
    assert_eq!(reads(&small, &accepted), reads(&large, &accepted));
    assert_eq!(reads(&small, &refused), reads(&large, &refused));
    // A refusal changes nothing: the state is the same before and after.
    let before = (large.head(), large.all_balances(), large.header());
    assert!(apply(&mut large, &refused).is_err());
    assert_eq!((large.head(), large.all_balances(), large.header()), before);
}

#[test]
fn reads_are_bounded_by_the_operation() {
    let mut state = base();
    apply(
        &mut state,
        &transfer("fund", "Assets:Cash", "Liabilities:Wallet", 100),
    )
    .unwrap();
    // A two-posting entry: key, two accounts, period, currency, policy, two
    // terms, two balances, and the limited wallet's outstanding hold debits
    // and credits, read again by the limit check.
    assert_eq!(
        reads(
            &state,
            &transfer("t", "Liabilities:Wallet", "Assets:Cash", 1)
        ),
        14
    );
    // A registration: its path and its one ancestor.
    let register = Op::Register {
        spec: AccountSpec::new("Assets:Bank", AccountKind::Asset, day(2, 1)),
        actor: "setup".into(),
    };
    assert_eq!(reads(&state, &register), 2);
    // A currency declaration: its latest version.
    let currency = Op::Currency {
        code: USD,
        exponent: 2,
        effective: day(2, 1),
        actor: "setup".into(),
    };
    assert_eq!(reads(&state, &currency), 1);
}
