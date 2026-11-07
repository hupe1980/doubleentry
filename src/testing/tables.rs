//! The exact-outcome table: each row runs one setup and one
//! operation on the reference [`Journal`] and on a [`Ledger`] over a fresh
//! store, and the store must answer exactly what the journal answers — the
//! same receipt (replays included) or the same rejection, field for field.
//! Every rejection code has a row; precedence rows break several rules at
//! once.

use time::Date;

use super::model::Check;
use crate::core::{BatchItem, ChangeInput, Clearing, Done, Op};
use crate::error::{Error, Rejection};
use crate::log::leaf_hash;
use crate::model::{
    AccountKind, AccountRef, Amount, BalanceLimit, Currency, Document, EUR, Entry, EntryId,
    IdempotencyKey, JPY, Label, LedgerId, LedgerPolicy, MemoSalt, PostAmount, Posting, PostingRef,
    Rate, USD,
};
use crate::store::{Erasure, LeafRead, Memo, Store};
use crate::{AccountSpec, Journal, Ledger};

/// What a row expects of the reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Expect {
    /// Accepted.
    Done,
    /// Refused with this code.
    Code(u16),
    /// A batch refused at this draft with this code.
    Batch(usize, u16),
}

/// One row: a setup, an operation and what the reference answers.
#[derive(Debug, Clone)]
pub(crate) struct Row {
    pub(crate) name: String,
    setup: Vec<Op<2>>,
    op: Op<2>,
    expect: Expect,
}

fn on(month: u8, day: u8) -> Date {
    time::Month::try_from(month)
        .ok()
        .and_then(|month| Date::from_calendar_date(2026, month, day).ok())
        .unwrap_or(Date::MIN)
}

fn eur(text: &str) -> Amount<2> {
    text.parse().unwrap_or(Amount::ZERO)
}

fn record(entry: Entry<2>) -> Op<2> {
    Op::Record(Box::new(entry))
}

fn sale(key: &str, date: Date, amount: &str) -> Entry<2> {
    Entry::new(key, date)
        .debit("Assets:Cash", eur(amount), EUR)
        .credit("Income:Sales", eur(amount), EUR)
}

fn rent(key: &str) -> Entry<2> {
    Entry::new(key, on(3, 5))
        .debit("Expenses:Rent", eur("800.00"), EUR)
        .credit("Assets:Cash", eur("800.00"), EUR)
}

fn register(path: &str, kind: AccountKind, from: Date) -> Op<2> {
    Op::Register {
        spec: AccountSpec::new(path, kind, from),
        actor: String::from("setup"),
    }
}

fn currency(code: Currency, exponent: u8, effective: Date) -> Op<2> {
    Op::Currency {
        code,
        exponent,
        effective,
        actor: String::from("setup"),
    }
}

/// An account named by one of this table's path literals.
#[allow(clippy::expect_used)] // Proven total: every literal passed here is a valid path.
fn account(path: &str) -> AccountRef {
    AccountRef::Path(path.parse().expect("the table's path literals are valid"))
}

fn change(path: &str, change: ChangeInput, effective: Date) -> Op<2> {
    Op::Change {
        account: account(path),
        change,
        effective,
        actor: String::from("setup"),
    }
}

fn seal(period: &str) -> Op<2> {
    Op::Seal {
        period: period.to_owned(),
    }
}

fn period(id: &str, start: Date, end: Date) -> Op<2> {
    Op::Period {
        id: id.to_owned(),
        start,
        end,
    }
}

fn policy(required: &[&str], drift: Option<u32>) -> Op<2> {
    let required: Vec<Label> = required
        .iter()
        .filter_map(|axis| Label::new(axis).ok())
        .collect();
    Op::Policy {
        policy: LedgerPolicy::new(required, drift).unwrap_or_default(),
        effective: on(1, 1),
        actor: String::from("setup"),
    }
}

/// EUR, six accounts (the wallet limited) and the months of 2026: positions
/// 1 to 19.
fn base() -> Vec<Op<2>> {
    let mut ops = vec![
        currency(EUR, 2, on(1, 1)),
        register("Assets:Cash", AccountKind::Asset, on(1, 1)),
        register("Income:Sales", AccountKind::Income, on(1, 1)),
        Op::Register {
            spec: AccountSpec::new("Liabilities:Wallet", AccountKind::Liability, on(1, 1))
                .limit(BalanceLimit::NoDebitBalance),
            actor: String::from("setup"),
        },
        register("Expenses:Rent", AccountKind::Expense, on(1, 1)),
        register("Equity:Retained", AccountKind::Equity, on(1, 1)),
        register("Assets:Receivables", AccountKind::Asset, on(1, 1)),
    ];
    for month in 1u8..=12 {
        let start = on(month, 1);
        let end = time::Month::try_from(month)
            .ok()
            .and_then(|of| Date::from_calendar_date(2026, of, of.length(2026)).ok())
            .unwrap_or(start);
        ops.push(period(&format!("2026-{month:02}"), start, end));
    }
    ops
}

/// The position the `n`-th extra setup op is written at (after the base).
fn at(n: u64) -> u64 {
    20u64.saturating_add(n)
}

/// A row: `setup` after the base, then `op`.
fn row(name: &str, setup: Vec<Op<2>>, op: Op<2>, expect: Expect) -> Row {
    let mut all = base();
    all.extend(setup);
    Row {
        name: name.to_owned(),
        setup: all,
        op,
        expect,
    }
}

/// The receivables setup: an invoice (debit receivables 1190.00 on 03-01,
/// position 20) and a payment (credit receivables 500.00 on 03-10, position
/// 21).
fn receivables() -> Vec<Op<2>> {
    vec![
        record(
            Entry::new("inv-1", on(3, 1))
                .debit("Assets:Receivables", eur("1190.00"), EUR)
                .credit("Income:Sales", eur("1190.00"), EUR),
        ),
        record(
            Entry::new("pay-1", on(3, 10))
                .debit("Assets:Cash", eur("500.00"), EUR)
                .credit("Assets:Receivables", eur("500.00"), EUR),
        ),
    ]
}

/// A hold of `amount` (debit `Assets:Cash`, credit `Income:Sales`) booked
/// on `date`, expiring on `expires`.
fn hold(key: &str, date: Date, expires: Option<Date>, amount: &str) -> Entry<2> {
    let draft = match expires {
        Some(expires) => Entry::open_hold(key, date).expires(expires),
        None => Entry::open_hold(key, date),
    };
    draft
        .debit("Assets:Cash", eur(amount), EUR)
        .credit("Income:Sales", eur(amount), EUR)
}

/// An atomic batch of `drafts`.
fn batch(drafts: impl IntoIterator<Item = Entry<2>>) -> Op<2> {
    mixed(drafts.into_iter().map(BatchItem::Record))
}

