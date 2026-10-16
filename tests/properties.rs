//! Properties of the reads: statements reconcile on both date bases,
//! dimension filters partition every balance, and the Konto/Gegenkonto
//! pairing re-sums, never mixes groups, stays small, is deterministic and
//! mirrors a reversal.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use std::collections::BTreeMap;

use doubleentry::export::pairs;
use doubleentry::{
    AccountHandle, AccountKind, Amount, Balance, BalanceQuery, Currency, Date, Dimensions, Entry,
    Journal, Layer, LedgerId, Posting, Side,
    currency::{EUR, USD},
    date,
    record::PostingRecord,
};
use proptest::prelude::*;

/// A small deterministic generator, so one `u64` seed is one case.
struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound.max(1)
    }
}

const ACCOUNTS: [&str; 3] = ["Assets:Bank", "Assets:Cash", "Assets:Card"];
const CENTRES: [Option<&str>; 3] = [Some("A"), Some("B"), None];

fn day(offset: u64) -> Date {
    date!("2026-01-01")
        .checked_add(time::Duration::days(offset as i64))
        .unwrap()
}

/// A journal of up to eight entries booked over the first quarter, value
/// dates up to ten days later, some posting tagged with a cost centre.
fn ledger(seed: u64) -> Journal {
    let mut random = Random(seed);
    let mut books = Journal::new(LedgerId::new("example.com/properties").unwrap());
    books.declare_currency(EUR, 2, date!("2026-01-01")).unwrap();
    for path in ACCOUNTS {
        books
            .open(path, AccountKind::Asset, date!("2026-01-01"))
            .unwrap();
    }
    books.define_months(2026, 1..=4).unwrap();
    for n in 0..random.below(9) {
        let debit = random.below(3) as usize;
        let credit = (debit + 1 + random.below(2) as usize) % 3;
        let booked = random.below(90);
        let amount = Amount::from_minor(1 + random.below(100_000) as i128).unwrap();
        let mut posting = Posting::debit(ACCOUNTS[debit], amount, EUR);
        if let Some(centre) = CENTRES[random.below(3) as usize] {
            posting = posting.dimension("cost_centre", centre);
        }
        books
            .record(
                Entry::new(format!("entry-{n}"), day(booked))
                    .value_date(day(booked + random.below(11)))
                    .posting(posting)
                    .credit(ACCOUNTS[credit], amount, EUR),
            )
            .unwrap();
    }
    books
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10_000))]

    /// SC-002: `opening + Σ lines = closing`, `opening = balance(through
    /// start − 1)` and `closing = balance(through end)`, on both bases.
    #[test]
    fn report_statement_identities(seed in any::<u64>(), a in 0u64..110, b in 0u64..110) {
        let books = ledger(seed);
        let (start, end) = (day(a.min(b)), day(a.max(b)));
        for account in ACCOUNTS {
            for by_value in [false, true] {
                let basis = |query: BalanceQuery| if by_value { query.by_value_date() } else { query };
                let statement = books
                    .statement(account, EUR, &basis(BalanceQuery::between(start, end)))
                    .unwrap();
                let mut running = statement.opening;
                for line in &statement.lines {
                    running = running.post(line.side, line.amount).unwrap();
                    prop_assert_eq!(line.running, running);
                }
                prop_assert_eq!(running, statement.closing);
                let before = start.previous_day().unwrap();
                prop_assert_eq!(
                    statement.opening,
                    books.balance(account, EUR, &basis(BalanceQuery::through(before))).unwrap()
                );
                prop_assert_eq!(
                    statement.closing,
                    books.balance(account, EUR, &basis(BalanceQuery::through(end))).unwrap()
                );
            }
        }
    }

    /// SC-002: `A + B + missing(axis) = unfiltered` for every account.
    #[test]
    fn report_filter_partition(seed in any::<u64>(), through in 0u64..110) {
        let books = ledger(seed);
        let window = BalanceQuery::through(day(through));
        for account in ACCOUNTS {
            let balance = |query: &BalanceQuery| books.balance(account, EUR, query).unwrap();
            let parts = [
                balance(&window.clone().matching("cost_centre", "A")),
                balance(&window.clone().matching("cost_centre", "B")),
                balance(&window.clone().missing("cost_centre")),
            ];
            let sum = parts
                .iter()
                .try_fold(Balance::ZERO, |total, part| total.checked_add(part))
                .unwrap();
            prop_assert_eq!(sum, balance(&window));
        }
    }
}

