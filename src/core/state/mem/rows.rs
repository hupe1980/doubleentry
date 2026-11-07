//! The projection of a [`MemState`]: every table a store keeps, as the rows
//! [`ProjectionPart`] defines — what an audit expects a store to hold after
//! the log it replayed, and what [`MemoryStore`](crate::store::MemoryStore)
//! answers.

use std::collections::BTreeMap;

use super::{Line, MemState};
use crate::core::{BalanceKey, EntryView};
use crate::log::leaf_hash;
use crate::model::{Balance, ClearingAction, EntryId, Layer};
use crate::store::cells::{
    amount, bytes, currency, date, dimensions, handle, how, int, kind, layer, limit, opt_date,
    side, text,
};
use crate::store::{Cell, ProjectionPart, ProjectionRow, SCHEMA_ID};

/// A posting at (position, index), with its balance and line.
type LineAt<'a, const P: u8> = (u64, u16, (&'a BalanceKey, &'a Line<P>));

/// Rows of one part, in key order.
pub(crate) type Rows<'a> = Box<dyn Iterator<Item = ProjectionRow> + Send + 'a>;

fn row(cells: Vec<Cell>) -> ProjectionRow {
    ProjectionRow::new(cells)
}

/// `rows` sorted by their key in `part`.
fn sorted<'a>(part: ProjectionPart, mut rows: Vec<ProjectionRow>) -> Rows<'a> {
    rows.sort_by_cached_key(|row| row.key(part));
    Box::new(rows.into_iter())
}

fn balance_cells<const P: u8>(balance: &Balance<P>) -> [Cell; 3] {
    [
        amount(balance.debit()),
        amount(balance.credit()),
        int(balance.postings()),
    ]
}