/// An atomic batch of entries, clearings and resets.
fn mixed(items: impl IntoIterator<Item = BatchItem<2>>) -> Op<2> {
    Op::Batch(items.into_iter().collect())
}

/// A draw of `amount` from the limited wallet into cash.
fn draw(key: &str, amount: &str) -> Entry<2> {
    Entry::new(key, on(3, 2))
        .debit("Liabilities:Wallet", eur(amount), EUR)
        .credit("Assets:Cash", eur(amount), EUR)
}

/// A top-up of `amount` into the limited wallet.
fn top_up(key: &str, amount: &str) -> Entry<2> {
    Entry::new(key, on(3, 1))
        .debit("Assets:Cash", eur(amount), EUR)
        .credit("Liabilities:Wallet", eur(amount), EUR)
}

fn clear(key: &str, debit: PostingRef, credit: PostingRef, amount: &str, date: Date) -> Op<2> {
    Op::Clear {
        key: key.to_owned(),
        debit,
        credit,
        amount: eur(amount),
        date,
    }
}

/// Every row of the table.
pub(crate) fn rows(ledger: &LedgerId) -> Vec<Row> {
    let id = |key: &str| {
        IdempotencyKey::new(key)
            .map(|key| EntryId::derive(ledger, &key))
            .unwrap_or_else(|_| EntryId::from_bytes([0; 16]))
    };
    let invoice = PostingRef::at(id("inv-1"), 0);
    let income = PostingRef::at(id("inv-1"), 1);
    let payment = PostingRef::at(id("pay-1"), 1);
    let ghost = EntryId::from_bytes([9; 16]);
    let max = Amount::<2>::MAX;
    let wide = |posting: Posting<2>| {
        (0..8).fold(posting, |posting, axis| {
            posting.dimension(format!("{axis}{}", "a".repeat(63)), "v".repeat(64))
        })
    };
    let reversal = |key: &str, date: Date| {
        Entry::new(key, date)
            .reverses(id("rent"))
            .credit("Expenses:Rent", eur("800.00"), EUR)
            .debit("Assets:Cash", eur("800.00"), EUR)
    };
    let quotes = [
        "AUD", "BHD", "BRL", "CAD", "CHF", "CNY", "CZK", "DKK", "GBP", "HKD", "HUF", "INR", "JPY",
        "KRW", "KWD", "MXN", "NOK",
    ];
    let salted = |entry: Entry<2>, text: String| entry.memo_salted(text, MemoSalt([7; 16]));
    let held = |expires: Option<Date>| vec![record(hold("hold", on(3, 1), expires, "100.00"))];
    let partial = |amount: &str| PostAmount::Partial(eur(amount));
    vec![
        // ── Structural (1xx) ────────────────────────────────────────────────
        row(
            "100_invalid_key",
            vec![],
            record(sale("", on(1, 15), "1.00")),
            Expect::Code(100),
        ),
        row(
            "101_too_few_postings",
            vec![],
            record(Entry::new("lone", on(1, 15)).debit("Assets:Cash", eur("1.00"), EUR)),
            Expect::Code(101),
        ),
        row(
            "102_too_many_postings",
            vec![],
            record((0..1024).fold(
                Entry::new("many", on(1, 15)).credit("Income:Sales", eur("10.24"), EUR),
                |draft, _| draft.debit("Assets:Cash", eur("0.01"), EUR),
            )),
            Expect::Code(102),
        ),
        row(
            "103_invalid_path",
            vec![],
            record(
                Entry::new("bad-path", on(1, 15))
                    .debit("Assets:Cash", eur("1.00"), EUR)
                    .credit("Income::Sales", eur("1.00"), EUR),
            ),
            Expect::Code(103),
        ),
        row(
            "104_non_positive_amount",
            vec![],
            record(sale("zero", on(1, 15), "0.00")),
            Expect::Code(104),
        ),
        row(
            "105_invalid_dimensions",
            vec![],
            record(
                Entry::new("dims", on(1, 15))
                    .posting((0..9).fold(
                        Posting::debit("Assets:Cash", eur("1.00"), EUR),
                        |p, axis| p.dimension(format!("axis{axis}"), "x"),
                    ))
                    .credit("Income:Sales", eur("1.00"), EUR),
            ),
            Expect::Code(105),
        ),
        row(
            "106_unbalanced",
            vec![],
            record(
                Entry::new("unbalanced", on(1, 15))
                    .debit("Assets:Cash", eur("10.00"), EUR)
                    .credit("Income:Sales", eur("9.00"), EUR),
            ),
            Expect::Code(106),
        ),
        row(
            "107_invalid_kind",
            vec![],
            record(sale("kind", on(1, 15), "1.00").kind(" padded")),
            Expect::Code(107),
        ),
        row(
            "108_invalid_memo",
            vec![],
            record(salted(sale("memo", on(1, 15), "1.00"), "x".repeat(4097))),
            Expect::Code(108),
        ),
        row(
            "109_too_many_documents",
            vec![],
            record((0..17).fold(
                sale("docs", on(1, 15), "1.00"),
                |draft, n| match Document::new("invoice", format!("RE-{n}")) {
                    Ok(document) => draft.document(document),
                    Err(_) => draft,
                },
            )),
            Expect::Code(109),
        ),
        row(
            "110_too_many_rates",
            vec![],
            record(
                quotes
                    .iter()
                    .fold(
                        sale("rates", on(1, 15), "1.00"),
                        |draft, quote| match Currency::new(quote)
                            .ok()
                            .and_then(|quote| Rate::new(EUR, quote, 11, -1).ok())
                        {
                            Some(rate) => draft.rate(rate),
                            None => draft,
                        },
                    ),
            ),
            Expect::Code(110),
        ),
        row(
            "111_record_too_large",
            vec![],
            record((0..512).fold(Entry::new("wide", on(1, 15)), |draft, _| {
                draft
                    .posting(wide(Posting::debit("Assets:Cash", eur("1.00"), EUR)))
                    .posting(wide(Posting::credit("Income:Sales", eur("1.00"), EUR)))
            })),
            Expect::Code(111),
        ),
        row(
            "113_total_out_of_range",
            vec![],
            record(
                Entry::new("huge", on(1, 15))
                    .debit("Assets:Cash", max, EUR)
                    .debit("Expenses:Rent", max, EUR)
                    .credit("Income:Sales", max, EUR)
                    .credit("Equity:Retained", max, EUR),
            ),
            Expect::Code(113),
        ),
        row(
            "114_memo_salt_missing",
            vec![],
            record(sale("memo-salt", on(1, 15), "1.00").memo("Rechnung 4711")),
            Expect::Code(114),
        ),
        row(
            "140_hold_legs",
            vec![],
            record(
                Entry::open_hold("three-legs", on(3, 1))
                    .debit("Assets:Cash", eur("100.00"), EUR)
                    .credit("Income:Sales", eur("60.00"), EUR)
                    .credit("Income:Sales", eur("40.00"), EUR),
            ),
            Expect::Code(140),
        ),
        row(
            "141_hold_expiry",
            vec![],
            record(hold("born-expired", on(3, 10), Some(on(3, 10)), "1.00")),
            Expect::Code(141),
        ),
        row(
            "142_resolution_postings",
            vec![],
            record(
                Entry::void_hold("void", on(3, 2), ghost)
                    .debit("Assets:Cash", eur("1.00"), EUR)
                    .credit("Income:Sales", eur("1.00"), EUR),
            ),
            Expect::Code(142),
        ),
        row(
            "143_pending_outside_hold",
            vec![],
            record(
                Entry::new("pending", on(3, 1))
                    .posting(Posting::debit("Assets:Cash", eur("1.00"), EUR).pending())
                    .posting(Posting::credit("Income:Sales", eur("1.00"), EUR).pending()),
            ),
            Expect::Code(143),
        ),
        row(
            "144_reversal_with_hold_action",
            vec![],
            record(hold("hold-rev", on(3, 1), None, "1.00").reverses(ghost)),
            Expect::Code(144),
        ),
        row(
            "145_post_amount_not_positive",
            vec![],
            record(Entry::post_hold("post", on(3, 2), ghost, partial("0.00"))),
            Expect::Code(145),
        ),
        row(
            "120_non_positive_clearing_amount",
            receivables(),
            clear("clr", invoice, payment, "0.00", on(3, 20)),
            Expect::Code(120),
        ),
        row(
            "121_invalid_period_range",
            vec![],
            period("2027-01", Date::MAX, on(1, 1)),
            Expect::Code(121),
        ),
        row(
            "122_invalid_open_window",
            vec![],
            Op::Register {
                spec: AccountSpec::new("Assets:Short", AccountKind::Asset, on(5, 1))
                    .open_to(on(4, 30)),
                actor: String::from("setup"),
            },
            Expect::Code(122),
        ),
        row(
            "123_exponent_too_large",
            vec![],
            currency(USD, 3, on(1, 1)),
            Expect::Code(123),
        ),
        // ── Idempotency (2xx) ───────────────────────────────────────────────
        row(
            "200_key_conflict",
            vec![record(sale("k", on(1, 15), "10.00"))],
            record(sale("k", on(1, 15), "11.00")),
            Expect::Code(200),
        ),
        row(
            "identical_retry_is_a_replay",
            vec![record(salted(
                sale("k", on(1, 15), "10.00"),
                String::from("memo"),
            ))],
            record(salted(sale("k", on(1, 15), "10.00"), String::from("memo"))),
            Expect::Done,
        ),
        // ── Context (3xx) ───────────────────────────────────────────────────
        row(
            "300_no_period",
            vec![],
            record(sale("next-year", Date::MAX, "1.00")),
            Expect::Code(300),
        ),
        row(
            "301_period_sealed",
            vec![seal("2026-01")],
            record(sale("late", on(1, 31), "1.00")),
            Expect::Code(301),
        ),
        row(
            "302_unknown_account",
            vec![],
            record(
                Entry::new("unknown", on(1, 15))
                    .debit("Assets:Nope", eur("1.00"), EUR)
                    .credit("Income:Sales", eur("1.00"), EUR),
            ),
            Expect::Code(302),
        ),
        row(
            "303_not_a_leaf",
            vec![register("Assets:Cash:Petty", AccountKind::Asset, on(1, 1))],
            record(sale("parent", on(1, 15), "1.00")),
            Expect::Code(303),
        ),
        row(
            "304_account_not_open",
            vec![register("Assets:Bank", AccountKind::Asset, on(6, 1))],
            record(
                Entry::new("early", on(3, 15))
                    .debit("Assets:Bank", eur("1.00"), EUR)
                    .credit("Income:Sales", eur("1.00"), EUR),
            ),
            Expect::Code(304),
        ),
        row(
            "305_undeclared_currency",
            vec![],
            record(
                Entry::new("usd", on(1, 15))
                    .debit("Assets:Cash", eur("1.00"), USD)
                    .credit("Income:Sales", eur("1.00"), USD),
            ),
            Expect::Code(305),
        ),
        row(
            "306_excess_precision",
            vec![currency(JPY, 0, on(1, 1))],
            record(
                Entry::new("yen", on(1, 15))
                    .debit("Assets:Cash", eur("1.50"), JPY)
                    .credit("Income:Sales", eur("1.50"), JPY),
            ),
            Expect::Code(306),
        ),
        row(
            "307_missing_dimension",
            vec![policy(&["segment"], None)],
            record(
                Entry::new("no-segment", on(1, 15))
                    .posting(
                        Posting::debit("Assets:Cash", eur("1.00"), EUR)
                            .dimension("segment", "Retail"),
                    )
                    .credit("Income:Sales", eur("1.00"), EUR),
            ),
            Expect::Code(307),
        ),
        row(
            "308_value_date_drift",
            vec![policy(&[], Some(5))],
            record(sale("drift", on(1, 15), "1.00").value_date(on(1, 25))),
            Expect::Code(308),
        ),
        row(
            "309_at_or_below_watermark",
            vec![seal("2026-01")],
            currency(USD, 2, on(1, 31)),
            Expect::Code(309),
        ),
        row(
            "310_effective_date_regresses",
            vec![change(
                "Assets:Cash",
                ChangeInput::Limit(BalanceLimit::NoCreditBalance),
                on(3, 1),
            )],
            change(
                "Assets:Cash",
                ChangeInput::Limit(BalanceLimit::Unlimited),
                on(2, 15),
            ),
            Expect::Code(310),
        ),
        row(
            "311_unknown_period",
            vec![],
            seal("2027-01"),
            Expect::Code(311),
        ),
        // ── References (4xx) ────────────────────────────────────────────────
        row(
            "400_reversal_target_unknown",
            vec![],
            record(sale("rev-ghost", on(1, 15), "1.00").reverses(ghost)),
            Expect::Code(400),
        ),
        row(
            "401_already_reversed",
            vec![record(rent("rent")), record(reversal("rev-1", on(4, 1)))],
            record(reversal("rev-2", on(4, 2))),
            Expect::Code(401),
        ),
        // PoC: F2 poc_f2_reversal_replay_with_new_id_refused_by_sqlite_only
        row(
            "regress_f2_reversal_retry_is_replay",
            vec![record(rent("rent")), record(reversal("rev-1", on(4, 1)))],
            record(reversal("rev-1", on(4, 1))),
            Expect::Done,
        ),
        row(
            "402_reversal_before_target",
            vec![record(rent("rent"))],
            record(reversal("rev-early", on(3, 4))),
            Expect::Code(402),
        ),
        row(
            "403_not_a_mirror",
            vec![record(rent("rent"))],
            record(
                Entry::new("rev-changed", on(3, 6))
                    .reverses(id("rent"))
                    .debit("Assets:Cash", eur("800.00"), EUR)
                    .credit("Expenses:Rent", eur("800.00"), EUR),
            ),
            Expect::Code(403),
        ),
        row(
            "410_clearing_target_unknown",
            receivables(),
            clear("clr", PostingRef::at(ghost, 0), payment, "1.00", on(3, 20)),
            Expect::Code(410),
        ),
        row(
            "411_clearing_side_mismatch",
            receivables(),
            clear("clr", payment, invoice, "1.00", on(3, 20)),
            Expect::Code(411),
        ),
        row(
            "412_clearing_key_mismatch",
            receivables(),
            clear("clr", invoice, income, "1.00", on(3, 20)),
            Expect::Code(412),
        ),
        row(
            "413_clearing_precision",
            vec![
                currency(JPY, 0, on(1, 1)),
                record(
                    Entry::new("yen-inv", on(3, 1))
                        .debit("Assets:Receivables", eur("100.00"), JPY)
                        .credit("Income:Sales", eur("100.00"), JPY),
                ),
                record(
                    Entry::new("yen-pay", on(3, 2))
                        .debit("Assets:Cash", eur("100.00"), JPY)
                        .credit("Assets:Receivables", eur("100.00"), JPY),
                ),
            ],
            clear(
                "clr",
                PostingRef::at(id("yen-inv"), 0),
                PostingRef::at(id("yen-pay"), 1),
                "0.50",
                on(3, 20),
            ),
            Expect::Code(413),
        ),
        row(
            "414_over_applied",
            receivables(),
            clear("clr", invoice, payment, "500.01", on(3, 20)),
            Expect::Code(414),
        ),
        row(
            "415_clearing_before_posting",
            receivables(),
            clear("clr", invoice, payment, "100.00", on(3, 5)),
            Expect::Code(415),
        ),
        row(
            "416_clearing_unknown",
            receivables(),
            Op::ResetClearing {
                key: String::from("reset"),
                clearing: at(0),
                date: on(3, 20),
            },
            Expect::Code(416),
        ),
        row(
            "417_clearing_already_reset",
            {
                let mut setup = receivables();
                setup.push(clear("clr", invoice, payment, "100.00", on(3, 20)));
                setup.push(Op::ResetClearing {
                    key: String::from("reset-1"),
                    clearing: at(2),
                    date: on(3, 21),
                });
                setup
            },
            Op::ResetClearing {
                key: String::from("reset-2"),
                clearing: at(2),
                date: on(3, 22),
            },
            Expect::Code(417),
        ),
        // PoC: Q-S13 (reasoned finding; the rule is the core's)
        row(
            "418_reset_before_clearing",
            {
                let mut setup = receivables();
                setup.push(clear("clr", invoice, payment, "100.00", on(3, 20)));
                setup
            },
            Op::ResetClearing {
                key: String::from("reset"),
                clearing: at(2),
                date: on(3, 19),
            },
            Expect::Code(418),
        ),
        row(
            "420_account_exists",
            vec![],
            register("Assets:Cash", AccountKind::Liability, on(1, 1)),
            Expect::Code(420),
        ),
        row(
            "421_parent_has_postings",
            vec![record(sale("cash", on(1, 15), "1.00"))],
            register("Assets:Cash:Petty", AccountKind::Asset, on(1, 1)),
            Expect::Code(421),
        ),
        row(
            "422_class_frozen",
            vec![record(sale("cash", on(1, 15), "1.00"))],
            change(
                "Assets:Cash",
                ChangeInput::Class(Some(String::from("bank"))),
                on(2, 1),
            ),
            Expect::Code(422),
        ),
        row(
            "423_postings_after_close",
            vec![record(sale("cash", on(1, 15), "1.00"))],
            change(
                "Assets:Cash",
                ChangeInput::OpenTo(Some(on(1, 10))),
                on(1, 10),
            ),
            Expect::Code(423),
        ),
        row(
            "424_exponent_lowered",
            vec![],
            currency(EUR, 1, on(2, 1)),
            Expect::Code(424),
        ),
        row(
            "425_period_exists",
            vec![],
            period("2026-01", on(1, 1), on(1, 30)),
            Expect::Code(425),
        ),
        // PoC: G1 poc_g1_define_period_reopens_sealed_and_moves_range
        row(
            "regress_g1_period_redefinition_refused",
            vec![seal("2026-01")],
            period("2026-01", on(1, 2), on(1, 31)),
            Expect::Code(425),
        ),
        // PoC: F5 poc_f5_define_period_can_write_sealed_without_a_seal
        row(
            "regress_f5_sealed_only_by_seal_record",
            vec![period("2026-01", on(1, 1), on(1, 31))],
            record(sale("open", on(1, 15), "1.00")),
            Expect::Done,
        ),
        row(
            "426_period_gap",
            vec![],
            period(
                "2027-02",
                Date::from_calendar_date(2027, time::Month::February, 1).unwrap_or(Date::MAX),
                Date::from_calendar_date(2027, time::Month::February, 28).unwrap_or(Date::MAX),
            ),
            Expect::Code(426),
        ),
        row(
            "427_predecessor_not_sealed",
            vec![],
            seal("2026-02"),
            Expect::Code(427),
        ),
        row(
            "429_closing_equity_in_scope",
            vec![record(sale("sale", on(1, 15), "1.00"))],
            Op::CloseYear {
                key: String::from("close"),
                on: on(12, 31),
                equity: account("Income:Sales"),
            },
            Expect::Code(429),
        ),
        row(
            "430_closing_account_not_open",
            vec![
                record(sale("sale", on(1, 15), "1.00")),
                change(
                    "Income:Sales",
                    ChangeInput::OpenTo(Some(on(1, 31))),
                    on(1, 31),
                ),
            ],
            Op::CloseYear {
                key: String::from("close"),
                on: on(6, 30),
                equity: account("Equity:Retained"),
            },
            Expect::Code(430),
        ),
        row(
            "close_year_drafts",
            vec![record(sale("sale", on(1, 15), "1.00"))],
            Op::CloseYear {
                key: String::from("close"),
                on: on(12, 31),
                equity: account("Equity:Retained"),
            },
            Expect::Done,
        ),
        // ── Holds (44x) ─────────────────────────────────────────────────────
        row(
            "440_hold_not_found",
            vec![],
            record(Entry::void_hold("void", on(3, 2), ghost)),
            Expect::Code(440),
        ),
        row(
            "441_not_a_hold",
            vec![record(sale("sale", on(3, 1), "1.00"))],
            record(Entry::void_hold("void", on(3, 2), id("sale"))),
            Expect::Code(441),
        ),
        row(
            "442_hold_resolved",
            {
                let mut setup = held(None);
                setup.push(record(Entry::void_hold("void", on(3, 2), id("hold"))));
                setup
            },
            record(Entry::post_hold(
                "post",
                on(3, 3),
                id("hold"),
                PostAmount::Full,
            )),
            Expect::Code(442),
        ),
        row(
            "443_resolution_before_hold",
            held(None),
            record(Entry::void_hold("void", on(2, 28), id("hold"))),
            Expect::Code(443),
        ),
        row(
            "444_hold_expired",
            held(Some(on(3, 10))),
            record(Entry::post_hold(
                "post",
                on(3, 10),
                id("hold"),
                PostAmount::Full,
            )),
            Expect::Code(444),
        ),
        row(
            "445_hold_never_expires",
            held(None),
            record(Entry::expire_hold("expire", on(3, 20), id("hold"))),
            Expect::Code(445),
        ),
        row(
            "446_hold_not_expired",
            held(Some(on(3, 10))),
            record(Entry::expire_hold("expire", on(3, 9), id("hold"))),
            Expect::Code(446),
        ),
        row(
            "447_post_amount_precision",
            vec![
                currency(JPY, 0, on(1, 1)),
                record(
                    Entry::open_hold("hold", on(3, 1))
                        .debit("Assets:Cash", eur("100.00"), JPY)
                        .credit("Income:Sales", eur("100.00"), JPY),
                ),
            ],
            record(Entry::post_hold(
                "post",
                on(3, 2),
                id("hold"),
                partial("0.50"),
            )),
            Expect::Code(447),
        ),
        row(
            "448_post_exceeds_hold",
            held(None),
            record(Entry::post_hold(
                "post",
                on(3, 2),
                id("hold"),
                partial("150.00"),
            )),
            Expect::Code(448),
        ),
        row(
            "449_post_to_closed_account",
            {
                let mut setup = held(None);
                setup.push(change(
                    "Income:Sales",
                    ChangeInput::OpenTo(Some(on(3, 15))),
                    on(3, 15),
                ));
                setup
            },
            record(Entry::post_hold(
                "post",
                on(3, 20),
                id("hold"),
                PostAmount::Full,
            )),
            Expect::Code(449),
        ),
        row(
            "void_on_a_closed_account_is_recorded",
            {
                let mut setup = held(None);
                setup.push(change(
                    "Income:Sales",
                    ChangeInput::OpenTo(Some(on(3, 15))),
                    on(3, 15),
                ));
                setup
            },
            record(Entry::void_hold("void", on(3, 20), id("hold"))),
            Expect::Done,
        ),
        row(
            "partial_post_is_recorded",
            held(Some(on(3, 10))),
            record(Entry::post_hold(
                "post",
                on(3, 9),
                id("hold"),
                partial("60.00"),
            )),
            Expect::Done,
        ),
        row(
            "expiry_on_the_day_is_recorded",
            held(Some(on(3, 10))),
            record(Entry::expire_hold("expire", on(3, 10), id("hold"))),
            Expect::Done,
        ),
        row(
            "resolution_retry_is_a_replay",
            {
                let mut setup = held(None);
                setup.push(record(Entry::void_hold("void", on(3, 2), id("hold"))));
                setup
            },
            record(Entry::void_hold("void", on(3, 2), id("hold"))),
            Expect::Done,
        ),
        row(
            "450_hold_not_reversible",
            held(None),
            record(
                Entry::new("rev-hold", on(3, 2))
                    .reverses(id("hold"))
                    .credit("Assets:Cash", eur("100.00"), EUR)
                    .debit("Income:Sales", eur("100.00"), EUR),
            ),
            Expect::Code(450),
        ),
        // ── Limits (5xx) ────────────────────────────────────────────────────
        row(
            "500_balance_overflow",
            vec![record(
                Entry::new("max", on(1, 15))
                    .debit("Assets:Cash", max, EUR)
                    .credit("Income:Sales", max, EUR),
            )],
            record(sale("one-more", on(1, 15), "10.00")),
            Expect::Code(500),
        ),
        row(
            "501_limit_exceeded",
            vec![],
            record(
                Entry::new("overdraw", on(1, 15))
                    .debit("Liabilities:Wallet", eur("10.00"), EUR)
                    .credit("Assets:Cash", eur("10.00"), EUR),
            ),
            Expect::Code(501),
        ),
        // ── Precedence between stages ───────────────────────────────────────
        row(
            "precedence_structural_before_idempotency",
            vec![record(sale("used", on(1, 15), "10.00"))],
            record(Entry::new("used", on(1, 15)).debit("Assets:Cash", eur("1.00"), EUR)),
            Expect::Code(101),
        ),
        row(
            "precedence_idempotency_before_context",
            vec![record(sale("used", on(1, 15), "10.00")), seal("2026-01")],
            record(sale("used", on(1, 15), "11.00")),
            Expect::Code(200),
        ),
        row(
            "precedence_replay_after_the_seal",
            vec![record(sale("used", on(1, 15), "10.00")), seal("2026-01")],
            record(sale("used", on(1, 15), "10.00")),
            Expect::Done,
        ),
        row(
            "precedence_context_before_references",
            vec![],
            record(sale("ghost-rev", Date::MAX, "1.00").reverses(ghost)),
            Expect::Code(300),
        ),
        row(
            "precedence_references_before_limits",
            vec![record(rent("rent"))],
            record(
                Entry::new("rev-wallet", on(3, 6))
                    .reverses(id("rent"))
                    .debit("Liabilities:Wallet", eur("99.00"), EUR)
                    .credit("Assets:Cash", eur("99.00"), EUR),
            ),
            Expect::Code(403),
        ),
        row(
            "precedence_hold_shape_before_unknown_account",
            vec![],
            record(
                Entry::open_hold("ghost-hold", on(3, 1))
                    .debit("Assets:Nope", eur("100.00"), EUR)
                    .credit("Income:Sales", eur("60.00"), EUR)
                    .credit("Income:Sales", eur("40.00"), EUR),
            ),
            Expect::Code(140),
        ),
        row(
            "precedence_key_conflict_before_hold_resolved",
            {
                let mut setup = held(None);
                setup.push(record(Entry::void_hold("resolve", on(3, 2), id("hold"))));
                setup
            },
            record(Entry::post_hold(
                "resolve",
                on(3, 2),
                id("hold"),
                PostAmount::Full,
            )),
            Expect::Code(200),
        ),
        row(
            "precedence_overflow_before_limit",
            vec![
                record(
                    Entry::new("fund", on(2, 1))
                        .debit("Expenses:Rent", max, EUR)
                        .credit("Liabilities:Wallet", max, EUR),
                ),
                record(
                    Entry::new("draw-all", on(2, 2))
                        .debit("Liabilities:Wallet", max, EUR)
                        .credit("Assets:Cash", max, EUR),
                ),
            ],
            record(
                Entry::new("draw-more", on(2, 2))
                    .debit("Liabilities:Wallet", eur("0.01"), EUR)
                    .credit("Assets:Cash", eur("0.01"), EUR),
            ),
            Expect::Code(500),
        ),
        // ── Atomic batches ──────────────────────────────────────────────────
        row(
            "batch_records_in_order",
            vec![],
            batch([sale("b-1", on(3, 3), "1.00"), sale("b-2", on(3, 4), "2.00")]),
            Expect::Done,
        ),
        row("batch_empty", vec![], batch([]), Expect::Done),
        row(
            "batch_sees_its_predecessors",
            vec![],
            batch([top_up("top-up", "50.00"), draw("draw", "50.00")]),
            Expect::Done,
        ),
        row(
            "batch_refused_at_index_writes_nothing",
            vec![record(top_up("top-up", "50.00"))],
            batch([
                draw("draw-1", "30.00"),
                sale("between", on(3, 3), "1.00"),
                draw("draw-2", "30.00"),
            ]),
            Expect::Batch(2, 501),
        ),
        row(
            "batch_first_refusal_wins",
            vec![],
            batch([
                sale("ok", on(3, 3), "1.00"),
                sale("", on(3, 3), "1.00"),
                draw("overdraw", "1.00"),
            ]),
            Expect::Batch(1, 100),
        ),
        row(
            "batch_key_conflict_within",
            vec![],
            batch([
                sale("twice", on(3, 3), "1.00"),
                sale("twice", on(3, 3), "2.00"),
            ]),
            Expect::Batch(1, 200),
        ),
        row(
            "batch_replay_within",
            vec![],
            batch([
                sale("twice", on(3, 3), "1.00"),
                sale("twice", on(3, 3), "1.00"),
            ]),
            Expect::Done,
        ),
        row(
            "batch_identical_replays",
            vec![batch([
                sale("r-1", on(3, 3), "1.00"),
                sale("r-2", on(3, 3), "2.00"),
            ])],
            batch([sale("r-1", on(3, 3), "1.00"), sale("r-2", on(3, 3), "2.00")]),
            Expect::Done,
        ),
        row(
            "batch_replay_after_seal",
            vec![
                batch([sale("r-1", on(3, 3), "1.00")]),
                seal("2026-01"),
                seal("2026-02"),
                seal("2026-03"),
            ],
            batch([sale("r-1", on(3, 3), "1.00")]),
            Expect::Done,
        ),
        row(
            "batch_resolves_its_own_hold",
            vec![],
            batch([
                hold("held", on(3, 1), Some(on(3, 9)), "10.00"),
                Entry::post_hold("capture", on(3, 2), id("held"), partial("4.00")),
                sale("after", on(3, 2), "1.00"),
            ]),
            Expect::Done,
        ),
        row(
            "batch_hold_resolved_twice_within",
            vec![],
            batch([
                hold("held", on(3, 1), None, "10.00"),
                Entry::void_hold("void", on(3, 2), id("held")),
                Entry::void_hold("void-again", on(3, 2), id("held")),
            ]),
            Expect::Batch(2, 442),
        ),
        // ── Mixed batches: entries, clearings and resets ───────────────────
        row(
            "batch_records_a_payment_and_clears_it",
            vec![receivables().remove(0)],
            mixed([
                BatchItem::Record(
                    Entry::new("pay-2", on(3, 12))
                        .debit("Assets:Cash", eur("600.00"), EUR)
                        .credit("Assets:Receivables", eur("600.00"), EUR),
                ),
                BatchItem::Clear(Clearing::new(
                    "pay-2/inv-1",
                    invoice,
                    PostingRef::at(id("pay-2"), 1),
                    eur("600.00"),
                    on(3, 12),
                )),
            ]),
            Expect::Done,
        ),
        row(
            "batch_clearings_share_the_open_amount",
            receivables(),
            mixed([
                BatchItem::Clear(Clearing::new(
                    "c-1",
                    invoice,
                    payment,
                    eur("300.00"),
                    on(3, 20),
                )),
                BatchItem::Clear(Clearing::new(
                    "c-2",
                    invoice,
                    payment,
                    eur("300.00"),
                    on(3, 20),
                )),
            ]),
            Expect::Batch(1, 414),
        ),
        row(
            "batch_clearing_before_its_entry",
            vec![receivables().remove(0)],
            mixed([
                BatchItem::Clear(Clearing::new(
                    "early",
                    invoice,
                    PostingRef::at(id("pay-3"), 1),
                    eur("1.00"),
                    on(3, 12),
                )),
                BatchItem::Record(
                    Entry::new("pay-3", on(3, 12))
                        .debit("Assets:Cash", eur("1.00"), EUR)
                        .credit("Assets:Receivables", eur("1.00"), EUR),
                ),
            ]),
            Expect::Batch(0, 410),
        ),
        row(
            "batch_resets_its_own_clearing_and_reclears",
            receivables(),
            mixed([
                BatchItem::Clear(Clearing::new(
                    "c-1",
                    invoice,
                    payment,
                    eur("500.00"),
                    on(3, 20),
                )),
                BatchItem::reset("undo", at(2), on(3, 21)),
                BatchItem::Clear(Clearing::new(
                    "c-2",
                    invoice,
                    payment,
                    eur("500.00"),
                    on(3, 21),
                )),
            ]),
            Expect::Done,
        ),
        row(
            "batch_reset_twice_within",
            {
                let mut setup = receivables();
                setup.push(clear("clr", invoice, payment, "100.00", on(3, 20)));
                setup
            },
            mixed([
                BatchItem::reset("undo-1", at(2), on(3, 21)),
                BatchItem::reset("undo-2", at(2), on(3, 21)),
            ]),
            Expect::Batch(1, 417),
        ),
        row(
            "batch_clearing_replay_within",
            receivables(),
            mixed([
                BatchItem::Clear(Clearing::new(
                    "same",
                    invoice,
                    payment,
                    eur("100.00"),
                    on(3, 20),
                )),
                BatchItem::Clear(Clearing::new(
                    "same",
                    invoice,
                    payment,
                    eur("100.00"),
                    on(3, 20),
                )),
            ]),
            Expect::Done,
        ),
        // Entry ordinals count entries only, not the clearings between them.
        row(
            "batch_entry_after_a_clearing",
            receivables(),
            mixed([
                BatchItem::Clear(Clearing::new(
                    "c-1",
                    invoice,
                    payment,
                    eur("100.00"),
                    on(3, 20),
                )),
                BatchItem::Record(sale("after-clearing", on(3, 20), "1.00")),
            ]),
            Expect::Done,
        ),
        row(
            "124_batch_too_large",
            vec![],
            batch((0..=Op::<2>::MAX_BATCH).map(|n| sale(&format!("big-{n}"), on(3, 3), "1.00"))),
            Expect::Code(124),
        ),
    ]
}

