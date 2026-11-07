//! The surface rules that hold for every item: the one error and how
//! every payload folds into it, the error types a fallible public function
//! may return (read from the reviewed listing `tests/public-api.txt`), and
//! the prelude.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use doubleentry::{
    Entry, EntryId, Error, Rejection,
    error::{AmountError, BackendError, IntegrityError, IntegrityKind, QueryError, ValueError},
};

fn shareable<T: Send + Sync + Clone + 'static>() {}

/// T007: `Error` is cheap to move and can cross threads. The bound is
/// clippy's `result_large_err` (128 bytes): the rich `Rejection` stays
/// unboxed so it can be matched in place (amended from 64, 022 FR-015).
#[test]
fn the_error_is_small_and_shareable() {
    shareable::<Error>();
    assert!(size_of::<Error>() <= 128, "{}", size_of::<Error>());
}

/// T007: every payload folds into exactly its variant with its `Display`
/// prefix; only a backend failure is ever transient or may have applied.
#[test]
fn every_payload_folds_into_its_variant() {
    let id = EntryId::from_bytes([7; 16]);
    let cases: Vec<(Error, &str)> = vec![
        (
            Rejection::InvalidKey {
                reason: ValueError::Empty,
            }
            .into(),
            "rejected: [100] ",
        ),
        (ValueError::Empty.into(), "invalid value: "),
        (AmountError::Overflow.into(), "amount: "),
        (QueryError::UnknownEntry { id }.into(), "query: "),
        (
            IntegrityError::new(IntegrityKind::HeadMismatch, Some(42)).into(),
            "integrity failure at position 42: ",
        ),
        (
            Entry::<2>::new("k", doubleentry::date!("2026-03-15"))
                .kind("")
                .fingerprint()
                .unwrap_err()
                .into(),
            "rejected: [",
        ),
        (
            doubleentry::log::ProofError::EmptyTree.into(),
            "transparency: proof: ",
        ),
        (
            Error::BatchRejected {
                index: 3,
                rejection: Rejection::InvalidKey {
                    reason: ValueError::Empty,
                },
            },
            "rejected: item 3 of the batch: [100] ",
        ),
    ];
    for (error, prefix) in &cases {
        assert!(error.to_string().starts_with(prefix), "{error}");
        assert!(
            !error.is_transient() && !error.may_have_applied(),
            "{error}"
        );
        assert!(std::error::Error::source(error).is_some(), "{error}");
    }
    assert!(matches!(cases[0].0, Error::Rejected(_)));
    assert!(matches!(cases[1].0, Error::Value(_)));
    assert!(matches!(cases[2].0, Error::Amount(_)));
    assert!(matches!(cases[3].0, Error::Query(_)));
    assert!(matches!(cases[4].0, Error::Integrity(_)));
    assert!(matches!(cases[5].0, Error::Rejected(_)));
    assert!(matches!(
        cases[6].0,
        Error::Transparency(doubleentry::transparency::Error::Proof(_))
    ));
    assert_eq!(cases[0].0.rejection().map(Rejection::code), Some(100));
    assert_eq!(cases[7].0.rejection().map(Rejection::code), Some(100));

    let io = || std::io::Error::other("connection reset");
    let transient: Error = BackendError::transient(io()).into();
    assert!(transient.is_transient() && !transient.may_have_applied());
    assert_eq!(
        transient.to_string(),
        "backend failure: connection reset (transient)"
    );
    let unknown: Error = BackendError::transient(io()).outcome_unknown().into();
    assert!(unknown.is_transient() && unknown.may_have_applied());
    let permanent: Error = BackendError::permanent(io()).into();
    assert!(!permanent.is_transient() && !permanent.may_have_applied());
    assert_eq!(permanent.rejection(), None);
}

