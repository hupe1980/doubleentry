//! Sign checkpoints, have a witness cosign them, and verify each cosigned head as a third party.
use doubleentry::prelude::*;
use doubleentry::testing::TestWitness;
use doubleentry::transparency::{self as t, Next};

const START: Date = date!("2026-01-01");

fn main() -> Result<()> {
    let mut books = Journal::new(LedgerId::new("example.com/acme")?);
    books.declare_currency(EUR, 2, START)?;
    books.define_months(2026, 1..=12)?;
    books.open("Assets:Cash", AccountKind::Asset, START)?;
    books.open("Income:Sales", AccountKind::Income, START)?;
    let log_key = t::LogKey::from_seed(books.id(), [1; 32]);
    let mut witness = TestWitness::new("witness.example", [7; 32])?.log(log_key.vkey()); // a C2SP witness
    let quorum = [("witness.example".to_string(), witness.vkey())];
    let mut auditor = t::Verifier::from_policy(t::Policy::k_of_n(log_key.vkey(), 1, quorum)?);

    let mut old = 0;
    for i in 0..3 {
        let sale = Entry::new(format!("sale-{i}"), date!("2026-03-01"));
        let sale = sale.debit("Assets:Cash", amount!("1.00"), EUR);
        books.record(sale.credit("Income:Sales", amount!("1.00"), EUR))?;
        let checkpoint = books.checkpoint(&log_key)?;
        let mut session = t::WitnessSession::new(checkpoint.clone(), witness.vkey(), old)?;
        let proof = match session.needed_proof() {
            Some((from, to)) => Some(books.prove_consistency(from, to)?),
            None => None,
        };
        let reply = witness.post(&session.request(proof.as_ref())?); // the caller's HTTP POST
        let next = session.receive(reply.status, reply.content_type, &reply.body)?;
        let Next::Cosigned {
            cosignature,
            witness_size,
        } = next
        else {
            unreachable!()
        };
        old = witness_size;
        let cosigned = checkpoint.with_cosignature(&cosignature)?;
        let update = auditor.update(&cosigned, proof.as_ref())?;
        println!("auditor: {update:?}");
    }
    Ok(())
}