/// A refusal: the rejection, and the refused draft's index for a batch.
type Refused = (Option<usize>, Rejection);

/// A façade outcome as a comparable value.
fn outcome(result: Result<Done<2>, Error>) -> Result<Result<Done<2>, Refused>, String> {
    match result {
        Ok(Done::BatchRejected { index, rejection }) => Ok(Err((Some(index), rejection))),
        Ok(done) => Ok(Ok(done)),
        Err(Error::Rejected(rejection)) => Ok(Err((None, rejection))),
        Err(error) => Err(error.to_string()),
    }
}

/// Runs one row on the reference and on `store`.
pub(crate) async fn run<S: Store<2>>(store: S, row: &Row, ledger: &LedgerId) -> Check {
    let mut journal = Journal::new(ledger.clone()).as_actor("setup");
    let books = Ledger::new(store, ledger.clone())
        .await
        .map_err(|error| format!("the store did not create the ledger: {error}"))?
        .as_actor("setup");
    for (step, op) in row.setup.iter().enumerate() {
        let expected = outcome(journal.run(op))
            .map_err(|error| format!("setup step {step} failed on the reference: {error}"))?;
        let actual = outcome(books.run(op.clone()).await)
            .map_err(|error| format!("setup step {step}: {error}"))?;
        if expected != actual {
            return Err(format!(
                "setup step {step}: expected {expected:?}, got {actual:?}"
            ));
        }
    }
    let before = books.head().await.map_err(|error| error.to_string())?;
    let expected =
        outcome(journal.run(&row.op)).map_err(|error| format!("the reference failed: {error}"))?;
    match (row.expect, &expected) {
        (Expect::Done, Ok(_)) => {}
        (Expect::Code(code), Err((None, rejection))) if rejection.code() == code => {}
        (Expect::Batch(index, code), Err((Some(at), rejection)))
            if (*at, rejection.code()) == (index, code) => {}
        _ => {
            return Err(format!(
                "the reference answered {expected:?}, the row expects {:?}",
                row.expect
            ));
        }
    }
    let actual = outcome(books.run(row.op.clone()).await)?;
    if actual != expected {
        return Err(format!("expected {expected:?}, got {actual:?}"));
    }
    let after = books.head().await.map_err(|error| error.to_string())?;
    if after != journal.head() {
        return Err(String::from("the store's head is not the reference's"));
    }
    if expected.is_err() && after != before {
        return Err(String::from("a refused operation changed the store"));
    }
    // Every table the row left must be the replay of its log.
    books
        .audit()
        .await
        .map_err(|error| format!("the audit after the row: {error}"))?;
    Ok(())
}

