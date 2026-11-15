//! The conversion against a mock of its routes: what is asked of the sender, in which order, and
//! never twice.

use std::sync::{Arc, Mutex, atomic::Ordering};

use serde_json::{Value, json};
use wiremock::{Mock, Request, matchers::method};

use super::*;
use crate::{
    HostError, SendFuture, StatusFuture,
    error::VerificationKind,
    flows::{Recipient, TradeTerms, TransferRequest},
    model::Funding,
    testing::{NOW, at, harness::*, json::fixture, stamp},
    types::{ClientId, EscrowEnvironment},
};

const TRANSFER: &str = "0192f1c2-3d4e-7f00-8a1b-2c3d4e5f6a10";
const CONVERSION: &str = "0192f1c2-3d4e-7f00-8a1b-2c3d4e5f6a50";
const HASH: &str = "0xb1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1";
const APPROVAL_HASH: &str = "0xa1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1";

fn amount(units: u64) -> Amount {
    Amount::new(U256::from(units))
}

/// 5.0025 native USDC into at least 5 pUSD.
fn terms() -> ConversionTerms {
    ConversionTerms::new(amount(5_002_500), amount(5_000_000))
}

fn escrow() -> Address {
    deployments::escrow(POLYMARKET.id, EscrowEnvironment::Production).unwrap()
}

/// A scripted sender: what it was asked, answered in turn (then [`HASH`]).
#[derive(Clone, Default)]
struct Sender {
    asked: Arc<Mutex<Vec<Transaction>>>,
    answers: Arc<Mutex<Vec<std::result::Result<B256, &'static str>>>>,
    status: Arc<Mutex<Option<SendStatus>>>,
}

impl Sender {
    fn answering(answers: Vec<std::result::Result<B256, &'static str>>) -> Self {
        Self { answers: Arc::new(Mutex::new(answers)), ..Self::default() }
    }

    fn asked(&self) -> Vec<Transaction> {
        self.asked.lock().unwrap().clone()
    }
}

impl TransactionSender for Sender {
    fn send(&self, transaction: Transaction) -> SendFuture<'_> {
        self.asked.lock().unwrap().push(transaction);
        let answer = {
            let mut answers = self.answers.lock().unwrap();
            if answers.is_empty() { Ok(HASH.parse().unwrap()) } else { answers.remove(0) }
        };
        Box::pin(async move {
            answer.map_err(|kind| {
                let error: HostError = kind.into();
                if kind == "declined" {
                    SendError::NotSent(error)
                } else {
                    SendError::Unknown(error)
                }
            })
        })
    }

    fn status<'a>(&'a self, _: &'a str) -> StatusFuture<'a> {
        let status = *self.status.lock().unwrap();
        Box::pin(async move { Ok(status.unwrap_or(SendStatus::Unknown)) })
    }
}

/// The mock's conversion and the payer's native USDC, changed by what it is sent.
struct Server {
    conversion: Value,
    allowance: u64,
    balance: u64,
}

struct Mocked {
    harness: Harness,
    session: Session,
    intent: TransferIntent,
    server: Arc<Mutex<Server>>,
}