/// The error type of every `Result<_, E>` in a line of the listing.
fn error_types(line: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = line;
    while let Some(at) = rest.find("Result<") {
        let args = &rest[at + "Result<".len()..];
        let (mut depth, mut last, mut end) = (1, 0, args.len());
        for (index, c) in args.char_indices() {
            match c {
                '<' => depth += 1,
                '>' if depth == 1 => {
                    end = index;
                    break;
                }
                '>' => depth -= 1,
                ',' if depth == 1 => last = index + 1,
                _ => {}
            }
        }
        if last > 0 {
            found.push(args[last..end].trim().to_owned());
        }
        rest = &rest[at + 1..];
    }
    found
}

/// T011 (FR-012): a fallible public function returns the one `Error`, a
/// narrow error of its module that folds into it, or — for implementers of
/// a store and the conformance suite — the protocol's own types.
#[test]
fn fallible_functions_return_the_known_errors() {
    let allowed = [
        // The one error, and the narrow errors of standalone functions.
        "doubleentry::Error",
        "doubleentry::error::ValueError",
        "doubleentry::error::AmountError",
        "doubleentry::error::BuildError",
        "doubleentry::log::ProofError",
        "doubleentry::record::DecodeError",
        "doubleentry::record::SealOrderError",
        "doubleentry::error::QueryError",
        // Transparency artefacts: notes, checkpoints, keys, policies,
        // witness replies, bundles, tiles and the verifier (spec 024).
        "doubleentry::transparency::Error",
        // The export's own check, for a recipient without a ledger.
        "doubleentry::export::VerifyError",
        // The store protocol: a store's failures, the core's refusal inside
        // a write, and the audit's replay.
        "doubleentry::store::StoreError",
        "doubleentry::Rejection",
        "doubleentry::error::AuditFailure",
        // The conformance suite's report of a failing store.
        "doubleentry::testing::Failure",
        // `Result`'s own default, trait-associated errors and serde's.
        "E",
        "Self::Error",
        "Self::Err",
    ];
    let listing = include_str!("public-api.txt");
    let mut checked = 0;
    for line in listing.lines() {
        for error in error_types(line) {
            checked += 1;
            let serde = error.starts_with('<') && error.contains(" as serde_core::");
            assert!(
                serde || allowed.contains(&error.as_str()),
                "{error} in: {line}"
            );
        }
    }
    assert!(checked > 100, "the listing names {checked} results");
}

/// 025 T004: every query error is one lowercase sentence under the
/// `query` prefix.
#[test]
fn every_query_error_reads_as_one_sentence() {
    use doubleentry::{currency::EUR, date, error::OverflowContext};
    let errors = [
        QueryError::InvalidWindow {
            start: date!("2026-03-31"),
            end: date!("2026-03-01"),
        },
        QueryError::UnknownPeriod {
            id: "2027-13".into(),
        },
        QueryError::UnknownAccount {
            account: "Assets".into(),
        },
        QueryError::InvalidLabel { text: " ".into() },
        QueryError::UnknownEntry {
            id: EntryId::from_bytes([1; 16]),
        },
        QueryError::NotSealed {
            period: "2026-03".into(),
        },
        QueryError::PositionBeyondHead {
            position: 250,
            size: 100,
        },
        QueryError::InvalidSizes { old: 0, new: 3 },
        QueryError::Overflow {
            context: OverflowContext::Totals(EUR),
        },
    ];
    for error in errors {
        let text = Error::from(error.clone()).to_string();
        let sentence = text.strip_prefix("query: ").unwrap();
        assert!(
            !sentence.ends_with('.') && !sentence.contains('\n'),
            "{text}"
        );
        assert!(!sentence.starts_with(char::is_uppercase), "{text}");
    }
    assert_eq!(
        QueryError::Overflow {
            context: OverflowContext::Totals(EUR)
        }
        .to_string(),
        "an amount cannot hold the totals in EUR"
    );
}

