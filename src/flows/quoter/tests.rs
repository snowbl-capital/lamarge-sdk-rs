use serde_json::{Value, json};
use wiremock::{
    Mock, ResponseTemplate,
    matchers::{header, method, path},
};

use super::*;
use crate::{
    MakerCredential, RetryPolicy,
    error::VerificationKind,
    model::CredentialCreated,
    protocol::checks::lines::line_receive,
    testing::{IDENTITY, NOW, OTHER, OWNER, harness::*, json::fixture, sample},
    types::EscrowEnvironment,
};

const PREPARATION: &str = "0192f1c2-3d4e-7f00-8a1b-2c3d4e5f6a61";
const HL_POOL: &str = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const PM_POOL: &str = "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
/// The last admission time every publication here asks.
const VALID_UNTIL: i64 = NOW + 60;

fn price(text: &str) -> Price {
    text.parse().unwrap()
}

fn venue(name: &str) -> VenueId {
    name.parse().unwrap()
}

/// The fixture credential, quoting with `quote_signer`.
fn credential(quote_signer: Address) -> MakerCredential {
    let mut created = fixture("credential_created");
    created["quote_signer"] = json!(quote_signer);
    serde_json::from_value::<CredentialCreated>(created).unwrap().into()
}

fn bearer() -> String {
    let created = fixture("credential_created");
    format!("Bearer {}", created["token"].as_str().unwrap())
}

async fn quoter(harness: &Harness) -> Quoter {
    quoter_of(&harness.client, harness).await
}

async fn quoter_of(client: &crate::Client, harness: &Harness) -> Quoter {
    client.quoter(credential(harness.address()), harness.signer()).await.unwrap()
}

/// [`OWNER`]'s live Hyperliquid and Polymarket pools at sequence 1, quoted by `quote_signer`.
fn pools(quote_signer: Address) -> Vec<Value> {
    [("hyperliquid", HL_POOL), ("polymarket", PM_POOL)]
        .into_iter()
        .map(|(venue, id)| {
            let mut order = fixture("maker_preparation/order");
            let patch = json!({
                "preparation_id": null, "capital_pool": true, "id": id, "venue": venue,
                "owner_address": OWNER, "state": "live", "revision": "2", "last_sequence": 1,
                "quote_identity_id": IDENTITY, "quote_signer": quote_signer,
            });
            order.as_object_mut().unwrap().extend(patch.as_object().unwrap().clone());
            order
        })
        .collect()
}

/// Buys and sells on both venues: a thousand USDC of stock on Hyperliquid, a hundred on
/// Polymarket.
fn price_lines() -> Vec<PriceLine> {
    vec![
        PriceLine::new(venue("hyperliquid"), Amount::from(1_000_000_000u64))
            .buy(price("0.9994"))
            .sell(price("1.0002")),
        PriceLine::new(venue("polymarket"), Amount::from(100_000_000u64))
            .buy(price("1"))
            .sell(price("1.0014")),
    ]
}

fn recipients() -> BTreeMap<VenueId, Address> {
    BTreeMap::from([(venue("hyperliquid"), OWNER), (venue("polymarket"), OWNER)])
}

fn request(lines: Vec<PriceLine>) -> QuoteLinesRequest {
    QuoteLinesRequest::new(lines, recipients(), DateTime::from_timestamp(VALID_UNTIL, 0).unwrap())
}

fn wire(lines: &[PriceLine]) -> Vec<Value> {
    lines.iter().map(|line| serde_json::to_value(line.to_wire()).unwrap()).collect()
}

