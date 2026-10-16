//! A ledger others can check: sign the head as a C2SP checkpoint, collect
//! cosignatures from two witnesses, publish tiles, hand an auditor one
//! entry and one sealed balance as offline bundles, and audit them.
//!
//! The witnesses are `testing::TestWitness` — in production they are
//! somebody else's stock `tlog-witness` servers and `post` is your HTTP
//! client.
//!
//! `cargo run --example transparency --features witness,testing`

use std::collections::BTreeMap;

use doubleentry::prelude::*;
use doubleentry::store::MemoryStore;
use doubleentry::testing::TestWitness;
use doubleentry::transparency::{
    LogKey, Next, Policy, ProofBundle, Proven, SignedCheckpoint, Threshold, TileId, Verifier,
    WitnessSession,
};

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    // ── The operator ────────────────────────────────────────────────────
    let id = LedgerId::new("example.com/ledgers/acme")?;
    let ledger = Ledger::new(MemoryStore::new(), id.clone())
        .await?
        .as_actor("setup");
    ledger.declare_currency(EUR, 2, date!("2026-01-01")).await?;
    ledger
        .open("Assets:Cash", AccountKind::Asset, date!("2026-01-01"))
        .await?;
    ledger
        .open("Income:Sales", AccountKind::Income, date!("2026-01-01"))
        .await?;
    ledger.define_months(2026, 9..=10).await?;
    let invoice = ledger
        .record(
            Entry::new("invoice-2026-0001", date!("2026-09-15"))
                .debit("Assets:Cash", amount!("1190.00"), EUR)
                .credit("Income:Sales", amount!("1190.00"), EUR),
        )
        .await?;
    ledger.seal_period("2026-09").await?;

    // The ledger's key comes from the operator's KMS as a 32-byte seed.
    let key = LogKey::from_seed(&id, [7; 32]);
    println!("publish the vkey: {}", key.vkey());

    // Two witnesses, by somebody else; the policy needs both.
    let mut witnesses = BTreeMap::new();
    let mut policy = Policy::builder().log(key.vkey());
    for (name, seed) in [("w1", [9; 32]), ("w2", [10; 32])] {
        let witness = TestWitness::new(&format!("witness.example/{name}"), seed)?.log(key.vkey());
        policy = policy.witness(name, witness.vkey());
        witnesses.insert(name, witness);
    }
    let policy = policy
        .group("both", Threshold::All, &["w1", "w2"])
        .quorum("both")
        .build()?;
    println!("the auditor's policy:\n{policy}");

    // Sign the head and collect a cosignature from each witness. The size
    // each witness holds is the operator's to remember (0 if unknown).
    let mut held: BTreeMap<&str, u64> = BTreeMap::new();
    let mut signed = ledger.checkpoint(&key).await?;
    for (name, witness) in &mut witnesses {
        let old = held.get(name).copied().unwrap_or(0);
        let mut session = WitnessSession::new(signed.clone(), witness.vkey(), old)?;
        loop {
            let proof = match session.needed_proof() {
                Some((old, new)) => Some(ledger.prove_consistency(old, new).await?),
                None => None,
            };
            let body = session.request(proof.as_ref())?;
            let reply = witness.post(&body); // your HTTP POST to <prefix>/add-checkpoint
            match session.receive(reply.status, reply.content_type, &reply.body)? {
                Next::Cosigned {
                    cosignature,
                    witness_size,
                } => {
                    held.insert(name, witness_size);
                    signed = signed.with_cosignature(&cosignature)?;
                    break;
                }
                Next::Retry => {} // 409: the session adopted the witness's size
                other => return Err(format!("unexpected witness answer: {other:?}").into()),
            }
        }
    }
    policy.verify(&signed)?; // publish only a checkpoint the quorum cosigned
    println!("checkpoint:\n{signed}");

    // Publish the log as tiles: hashes only. Entry bundles would disclose
    // every record in full, so they are a separate, explicit call.
    let mut published = 0usize;
    for tile in TileId::changed(0, signed.checkpoint().head().size) {
        let bytes = ledger.tile(tile).await?; // write to <prefix>/<tile.path()>
        published = published.saturating_add(bytes.len());
    }
    println!("tiles: {published} bytes of hashes");

    // Hand out one entry and one sealed balance as offline proofs.
    let entry = ledger.bundle_entry(invoice.id, &signed).await?.to_bytes();
    let cash = ledger
        .bundle_balance("Assets:Cash", EUR, Layer::Settled, "2026-09", &signed)
        .await?
        .to_bytes();

    // ── The auditor ─────────────────────────────────────────────────────
    let mut verifier = Verifier::from_policy(policy.clone());
    verifier.update(&SignedCheckpoint::parse(&signed.to_bytes())?, None)?;
    for file in [entry, cash] {
        match verifier.verify(&ProofBundle::parse(&file)?)? {
            Proven::Record(record) => println!(
                "entry {} is at position {} of {}",
                record.entry_id.ok_or("an entry has an id")?,
                record.position,
                record.origin
            ),
            Proven::Balance(balance) => println!(
                "{} closed {} at {} {}",
                balance.account,
                balance.period,
                balance.balance.net(),
                balance.currency
            ),
            other => println!("proven: {other:?}"),
        }
    }
    // Offline, without auditor state: the policy alone.
    let bundle = ledger.bundle_entry(invoice.id, &signed).await?;
    assert!(matches!(bundle.verify(&policy)?, Proven::Record(_)));
    Ok(())
}
