# lamarge

Rust client for the [LaMarge API](https://docs.lamarge.trade): market data, the event feed, sign-in, venue setup, takes, transfers, withdrawals and passive market making on escrow venues.

Before a signer sees anything, the crate:

- checks every item to sign against your request and the deployment values this release pins;
- saves the signatures of takes, transfers, withdrawals, maker capital and wallet permits in your `store::Journal` before submitting them, and reuses them when the same request runs again; venue setup steps (setup permits included), closes and quote publications are not journalled and may sign again after an interruption;
- keeps your client IDs stable across retries.

## Install

```sh
cargo add lamarge chrono serde_json
cargo add tokio --features macros,rt-multi-thread
cargo add alloy-signer-local
```

The crate runs on Tokio. Any `alloy_signer::Signer` signs: a local key, a KMS key or a Ledger. TLS is rustls with the platform verifier: slim and distroless images need system CA roots (`ca-certificates`). HTTP requests honour `HTTPS_PROXY`; the event feed's WebSocket connects directly and, without a direct route, falls back to HTTP replay. Lighter's public API is read directly to check account owners; `ClientBuilder::lighter_reads` sends those reads through your own client, from your address and within your rate budget. Amounts are base units of each venue's token (`types::Amount::parse_venue`).

## Examples

### Market data

```rust,no_run
use lamarge::{Client, Environment, book::SweepTerms, types::VenueId};

#[tokio::main]
async fn main() -> lamarge::Result<()> {
    let client = Client::new(Environment::Production)?;
    client.check_deployment().await?;

    let book = client.market().book(&VenueId::Hyperliquid, &VenueId::Polymarket).await?;
    let terms = SweepTerms::new(Some(book.taker_fee_bps));
    if let Some(limits) = book.limits(None, &terms, chrono::Utc::now())? {
        println!("pay {} to {} base units", limits.minimum.pay, limits.maximum.pay);
    }
    Ok(())
}
```

### Take

From Hyperliquid margin into the session's Polymarket account, priced from the book with 10 bps slippage. The whole request resumes the take: it is saved before the first call and reused on every run.

```rust,no_run
use alloy_signer_local::PrivateKeySigner;
use lamarge::{
    Client, Environment,
    book::{SweepTerms, slippage_minimum},
    flows::{Recipient, TakeRequest, TradeTerms},
    store::FileStore,
    types::{Amount, Bps, ClientId, VenueId},
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let signer: PrivateKeySigner = std::env::var("LAMARGE_PRIVATE_KEY")?.parse()?;
    let me = signer.address();
    let client = Client::new(Environment::Production)?;
    let session = client.sign_in(signer, FileStore::open("lamarge-state")?).await?;

    let (from, to) = (VenueId::Hyperliquid, VenueId::Polymarket);
    for venue in [&from, &to] {
        session.link_account(venue).await?;
    }
    let saved = "take-1.json";
    let request: TakeRequest = match std::fs::read(saved) {
        Ok(json) => serde_json::from_slice(&json)?,
        Err(_) => {
            let amount_in = Amount::parse_venue(&from, "100")?;
            let book = client.market().book(&from, &to).await?;
            let fee = book.taker_fee_bps;
            let swept = book.sweep(amount_in, &SweepTerms::new(Some(fee)), chrono::Utc::now())?;
            let min_out = slippage_minimum(swept.out, Bps::new(10).ok_or("slippage")?);
            let terms = TradeTerms::new(from, to, amount_in, min_out, Recipient::Account(me))
                .max_fee_bps(fee);
            let request = TakeRequest::new(ClientId::new("take-1")?, terms);
            std::fs::write(saved, serde_json::to_vec(&request)?)?;
            request
        }
    };
    let fills = session.take(&request).await?;
    println!("{} fills", fills.len());
    session.sign_out().await?;
    Ok(())
}
```

Runnable programs in [`examples/`](https://github.com/snowbl-capital/lamarge-sdk-rs/tree/main/examples):

| Example | Key | Does |
|---|---|---|
| `market_data` | none | venue prices, book limits, the public event feed |
| `taker_transfer` | financial | a resumable transfer from Hyperliquid margin into Polymarket |
| `maker_provision` | financial | Hyperliquid setup, a maker credential, pool capital |
| `maker_quote` | quote | lease, quote lines, fills on the private feed |

## Features

| Feature | Default | Enables |
|---|---|---|
| `aws-lc-rs` | yes | rustls with the aws-lc-rs provider (needs cmake on some platforms) |
| `ring` | no | rustls with the ring provider |

One provider is required; with both, aws-lc-rs is used. The crate never installs a process-wide rustls provider.

## MSRV

Rust 1.94.1. It is raised only in a minor release, noted in the [changelog](https://github.com/snowbl-capital/lamarge-sdk-rs/blob/main/CHANGELOG.md).

## Documentation

- API: [docs.rs/lamarge](https://docs.rs/lamarge)
- Guide: [docs.lamarge.trade/rust-sdk](https://docs.lamarge.trade/rust-sdk)

## Licence

Licensed under either of [Apache License, Version 2.0](https://github.com/snowbl-capital/lamarge-sdk-rs/blob/main/LICENSE-APACHE) or [MIT license](https://github.com/snowbl-capital/lamarge-sdk-rs/blob/main/LICENSE-MIT) at your option.