/// T011: the narrow errors fold into `Error` with `?`.
#[test]
fn narrow_errors_fold_with_the_question_mark() {
    fn value() -> doubleentry::Result<()> {
        doubleentry::LedgerId::new("not a valid id!")?;
        Ok(())
    }
    fn amount() -> doubleentry::Result<()> {
        "1.234".parse::<doubleentry::Amount>()?;
        Ok(())
    }
    fn query() -> doubleentry::Result<()> {
        Err(QueryError::UnknownEntry {
            id: EntryId::from_bytes([1; 16]),
        })?
    }
    assert!(matches!(value(), Err(Error::Value(_))));
    assert!(matches!(amount(), Err(Error::Amount(_))));
    assert!(matches!(query(), Err(Error::Query(_))));
}

/// T027 (FR-019): the prelude is the everyday vocabulary — every name in
/// it is used here — and it never grows past 35 names.
#[test]
fn the_prelude_is_the_everyday_vocabulary() {
    use doubleentry::prelude::*;

    fn names(source: &str) -> usize {
        source
            .lines()
            .filter_map(|line| line.trim().strip_prefix("pub use crate::"))
            .filter_map(|line| line.split_once('{').map(|(_, list)| list))
            .map(|list| list.trim_end_matches("};").split(',').count())
            .sum()
    }
    let count = names(include_str!("../src/prelude.rs"));
    assert!(count <= 35, "{count} names");

    let run = || -> Result<Recorded> {
        let mut books: Journal = Journal::new(LedgerId::new("example.com/acme")?);
        let on: Date = date!("2026-01-01");
        for code in [EUR, USD, GBP, CHF, JPY] {
            books.declare_currency(code, if code == JPY { 0 } else { 2 }, on)?;
        }
        let cash: AccountHandle = books.open("Assets:Cash", AccountKind::Asset, on)?;
        books.open("Income:Sales", AccountKind::Income, on)?;
        books.set_limit(cash, BalanceLimit::Unlimited, on)?;
        books.define_months(2026, 1..=1)?;
        let amount: Amount<4> = amount!("10.0050");
        let amount: Amount = amount.rescale(Rounding::HalfEven)?;
        let third = amount.distribute(3)?[0];
        let path: AccountPath = "Assets:Cash".parse()?;
        let label = Label::new("region")?;
        let entry: Entry = Entry::new("sale", date!("2026-01-15"))
            .document(Document::new("invoice", "INV-1")?)
            .rate(Rate::new(EUR, USD, 108, -2)?)
            .debit(path, third, EUR)
            .credit("Income:Sales", third, EUR);
        let recorded = books.record(entry)?;
        let id: EntryId = recorded.id;
        let _ = (PostingRef::at(id, 0), Side::Debit, Layer::Settled, label);
        let _: Option<Balance> = books.balance("Assets:Cash", EUR, &BalanceQuery::all()).ok();
        let _: fn(&Rejection) -> u16 = Rejection::code;
        let _: Option<Error> = None;
        let _: Option<Ledger<doubleentry::store::MemoryStore>> = None;
        Ok(recorded)
    };
    assert!(!run().unwrap().replayed);
}