impl<const P: u8> MemState<P> {
    /// The rows of `part`, in key order.
    pub(crate) fn rows(&self, part: ProjectionPart) -> Rows<'_> {
        match part {
            ProjectionPart::Meta => Box::new(
                self.leaves
                    .first()
                    .map(|genesis| {
                        row(vec![
                            Cell::Int(1),
                            text(SCHEMA_ID),
                            text(self.ledger.as_str()),
                            Cell::Int(i64::from(P)),
                            bytes(leaf_hash(&genesis.bytes).as_bytes()),
                        ])
                    })
                    .into_iter(),
            ),
            ProjectionPart::Leaves => {
                Box::new((0u64..).zip(&self.leaves).map(|(position, leaf)| {
                    row(vec![
                        int(position),
                        Cell::Int(leaf.bytes.first().map_or(0, |kind| i64::from(*kind))),
                        bytes(&leaf.bytes),
                    ])
                }))
            }
            ProjectionPart::Nodes => Box::new(
                (0u64..)
                    .zip(self.tree.stored())
                    .map(|(id, hash)| row(vec![int(id), bytes(hash.as_bytes())])),
            ),
            ProjectionPart::Accounts => Box::new(self.accounts.iter().map(|account| {
                let registration = &account.registration;
                row(vec![
                    handle(registration.handle),
                    text(&registration.path.to_string()),
                    kind(registration.kind),
                    int(account.position),
                ])
            })),
            ProjectionPart::AccountVersions => {
                let mut rows = Vec::new();
                for account in &self.accounts {
                    let handle = handle(account.registration.handle);
                    let head = |field: i64, version_position: u64, effective| {
                        vec![
                            handle.clone(),
                            Cell::Int(field),
                            int(version_position),
                            date(effective),
                        ]
                    };
                    for version in &account.windows {
                        let mut cells = head(0, version.position, version.effective);
                        cells.extend([
                            date(version.value.open_from),
                            opt_date(version.value.open_to),
                            Cell::Null,
                            Cell::Null,
                        ]);
                        rows.push(row(cells));
                    }
                    for version in &account.limits {
                        let mut cells = head(1, version.position, version.effective);
                        cells.extend([Cell::Null, Cell::Null, limit(version.value), Cell::Null]);
                        rows.push(row(cells));
                    }
                    for version in &account.classes {
                        let mut cells = head(2, version.position, version.effective);
                        cells.extend([
                            Cell::Null,
                            Cell::Null,
                            Cell::Null,
                            version
                                .value
                                .as_ref()
                                .map_or(Cell::Null, |class| text(class.as_str())),
                        ]);
                        rows.push(row(cells));
                    }
                }
                sorted(part, rows)
            }
            ProjectionPart::CurrencyVersions => sorted(
                part,
                self.currencies
                    .iter()
                    .flat_map(|(code, versions)| {
                        versions.iter().map(move |version| {
                            row(vec![
                                currency(*code),
                                int(version.position),
                                Cell::Int(i64::from(version.value)),
                                date(version.effective),
                            ])
                        })
                    })
                    .collect(),
            ),
            ProjectionPart::PolicyVersions => sorted(
                part,
                self.policies
                    .iter()
                    .map(|version| row(vec![int(version.position), date(version.effective)]))
                    .collect(),
            ),
            ProjectionPart::Periods => sorted(
                part,
                self.periods
                    .values()
                    .map(|view| {
                        row(vec![
                            text(view.id.as_str()),
                            date(view.start),
                            date(view.end),
                            int(view.position),
                        ])
                    })
                    .collect(),
            ),
            ProjectionPart::Entries => {
                Box::new(self.entries_by_position().into_iter().zip(0u64..).map(
                    |((position, id, view), ordinal)| {
                        let parts = view.record.parts();
                        row(vec![
                            int(position),
                            int(ordinal),
                            bytes(id.as_bytes()),
                            bytes(parts.key.as_str().as_bytes()),
                            parts
                                .reverses
                                .map_or(Cell::Null, |target| bytes(target.as_bytes())),
                            parts
                                .kind
                                .as_ref()
                                .map_or(Cell::Null, |kind| text(kind.as_str())),
                        ])
                    },
                ))
            }
            ProjectionPart::Memos => Box::new(self.entries_by_position().into_iter().map(
                move |(position, _, view)| {
                    let leaf = usize::try_from(position)
                        .ok()
                        .and_then(|index| self.leaves.get(index));
                    let fingerprint = leaf
                        .and_then(|leaf| leaf.fingerprint)
                        .map_or(Cell::Null, |fingerprint| bytes(fingerprint.as_bytes()));
                    let memo = leaf.and_then(|leaf| leaf.memo.as_ref());
                    let has_memo = view.record.parts().memo.is_some();
                    row(vec![
                        int(position),
                        fingerprint,
                        if has_memo { int(position) } else { Cell::Null },
                        memo.map_or(Cell::Null, |memo| bytes(&memo.salt.0)),
                        memo.map_or(Cell::Null, |memo| text(&memo.text)),
                    ])
                },
            )),
            ProjectionPart::Postings => Box::new(self.lines_by_position().into_iter().map(
                |(position, index, line)| {
                    let mut cells = vec![
                        int(position),
                        Cell::Int(i64::from(index)),
                        handle(line.0.handle),
                        currency(line.0.currency),
                        layer(line.0.layer),
                        side(line.1.side),
                        amount(line.1.amount),
                        date(line.1.booking),
                        date(line.1.value),
                        Cell::Text(dimensions(&line.1.dimensions)),
                    ];
                    cells.extend(balance_cells(&line.1.balance_after));
                    row(cells)
                },
            )),
            ProjectionPart::PostingDims => Box::new(self.lines_by_position().into_iter().flat_map(
                |(position, index, line)| {
                    let mut pairs: Vec<_> = line.1.dimensions.iter().collect();
                    pairs.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
                    pairs.into_iter().map(move |(axis, value)| {
                        row(vec![
                            int(position),
                            Cell::Int(i64::from(index)),
                            text(axis.as_str()),
                            text(value.as_str()),
                        ])
                    })
                },
            )),
            ProjectionPart::Holds => sorted(
                part,
                self.holds
                    .values()
                    .map(|view| {
                        let hold = view.row();
                        let (resolved_by, resolution) = match hold.resolution {
                            Some((by, kind)) => (bytes(by.as_bytes()), how(kind)),
                            None => (Cell::Null, Cell::Null),
                        };
                        row(vec![
                            int(hold.position),
                            bytes(hold.id.as_bytes()),
                            date(hold.booking),
                            opt_date(hold.expires),
                            handle(hold.debit),
                            handle(hold.credit),
                            currency(hold.currency),
                            amount(hold.amount),
                            resolved_by,
                            resolution,
                        ])
                    })
                    .collect(),
            ),
            ProjectionPart::Seals => sorted(
                part,
                self.periods
                    .values()
                    .filter_map(|view| {
                        let seal = view.sealed.as_ref()?;
                        Some(row(vec![text(view.id.as_str()), int(seal.position)]))
                    })
                    .collect(),
            ),
            ProjectionPart::SealLeaves => sorted(
                part,
                self.seal_trees
                    .iter()
                    .flat_map(|((period, code, tree_layer), tree)| {
                        tree.leaves().map(move |(index, balance)| {
                            let mut cells = vec![
                                text(period.as_str()),
                                currency(*code),
                                layer(*tree_layer),
                                Cell::Int(i64::from(index)),
                            ];
                            cells.extend(balance_cells(&balance));
                            row(cells)
                        })
                    })
                    .collect(),
            ),
            ProjectionPart::SealNodes => sorted(
                part,
                self.seal_trees
                    .iter()
                    .flat_map(|((period, code, tree_layer), tree)| {
                        tree.stored_nodes().map(move |(level, index, hash)| {
                            row(vec![
                                text(period.as_str()),
                                currency(*code),
                                layer(*tree_layer),
                                Cell::Int(i64::from(level)),
                                Cell::Int(i64::from(index)),
                                bytes(hash.as_bytes()),
                            ])
                        })
                    })
                    .collect(),
            ),
            ProjectionPart::Clearings => Box::new(self.clearings.values().map(|view| {
                let key = match &view.event.action {
                    ClearingAction::Apply { key, .. } | ClearingAction::Reset { key, .. } => key,
                };
                row(vec![int(view.position), bytes(key.as_str().as_bytes())])
            })),
            ProjectionPart::ClearingResets => Box::new(
                self.clearings
                    .values()
                    .filter_map(|view| Some(row(vec![int(view.position), int(view.reset_by?)]))),
            ),
            ProjectionPart::ClearingRefs => sorted(
                part,
                self.clearing_refs
                    .iter()
                    .map(|(key, position)| {
                        row(vec![
                            int(*position),
                            handle(key.handle),
                            currency(key.currency),
                            layer(key.layer),
                        ])
                    })
                    .collect(),
            ),
            ProjectionPart::Balances => sorted(
                part,
                self.balances
                    .iter()
                    .map(|(key, balance)| {
                        let mut cells =
                            vec![handle(key.handle), currency(key.currency), layer(key.layer)];
                        cells.extend(balance_cells(balance));
                        row(cells)
                    })
                    .collect(),
            ),
            ProjectionPart::Cleared => sorted(
                part,
                self.applied
                    .iter()
                    .filter_map(|(posting, applied)| {
                        let position = self.records.get(&posting.entry)?.position;
                        Some(row(vec![
                            int(position),
                            Cell::Int(i64::from(posting.index)),
                            amount(*applied),
                        ]))
                    })
                    .collect(),
            ),
            ProjectionPart::Outstanding => sorted(
                part,
                self.outstanding
                    .values()
                    .map(|reserved| {
                        row(vec![
                            handle(reserved.handle),
                            currency(reserved.currency),
                            amount(reserved.debits),
                            amount(reserved.credits),
                        ])
                    })
                    .collect(),
            ),
            ProjectionPart::Slices => {
                let mut slices: BTreeMap<(u32, String, String), Balance<P>> = BTreeMap::new();
                for posted in self.postings.values() {
                    if posted.key.layer != Layer::Settled {
                        continue;
                    }
                    let key = (
                        posted.key.handle.get(),
                        posted.key.currency.as_str().to_owned(),
                        dimensions(&posted.dimensions),
                    );
                    let slice = slices.entry(key).or_insert(Balance::ZERO);
                    // A slice sums a subset of the postings of one maintained
                    // balance, which fits, so the sum fits too.
                    *slice = slice.post(posted.side, posted.amount).unwrap_or(*slice);
                }
                sorted(
                    part,
                    slices
                        .into_iter()
                        .map(|((account, code, dims), balance)| {
                            let mut cells = vec![
                                Cell::Int(i64::from(account)),
                                Cell::Text(code),
                                Cell::Text(dims),
                            ];
                            cells.extend(balance_cells(&balance));
                            row(cells)
                        })
                        .collect(),
                )
            }
        }
    }

    /// Every entry with its id, by position.
    fn entries_by_position(&self) -> Vec<(u64, EntryId, &EntryView<P>)> {
        let mut entries: Vec<_> = self
            .records
            .iter()
            .map(|(id, view)| (view.position, *id, view))
            .collect();
        entries.sort_by_key(|(position, ..)| *position);
        entries
    }

    /// Every posting with its balance, by (position, index).
    fn lines_by_position(&self) -> Vec<LineAt<'_, P>> {
        let mut lines: Vec<_> = self
            .lines
            .iter()
            .map(|((key, position, index), line)| (*position, *index, (key, line)))
            .collect();
        lines.sort_by_key(|(position, index, _)| (*position, *index));
        lines
    }
}