impl Mocked {
    async fn new() -> Self {
        let harness = Harness::bare().await;
        let mut venues = venues(EscrowEnvironment::Production);
        for venue in venues.as_array_mut().unwrap() {
            if venue["id"] == "polymarket" {
                venue["chain"]["wallet_funding"]["conversion"] = json!({
                    "token": native_usdc(), "symbol": "USDC", "decimals": 6,
                    "converter": CONVERTER, "min_out_bps": 9_995, "minimum": "10000",
                });
            }
        }
        harness.serve_venues(venues).await;
        harness.serve_sign_in().await;
        let session = harness.session().await;
        let owner = harness.address();
        let now = harness.now().timestamp();
        let server = Arc::new(Mutex::new(Server {
            conversion: json!({
                "amount_in": "5002500", "min_out": "5000000", "id": CONVERSION, "chain_id": 137,
                "escrow": escrow(), "payer": owner, "converter": CONVERTER, "token": native_usdc(),
                "nonce": "3", "deadline": (now + 1_800).to_string(), "prepared_block": 1,
                "state": "prepared", "attempts": [], "tx_hash": null, "received": null,
                "block_number": null,
            }),
            allowance: 0,
            balance: 6_000_000,
        }));
        let state = Arc::clone(&server);
        Mock::given(method("POST"))
            .and(wiremock::matchers::path(format!("/v1/transfers/{TRANSFER}/conversion/prepare")))
            .respond_with(move |_: &Request| ok(state.lock().unwrap().conversion.clone()))
            .mount(&harness.server)
            .await;
        let state = Arc::clone(&server);
        Mock::given(method("POST"))
            .and(wiremock::matchers::path(format!("/v1/transfers/{TRANSFER}/conversion")))
            .respond_with(move |request: &Request| {
                let body: Value = request.body_json().unwrap();
                let mut server = state.lock().unwrap();
                match body.get("tx_hash") {
                    Some(hash) => {
                        server.conversion["attempts"].as_array_mut().unwrap().push(hash.clone());
                    }
                    None => server.conversion["state"] = json!("pending"),
                }
                ok(server.conversion.clone())
            })
            .mount(&harness.server)
            .await;
        let (state, clock) = (Arc::clone(&server), harness.clock());
        Mock::given(method("GET"))
            .and(wiremock::matchers::path("/v1/wallets/polymarket"))
            .respond_with(move |_: &Request| {
                let server = state.lock().unwrap();
                let observed = at(clock.load(Ordering::SeqCst) - NOW);
                ok(json!({
                    "conversion": {
                        "token": native_usdc(), "balance": server.balance.to_string(),
                        "allowance": server.allowance.to_string(),
                    },
                    "venue": "polymarket", "chain_id": 137, "escrow": escrow(),
                    "token": POLYMARKET.token, "symbol": "pUSD", "decimals": 6, "address": owner,
                    "wallet": "0", "available": "0", "committed_wallet": "0",
                    "committed_available": "0", "spendable_wallet": "0",
                    "spendable_available": "0", "block": 1, "observed_at": stamp(observed),
                    "payment_setup": {
                        "address": owner, "authorized": true, "pending": false, "hash": null,
                        "permit": null,
                    },
                }))
            })
            .mount(&harness.server)
            .await;
        let mut intent: Value = fixture("transfer_page")["data"][0].clone();
        for (field, value) in [
            ("from", json!("polymarket")),
            ("from_chain_id", json!(137)),
            ("funding", json!("wallet")),
            ("amount_in", json!("5000000")),
            ("payer", json!(owner)),
            ("conversion", json!({"amount_in": "5002500", "min_out": "5000000"})),
        ] {
            intent[field] = value;
        }
        let intent = serde_json::from_value(intent).unwrap();
        Self { harness, session, intent, server }
    }

    async fn pass(&self, sender: &Sender) -> Result<Progress> {
        convert(&self.session, &self.intent, &terms(), sender).await
    }

    async fn reports(&self) -> Vec<Value> {
        self.harness.bodies("POST", &format!("/v1/transfers/{TRANSFER}/conversion")).await
    }
}

fn word(value: u64) -> [u8; 32] {
    U256::from(value).to_be_bytes::<32>()
}

#[test]
fn terms_follow_the_escrow_floor() {
    let covering = ConversionTerms::covering(amount(5_000_000)).unwrap();
    assert_eq!(covering.amount_in(), amount(5_002_501));
    assert!(covering.valid(), "the most whose floor is min_out");
    let above = ConversionTerms::new(amount(5_002_502), amount(5_000_000));
    assert!(!above.valid(), "one more raises the floor");
    assert!(ConversionTerms::new(amount(10_000), amount(9_995)).valid());
    assert!(!ConversionTerms::new(amount(9_999), amount(9_999)).valid(), "below a cent");
}

