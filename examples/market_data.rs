//! Read-only: checks the production deployment, prints each enabled venue's prices and the
//! Hyperliquid to Polymarket book's limits, then follows the public event feed.
//!
//! ```sh
//! cargo run --example market_data
//! ```
#![expect(clippy::print_stdout, reason = "an example prints what it does")]

use futures_util::StreamExt as _;
use lamarge::{
    Client, Environment,
    book::{SweepTerms, delivery_floor},
    events::{Change, Snapshot},
    model::{Destination, EventData},
    types::{Price, VenueId},
};

#[tokio::main]
async fn main() -> lamarge::Result<()> {
    let client = Client::new(Environment::Production)?;
    println!("escrows: {:?}", client.check_deployment().await?);

    let market = client.market();
    for venue in market.markets().await?.venues {
        let price = |price: Option<Price>| price.map_or_else(|| "-".to_owned(), |p| p.to_string());
        println!(
            "{}: buy {}, sell {}, liquidity {} {}",
            venue.venue,
            price(venue.best_buy),
            price(venue.best_sell),
            venue.liquidity.format_units(venue.decimals),
            venue.symbol,
        );
    }

    let (from, to) = (VenueId::Hyperliquid, VenueId::Polymarket);
    let venues = market.venues().await?;
    let book = market.book(&from, &to).await?;
    let terms = SweepTerms::new(Some(book.taker_fee_bps)).floor(delivery_floor(
        &venues,
        &to,
        &Destination::Venue,
    ));
    match book.limits(None, &terms, chrono::Utc::now())? {
        Some(limits) => println!(
            "{from} -> {to}: pay {} to {} {from} base units",
            limits.minimum.pay, limits.maximum.pay
        ),
        None => println!("{from} -> {to}: nothing executable"),
    }

    let mut feed = client.events().subscribe();
    while let Some(change) = feed.next().await {
        match change? {
            Change::Resync(Snapshot::Public(snapshot)) => {
                println!("snapshot at {}: {} books", snapshot.cursor, snapshot.books.len());
            }
            Change::Events(page) => {
                for event in page.events {
                    match event.data {
                        EventData::BookChanged(changed) => {
                            println!("book changed: {:?} -> {:?}", changed.from, changed.to);
                        }
                        EventData::Trade(trade) => println!("trade: {trade:?}"),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}