/// `regress_f1_position_is_leaf_index` (PoC: F1): after N records the next
/// entry is recorded at position N, and its inclusion proof verifies at
/// size N + 1 against the leaf the store holds.
// PoC: F1 poc_f1_prune_whole_prefix_then_append_misnumbers
pub(crate) async fn regress_f1_position_is_leaf_index<S: Store<2>>(
    store: S,
    ledger: &LedgerId,
) -> Check {
    let books = Ledger::new(store, ledger.clone())
        .await
        .map_err(|error| error.to_string())?
        .as_actor("setup");
    for op in base() {
        books.run(op).await.map_err(|error| error.to_string())?;
    }
    for n in 0..20 {
        let size = books.head().await.map_err(|error| error.to_string())?.size;
        let recorded = books
            .record(sale(&format!("f1-{n}"), on(1, 15), "1.00"))
            .await
            .map_err(|error| error.to_string())?;
        if recorded.position != size {
            return Err(format!(
                "entry {n} was recorded at {}, the size was {size}",
                recorded.position
            ));
        }
        let head = books.head().await.map_err(|error| error.to_string())?;
        let proof = books
            .prove_entry(recorded.id)
            .await
            .map_err(|error| error.to_string())?;
        let leaf = books
            .store()
            .read_leaves(&LeafRead {
                from: size,
                through: None,
                kinds: None,
                limit: 1,
            })
            .await
            .map_err(|error| error.to_string())?;
        let bytes = leaf
            .items
            .first()
            .map(|leaf| leaf.bytes.clone())
            .ok_or("no leaf at the position")?;
        if head.size != size.saturating_add(1) {
            return Err(format!("the head after entry {n} has size {}", head.size));
        }
        proof
            .verify(&leaf_hash(&bytes), &head)
            .map_err(|error| format!("the proof of entry {n}: {error}"))?;
    }
    Ok(())
}

