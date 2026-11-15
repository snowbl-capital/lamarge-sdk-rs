//! On the quoting host: with the credential `maker_provision` saved and the quote key, sells the
//! Hyperliquid pool's stock and buys on Polymarket, renewing the lease and the lines every
//! 10 seconds, and follows the identity's fills on its private event feed.
//!
//! ```sh
//! LAMARGE_QUOTE_PRIVATE_KEY=0x… LAMARGE_POLYMARKET_ACCOUNT=0x… cargo run --example maker_quote
//! ```
//!
//! `LAMARGE_POLYMARKET_ACCOUNT` is the maker's address linked on Polymarket, paid for what the
//! pool sells. Stopping the process stops the quotes within 30 seconds, when the lease lapses.
#![expect(clippy::print_stdout, reason = "an example prints what it does")]

use std::{collections::BTreeMap, env, error::Error, fs, time::Duration};

use alloy_signer_local::PrivateKeySigner;
use futures_util::StreamExt as _;
use lamarge::{
    Address, Client, Environment, MakerCredential, Quoter,
    events::{Change, FillTracker},
    flows::{PriceLine, QuoteLinesRequest},
    model::{EventData, OrderState},
    types::{Amount, Price, VenueId},
};

const LEASE: Duration = Duration::from_secs(30);
const REFRESH: Duration = Duration::from_secs(10);
const VALIDITY: chrono::TimeDelta = chrono::TimeDelta::seconds(25);

/// The maker's prices: what it asks per unit delivered from its pool, what it pays per unit
/// received.
struct Prices {
    sell: Price,
    buy: Price,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let credential: MakerCredential = serde_json::from_slice(&fs::read("credential.json")?)?;
    let quote_key: PrivateKeySigner = env::var("LAMARGE_QUOTE_PRIVATE_KEY")?.parse()?;
    let polymarket_account: Address = env::var("LAMARGE_POLYMARKET_ACCOUNT")?.parse()?;
    let prices = Prices { sell: "1.0005".parse()?, buy: "0.9995".parse()? };

    let client = Client::new(Environment::Production)?;
    let quoter = client.quoter(credential, quote_key).await?;
    let mut feed = quoter.events().subscribe();
    let mut fills = FillTracker::default();
    let mut tick = tokio::time::interval(REFRESH);
    loop {
        tokio::select! {
            _ = tick.tick() => publish(&quoter, &prices, polymarket_account).await?,
            change = feed.next() => {
                let change = change.ok_or("event feed ended")??;
                fills.apply(&change);
                match &change {
                    Change::Resync(_) => println!("{} fills", fills.iter().count()),
                    Change::Events(page) => {
                        for event in &page.events {
                            if let EventData::FillChanged(fill) = &event.data {
                                println!("fill {} {}: {} out", fill.id, fill.state, fill.amount_out);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

/// Renews the lease, then publishes the lines with the pool's current stock.
async fn publish(quoter: &Quoter, prices: &Prices, recipient: Address) -> lamarge::Result<()> {
    quoter.lease(LEASE).await?;
    let orders = quoter.orders().list(None).await?;
    let stock = orders
        .iter()
        .find(|order| {
            order.capital_pool
                && order.venue == VenueId::Hyperliquid
                && order.state == OrderState::Live
        })
        .map_or(Amount::ZERO, |pool| pool.available_to_quote);
    let lines = vec![
        PriceLine::new(VenueId::Hyperliquid, stock).sell(prices.sell),
        PriceLine::new(VenueId::Polymarket, Amount::ZERO).buy(prices.buy),
    ];
    let recipients = BTreeMap::from([(VenueId::Polymarket, recipient)]);
    let request = QuoteLinesRequest::new(lines, recipients, chrono::Utc::now() + VALIDITY);
    let published = quoter.publish_lines(&request).await?;
    println!("lines revision {}: {stock} in stock", published.revision);
    Ok(())
}
