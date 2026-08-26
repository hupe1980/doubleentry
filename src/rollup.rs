//! Hierarchical reporting: a trial balance folded up the account tree.
//!
//! An account path is a hierarchy and only leaves are postable, which is what
//! makes a *node's* balance a single defensible number: everything beneath it. A
//! [`Rollup`] is that number, for every node at once.
//!
//! ```text
//! Assets                    1 190.00      ← subtree
//!   Assets:Bank             1 000.00
//!     Assets:Bank:Main      1 000.00      ← own
//!   Assets:Cash               190.00      ← own
//! ```
//!
//! Four things are worth knowing before reading the API:
//!
//! - **Grouping nodes are inferred.** `Assets` appears whether or not it is
//!   registered — that is grouping by path prefix, and it asserts nothing about
//!   `Assets` being an account. A node the registry knows carries its handle and
//!   its [`AccountKind`]; [`RollupNode::is_registered`] says which.
//! - **One `(currency, layer)` per report.** A parent cannot hold the sum of its
//!   children in two currencies. Iterate [`TrialBalance::currencies`] for a
//!   multi-currency report.
//! - **It is not a `TrialBalance`.** That type's rows sum to zero, and rows
//!   holding both a parent and its children do not. The *roots* do.
//! - **No backend method is needed.** The fold is pure over a trial balance and
//!   a registry, both of which a [`LedgerStore`](crate::LedgerStore) serves:
//!
//! ```ignore
//! let registry = AccountRegistry::from_records(store.accounts().await?)?;
//! let balances = store.trial_balance(BalanceQuery::through(as_at)).await?;
//! let sheet = Rollup::of(&balances, &registry, Currency::EUR, Layer::Settled)?;
//! ```

use std::collections::BTreeMap;

use crate::account::{AccountId, AccountKind, AccountPath, AccountRegistry};
use crate::balance::{Balance, BalanceKey, TrialBalance};
use crate::money::{Currency, MoneyError};
use crate::posting::Layer;

/// One node of a hierarchical report.
///
/// Carries **both** figures: a subtree total cannot be split back into the
/// postings that landed on the node itself, and a set of own-balances cannot be
/// summed by a reader who does not know the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct RollupNode<const P: u8> {
    /// The node's path.
    pub path: AccountPath,
    /// The handle, when the registry holds this path.
    ///
    /// `None` for a grouping node inferred from a longer path — see the
    /// [module documentation](self).
    pub account: Option<AccountId>,
    /// The registered classification, when there is one.
    pub kind: Option<AccountKind>,
    /// Postings that landed directly on this path.
    ///
    /// Zero for an aggregation node, which is what "only leaves are postable"
    /// means in figures.
    pub own: Balance<P>,
    /// This node and everything beneath it.
    ///
    /// The number a balance sheet prints.
    pub subtree: Balance<P>,
}

impl<const P: u8> RollupNode<P> {
    /// How many segments the path has. `1` for a top-level node.
    #[must_use]
    pub fn depth(&self) -> usize {
        self.path.depth()
    }

    /// True when the registry holds this path.
    ///
    /// False for a grouping node the report inferred from a longer path. Worth
    /// testing before reading [`kind`](Self::kind) as anything but absent.
    #[must_use]
    pub fn is_registered(&self) -> bool {
        self.account.is_some()
    }

    /// True when nothing was posted directly here.
    #[must_use]
    pub fn is_aggregate(&self) -> bool {
        self.own.is_empty()
    }
}

/// A trial balance folded up the account tree, for one currency and layer.
///
/// Nodes are in **path order**, which for segment-wise paths is depth-first
/// pre-order: a parent always precedes its children and siblings are adjacent.
/// Printing the nodes in order, indented by [`RollupNode::depth`], is the report.
///
/// ```
/// # use doubleentry::{Amount, Currency, Layer, Posting, TrialBalance};
/// # use doubleentry::account::AccountRegistry;
/// # use doubleentry::rollup::Rollup;
/// # use time::macros::date;
/// # type Eur = Amount<2>;
/// let mut accounts = AccountRegistry::new();
/// let bank = accounts.register_path("Assets:Bank:Main", date!(2026 - 01 - 01))?;
/// let cash = accounts.register_path("Assets:Cash", date!(2026 - 01 - 01))?;
/// let sales = accounts.register_path("Income:Sales", date!(2026 - 01 - 01))?;
///
/// let mut tb = TrialBalance::<2>::new();
/// tb.apply(&Posting::debit(bank, Eur::parse("1000.00")?, Currency::EUR))?;
/// tb.apply(&Posting::debit(cash, Eur::parse("190.00")?, Currency::EUR))?;
/// tb.apply(&Posting::credit(sales, Eur::parse("1190.00")?, Currency::EUR))?;
///
/// let sheet = Rollup::of(&tb, &accounts, Currency::EUR, Layer::Settled)?;
///
/// // `Assets` was never registered, and still heads its own subtree.
/// let assets = sheet.get(&"Assets".parse()?).expect("inferred from its children");
/// assert!(!assets.is_registered());
/// assert_eq!(assets.subtree.debits, Eur::parse("1190.00")?);
/// assert!(assets.is_aggregate());
///
/// // Truncating the tree keeps the totals: the report gets shorter, not wrong.
/// assert_eq!(sheet.to_depth(1).nodes().len(), 2);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct Rollup<const P: u8> {
    currency: Currency,
    layer: Layer,
    nodes: Vec<RollupNode<P>>,
}

