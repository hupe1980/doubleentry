//! Master data: account registrations and changes, currency declarations
//! and the ledger policy — each a dated record of the log.
//!
//! An identical repeat is `Unchanged` (setup code may run on every start).
//! A field's effective dates never decrease (310), nothing takes effect on or
//! before the end of the last sealed period (309), an account's path and kind
//! never change, its class is fixed once it has postings, a currency's
//! exponent is never lowered, and a policy takes effect only after the latest
//! booking date already recorded (431) — so no master-data record ever
//! contradicts an entry already in the log.

use time::Date;

use super::state::{AccountView, Header, StateView, Window};
use super::writeset::{AccountTerm, Done, IndexRow, Outcome, WriteSet};
use crate::error::{LabelField, Rejection};
use crate::model::{
    AccountAction, AccountChange, AccountEvent, AccountHandle, AccountKind, AccountPath,
    AccountRef, AccountRegistration, BalanceLimit, Currency, CurrencyDeclaration, Label,
    LedgerPolicy, PolicyEvent, Record,
};

use super::check::ChangeInput;

/// An account to register: path, kind and first open day, with optional
/// class, last open day and balance limit. Built infallibly; the path and
/// class are validated when the account is registered.
///
/// ```
/// use doubleentry::{AccountKind, AccountSpec, BalanceLimit, Journal, LedgerId};
/// use time::macros::date;
///
/// let mut books = Journal::new(LedgerId::new("example.com/acme")?);
/// let wallet = books.register(
///     AccountSpec::new("Liabilities:Wallets:alice", AccountKind::Liability, date!(2026 - 01 - 01))
///         .class("customer")
///         .limit(BalanceLimit::NoDebitBalance),
/// )?;
/// assert_eq!(wallet.get(), 0);
/// # Ok::<(), doubleentry::Error>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountSpec {
    path: String,
    kind: AccountKind,
    open_from: Date,
    class: Option<String>,
    open_to: Option<Date>,
    limit: BalanceLimit,
}

impl AccountSpec {
    /// An account at `path` of `kind`, open from `open_from` (the date the
    /// registration takes effect), without class, end or limit.
    pub fn new(path: impl AsRef<str>, kind: AccountKind, open_from: Date) -> Self {
        Self {
            path: path.as_ref().to_owned(),
            kind,
            open_from,
            class: None,
            open_to: None,
            limit: BalanceLimit::Unlimited,
        }
    }

    /// With a class (a label of the caller's choosing).
    #[must_use]
    pub fn class(self, class: impl AsRef<str>) -> Self {
        Self {
            class: Some(class.as_ref().to_owned()),
            ..self
        }
    }

    /// With a last open day.
    #[must_use]
    pub fn open_to(self, last_day: Date) -> Self {
        Self {
            open_to: Some(last_day),
            ..self
        }
    }

    /// With a balance limit.
    #[must_use]
    pub fn limit(self, limit: BalanceLimit) -> Self {
        Self { limit, ..self }
    }

    /// The path, as given.
    #[cfg(feature = "testing")]
    pub(crate) fn path(&self) -> &str {
        &self.path
    }

    /// The kind.
    #[cfg(feature = "testing")]
    pub(crate) const fn kind(&self) -> AccountKind {
        self.kind
    }
}

/// The actor of a master-data record (107 when it is not a label).
fn actor(actor: &str) -> Result<Label, Rejection> {
    Label::new(actor).map_err(|reason| Rejection::InvalidLabel {
        field: LabelField::Actor,
        reason,
    })
}

/// 309: nothing takes effect on or before the sealed watermark.
fn above_watermark<const P: u8>(header: &Header<P>, effective: Date) -> Result<(), Rejection> {
    match header.watermark {
        Some(watermark) if effective <= watermark => Err(Rejection::AtOrBelowWatermark {
            effective,
            watermark,
        }),
        _ => Ok(()),
    }
}

/// 310: a field's effective dates never decrease.
fn not_before(effective: Date, latest: Date) -> Result<(), Rejection> {
    if effective < latest {
        Err(Rejection::EffectiveDateRegresses { effective, latest })
    } else {
        Ok(())
    }
}