#[tokio::test]
async fn an_approval_comes_first_then_the_convert_call_after_its_fence() {
    let mocked = Mocked::new().await;
    let sender = Sender::answering(vec![Ok(APPROVAL_HASH.parse().unwrap())]);

    assert!(matches!(mocked.pass(&sender).await.unwrap(), Progress::Waiting));
    let [approval] = sender.asked().try_into().unwrap();
    assert_eq!(approval.id, format!("lamarge.conversion.{CONVERSION}.approve"));
    assert_eq!((approval.chain_id, approval.to, approval.value), (137, native_usdc(), U256::ZERO));
    let mut expected = APPROVE.to_vec();
    expected.extend_from_slice(&[0; 12]);
    expected.extend_from_slice(escrow().as_slice());
    expected.extend_from_slice(&word(5_002_500));
    assert_eq!(approval.data, expected);

    // The approval is pending: nothing is asked again until the allowance shows it.
    assert!(matches!(mocked.pass(&sender).await.unwrap(), Progress::Waiting));
    assert_eq!(sender.asked().len(), 1);
    assert!(mocked.reports().await.is_empty(), "nothing fenced before the approval lands");

    mocked.server.lock().unwrap().allowance = 5_002_500;
    assert!(matches!(mocked.pass(&sender).await.unwrap(), Progress::Waiting));
    let asked = sender.asked();
    let convert = &asked[1];
    assert_eq!(convert.id, format!("lamarge.conversion.{CONVERSION}.convert"));
    assert_eq!((convert.chain_id, convert.to), (137, escrow()));
    let deadline = u64::try_from(mocked.harness.now().timestamp() + 1_800).unwrap();
    let mut expected = CONVERT.to_vec();
    for value in [5_002_500, 5_000_000, 3, deadline] {
        expected.extend_from_slice(&word(value));
    }
    assert_eq!(convert.data, expected);
    assert_eq!(mocked.reports().await, [json!({}), json!({"tx_hash": HASH})], "fenced first");

    // Sent and reported: later passes only wait, then the escrow event makes it ready.
    assert!(matches!(mocked.pass(&sender).await.unwrap(), Progress::Waiting));
    assert_eq!((sender.asked().len(), mocked.reports().await.len()), (2, 2));
    {
        let mut server = mocked.server.lock().unwrap();
        server.conversion["state"] = json!("ready");
        server.conversion["tx_hash"] = json!(HASH);
        server.conversion["received"] = json!("5000000");
    }
    assert!(matches!(mocked.pass(&sender).await.unwrap(), Progress::Ready));
    assert_eq!(sender.asked().len(), 2);
}

#[tokio::test]
async fn a_declined_convert_is_asked_again_without_a_second_fence() {
    let mocked = Mocked::new().await;
    mocked.server.lock().unwrap().allowance = u64::MAX;
    let sender = Sender::answering(vec![Err("declined")]);

    assert!(matches!(mocked.pass(&sender).await.unwrap(), Progress::NotSent));
    assert_eq!(mocked.reports().await, [json!({})]);
    assert!(matches!(mocked.pass(&sender).await.unwrap(), Progress::Waiting));
    let asked = sender.asked();
    assert_eq!(asked.len(), 2);
    assert_eq!(asked[0], asked[1], "the same transaction, under the same id");
    assert_eq!(mocked.reports().await, [json!({}), json!({"tx_hash": HASH})]);
}

#[tokio::test]
async fn an_unknown_outcome_is_never_asked_again_until_the_sender_knows() {
    let mocked = Mocked::new().await;
    mocked.server.lock().unwrap().allowance = u64::MAX;
    let sender = Sender::answering(vec![Err("lost")]);

    assert!(matches!(mocked.pass(&sender).await.unwrap(), Progress::Waiting));
    assert!(matches!(mocked.pass(&sender).await.unwrap(), Progress::Waiting));
    assert_eq!(sender.asked().len(), 1, "an unknown outcome is waited for");

    *sender.status.lock().unwrap() = Some(SendStatus::Sent(HASH.parse().unwrap()));
    assert!(matches!(mocked.pass(&sender).await.unwrap(), Progress::Waiting));
    assert_eq!(sender.asked().len(), 1);
    assert_eq!(mocked.reports().await, [json!({}), json!({"tx_hash": HASH})], "the hash it knew");
}

#[tokio::test]
async fn a_prepared_conversion_off_the_pins_asks_nothing() {
    let sender = Sender::default();
    let mocked = Mocked::new().await;
    mocked.server.lock().unwrap().conversion["converter"] = json!(escrow());
    let Err(Error::Verification(error)) = mocked.pass(&sender).await else { panic!("refused") };
    assert_eq!(error.kind, VerificationKind::Differs);

    let mocked = Mocked::new().await;
    mocked.server.lock().unwrap().balance = 5_002_499;
    let Err(Error::Verification(error)) = mocked.pass(&sender).await else { panic!("refused") };
    assert_eq!(error.kind, VerificationKind::Changed, "the balance no longer covers it");
    assert!(sender.asked().is_empty());
    assert!(mocked.reports().await.is_empty());
}