/// Wire form of a report.
///
/// Separate from [`Rollup`] so deserialisation can re-establish the node
/// ordering before handing back a value.
#[cfg(feature = "serde")]
#[derive(serde::Deserialize)]
struct RollupOwned<const P: u8> {
    currency: Currency,
    layer: Layer,
    nodes: Vec<RollupNode<P>>,
}

/// A deserialised report is re-ordered, and duplicate paths are refused.
///
/// The node order is depth-first pre-order — what makes indenting by
/// [`RollupNode::depth`] a tree, and what [`Rollup::get`] binary-searches over.
/// A value read off a wire in some other order would answer `None` for nodes it
/// holds.
///
/// What is *not* re-established is that each node's figure is the sum of the
/// ones beneath it. A report makes no cryptographic claim; its numbers are the
/// sender's. Rebuild it with [`Rollup::of`] if you need more than that.
#[cfg(feature = "serde")]
impl<'de, const P: u8> serde::Deserialize<'de> for Rollup<P> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = RollupOwned::<P>::deserialize(d)?;
        let mut nodes = raw.nodes;
        nodes.sort_by(|a, b| a.path.cmp(&b.path));
        if nodes.windows(2).any(|pair| {
            pair.first()
                .zip(pair.get(1))
                .is_some_and(|(a, b)| a.path == b.path)
        }) {
            return Err(serde::de::Error::custom(
                "a rollup names one path more than once",
            ));
        }
        Ok(Self {
            currency: raw.currency,
            layer: raw.layer,
            nodes,
        })
    }
}

impl<const P: u8> Rollup<P> {
    /// Folds `balances` up the tree the registry describes.
    ///
    /// Only rows in `currency` and `layer` are read; the rest of the trial
    /// balance is another report.
    ///
    /// A row whose handle the registry does not know is **skipped**: it has no
    /// path, so there is no node it could belong under. That only happens for a
    /// trial balance and a registry that were not taken together.
    ///
    /// # Errors
    ///
    /// Returns [`MoneyError::Overflow`] if a subtree total does not fit.
    pub fn of(
        balances: &TrialBalance<P>,
        accounts: &AccountRegistry,
        currency: Currency,
        layer: Layer,
    ) -> Result<Self, MoneyError> {
        // What landed directly on each path, keyed by path so the tree order is
        // the map's own order.
        let mut own: BTreeMap<AccountPath, (Option<AccountId>, Balance<P>)> = BTreeMap::new();
        for (key, balance) in balances.iter() {
            if key.currency != currency || key.layer != layer {
                continue;
            }
            let Some(account) = accounts.get(key.account) else {
                continue;
            };
            let slot = own
                .entry(account.path.clone())
                .or_insert((Some(key.account), Balance::ZERO));
            slot.1 = slot.1.checked_add(balance)?;
        }

        // Every node the report shows: the paths that carry a balance, and every
        // ancestor of one. An ancestor need not be registered — see the module
        // documentation for why inferring it is grouping rather than inventing.
        let mut subtree: BTreeMap<AccountPath, Balance<P>> = BTreeMap::new();
        for path in own.keys() {
            subtree.insert(path.clone(), Balance::ZERO);
            for ancestor in path.ancestors() {
                subtree.entry(ancestor).or_insert(Balance::ZERO);
            }
        }

        // Each own-balance contributes to its own node and to every ancestor.
        // `O(rows · depth)`, and depth is bounded by `account::MAX_DEPTH`.
        for (path, (_, balance)) in &own {
            for target in std::iter::once(path.clone()).chain(path.ancestors()) {
                if let Some(slot) = subtree.get_mut(&target) {
                    *slot = slot.checked_add(balance)?;
                }
            }
        }

        let nodes = subtree
            .into_iter()
            .map(|(path, total)| {
                let (account, own_balance) = own
                    .get(&path)
                    .map_or((None, Balance::ZERO), |(id, b)| (*id, *b));
                // A grouping node may still be registered — the registry is
                // asked either way, so `account` is populated for any path it
                // knows, whether or not anything was posted to it.
                let account = account.or_else(|| accounts.id_of(&path));
                let kind = account.and_then(|id| accounts.get(id)).and_then(|a| a.kind);
                RollupNode {
                    path,
                    account,
                    kind,
                    own: own_balance,
                    subtree: total,
                }
            })
            .collect();

        Ok(Self {
            currency,
            layer,
            nodes,
        })
    }

