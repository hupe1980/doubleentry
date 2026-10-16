//! The façade spans: one span per `record`, `seal_period`, `prove_*` and
//! `audit`, with fixed fields — and never an amount, a memo or an account
//! path — on both façades; no event above `DEBUG`; and no `tracing`
//! dependency without the feature.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use std::process::Command;

/// SC-008: without the feature, `tracing` is not even a dependency.
#[test]
fn tracing_is_not_a_dependency_by_default() {
    let output = Command::new(env!("CARGO"))
        .args([
            "tree",
            "--edges",
            "normal",
            "--no-default-features",
            "--prefix",
            "none",
            "--locked",
            "--offline",
            "--package",
            "doubleentry",
        ])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let tree = String::from_utf8(output.stdout).unwrap();
    assert!(tree.lines().any(|line| line.starts_with("sha2 ")), "{tree}");
    assert!(
        !tree.lines().any(|line| line.starts_with("tracing")),
        "{tree}"
    );
}

#[cfg(feature = "tracing")]
mod spans {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    use doubleentry::store::MemoryStore;
    use doubleentry::{
        AccountKind, Entry, Journal, Ledger, LedgerId, MemoSalt, Rejection, amount, currency::EUR,
        date,
    };
    use tracing::field::{Field, Visit};
    use tracing::span::{Attributes, Id, Record};
    use tracing::{Event, Level, Metadata, Subscriber};

    /// One span: its name, level, target and fields as text.
    #[derive(Debug, Clone)]
    struct Span {
        name: &'static str,
        level: Level,
        target: &'static str,
        fields: BTreeMap<&'static str, String>,
    }

    /// Every span and the level of every event, in order, and the spans
    /// entered (the test runs on one thread).
    #[derive(Clone, Default)]
    struct Collector {
        spans: Arc<Mutex<Vec<Span>>>,
        events: Arc<Mutex<Vec<Level>>>,
        entered: Arc<Mutex<Vec<(Id, &'static Metadata<'static>)>>>,
        metadata: Arc<Mutex<BTreeMap<u64, &'static Metadata<'static>>>>,
    }

    struct Fields<'a>(&'a mut BTreeMap<&'static str, String>);