/// Memo erasure: the four outcomes, an unchanged head, proof and audit, the
/// text gone, and a later submission of the key a key conflict — on the
/// store as on the reference.
pub(crate) async fn erasure<S: Store<2>>(store: S, ledger: &LedgerId) -> Check {
    let books = Ledger::new(store, ledger.clone())
        .await
        .map_err(|error| error.to_string())?
        .as_actor("setup");
    let mut journal = Journal::new(ledger.clone()).as_actor("setup");
    let draft = sale("memo", on(1, 15), "1.00").memo_salted("Max Mustermann", MemoSalt([3; 16]));
    let mut ops = base();
    ops.push(record(draft.clone()));
    ops.push(record(sale("plain", on(1, 15), "1.00")));
    for op in &ops {
        journal.run(op).map_err(|error| error.to_string())?;
        books
            .run(op.clone())
            .await
            .map_err(|error| error.to_string())?;
    }
    let id = |key: &str| {
        IdempotencyKey::new(key)
            .map(|key| EntryId::derive(ledger, &key))
            .map_err(|error| error.to_string())
    };
    let (memo, plain) = (id("memo")?, id("plain")?);
    let store = books.store();
    let head = books.head().await.map_err(|error| error.to_string())?;
    let fail = |error: crate::store::StoreError| error.to_string();
    let outcomes = [
        store.erase_memo(memo).await.map_err(fail)?,
        store.erase_memo(memo).await.map_err(fail)?,
        store.erase_memo(plain).await.map_err(fail)?,
        store
            .erase_memo(EntryId::from_bytes([9; 16]))
            .await
            .map_err(fail)?,
    ];
    let expected = [
        Erasure::Erased,
        Erasure::AlreadyErased,
        Erasure::NoMemo,
        Erasure::UnknownEntry,
    ];
    if outcomes != expected {
        return Err(format!(
            "erasure outcomes: expected {expected:?}, got {outcomes:?}"
        ));
    }
    journal
        .erase_memo(memo)
        .map_err(|error| error.to_string())?;
    if store.memo(memo).await.map_err(fail)? != Some(Memo::Erased)
        || store.memo(plain).await.map_err(fail)? != Some(Memo::None)
    {
        return Err(String::from(
            "an erased memo still reads as kept, or a missing one as erased",
        ));
    }
    if books.head().await.map_err(|error| error.to_string())? != head {
        return Err(String::from("the erasure changed the head"));
    }
    let proof = books
        .prove_entry(memo)
        .await
        .map_err(|error| error.to_string())?;
    let leaf = journal
        .leaves()
        .find(|leaf| Some(leaf.position) == Some(proof.index))
        .map(|leaf| leaf.bytes.to_vec())
        .ok_or("no leaf")?;
    proof
        .verify(&leaf_hash(&leaf), &head)
        .map_err(|error| format!("the proof after erasure: {error}"))?;
    books
        .audit()
        .await
        .map_err(|error| format!("the audit after erasure: {error}"))?;
    let expected = outcome(journal.run(&record(draft.clone())))?;
    let actual = outcome(books.run(record(draft)).await)?;
    match (&expected, &actual) {
        (Err(a), Err(b)) if a == b && a.1.code() == 200 => Ok(()),
        _ => Err(format!(
            "a resubmission after erasure: expected a key conflict, got {expected:?} and {actual:?}"
        )),
    }
}