    /// The currency this report is in.
    #[must_use]
    pub fn currency(&self) -> Currency {
        self.currency
    }

    /// The layer this report covers.
    #[must_use]
    pub fn layer(&self) -> Layer {
        self.layer
    }

    /// Every node, in path order — parent before children, siblings adjacent.
    #[must_use]
    pub fn nodes(&self) -> &[RollupNode<P>] {
        &self.nodes
    }

    /// True when nothing was reported.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// The node at `path`, if the report holds one.
    #[must_use]
    pub fn get(&self, path: &AccountPath) -> Option<&RollupNode<P>> {
        self.nodes
            .binary_search_by(|node| node.path.cmp(path))
            .ok()
            .and_then(|at| self.nodes.get(at))
    }

    /// The top-level nodes.
    pub fn roots(&self) -> impl Iterator<Item = &RollupNode<P>> {
        self.nodes.iter().filter(|node| node.depth() == 1)
    }

    /// The same report, summarised to `depth` levels.
    ///
    /// Nothing is recomputed: a node's [`subtree`](RollupNode::subtree) already
    /// carries everything beneath it, so truncating makes the report shorter
    /// rather than wrong, and the surviving deepest nodes are where the pruned
    /// figures show. [`own`](RollupNode::own) keeps its meaning throughout.
    /// A `depth` of `0` yields an empty report.
    #[must_use]
    pub fn to_depth(&self, depth: usize) -> Self {
        Self {
            currency: self.currency,
            layer: self.layer,
            nodes: self
                .nodes
                .iter()
                .filter(|node| node.depth() <= depth)
                .cloned()
                .collect(),
        }
    }

    /// The total across every root.
    ///
    /// Equal to [`TrialBalance::totals`] for the same currency and layer, since
    /// every own-balance sits under exactly one root — so a report over balanced
    /// books is balanced.
    ///
    /// # Errors
    ///
    /// Returns [`MoneyError::Overflow`] if the sum does not fit.
    pub fn total(&self) -> Result<Balance<P>, MoneyError> {
        let mut acc = Balance::ZERO;
        for root in self.roots() {
            acc = acc.checked_add(&root.subtree)?;
        }
        Ok(acc)
    }