/// A balanced entry of 1–20 legs over up to four (currency, layer) groups,
/// in shuffled order.
fn entry(seed: u64) -> Vec<PostingRecord> {
    let mut random = Random(seed);
    let groups: [(Currency, Layer); 4] = [
        (EUR, Layer::Settled),
        (USD, Layer::Settled),
        (EUR, Layer::Pending),
        (USD, Layer::Pending),
    ];
    let mut legs = Vec::new();
    let mut budget = 20;
    for (currency, layer) in groups.into_iter().take(1 + random.below(4) as usize) {
        if budget < 2 {
            break;
        }
        let debits = 1 + random.below((budget - 1).min(9));
        let mut amounts: Vec<u64> = (0..debits).map(|_| 1 + random.below(500)).collect();
        if random.below(3) == 0 {
            // Repeated amounts exercise the exact pass.
            amounts.iter_mut().for_each(|amount| *amount = 50);
        }
        let total: u64 = amounts.iter().sum();
        let credits = 1 + random.below((budget - debits).min(total).min(9));
        let mut cuts: Vec<u64> = (0..credits - 1)
            .map(|_| 1 + random.below(total - 1))
            .collect();
        cuts.sort_unstable();
        cuts.dedup();
        let mut previous = 0;
        let mut parts = Vec::new();
        for cut in cuts.into_iter().chain([total]) {
            parts.push(cut - previous);
            previous = cut;
        }
        budget -= debits + parts.len() as u64;
        let leg = |side, amount: u64| PostingRecord {
            handle: AccountHandle::new(random_handle(seed, amount)).unwrap(),
            side,
            amount: Amount::from_minor(amount as i128).unwrap(),
            currency,
            layer,
            dimensions: Dimensions::new(),
        };
        legs.extend(amounts.into_iter().map(|amount| leg(Side::Debit, amount)));
        legs.extend(parts.into_iter().map(|amount| leg(Side::Credit, amount)));
    }
    for i in (1..legs.len()).rev() {
        legs.swap(i, random.below(i as u64 + 1) as usize);
    }
    legs
}

fn random_handle(seed: u64, amount: u64) -> u32 {
    ((seed ^ amount) % 16) as u32
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10_000))]

    /// SC-004: the pairs re-sum to every posting, stay within their
    /// (currency, layer), number at most `d + c − 1` per group, come out the
    /// same when recomputed, and a reversal pairs as the original swapped.
    #[test]
    fn export_pairs(seed in any::<u64>()) {
        let legs = entry(seed);
        let found = pairs(&legs);
        let mut paired: BTreeMap<u16, i128> = BTreeMap::new();
        let mut per_group: BTreeMap<(Currency, Layer), usize> = BTreeMap::new();
        for (ordinal, pair) in found.iter().enumerate() {
            prop_assert_eq!(usize::from(pair.ordinal), ordinal);
            let (debit, credit) = (&legs[usize::from(pair.debit)], &legs[usize::from(pair.credit)]);
            prop_assert_eq!((debit.side, credit.side), (Side::Debit, Side::Credit));
            prop_assert_eq!((debit.currency, debit.layer), (credit.currency, credit.layer));
            prop_assert!(pair.amount.is_positive());
            *paired.entry(pair.debit).or_default() += pair.amount.to_minor();
            *paired.entry(pair.credit).or_default() += pair.amount.to_minor();
            *per_group.entry((debit.currency, debit.layer)).or_default() += 1;
        }
        for (index, leg) in (0u16..).zip(&legs) {
            prop_assert_eq!(paired.get(&index).copied(), Some(leg.amount.to_minor()));
        }
        for ((currency, layer), count) in per_group {
            let legs_in = legs.iter().filter(|leg| (leg.currency, leg.layer) == (currency, layer)).count();
            prop_assert!(count < legs_in);
        }
        prop_assert_eq!(&pairs(&legs), &found);
        let mirrored: Vec<PostingRecord> = legs
            .iter()
            .map(|leg| PostingRecord { side: leg.side.opposite(), ..leg.clone() })
            .collect();
        let mut swapped: Vec<(u16, u16, Amount)> =
            found.iter().map(|pair| (pair.credit, pair.debit, pair.amount)).collect();
        swapped.sort_unstable_by_key(|(debit, credit, _)| (*debit, *credit));
        let reversed: Vec<(u16, u16, Amount)> =
            pairs(&mirrored).iter().map(|pair| (pair.debit, pair.credit, pair.amount)).collect();
        prop_assert_eq!(reversed, swapped);
    }
}