/// `reads_honour_through` (PoC: A3-S8): `entry`, `holds` by id and `seals`
/// read only what was recorded below the `through` asked — what proofs at a
/// historical size and the audit rely on.
pub(crate) async fn reads_honour_through<S: Store<2>>(store: S, ledger: &LedgerId) -> Check {
    use crate::store::{EntryRef, HoldRead};
    let books = Ledger::new(store, ledger.clone())
        .await
        .map_err(|error| error.to_string())?
        .as_actor("setup");
    for op in base() {
        books.run(op).await.map_err(|error| error.to_string())?;
    }
    let early = books
        .record(sale("early", on(1, 10), "1.00"))
        .await
        .map_err(|error| error.to_string())?;
    let then = books.head().await.map_err(|error| error.to_string())?.size;
    let late = books
        .record(sale("late", on(1, 11), "1.00"))
        .await
        .map_err(|error| error.to_string())?;
    let held = books
        .record(hold("late-hold", on(1, 12), None, "1.00"))
        .await
        .map_err(|error| error.to_string())?;
    books
        .seal_period("2026-01")
        .await
        .map_err(|error| error.to_string())?;
    let store = books.store();
    let failed = |error: crate::store::StoreError| error.to_string();
    if store
        .entry(EntryRef::Id(early.id), Some(then))
        .await
        .map_err(failed)?
        .is_none()
    {
        return Err(String::from("an entry below `through` was not found"));
    }
    for (what, id) in [("entry", late.id), ("hold", held.id)] {
        if store
            .entry(EntryRef::Id(id), Some(then))
            .await
            .map_err(failed)?
            .is_some()
        {
            return Err(format!(
                "`entry` returned an {what} recorded at or after `through`"
            ));
        }
    }
    let holds = store
        .holds(&HoldRead::ById {
            id: held.id,
            through: Some(then),
        })
        .await
        .map_err(failed)?;
    if !holds.items.is_empty() {
        return Err(String::from(
            "`holds` returned a hold opened at or after `through`",
        ));
    }
    if !store.seals(Some(then)).await.map_err(failed)?.is_empty() {
        return Err(String::from(
            "`seals` returned a seal recorded at or after `through`",
        ));
    }
    if store.seals(None).await.map_err(failed)?.len() != 1 {
        return Err(String::from("`seals` lost the seal"));
    }
    Ok(())
}