/// T037 (FR-024): every public enum is `#[non_exhaustive]` but the three
/// closed by definition, and every public struct with public fields is,
/// unless callers or store implementers must build it — the reviewed list
/// below: request and option types (built with `..Default::default()` or
/// literally), the rows, views and facts a third-party store answers with,
/// the record payloads of the frozen leaf format, and values defined by
/// RFC 6962 (heads and proofs). A new struct joins the list only by review.
#[test]
fn public_enums_and_output_structs_are_non_exhaustive() {
    const CLOSED_ENUMS: [&str; 3] = [
        "doubleentry::AccountKind",
        "doubleentry::Layer",
        "doubleentry::Side",
    ];
    const CONSTRUCTIBLE: [&str; 51] = [
        // Values: RFC 6962 heads and proofs, references, scaled amounts.
        "doubleentry::TreeHead",
        "doubleentry::log::InclusionProof",
        "doubleentry::log::ConsistencyProof",
        "doubleentry::log::BalanceProof",
        "doubleentry::log::SealedBalanceProof",
        "doubleentry::PostingRef",
        "doubleentry::error::ScaledAmount",
        "doubleentry::store::BalanceKey",
        "doubleentry::MemoSalt",
        // Record payloads of the frozen leaf format.
        "doubleentry::record::Genesis",
        "doubleentry::record::AccountEvent",
        "doubleentry::record::AccountRegistration",
        "doubleentry::record::ClearingEvent",
        "doubleentry::record::CurrencyDeclaration",
        "doubleentry::record::EntryParts",
        "doubleentry::record::PolicyEvent",
        "doubleentry::record::PostingRecord",
        "doubleentry::record::SealParts",
        "doubleentry::record::TreeHeadEntry",
        // Inputs: a leaf to replay, pages, read requests, options.
        "doubleentry::log::ReplayLeaf",
        "doubleentry::Page",
        "doubleentry::PageRead",
        "doubleentry::store::BalanceRead",
        "doubleentry::store::LeafRead",
        "doubleentry::store::PostingRead",
        "doubleentry::store::ProjectionRead",
        "doubleentry::store::PostingCursor",
        "doubleentry::store::SqlOptions",
        "doubleentry::testing::Config",
        // What a store answers with: rows, views and facts.
        "doubleentry::store::AccountRow",
        "doubleentry::store::AccountView",
        "doubleentry::store::ClearingView",
        "doubleentry::store::EntryMeta",
        "doubleentry::store::EntryView",
        "doubleentry::store::Header",
        "doubleentry::store::HoldRow",
        "doubleentry::store::HoldView",
        "doubleentry::store::LatestTerms",
        "doubleentry::store::Leaf",
        "doubleentry::store::LedgerInfo",
        "doubleentry::store::MemoPlain",
        "doubleentry::store::Movement",
        "doubleentry::store::Outstanding",
        "doubleentry::store::PeriodView",
        "doubleentry::store::PostingLine",
        "doubleentry::store::SealRow",
        "doubleentry::store::Slice",
        "doubleentry::store::StoredEntry",
        "doubleentry::store::Terms",
        "doubleentry::store::Versioned",
        "doubleentry::store::Window",
    ];
    let listing = include_str!("public-api.txt");
    let item = |line: &str, keyword: &str| -> Option<(bool, String)> {
        let (marked, rest) = match line.strip_prefix("#[non_exhaustive] ") {
            Some(rest) => (true, rest),
            None => (false, line),
        };
        let name = rest.strip_prefix(keyword)?;
        let end = name.find(['<', '(', ' ']).unwrap_or(name.len());
        Some((marked, name[..end].to_owned()))
    };
    let mut enums = 0;
    for line in listing.lines() {
        if let Some((marked, name)) = item(line, "pub enum ") {
            enums += 1;
            assert!(
                marked || CLOSED_ENUMS.contains(&name.as_str()) || name.contains("::prelude::"),
                "{name} is not #[non_exhaustive]"
            );
        }
    }
    let mut structs = 0;
    for line in listing.lines() {
        let Some((marked, name)) = item(line, "pub struct ") else {
            continue;
        };
        let field = format!("pub {name}::");
        let tuple = line.contains("(pub ");
        let has_fields = tuple
            || listing
                .lines()
                .any(|line| line.starts_with(&field) && line.contains(": "));
        if has_fields && !name.contains("::prelude::") {
            structs += 1;
            assert!(
                marked || CONSTRUCTIBLE.contains(&name.as_str()),
                "{name} has public fields and is neither #[non_exhaustive] nor reviewed as constructible"
            );
        }
    }
    assert!(
        enums > 40 && structs > 60,
        "{enums} enums, {structs} structs"
    );
}