#[tokio::test]
async fn a_preview_tells_the_transactions_without_asking() {
    let mocked = Mocked::new().await;
    let preview = mocked.session.conversion_preview(&terms()).await.unwrap();
    let seen = (preview.chain_id, preview.approval, preview.transactions, preview.covered);
    assert_eq!(seen, (137, true, 2, true));
    mocked.server.lock().unwrap().allowance = u64::MAX;
    let preview = mocked.session.conversion_preview(&terms()).await.unwrap();
    assert_eq!((preview.approval, preview.transactions), (false, 1));
}

#[tokio::test]
async fn a_conversion_needs_a_sender_and_a_wallet_transfer_from_polymarket() {
    let mocked = Mocked::new().await;
    let owner = mocked.harness.address();
    let trade = |from: VenueId, funding: Funding| {
        let (amount_in, min_out) = (amount(5_000_000), amount(4_990_000));
        TradeTerms::new(from, VenueId::Hyperliquid, amount_in, min_out, Recipient::Account(owner))
            .funding(funding)
    };
    let request = |terms: TradeTerms| {
        TransferRequest::new(ClientId::new("move-1").unwrap(), terms).conversion(self::terms())
    };

    let wallet = request(trade(VenueId::Polymarket, Funding::Wallet));
    let error = mocked.session.transfer(&wallet).await.unwrap_err();
    assert!(matches!(error, Error::TransactionSenderRequired), "{error:?}");
    let sender = Sender::default();
    let refused =
        [trade(VenueId::Polymarket, Funding::Venue), trade(VenueId::Hyperliquid, Funding::Wallet)];
    for terms in refused {
        let error = mocked.session.transfer_with(&request(terms), &sender).await.unwrap_err();
        assert!(matches!(error, Error::InvalidRequest(_)), "{error:?}");
    }
    assert!(mocked.harness.received_only_auth().await, "refused before any request");
}

#[tokio::test]
async fn a_transfer_with_a_sender_posts_its_conversion_and_converts_first() {
    let mocked = Mocked::new().await;
    mocked.server.lock().unwrap().allowance = u64::MAX;
    let owner = mocked.harness.address();
    let mut intent: Value = fixture("transfer_page")["data"][0].clone();
    for (field, value) in [
        ("client_transfer_id", json!("move-1")),
        ("from", json!("polymarket")),
        ("to", json!("hyperliquid")),
        ("from_chain_id", json!(137)),
        ("to_chain_id", json!(999)),
        ("funding", json!("wallet")),
        ("destination", json!("venue")),
        ("amount_in", json!("5000000")),
        ("min_out", json!("4990000")),
        ("max_parts", json!(8)),
        ("max_fee_bps", Value::Null),
        ("from_dex", Value::Null),
        ("then", Value::Null),
        ("move_id", Value::Null),
        ("previous_transfer_id", Value::Null),
        ("cancelled_at", Value::Null),
        ("take_id", Value::Null),
        ("withdrawal", Value::Null),
        ("withdrawal_preparation", Value::Null),
        ("funding_plan", Value::Null),
        ("funding_plan_error", Value::Null),
        ("payer", json!(owner)),
        ("recipient", json!(owner)),
        ("conversion", json!({"amount_in": "5002500", "min_out": "5000000", "id": CONVERSION})),
    ] {
        intent[field] = value;
    }
    mocked.harness.mount("POST", "/v1/transfers", vec![ok(intent.clone())]).await;
    mocked.harness.mount("GET", &format!("/v1/transfers/{TRANSFER}"), vec![ok(intent)]).await;

    let trade = TradeTerms::new(
        VenueId::Polymarket,
        VenueId::Hyperliquid,
        amount(5_000_000),
        amount(4_990_000),
        Recipient::Account(owner),
    )
    .funding(Funding::Wallet);
    let request = TransferRequest::new(ClientId::new("move-1").unwrap(), trade)
        .conversion(terms())
        .arrival_timeout(std::time::Duration::ZERO);
    let sender = Sender::default();
    let outcome = mocked.session.transfer_with(&request, &sender).await.unwrap();
    let crate::flows::TransferOutcome::Pending { reason, .. } = outcome else { panic!("pending") };
    assert_eq!(reason, crate::flows::PendingReason::ConversionUnconfirmed);
    assert_eq!(sender.asked().len(), 1, "the convert call, the allowance being enough");
    let [posted] = mocked.harness.bodies("POST", "/v1/transfers").await.try_into().unwrap();
    assert_eq!(posted["conversion"], json!({"amount_in": "5002500", "min_out": "5000000"}));
    assert_eq!(posted["funding"], "wallet");
}