/// `open_holds_by_account` (PoC: A3-S8): an account's open holds, on either
/// side, page by page — and none once resolved.
pub(crate) async fn open_holds_by_account<S: Store<2>>(store: S, ledger: &LedgerId) -> Check {
    use crate::model::AccountHandle;
    use crate::store::HoldRead;
    use crate::{HoldCursor, PageRead};
    let books = Ledger::new(store, ledger.clone())
        .await
        .map_err(|error| error.to_string())?
        .as_actor("setup");
    for op in base() {
        books.run(op).await.map_err(|error| error.to_string())?;
    }
    let mut opened = Vec::new();
    for n in 0..3 {
        opened.push(
            books
                .record(hold(&format!("hold-{n}"), on(2, 1), None, "1.00"))
                .await
                .map_err(|error| error.to_string())?,
        );
    }
    let first = opened.first().ok_or("no hold")?;
    books
        .record(Entry::void_hold("void-0", on(2, 2), first.id))
        .await
        .map_err(|error| error.to_string())?;
    let open: Vec<u64> = opened.iter().skip(1).map(|hold| hold.position).collect();
    // Assets:Cash (handle 0) is debited, Income:Sales (handle 1) credited.
    for handle in [0u32, 1] {
        let handle = AccountHandle::new(handle).ok_or("no handle")?;
        let mut page: PageRead<HoldCursor> = PageRead::first(1);
        let mut found = Vec::new();
        loop {
            let read = books
                .store()
                .holds(&HoldRead::OpenByAccount {
                    handle,
                    page: page.clone(),
                })
                .await
                .map_err(|error| error.to_string())?;
            found.extend(read.items.iter().map(|hold| hold.position));
            match read.next {
                Some(next) => page = page.after(next),
                None => break,
            }
        }
        if found != open {
            return Err(format!(
                "the open holds of account {handle} are {found:?}, expected {open:?}"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The codes no store can be driven to: 428 needs 2³² registered accounts
    /// (the rule core proves it with a stub view).
    const UNREACHABLE: [u16; 1] = [428];

    /// Every code of the rule core's catalogue.
    const CODES: [u16; 74] = [
        100, 101, 102, 103, 104, 105, 106, 107, 108, 109, 110, 111, 113, 114, 120, 121, 122, 123,
        140, 141, 142, 143, 144, 145, 200, 300, 301, 302, 303, 304, 305, 306, 307, 308, 309, 310,
        311, 400, 401, 402, 403, 410, 411, 412, 413, 414, 415, 416, 417, 418, 420, 421, 422, 423,
        424, 425, 426, 427, 428, 429, 430, 440, 441, 442, 443, 444, 445, 446, 447, 448, 449, 450,
        500, 501,
    ];

    #[test]
    fn every_reachable_code_has_a_row() {
        let ledger = LedgerId::new("example.com/conformance").unwrap();
        let rows = rows(&ledger);
        for code in CODES {
            let covered = rows.iter().any(|row| row.expect == Expect::Code(code));
            assert!(
                covered || UNREACHABLE.contains(&code),
                "code {code} has no row"
            );
        }
        let mut names: Vec<&str> = rows.iter().map(|row| row.name.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), rows.len(), "row names are unique");
    }
}