/// Registers an account: 103, 107, 122 → identical repeat → 309 → 420, 421,
/// 428.
pub(crate) fn register<const P: u8>(
    view: &impl StateView<P>,
    spec: &AccountSpec,
    by: &str,
) -> Result<Outcome<P>, Rejection> {
    let path = AccountPath::parse(&spec.path).map_err(|reason| Rejection::InvalidPath {
        posting: None,
        reason,
    })?;
    let class = spec
        .class
        .as_deref()
        .map(Label::new)
        .transpose()
        .map_err(|reason| Rejection::InvalidLabel {
            field: LabelField::Class,
            reason,
        })?;
    let actor = actor(by)?;
    if let Some(open_to) = spec.open_to
        && open_to < spec.open_from
    {
        return Err(Rejection::InvalidOpenWindow {
            open_from: spec.open_from,
            open_to,
        });
    }
    let existing = view.account_by_path(&path);
    let mut ancestors = Vec::new();
    let mut parent = path.parent();
    while let Some(path) = parent {
        parent = path.parent();
        ancestors.push(view.account_by_path(&path));
    }
    let header = view.header();
    let same = |account: &AccountView| {
        let registered = &account.registration;
        registered.kind == spec.kind
            && registered.class == class
            && registered.open_from == spec.open_from
            && registered.open_to == spec.open_to
            && registered.limit == spec.limit
    };
    if let Some(account) = &existing
        && same(account)
    {
        return Ok(Outcome::Unchanged(Done::Account(account.handle())));
    }
    above_watermark(&header, spec.open_from)?;
    if let Some(account) = existing {
        return Err(Rejection::AccountExists {
            path,
            handle: account.handle(),
        });
    }
    if let Some(parent) = ancestors
        .into_iter()
        .flatten()
        .find(|ancestor| ancestor.last_booked.is_some())
    {
        return Err(Rejection::ParentHasPostings {
            parent: parent.path().clone(),
        });
    }
    let handle = AccountHandle::new(header.accounts).ok_or(Rejection::TooManyAccounts)?;
    let registration = AccountRegistration {
        handle,
        path,
        kind: spec.kind,
        class,
        open_from: spec.open_from,
        open_to: spec.open_to,
        limit: spec.limit,
    };
    let position = header.size;
    let effective = registration.open_from;
    let versions = [
        AccountTerm::Window(Window {
            open_from: registration.open_from,
            open_to: registration.open_to,
        }),
        AccountTerm::Limit(registration.limit),
        AccountTerm::Class(registration.class.clone()),
    ];
    let write = WriteSet::of(
        position,
        Record::Account(AccountEvent {
            action: AccountAction::Register(registration.clone()),
            effective,
            actor,
        }),
        Done::Account(handle),
    )
    .with(IndexRow::Account {
        registration,
        position,
    });
    Ok(Outcome::from(versions.into_iter().fold(
        write,
        |write, term| {
            write.with(IndexRow::AccountVersion {
                handle,
                term,
                effective,
                position,
            })
        },
    )))
}