// ── Holds (023 SC-004) ──────────────────────────────────────────────────────

const PARTIES: [&str; 3] = ["Assets:Bank", "Liabilities:W1", "Liabilities:W2"];

/// What the model knows of one hold.
#[derive(Debug, Clone)]
struct ModelHold {
    booked: Date,
    expires: Option<Date>,
    debit: usize,
    credit: usize,
    amount: Amount,
    resolved: Option<doubleentry::HoldStatus>,
}

/// The status the model predicts for a resolution of `hold` on `on`, or
/// the code that refuses it.
fn predict(
    hold: &ModelHold,
    action: &doubleentry::record::HoldAction,
    on: Date,
    by: doubleentry::EntryId,
) -> Result<doubleentry::HoldStatus, u16> {
    use doubleentry::{HoldStatus, PostAmount, record::HoldAction};
    if hold.resolved.is_some() {
        return Err(442);
    }
    match *action {
        HoldAction::Post { amount, .. } => {
            if hold.expires.is_some_and(|expires| on >= expires) {
                return Err(444);
            }
            let posted = match amount {
                PostAmount::Full => hold.amount,
                PostAmount::Partial(part) if part > hold.amount => return Err(448),
                PostAmount::Partial(part) => part,
                _ => unreachable!("a post is full or partial"),
            };
            Ok(HoldStatus::Posted { by, amount: posted })
        }
        HoldAction::Void { .. } => Ok(HoldStatus::Voided { by }),
        HoldAction::Expire { .. } => match hold.expires {
            None => Err(445),
            Some(expires) if on < expires => Err(446),
            Some(_) => Ok(HoldStatus::Expired { by }),
        },
        _ => unreachable!("only resolutions are predicted"),
    }
}

/// The model's invariants against the journal: each hold's status (so a
/// hold resolves at most once), what remains of it, and every account's
/// Pending net as the fold of the open holds' legs.
fn check_holds(books: &Journal, holds: &BTreeMap<doubleentry::EntryId, ModelHold>) {
    let mut reserved = [Amount::ZERO; 3];
    for (id, hold) in holds {
        let state = books.hold(*id).unwrap();
        let status = hold.resolved.unwrap_or(doubleentry::HoldStatus::Open);
        assert_eq!(state.status, status, "the hold {id}");
        let remaining = state.remaining();
        assert!(remaining == state.amount || remaining.is_zero());
        assert_eq!(remaining.is_zero(), hold.resolved.is_some());
        if hold.resolved.is_none() {
            reserved[hold.debit] = reserved[hold.debit].checked_add(hold.amount).unwrap();
            reserved[hold.credit] = reserved[hold.credit].checked_sub(hold.amount).unwrap();
        }
    }
    for (party, reserved) in PARTIES.iter().zip(reserved) {
        let pending = books
            .balance(*party, EUR, &BalanceQuery::all().pending())
            .unwrap()
            .net();
        assert_eq!(pending, reserved, "the Pending net of {party}");
    }
}

