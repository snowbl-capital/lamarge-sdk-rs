//! Read-only checks against production, ignored by default: `cargo test --test live -- --ignored`.
//! Nothing is signed or written.
#![expect(clippy::unwrap_used, clippy::panic, reason = "helpers fail the test that calls them")]

use std::{sync::Arc, time::Duration};

use futures_util::StreamExt as _;
use lamarge::{
    Client, Environment,
    events::{Change, Snapshot},
    model::{self, Book, EventPage, ExplorerTransfer, Health, Info, Markets, Page, Venue},
    types::{EscrowEnvironment, VenueId},
};
use rustls_platform_verifier::BuilderVerifierExt as _;
use serde::de::DeserializeOwned;

const API: &str = "https://api.lamarge.trade";

fn client() -> Client {
    Client::new(Environment::Production).unwrap()
}

/// A plain HTTP client, to read the raw answers the SDK's models must account for in full.
fn raw_http() -> reqwest::Client {
    #[cfg(feature = "aws-lc-rs")]
    let provider = rustls::crypto::aws_lc_rs::default_provider();
    #[cfg(all(feature = "ring", not(feature = "aws-lc-rs")))]
    let provider = rustls::crypto::ring::default_provider();
    let tls = rustls::ClientConfig::builder_with_provider(Arc::new(provider))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_platform_verifier()
        .unwrap()
        .with_no_client_auth();
    reqwest::Client::builder()
        .tls_backend_preconfigured(tls)
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap()
}

/// Decodes `GET {API}{path}` as `T`, failing on any field `T` does not read.
async fn strict<T: DeserializeOwned>(http: &reqwest::Client, path: &str) -> T {
    let response = http.get(format!("{API}{path}")).send().await.unwrap();
    let status = response.status();
    // A degraded server answers /health with 503 and the same body.
    assert!(status.is_success() || (path == "/health" && status == 503), "{path}: {status}");
    let body = response.text().await.unwrap();
    let mut ignored = Vec::new();
    let value = serde_ignored::deserialize(&mut serde_json::Deserializer::from_str(&body), |at| {
        ignored.push(at.to_string());
    })
    .unwrap_or_else(|error| panic!("{path}: {error}"));
    assert!(ignored.is_empty(), "{path}: fields the model does not read: {ignored:?}");
    value
}

#[tokio::test]
#[ignore = "reads production"]
async fn production_matches_the_pinned_deployments() {
    assert_eq!(client().check_deployment().await.unwrap(), EscrowEnvironment::Production);
}

#[tokio::test]
#[ignore = "reads production"]
async fn market_reads_decode_strictly() {
    let (client, http) = (client(), raw_http());
    let market = client.market();

    let venues = market.venues().await.unwrap();
    assert_eq!(strict::<Vec<Venue>>(&http, "/v1/venues").await, venues);
    let info = market.info().await.unwrap();
    assert_eq!(strict::<Info>(&http, "/v1/info").await.operator, info.operator);
    market.markets().await.unwrap();
    strict::<Markets>(&http, "/v1/markets").await;
    market.health().await.unwrap();
    strict::<Health>(&http, "/health").await;
    client.explorer().pending_trades(Some(5)).await.unwrap();
    let page: Page<ExplorerTransfer> = strict(&http, "/v1/explorer/transfers?limit=20").await;
    assert!(page.data.len() <= 20);

    let pinned: Vec<&VenueId> = venues
        .iter()
        .filter(|venue| venue.enabled && venue.id.decimals().is_some())
        .map(|venue| &venue.id)
        .collect();
    assert!(pinned.len() >= 2, "enabled pinned venues: {pinned:?}");
    for from in &pinned {
        for to in pinned.iter().filter(|to| *to != from) {
            let book = market.book(from, to).await.unwrap();
            assert_eq!((&book.from, &book.to), (*from, *to));
            strict::<Book>(&http, &format!("/v1/books?from={from}&to={to}")).await;
        }
    }
}

#[tokio::test]
#[ignore = "reads production"]
async fn the_public_journal_replays_from_its_snapshot() {
    let (client, http) = (client(), raw_http());
    let snapshot = client.events().snapshot().await.unwrap();
    strict::<model::PublicSnapshot>(&http, "/v1/snapshot").await;
    let page = client.events().page(snapshot.cursor, Some(50)).await.unwrap();
    assert!(page.next_cursor >= snapshot.cursor);
    strict::<EventPage>(&http, &format!("/v1/events?after={}&limit=50", snapshot.cursor)).await;
}

#[tokio::test]
#[ignore = "reads production"]
async fn a_subscription_starts_with_a_public_resync() {
    let mut feed = client().events().subscribe();
    let first = tokio::time::timeout(Duration::from_secs(10), feed.next())
        .await
        .expect("a change within 10 s")
        .expect("the stream is open")
        .unwrap();
    assert!(matches!(first, Change::Resync(Snapshot::Public(_))), "{first:?}");
}