/// The preparation the server builds for `lines` over `pools`: one draft per pool with stock,
/// quoting every other venue with a buy price, signed by `quote_signer`; pools without stock
/// are cancelled.
fn preparation(lines: &[PriceLine], pools: &[Value], quote_signer: Address) -> Value {
    let valid_until = u64::try_from(VALID_UNTIL).unwrap();
    let expires_at = valid_until + 3601;
    let mut drafts = Vec::new();
    let mut cancels = Vec::new();
    for (index, pool) in pools.iter().enumerate() {
        let supply = venue(pool["venue"].as_str().unwrap());
        let line = lines.iter().find(|line| line.venue == supply).unwrap();
        let stock = line.stock.get();
        if stock.is_zero() {
            cancels.push(pool["id"].clone());
            continue;
        }
        let quotes: Vec<Value> = lines
            .iter()
            .filter(|other| other.venue != supply && other.buy.is_some())
            .map(|other| {
                let receive = line_receive(
                    stock,
                    line.sell.unwrap(),
                    other.buy.unwrap(),
                    &other.venue,
                    &supply,
                )
                .unwrap();
                let chain = deployments::chain(&other.venue).unwrap();
                json!({
                    "venue": other.venue.as_str(), "chain_id": chain.id, "token": chain.token,
                    "recipient": OWNER, "supply": stock.to_string(), "receive": receive.to_string(),
                    "min_fill": "1000000",
                })
            })
            .collect();
        let chain = deployments::chain(&supply).unwrap();
        let mut typed = sample("quote_set");
        typed["domain"]["chainId"] = json!(chain.id);
        typed["domain"]["verifyingContract"] =
            json!(deployments::escrow(chain.id, EscrowEnvironment::Production).unwrap());
        let signed: Vec<Value> = quotes
            .iter()
            .map(|quote| {
                json!({
                    "venue": quote["venue"], "chainId": quote["chain_id"], "token": quote["token"],
                    "recipient": quote["recipient"], "supply": quote["supply"],
                    "receive": quote["receive"], "minFill": quote["min_fill"],
                })
            })
            .collect();
        typed["message"] = json!({
            "owner": OWNER, "offerId": pool["id"], "sequence": 2, "expiresAt": expires_at,
            "quotes": signed,
        });
        drafts.push(json!({
            "order_id": pool["id"], "venue": supply.as_str(),
            "draft_id": format!("0192f1c2-3d4e-7f00-8a1b-2c3d4e5f6a7{index}"), "sequence": 2,
            "valid_until": valid_until, "expires_at": expires_at,
            "quote_size": stock.to_string(), "quotes": quotes,
            "to_sign": [{"kind": "quote_set", "signer": quote_signer, "typed_data": typed}],
        }));
    }
    json!({
        "id": PREPARATION, "quote_identity_id": IDENTITY, "expected_revision": "3",
        "valid_until": valid_until, "lines": wire(lines), "drafts": drafts, "cancels": cancels,
    })
}