/// One random sequence of holds, posts (full and partial), voids,
/// expiries, fundings, limit changes and seals.
fn holds_case(seed: u64) {
    use doubleentry::{
        AccountSpec, BalanceLimit, Error, PostAmount, Rejection, record::HoldAction,
    };
    let mut random = Random(seed);
    let mut books = Journal::new(LedgerId::new("example.com/holds").unwrap());
    books.declare_currency(EUR, 2, date!("2026-01-01")).unwrap();
    for party in PARTIES {
        books
            .register(AccountSpec::new(
                party,
                AccountKind::Liability,
                date!("2026-01-01"),
            ))
            .unwrap();
    }
    books.define_months(2026, 1..=6).unwrap();
    let last = date!("2026-06-30");
    let mut first_open = date!("2026-01-01");
    let mut sealed = 0u8;
    let mut holds: BTreeMap<doubleentry::EntryId, ModelHold> = BTreeMap::new();
    let mut order: Vec<doubleentry::EntryId> = Vec::new();
    let pick_day = |random: &mut Random, from: Date| {
        let date = from
            .checked_add(time::Duration::days(random.below(20) as i64))
            .unwrap();
        date.min(last)
    };
    for step in 0..(5 + random.below(30)) {
        let key = format!("step-{step}");
        match random.below(10) {
            0..=2 => {
                let debit = random.below(3) as usize;
                let credit = (debit + 1 + random.below(2) as usize) % 3;
                let amount = Amount::from_minor(1 + random.below(50_000) as i128).unwrap();
                let booked = pick_day(&mut random, first_open);
                let mut draft = Entry::open_hold(key.as_str(), booked);
                let mut expires = None;
                if random.below(2) == 0 {
                    let on = booked
                        .checked_add(time::Duration::days(1 + random.below(15) as i64))
                        .unwrap();
                    draft = draft.expires(on);
                    expires = Some(on);
                }
                let draft =
                    draft
                        .debit(PARTIES[debit], amount, EUR)
                        .credit(PARTIES[credit], amount, EUR);
                match books.record(draft) {
                    Ok(recorded) => {
                        holds.insert(
                            recorded.id,
                            ModelHold {
                                booked,
                                expires,
                                debit,
                                credit,
                                amount,
                                resolved: None,
                            },
                        );
                        order.push(recorded.id);
                    }
                    Err(Error::Rejected(Rejection::LimitExceeded { .. })) => {}
                    Err(error) => panic!("seed {seed} step {step}: a hold failed: {error}"),
                }
            }
            3..=6 if !order.is_empty() => {
                let id = order[random.below(order.len() as u64) as usize];
                let hold = holds[&id].clone();
                let base = match hold.expires {
                    Some(expires) if random.below(2) == 0 => expires,
                    _ => hold.booked,
                };
                let on = pick_day(&mut random, base.max(first_open));
                let action = match random.below(4) {
                    0 => HoldAction::Post {
                        hold: id,
                        amount: PostAmount::Full,
                    },
                    1 => HoldAction::Post {
                        hold: id,
                        amount: PostAmount::Partial(
                            Amount::from_minor(
                                1 + random.below(hold.amount.to_minor() as u64 + 10) as i128,
                            )
                            .unwrap(),
                        ),
                    },
                    2 => HoldAction::Void { hold: id },
                    _ => HoldAction::Expire { hold: id },
                };
                let draft = match action {
                    HoldAction::Post { amount, .. } => {
                        Entry::post_hold(key.as_str(), on, id, amount)
                    }
                    HoldAction::Void { .. } => Entry::void_hold(key.as_str(), on, id),
                    _ => Entry::expire_hold(key.as_str(), on, id),
                };
                let by = draft.id(books.id()).unwrap();
                let outcome = books.record(draft);
                match (predict(&hold, &action, on, by), outcome) {
                    (Ok(status), Ok(recorded)) => {
                        assert_eq!(recorded.id, by);
                        holds.get_mut(&id).unwrap().resolved = Some(status);
                    }
                    (Err(code), Err(Error::Rejected(rejection))) => {
                        assert_eq!(rejection.code(), code, "seed {seed} step {step}");
                    }
                    (predicted, outcome) => {
                        panic!("seed {seed} step {step}: predicted {predicted:?}, got {outcome:?}")
                    }
                }
            }
            7 => {
                let party = PARTIES[1 + random.below(2) as usize];
                let amount = Amount::from_minor(1 + random.below(100_000) as i128).unwrap();
                let funding = books.record(
                    Entry::new(key.as_str(), pick_day(&mut random, first_open))
                        .debit("Assets:Bank", amount, EUR)
                        .credit(party, amount, EUR),
                );
                match funding {
                    Ok(_) | Err(Error::Rejected(Rejection::LimitExceeded { .. })) => {}
                    Err(error) => panic!("seed {seed} step {step}: a funding failed: {error}"),
                }
            }
            8 => {
                let limit = [
                    BalanceLimit::Unlimited,
                    BalanceLimit::NoDebitBalance,
                    BalanceLimit::NoCreditBalance,
                ][random.below(3) as usize];
                let party = PARTIES[random.below(3) as usize];
                // A change dated before an earlier one of the same field is
                // refused (310); the limit in force is always the latest.
                let _ = books.set_limit(party, limit, last);
            }
            _ if sealed < 5 && first_open < last => {
                sealed += 1;
                books.seal_period(format!("2026-{sealed:02}")).unwrap();
                first_open =
                    Date::from_calendar_date(2026, time::Month::try_from(sealed + 1).unwrap(), 1)
                        .unwrap();
            }
            _ => {}
        }
        check_holds(&books, &holds);
    }
    books.audit().unwrap();
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10_000))]

    /// `holds_model` (023 SC-004): after every step of a random sequence,
    /// each hold is resolved at most once and exactly as predicted,
    /// remaining is its amount or zero, every Pending net is the fold of
    /// the open holds, and the audit replays the hold index and the
    /// reserved amounts.
    #[test]
    fn holds_model(seed in any::<u64>()) {
        holds_case(seed);
    }
}