    /// The key one node's own balance was read under, when it is registered.
    ///
    /// The way back from a report line to the statement or open-item list behind
    /// it. `None` for an inferred node, which has no balance of its own.
    #[must_use]
    pub fn key_of(&self, node: &RollupNode<P>) -> Option<BalanceKey> {
        node.account.map(|account| BalanceKey {
            account,
            currency: self.currency,
            layer: self.layer,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::money::Amount;
    use crate::posting::Posting;
    use time::macros::date;

    type Eur = Amount<2>;

    struct Books {
        accounts: AccountRegistry,
        balances: TrialBalance<2>,
    }

    impl Books {
        /// Only leaves registered — the case a caller falls into by default, and
        /// the one that would make a rollup useless if nodes were not inferred.
        fn leaves_only() -> Self {
            let mut accounts = AccountRegistry::new();
            let bank = accounts
                .register_path("Assets:Bank:Main", date!(2026 - 01 - 01))
                .expect("registers");
            let cash = accounts
                .register_path("Assets:Cash", date!(2026 - 01 - 01))
                .expect("registers");
            let sales = accounts
                .register_path("Income:Sales", date!(2026 - 01 - 01))
                .expect("registers");

            let mut balances = TrialBalance::<2>::new();
            for posting in [
                Posting::debit(bank, Eur::from_minor(100_000), Currency::EUR),
                Posting::debit(cash, Eur::from_minor(19_000), Currency::EUR),
                Posting::credit(sales, Eur::from_minor(119_000), Currency::EUR),
            ] {
                balances.apply(&posting).expect("no overflow");
            }
            Self { accounts, balances }
        }

        fn report(&self) -> Rollup<2> {
            Rollup::of(
                &self.balances,
                &self.accounts,
                Currency::EUR,
                Layer::Settled,
            )
            .expect("no overflow")
        }
    }

    fn path(s: &str) -> AccountPath {
        AccountPath::parse(s).expect("valid")
    }

    #[test]
    fn a_node_carries_the_sum_of_everything_beneath_it() {
        let report = Books::leaves_only().report();
        let assets = report.get(&path("Assets")).expect("inferred");
        assert_eq!(assets.subtree.debits, Eur::from_minor(119_000));
        assert_eq!(assets.own, Balance::ZERO);
        assert!(assets.is_aggregate());
    }

    #[test]
    fn grouping_nodes_are_inferred_from_the_paths_that_exist() {
        let report = Books::leaves_only().report();
        let paths: Vec<String> = report
            .nodes()
            .iter()
            .map(|n| n.path.to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            paths,
            vec![
                "Assets",
                "Assets:Bank",
                "Assets:Bank:Main",
                "Assets:Cash",
                "Income",
                "Income:Sales",
            ],
            "parent before children, siblings adjacent"
        );
        // Neither `Assets` nor `Assets:Bank` was ever registered.
        assert!(
            !report
                .get(&path("Assets"))
                .expect("present")
                .is_registered()
        );
        assert!(
            report
                .get(&path("Assets:Bank:Main"))
                .expect("present")
                .is_registered()
        );
    }

    #[test]
    fn a_registered_node_carries_its_handle_and_kind() {
        let mut books = Books::leaves_only();
        // Register the grouping node after the fact; it is not postable, and the
        // report should now name it.
        let assets = books
            .accounts
            .register(
                crate::account::Account::new(path("Assets"), date!(2026 - 01 - 01))
                    .with_kind(AccountKind::Asset),
            )
            .expect("registers");
        let report = books.report();
        let node = report.get(&path("Assets")).expect("present");
        assert_eq!(node.account, Some(assets));
        assert_eq!(node.kind, Some(AccountKind::Asset));
        assert!(node.is_aggregate(), "a node is still not posted to");
        assert_eq!(node.subtree.debits, Eur::from_minor(119_000));
    }

    #[test]
    fn the_roots_add_up_to_the_trial_balance() {
        let books = Books::leaves_only();
        let report = books.report();
        let total = report.total().expect("no overflow");
        assert_eq!(
            total,
            books
                .balances
                .totals(Currency::EUR, Layer::Settled)
                .expect("no overflow")
        );
        assert!(total.is_balanced(), "a rollup of balanced books balances");
    }

    #[test]
    fn truncating_keeps_the_totals() {
        let report = Books::leaves_only().report();
        let short = report.to_depth(1);
        assert_eq!(short.nodes().len(), 2);
        assert_eq!(
            short.total().expect("ok"),
            report.total().expect("ok"),
            "a shorter report, not a different one"
        );
        assert_eq!(
            short.get(&path("Assets")).expect("present").subtree.debits,
            Eur::from_minor(119_000)
        );
        assert!(report.to_depth(0).is_empty());
    }

    #[test]
    fn a_report_covers_one_currency_and_one_layer() {
        let mut books = Books::leaves_only();
        let cash = books
            .accounts
            .id_of(&path("Assets:Cash"))
            .expect("registered");
        books
            .balances
            .apply(&Posting::debit(
                cash,
                Eur::from_minor(500_000),
                Currency::USD,
            ))
            .expect("no overflow");
        books
            .balances
            .apply(
                &Posting::debit(cash, Eur::from_minor(700_000), Currency::EUR)
                    .in_layer(Layer::Pending),
            )
            .expect("no overflow");

        // Neither the other currency nor the other layer reaches the EUR report.
        let report = books.report();
        assert_eq!(
            report.get(&path("Assets")).expect("present").subtree.debits,
            Eur::from_minor(119_000)
        );

        let usd = Rollup::of(
            &books.balances,
            &books.accounts,
            Currency::USD,
            Layer::Settled,
        )
        .expect("ok");
        assert_eq!(usd.currency(), Currency::USD);
        assert_eq!(
            usd.get(&path("Assets")).expect("present").subtree.debits,
            Eur::from_minor(500_000)
        );

        let pending = Rollup::of(
            &books.balances,
            &books.accounts,
            Currency::EUR,
            Layer::Pending,
        )
        .expect("ok");
        assert_eq!(pending.layer(), Layer::Pending);
        assert_eq!(
            pending
                .get(&path("Assets"))
                .expect("present")
                .subtree
                .debits,
            Eur::from_minor(700_000)
        );
    }

    #[test]
    fn a_balance_on_an_unknown_handle_is_skipped_rather_than_guessed_at() {
        let mut books = Books::leaves_only();
        books
            .balances
            .apply(&Posting::debit(
                AccountId::from_index(9_999),
                Eur::from_minor(1),
                Currency::EUR,
            ))
            .expect("no overflow");
        // It has no path, so there is no subtree it could belong to. The report
        // leaves it out rather than putting it somewhere.
        let report = books.report();
        assert_eq!(report.total().expect("ok").debits, Eur::from_minor(119_000));
    }

    #[test]
    fn a_node_that_also_carries_postings_is_not_counted_twice() {
        // The journal forbids this — only leaves are postable — but a registry
        // built directly does not, and a report must stay arithmetically sound
        // rather than relying on a rule enforced somewhere else.
        let mut accounts = AccountRegistry::new();
        let parent = accounts
            .register(crate::account::Account::new(
                path("A"),
                date!(2020 - 01 - 01),
            ))
            .expect("registers");
        let child = accounts
            .register(crate::account::Account::new(
                path("A:B"),
                date!(2020 - 01 - 01),
            ))
            .expect("registers");

        let mut balances = TrialBalance::<2>::new();
        for posting in [
            Posting::debit(parent, Eur::from_minor(100), Currency::EUR),
            Posting::debit(child, Eur::from_minor(50), Currency::EUR),
        ] {
            balances.apply(&posting).expect("no overflow");
        }

        let report =
            Rollup::of(&balances, &accounts, Currency::EUR, Layer::Settled).expect("no overflow");
        let node = report.get(&path("A")).expect("present");
        assert_eq!(node.own.debits, Eur::from_minor(100));
        assert_eq!(node.subtree.debits, Eur::from_minor(150));
        assert_eq!(
            report.total().expect("no overflow"),
            balances
                .totals(Currency::EUR, Layer::Settled)
                .expect("no overflow")
        );
    }

    #[test]
    fn an_empty_trial_balance_reports_nothing() {
        let accounts = AccountRegistry::new();
        let report = Rollup::of(
            &TrialBalance::<2>::new(),
            &accounts,
            Currency::EUR,
            Layer::Settled,
        )
        .expect("ok");
        assert!(report.is_empty());
        assert_eq!(report.total().expect("ok"), Balance::ZERO);
        assert_eq!(report.roots().count(), 0);
    }

    #[cfg(feature = "serde")]
    #[test]
    fn a_report_read_off_a_wire_is_still_in_tree_order() {
        let report = Books::leaves_only().report();
        let mut wire: serde_json::Value = serde_json::to_value(&report).expect("serialises");
        // Shuffle the nodes on the way in, the way any transport is free to.
        if let Some(nodes) = wire.get_mut("nodes").and_then(|n| n.as_array_mut()) {
            nodes.reverse();
        }
        let back: Rollup<2> = serde_json::from_value(wire).expect("deserialises");
        assert_eq!(back, report, "re-ordered on the way in");
        assert!(back.get(&path("Assets:Cash")).is_some());
    }

    #[cfg(feature = "serde")]
    #[test]
    fn a_report_naming_one_path_twice_is_refused() {
        let report = Books::leaves_only().report();
        let mut wire: serde_json::Value = serde_json::to_value(&report).expect("serialises");
        if let Some(nodes) = wire.get_mut("nodes").and_then(|n| n.as_array_mut())
            && let Some(first) = nodes.first().cloned()
        {
            nodes.push(first);
        }
        assert!(serde_json::from_value::<Rollup<2>>(wire).is_err());
    }

    #[test]
    fn a_node_names_the_key_its_own_balance_came_from() {
        let books = Books::leaves_only();
        let report = books.report();
        let cash = report.get(&path("Assets:Cash")).expect("present");
        let key = report.key_of(cash).expect("registered");
        assert_eq!(books.balances.get_or_zero(&key), cash.own);
        // An inferred node has no key, because it has no balance of its own to
        // read back.
        assert!(
            report
                .key_of(report.get(&path("Assets")).expect("present"))
                .is_none()
        );
    }
}
