//! The account master data a report needs, folded from account records in
//! log order: each account's path and kind, and its class by effective date.

use std::collections::BTreeMap;

use time::Date;

use crate::model::{
    AccountAction, AccountChange, AccountEvent, AccountHandle, AccountKind, AccountPath, Label,
};

/// Every account registered in a log prefix, with its class history.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct AccountBook {
    accounts: BTreeMap<AccountHandle, Registered>,
}

/// One registered account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Registered {
    path: AccountPath,
    kind: AccountKind,
    /// (effective date, class), in log order.
    classes: Vec<(Date, Option<Label>)>,
}

impl AccountBook {
    /// The book after one more account record, in log order.
    pub(crate) fn feed(&mut self, event: &AccountEvent) {
        match &event.action {
            AccountAction::Register(registration) => {
                self.accounts.insert(
                    registration.handle,
                    Registered {
                        path: registration.path.clone(),
                        kind: registration.kind,
                        classes: vec![(event.effective, registration.class.clone())],
                    },
                );
            }
            AccountAction::Change {
                handle,
                change: AccountChange::Class(class),
            } => {
                if let Some(account) = self.accounts.get_mut(handle) {
                    account.classes.push((event.effective, class.clone()));
                }
            }
            AccountAction::Change { .. } => {}
        }
    }

    /// The account registered under `handle`.
    pub(crate) fn get(&self, handle: AccountHandle) -> Option<&Registered> {
        self.accounts.get(&handle)
    }

    /// Every account: (path, handle, kind), by path.
    pub(crate) fn by_path(&self) -> BTreeMap<AccountPath, (AccountHandle, AccountKind)> {
        self.accounts
            .iter()
            .map(|(handle, account)| (account.path.clone(), (*handle, account.kind)))
            .collect()
    }
}

impl Registered {
    /// The path.
    pub(crate) const fn path(&self) -> &AccountPath {
        &self.path
    }

    /// The kind.
    pub(crate) const fn kind(&self) -> AccountKind {
        self.kind
    }

    /// The latest class: the one registered or last set.
    pub(crate) fn class(&self) -> Option<&Label> {
        self.classes.last().and_then(|(_, class)| class.as_ref())
    }

    /// The class in force on `date`: the last one set with an effective
    /// date on or before it (the registered one before any).
    pub(crate) fn class_on(&self, date: Date) -> Option<&Label> {
        self.classes
            .iter()
            .rev()
            .find(|(effective, _)| *effective <= date)
            .or_else(|| self.classes.first())
            .and_then(|(_, class)| class.as_ref())
    }
}