/// Serves revision 3 of the lines, `pools`, `prepared`, and publications as revision 4.
async fn serve_lines(harness: &Harness, pools: &[Value], prepared: &Value) {
    let lines = json!({"quote_identity_id": IDENTITY, "revision": "3", "lines": []});
    Mock::given(method("GET"))
        .and(path("/v1/quote-lines"))
        .respond_with(ok(lines))
        .mount(&harness.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/orders"))
        .respond_with(ok(json!(pools)))
        .mount(&harness.server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/quote-lines/prepare"))
        .and(header("authorization", bearer().as_str()))
        .respond_with(ok(prepared.clone()))
        .mount(&harness.server)
        .await;
    let published =
        json!({"quote_identity_id": IDENTITY, "revision": "4", "lines": prepared["lines"]});
    Mock::given(method("POST"))
        .and(path("/v1/quote-lines"))
        .respond_with(ok(published))
        .mount(&harness.server)
        .await;
}

#[tokio::test]
async fn lines_are_prepared_checked_signed_and_published() {
    let harness = Harness::new().await;
    let quoter = quoter(&harness).await;
    let before = harness.signatures();
    let pools = pools(harness.address());
    let prepared = preparation(&price_lines(), &pools, harness.address());
    serve_lines(&harness, &pools, &prepared).await;
    let published = quoter.publish_lines(&request(price_lines())).await.unwrap();
    assert_eq!(published.revision, Revision::new(4));
    let asked = harness.bodies("POST", "/v1/quote-lines/prepare").await;
    assert_eq!(
        asked,
        [json!({
            "quote_identity_id": IDENTITY, "expected_revision": "3", "valid_until": VALID_UNTIL,
            "lines": wire(&price_lines()),
        })]
    );
    let publications = harness.bodies("POST", "/v1/quote-lines").await;
    let [publication] = publications.as_slice() else { panic!("{publications:?}") };
    assert_eq!(publication["preparation_id"], PREPARATION);
    let drafts = prepared["drafts"].as_array().unwrap();
    let signatures = publication["signatures"].as_array().unwrap();
    assert_eq!(signatures.len(), 2);
    for (signed, draft) in signatures.iter().zip(drafts) {
        assert_eq!(signed["draft_id"], draft["draft_id"]);
        let typed = &draft["to_sign"][0]["typed_data"];
        assert_eq!(signer_of(signed, &digest(typed)), harness.address());
    }
    assert_eq!(harness.signatures() - before, 2);
}

#[tokio::test]
async fn pools_without_stock_are_cancelled() {
    let harness = Harness::new().await;
    let quoter = quoter(&harness).await;
    let before = harness.signatures();
    let lines = vec![
        price_lines().remove(0),
        PriceLine::new(venue("polymarket"), Amount::ZERO).buy(price("1")),
    ];
    let pools = pools(harness.address());
    let prepared = preparation(&lines, &pools, harness.address());
    assert_eq!(prepared["cancels"], json!([PM_POOL]));
    serve_lines(&harness, &pools, &prepared).await;
    quoter.publish_lines(&request(lines)).await.unwrap();
    let publications = harness.bodies("POST", "/v1/quote-lines").await;
    assert_eq!(publications[0]["signatures"].as_array().unwrap().len(), 1);
    assert_eq!(harness.signatures() - before, 1);
}

/// One case per input the flow supplies to the checks: the pools it read and the identity.
/// Each shows that nothing is signed or published.
#[tokio::test]
async fn a_failed_check_signs_nothing() {
    type Edit = fn(&mut Value, &mut Vec<Value>);
    let cases: [(Edit, VerificationKind, &str); 3] = [
        (
            |_, pools| pools[0]["state"] = json!("funding"),
            VerificationKind::Changed,
            details::POOL_STATE,
        ),
        (
            |_, pools| pools[0]["quote_signer"] = json!(OTHER),
            VerificationKind::Changed,
            details::POOL_QUOTE_SIGNER,
        ),
        (
            |p, _| p["quote_identity_id"] = json!("0192f1c2-3d4e-7f00-8a1b-2c3d4e5f6aff"),
            VerificationKind::Differs,
            details::QUOTE_IDENTITY_ID,
        ),
    ];
    for (edit, kind, detail) in cases {
        let harness = Harness::new().await;
        let quoter = quoter(&harness).await;
        let mut pools = pools(harness.address());
        let mut prepared = preparation(&price_lines(), &pools, harness.address());
        edit(&mut prepared, &mut pools);
        serve_lines(&harness, &pools, &prepared).await;
        let Err(Error::Verification(error)) = quoter.publish_lines(&request(price_lines())).await
        else {
            panic!("{detail} is refused");
        };
        assert_eq!((error.kind, error.detail), (kind, detail));
        assert!(harness.requests("POST", "/v1/quote-lines").await.is_empty());
        assert_eq!(harness.signatures(), 0);
    }
}

#[tokio::test]
async fn inconsistent_requests_send_nothing() {
    let harness = Harness::new().await;
    let quoter = quoter(&harness).await;
    let received = harness.server.received_requests().await.unwrap().len();
    let [hl, pm] = <[PriceLine; 2]>::try_from(price_lines()).unwrap();
    let at = |offset| DateTime::from_timestamp(NOW + offset, 0).unwrap();
    let invalid = [
        ("a venue twice", request(vec![hl.clone(), hl.clone()])),
        ("buy above sell", request(vec![hl.clone().buy(price("1.0003")), pm.clone()])),
        (
            "stock without a sell price",
            request(vec![PriceLine::new(venue("gmx"), Amount::from(1u64))]),
        ),
        ("no recipient", QuoteLinesRequest::new(vec![hl.clone()], BTreeMap::new(), at(60))),
        ("valid until now", QuoteLinesRequest::new(price_lines(), recipients(), at(0))),
        ("valid past five minutes", QuoteLinesRequest::new(price_lines(), recipients(), at(301))),
    ];
    for (name, request) in invalid {
        let result = quoter.publish_lines(&request).await;
        assert!(matches!(result, Err(Error::InvalidRequest(_))), "{name}: {result:?}");
    }
    let unknown = request(vec![PriceLine::new(venue("orderly"), Amount::ZERO)]);
    let Err(Error::Verification(error)) = quoter.publish_lines(&unknown).await else {
        panic!("an unpinned venue is unsupported");
    };
    assert_eq!((error.kind, error.detail), (VerificationKind::Unsupported, details::VENUE));
    for ttl in [Duration::from_secs(4), Duration::from_millis(120_001)] {
        assert!(matches!(quoter.lease(ttl).await, Err(Error::InvalidRequest(_))), "{ttl:?}");
    }
    assert_eq!(harness.server.received_requests().await.unwrap().len(), received);
    // Five minutes ahead is still accepted.
    let pools = pools(harness.address());
    serve_lines(&harness, &pools, &preparation(&price_lines(), &pools, harness.address())).await;
    let edge = QuoteLinesRequest::new(price_lines(), recipients(), at(300));
    let Err(Error::Verification(error)) = quoter.publish_lines(&edge).await else {
        panic!("the served preparation names another valid_until");
    };
    assert_eq!(error.detail, details::VALID_UNTIL);
}

#[tokio::test]
async fn leases_report_the_latest_fresh_round_trip() {
    let harness = Harness::new().await;
    let quoter = quoter(&harness).await;
    let route = format!("/v1/quote-identities/{IDENTITY}/lease");
    let lease = fixture("lease");
    Mock::given(method("PUT"))
        .and(path(&route))
        .and(header("authorization", bearer().as_str()))
        .respond_with(ok(lease).set_delay(Duration::from_millis(40)))
        .mount(&harness.server)
        .await;
    let ttl = Duration::from_secs(30);
    // Nothing timed yet: no telemetry.
    assert_eq!(quoter.lease(ttl).await.unwrap().generation, Revision::new(3));
    quoter.lease(Duration::from_millis(30_900)).await.unwrap();
    tokio::time::pause();
    tokio::time::advance(Duration::from_millis(MAX_RTT_AGE_MS + 1)).await;
    tokio::time::resume();
    quoter.lease(ttl).await.unwrap();
    quoter.lease(ttl).await.unwrap();
    let sent = harness.bodies("PUT", &route).await;
    assert_eq!(sent[0], json!({"ttl_seconds": 30}));
    let (rtt, age) =
        (sent[1]["api_rtt_ms"].as_u64().unwrap(), sent[1]["api_rtt_age_ms"].as_u64().unwrap());
    assert!((40..MAX_RTT_MS).contains(&rtt), "{rtt}");
    assert!(age < 1_000, "{age}");
    assert_eq!(sent[1]["ttl_seconds"], 30, "whole seconds, rounded down");
    assert_eq!(
        sent[2],
        json!({"ttl_seconds": 30}),
        "a round trip over two minutes old is not sent"
    );
    assert!(sent[3]["api_rtt_ms"].as_u64().unwrap() >= 40);
}

#[test]
fn telemetry_bounds_are_inclusive() {
    let now = Instant::now() + Duration::from_secs(600);
    let sample = |rtt_ms: u64, age_ms: u64| {
        telemetry(Some((Duration::from_millis(rtt_ms), now - Duration::from_millis(age_ms))), now)
    };
    assert_eq!(sample(MAX_RTT_MS, MAX_RTT_AGE_MS), Some((MAX_RTT_MS, MAX_RTT_AGE_MS)));
    assert_eq!(sample(MAX_RTT_MS + 1, 0), None);
    assert_eq!(sample(0, MAX_RTT_AGE_MS + 1), None);
    assert_eq!(sample(12, 34), Some((12, 34)));
    assert_eq!(telemetry(None, now), None);
}

#[tokio::test]
async fn lines_and_cancellations_use_the_credential() {
    let harness = Harness::new().await;
    let quoter = quoter(&harness).await;
    let lines = fixture("quote_lines");
    let cancellation = fixture("identity_cancellation");
    Mock::given(method("GET"))
        .and(path("/v1/quote-lines"))
        .and(header("authorization", bearer().as_str()))
        .respond_with(ok(lines))
        .mount(&harness.server)
        .await;
    Mock::given(method("DELETE"))
        .and(path(format!("/v1/quote-identities/{IDENTITY}/quotes")))
        .and(header("authorization", bearer().as_str()))
        .respond_with(ok(cancellation))
        .mount(&harness.server)
        .await;
    assert_eq!(quoter.lines().await.unwrap().revision, Revision::new(12));
    assert_eq!(quoter.cancel_all().await.unwrap().orders, 2);
    assert_eq!(harness.signatures(), 0, "reads and cancellations sign nothing");
}

#[tokio::test]
async fn a_publication_is_retried_with_the_same_signatures() {
    let harness = Harness::new().await;
    let retrying = harness.client_with(RetryPolicy::new(3, Duration::ZERO, Duration::ZERO));
    let quoter = quoter_of(&retrying, &harness).await;
    let before = harness.signatures();
    let pools = pools(harness.address());
    let prepared = preparation(&price_lines(), &pools, harness.address());
    Mock::given(method("POST"))
        .and(path("/v1/quote-lines"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&harness.server)
        .await;
    serve_lines(&harness, &pools, &prepared).await;
    quoter.publish_lines(&request(price_lines())).await.unwrap();
    let publications = harness.bodies("POST", "/v1/quote-lines").await;
    assert_eq!(publications.len(), 2);
    assert_eq!(publications[0], publications[1]);
    assert_eq!(harness.signatures() - before, 2);
    assert_eq!(harness.requests("POST", "/v1/quote-lines/prepare").await.len(), 1);
}

#[test]
fn price_lines_go_on_the_wire_as_built() {
    let line = PriceLine::new(venue("hyperliquid"), Amount::from(5u64))
        .buy(price("0.999"))
        .sell(price("1.001"))
        .min_fill(Amount::from(2u64));
    assert_eq!(
        serde_json::to_value(line.to_wire()).unwrap(),
        json!({"venue": "hyperliquid", "buy": "0.999", "sell": "1.001", "stock": "5", "min_fill": "2"})
    );
    let bare = serde_json::to_value(PriceLine::new(venue("gmx"), Amount::ZERO).to_wire()).unwrap();
    assert_eq!(bare, json!({"venue": "gmx", "buy": null, "sell": null, "stock": "0"}));
}
