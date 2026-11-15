//! A transfer from Hyperliquid margin into the session's Polymarket account: links both venues,
//! prices the book with 10 bps slippage, then runs the transfer and prints its outcome.
//!
//! ```sh
//! LAMARGE_PRIVATE_KEY=0x… LAMARGE_AMOUNT=100 LAMARGE_TRANSFER_ID=transfer-1 \
//!     cargo run --example taker_transfer
//! ```
//!
//! The first run saves the request to `transfer-<id>.json` before the transfer starts; a
//! rerun with the same `LAMARGE_TRANSFER_ID` reuses that request and resumes the transfer (a
//! pending one goes on, a filled one prints its fills again), never withdrawing twice. A new
//! transfer needs a new id.
#![expect(clippy::print_stdout, reason = "an example prints what it does")]

use std::{env, error::Error, fs, io::ErrorKind};

use alloy_signer_local::PrivateKeySigner;
use lamarge::{
    Client, Environment, Session,
    book::{SweepTerms, slippage_minimum},
    flows::{Recipient, TradeTerms, TransferOutcome, TransferRequest},
    store::FileStore,
    types::{Amount, Bps, ClientId, VenueId},
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let key: PrivateKeySigner = env::var("LAMARGE_PRIVATE_KEY")?.parse()?;
    let id = ClientId::new(env::var("LAMARGE_TRANSFER_ID")?)?;
    let client = Client::new(Environment::Production)?;
    let session = client.sign_in(key, FileStore::open("taker-state")?).await?;

    let saved = format!("transfer-{id}.json");
    let request: TransferRequest = match fs::read(&saved) {
        Ok(json) => serde_json::from_slice(&json)?,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            let request = TransferRequest::new(id, priced_terms(&session).await?);
            fs::write(&saved, serde_json::to_vec(&request)?)?;
            request
        }
        Err(error) => return Err(error.into()),
    };

    match session.transfer(&request).await? {
        TransferOutcome::Filled { intent, fills } => {
            println!("transfer {} filled:", intent.id);
            for fill in fills {
                println!(
                    "  {} pays {} for {} ({})",
                    fill.id, fill.amount_in, fill.amount_out, fill.state
                );
            }
        }
        TransferOutcome::Pending { intent, reason } => {
            println!("transfer {} pending ({reason:?}): run it again later", intent.id);
        }
        _ => println!("unknown outcome: upgrade the crate"),
    }
    session.sign_out().await?;
    Ok(())
}

/// `LAMARGE_AMOUNT` from Hyperliquid into the session's Polymarket account, both linked, for
/// at least the book's price less 10 bps.
async fn priced_terms(session: &Session) -> Result<TradeTerms, Box<dyn Error>> {
    let (from, to) = (VenueId::Hyperliquid, VenueId::Polymarket);
    for venue in [&from, &to] {
        session.link_account(venue).await?;
    }
    let amount_in = Amount::parse_venue(&from, &env::var("LAMARGE_AMOUNT")?)?;
    let book = session.client().market().book(&from, &to).await?;
    let fee = book.taker_fee_bps;
    let swept = book.sweep(amount_in, &SweepTerms::new(Some(fee)), chrono::Utc::now())?;
    let min_out = slippage_minimum(swept.out, Bps::new(10).ok_or("invalid slippage")?);
    let me = Recipient::Account(session.address());
    Ok(TradeTerms::new(from, to, amount_in, min_out, me).max_fee_bps(fee))
}