/// Changes one account field: 107 → 302 → identical latest value → 122 →
/// 309 → 310 → 422 → 423.
pub(crate) fn change<const P: u8>(
    view: &impl StateView<P>,
    account: &AccountRef,
    change: &ChangeInput,
    effective: Date,
    by: &str,
) -> Result<Outcome<P>, Rejection> {
    let change = match change {
        ChangeInput::OpenTo(open_to) => AccountChange::OpenTo(*open_to),
        ChangeInput::Limit(limit) => AccountChange::Limit(*limit),
        ChangeInput::Class(class) => {
            AccountChange::Class(class.as_deref().map(Label::new).transpose().map_err(
                |reason| Rejection::InvalidLabel {
                    field: LabelField::Class,
                    reason,
                },
            )?)
        }
    };
    let actor = actor(by)?;
    let found = match account {
        AccountRef::Path(path) => view.account_by_path(path),
        AccountRef::Handle(handle) => view.account(*handle),
    }
    .ok_or_else(|| Rejection::UnknownAccount {
        account: account.clone(),
    })?;
    let header = view.header();
    let latest = &found.latest;
    let (unchanged, since) = match &change {
        AccountChange::OpenTo(open_to) => (
            latest.window.value.open_to == *open_to,
            latest.window.effective,
        ),
        AccountChange::Limit(limit) => (latest.limit.value == *limit, latest.limit.effective),
        AccountChange::Class(class) => (latest.class.value == *class, latest.class.effective),
    };
    if unchanged && effective >= since {
        return Ok(Outcome::Unchanged(Done::Changed));
    }
    // 122 is structural: it precedes the context checks.
    let open_from = found.registration.open_from;
    if let AccountChange::OpenTo(Some(open_to)) = change
        && open_to < open_from
    {
        return Err(Rejection::InvalidOpenWindow { open_from, open_to });
    }
    above_watermark(&header, effective)?;
    not_before(effective, since)?;
    if matches!(change, AccountChange::Class(_)) && found.last_booked.is_some() {
        return Err(Rejection::ClassFrozen {
            account: found.path().clone(),
        });
    }
    if let (AccountChange::OpenTo(Some(open_to)), Some(last_booked)) = (&change, found.last_booked)
        && last_booked > *open_to
    {
        return Err(Rejection::PostingsAfterClose {
            account: found.path().clone(),
            last_booked,
        });
    }
    let handle = found.handle();
    let term = match &change {
        AccountChange::OpenTo(open_to) => AccountTerm::Window(Window {
            open_from,
            open_to: *open_to,
        }),
        AccountChange::Limit(limit) => AccountTerm::Limit(*limit),
        AccountChange::Class(class) => AccountTerm::Class(class.clone()),
    };
    let position = header.size;
    Ok(Outcome::from(
        WriteSet::of(
            position,
            Record::Account(AccountEvent {
                action: AccountAction::Change { handle, change },
                effective,
                actor,
            }),
            Done::Changed,
        )
        .with(IndexRow::AccountVersion {
            handle,
            term,
            effective,
            position,
        }),
    ))
}

/// Declares a currency: 107, 123 → identical latest exponent → 309 → 310 →
/// 424.
pub(crate) fn currency<const P: u8>(
    view: &impl StateView<P>,
    code: Currency,
    exponent: u8,
    effective: Date,
    by: &str,
) -> Result<Outcome<P>, Rejection> {
    let actor = actor(by)?;
    if exponent > P {
        return Err(Rejection::ExponentTooLarge { exponent, max: P });
    }
    let latest = view.currency_latest(code);
    let header = view.header();
    if let Some(latest) = &latest
        && latest.value == exponent
        && effective >= latest.effective
    {
        return Ok(Outcome::Unchanged(Done::Changed));
    }
    above_watermark(&header, effective)?;
    if let Some(latest) = latest {
        not_before(effective, latest.effective)?;
        if exponent < latest.value {
            return Err(Rejection::ExponentLowered {
                currency: code,
                current: latest.value,
                requested: exponent,
            });
        }
    }
    let position = header.size;
    Ok(Outcome::from(
        WriteSet::of(
            position,
            Record::Currency(CurrencyDeclaration {
                code,
                exponent,
                effective,
                actor,
            }),
            Done::Changed,
        )
        .with(IndexRow::CurrencyVersion {
            code,
            exponent,
            effective,
            position,
        }),
    ))
}

/// Sets the ledger policy: 107 → identical latest policy → 309 → 310 → 431.
pub(crate) fn policy<const P: u8>(
    view: &impl StateView<P>,
    policy: &LedgerPolicy,
    effective: Date,
    by: &str,
) -> Result<Outcome<P>, Rejection> {
    let actor = actor(by)?;
    let latest = view.policy_latest();
    let header = view.header();
    if let Some(latest) = &latest
        && latest.value == *policy
        && effective >= latest.effective
    {
        return Ok(Outcome::Unchanged(Done::Changed));
    }
    above_watermark(&header, effective)?;
    if let Some(latest) = latest {
        not_before(effective, latest.effective)?;
    }
    // 431: recorded entries were checked against the policy then in force,
    // so a new policy may not reach back over them.
    if let Some(last_booking) = header.last_booking
        && effective <= last_booking
    {
        return Err(Rejection::PolicyBeforeBookings {
            effective,
            last_booking,
        });
    }
    let position = header.size;
    Ok(Outcome::from(
        WriteSet::of(
            position,
            Record::Policy(PolicyEvent {
                policy: policy.clone(),
                effective,
                actor,
            }),
            Done::Changed,
        )
        .with(IndexRow::PolicyVersion {
            policy: policy.clone(),
            effective,
            position,
        }),
    ))
}