    impl Visit for Fields<'_> {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.0.insert(field.name(), format!("{value:?}"));
        }
        fn record_str(&mut self, field: &Field, value: &str) {
            self.0.insert(field.name(), value.to_owned());
        }
    }

    impl Subscriber for Collector {
        fn enabled(&self, _: &Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, attributes: &Attributes<'_>) -> Id {
            let metadata = attributes.metadata();
            let mut span = Span {
                name: metadata.name(),
                level: *metadata.level(),
                target: metadata.target(),
                fields: BTreeMap::new(),
            };
            attributes.record(&mut Fields(&mut span.fields));
            let mut spans = self.spans.lock().unwrap();
            spans.push(span);
            let id = spans.len() as u64;
            self.metadata.lock().unwrap().insert(id, metadata);
            Id::from_u64(id)
        }
        fn record(&self, id: &Id, values: &Record<'_>) {
            let mut spans = self.spans.lock().unwrap();
            let span = &mut spans[id.into_u64() as usize - 1];
            values.record(&mut Fields(&mut span.fields));
        }
        fn record_follows_from(&self, _: &Id, _: &Id) {}
        fn event(&self, event: &Event<'_>) {
            self.events.lock().unwrap().push(*event.metadata().level());
        }
        fn enter(&self, id: &Id) {
            let metadata = self.metadata.lock().unwrap()[&id.into_u64()];
            self.entered.lock().unwrap().push((id.clone(), metadata));
        }
        fn exit(&self, _: &Id) {
            self.entered.lock().unwrap().pop();
        }
        fn current_span(&self) -> tracing_core::span::Current {
            match self.entered.lock().unwrap().last() {
                Some((id, metadata)) => tracing_core::span::Current::new(id.clone(), metadata),
                None => tracing_core::span::Current::none(),
            }
        }
    }

    impl Collector {
        /// The spans of the façades (target `doubleentry`), taken.
        fn facade(&self) -> Vec<Span> {
            let spans = std::mem::take(&mut *self.spans.lock().unwrap());
            spans
                .into_iter()
                .filter(|span| span.target == "doubleentry")
                .collect()
        }
    }

    fn fields(span: &Span) -> Vec<&'static str> {
        span.fields.keys().copied().collect()
    }

    /// The scenario's spans, checked the same on either façade.
    fn check(spans: &[Span], code: u16) {
        let names: Vec<_> = spans.iter().map(|span| span.name).collect();
        assert_eq!(
            names,
            [
                "record",
                "record",
                "record",
                "seal_period",
                "prove",
                "audit"
            ],
            "{spans:#?}"
        );
        let outcomes: Vec<_> = spans[..3]
            .iter()
            .map(|span| span.fields["outcome"].as_str())
            .collect();
        assert_eq!(outcomes, ["new", "replay", "rejected"]);
        for span in &spans[..2] {
            assert_eq!(
                fields(span),
                ["key", "ledger", "outcome", "position"],
                "{span:?}"
            );
            assert_eq!(span.fields["key"], "sale-1");
            assert_eq!(span.fields["ledger"], "example.com/acme");
            assert_eq!(span.level, Level::INFO);
        }
        assert_eq!(spans[2].fields["code"], code.to_string());
        assert!(spans[3].fields["position"].parse::<u64>().is_ok());
        assert_eq!(spans[3].fields["period"], "2026-03");
        assert_eq!(spans[4].level, Level::DEBUG);
        assert_eq!(fields(&spans[4]), ["kind", "ledger", "size"]);
        assert_eq!(spans[4].fields["kind"], "entry");
        assert_eq!(spans[5].fields["outcome"], "ok");
        assert!(spans[5].fields["records"].parse::<u64>().unwrap() > 1);
        for span in spans {
            assert!([Level::INFO, Level::DEBUG].contains(&span.level));
            for value in span.fields.values() {
                for secret in ["1190", "11.90", "Assets", "Income", "customer note"] {
                    assert!(!value.contains(secret), "{span:?} carries {secret}");
                }
            }
        }
    }

    fn sale(key: &str, debit: &str, credit: &str) -> Entry {
        Entry::new(key, date!("2026-03-15"))
            .debit("Assets:Cash", debit.parse().unwrap(), EUR)
            .credit("Income:Sales", credit.parse().unwrap(), EUR)
            .memo("customer note")
    }

    #[tokio::test]
    async fn facade_operations_have_spans_with_safe_fields() {
        let collector = Collector::default();
        let _default = tracing::dispatcher::set_default(&tracing::Dispatch::new(collector.clone()));
        let id = LedgerId::new("example.com/acme").unwrap();
        let on = date!("2026-01-01");

        let mut journal = Journal::new(id.clone()).with_salt_source(|| MemoSalt([3; 16]));
        journal.declare_currency(EUR, 2, on).unwrap();
        journal.open("Assets:Cash", AccountKind::Asset, on).unwrap();
        journal
            .open("Income:Sales", AccountKind::Income, on)
            .unwrap();
        journal.define_months(2026, 3..=3).unwrap();
        collector.facade();
        let recorded = journal.record(sale("sale-1", "11.90", "11.90")).unwrap();
        journal.record(sale("sale-1", "11.90", "11.90")).unwrap();
        let refused = journal.record(sale("sale-2", "0.01", "0.02")).unwrap_err();
        let code = refused.rejection().map(Rejection::code).unwrap();
        journal.seal_period("2026-03").unwrap();
        journal.prove_entry(recorded.id).unwrap();
        journal.audit().unwrap();
        check(&collector.facade(), code);

        let books = Ledger::new(MemoryStore::new(), id)
            .await
            .unwrap()
            .with_salt_source(|| MemoSalt([3; 16]));
        books.declare_currency(EUR, 2, on).await.unwrap();
        books
            .open("Assets:Cash", AccountKind::Asset, on)
            .await
            .unwrap();
        books
            .open("Income:Sales", AccountKind::Income, on)
            .await
            .unwrap();
        books.define_months(2026, 3..=3).await.unwrap();
        collector.facade();
        let amount: doubleentry::Amount = amount!("11.90");
        assert_eq!(amount.to_string(), "11.90");
        books
            .record(sale("sale-1", "11.90", "11.90"))
            .await
            .unwrap();
        books
            .record(sale("sale-1", "11.90", "11.90"))
            .await
            .unwrap();
        books
            .record(sale("sale-2", "0.01", "0.02"))
            .await
            .unwrap_err();
        books.seal_period("2026-03").await.unwrap();
        books.prove_entry(recorded.id).await.unwrap();
        books.audit().await.unwrap();
        check(&collector.facade(), code);

        let events = collector.events.lock().unwrap();
        assert!(
            events.iter().all(|level| *level >= Level::DEBUG),
            "{events:?}"
        );
    }
}
