//! Move flow tests, ported from `sdk/src/move.test.ts` and the journalled bridge of
//! `sdk/src/bridge.test.ts` (each test names its source), on a stateful mock of moves and bridges
//! over Relay's saved orders, with the transfer flow's mock API behind it.

use std::sync::{Arc, Mutex};

use alloy_primitives::{Address, B256, U256};
use serde_json::{Value, json};
use wiremock::{Mock, Request, ResponseTemplate, matchers::path_regex};

use super::*;
use crate::{
    SendFuture, SendStatus, StatusFuture, Transaction,
    error::VerificationKind,
    flows::transfer::tests::simulated,
    model::MoveEndpoint,
    protocol::{
        gmx,
        relay::{bridge_authorization, bridge_send_asset, relay_order_id},
    },
    store::{Journal, MemoryStore},
    testing::{NOW, harness::*, json::fixture, time},
    types::EscrowEnvironment,
};

const VECTORS: &str =
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/vectors/relay.json"));
const MOVE: &str = "0192f1c2-3d4e-7f00-8a1b-2c3d4e5f6a60";
/// The transfer the transfer mock serves.
const T1: &str = "0192f1c2-3d4e-7f00-8a1b-2c3d4e5f6a10";
/// The GMX withdrawal.
const DIRECT: &str = "0192f1c2-3d4e-7f00-8a1b-2c3d4e5f6a90";
const ARBITRUM: u64 = 42_161;
const ARBITRUM_USDC: &str = "0xaf88d065e77c8cc2239327c5edb3a432268e5831";
const HASH: B256 = B256::repeat_byte(0x22);
const HUNDRED: u64 = 100_000_000;

/// The vector `name`'s request and order, paid by `payer`.
fn case(name: &str, payer: Address) -> Value {
    let vectors: Value = serde_json::from_str(VECTORS).unwrap();
    let dead = vectors["payer"].as_str().unwrap();
    let text = vectors["cases"][name]
        .to_string()
        .replace(&dead.to_lowercase(), &format!("{payer:#x}"))
        .replace(dead, &format!("{payer:#x}"));
    serde_json::from_str(&text).unwrap()
}

/// What the server holds of the move and its bridges.
struct Api {
    request: Value,
    quote: Value,
    /// Relay's order every bridge serves.
    order: Value,
    bridges: Vec<Value>,
    transfers: Vec<Value>,
    closed: Option<Value>,
    /// Answers to bridge creations, in turn, before any is created.
    refusals: Vec<ResponseTemplate>,
    balance: U256,
    allowance: U256,
    /// Another store holds every fence.
    fenced_elsewhere: bool,
    /// The GMX withdrawal, once created.
    direct: Option<Value>,
}

impl Api {
    fn view(&self) -> Value {
        let mut view = fixture("move");
        for field in ["client_move_id", "from", "to", "amount_in", "min_out", "payer", "recipient"]
        {
            view[field] = self.request[field].clone();
        }
        view["id"] = json!(MOVE);
        view["max_fee_bps"] = self.request.get("max_fee_bps").cloned().unwrap_or(Value::Null);
        view["transfers"] = json!(self.transfers);
        view["bridges"] = json!(self.bridges);
        let closed = self.closed.as_ref();
        view["closed_at"] = closed.map_or(Value::Null, |_| json!(time(0)));
        view["unallocated"] = closed.map_or(Value::Null, |body| body["unallocated"].clone());
        view["unallocated_reason"] =
            closed.and_then(|body| body.get("unallocated_reason").cloned()).unwrap_or(Value::Null);
        view["state"] = json!(if closed.is_some() { "settling" } else { "open" });
        view
    }

    fn bridge(&mut self, body: &Value) -> Value {
        if let Some(found) = self
            .bridges
            .iter()
            .find(|bridge| bridge["client_bridge_id"] == body["client_bridge_id"])
        {
            return found.clone();
        }
        let from = match body.get("from_wallet") {
            Some(wallet) => {
                json!({"venue": "wallet", "chain_id": wallet["chain_id"], "token": wallet["token"]})
            }
            None => self.request["from"].clone(),
        };
        let hypercore = from["venue"] == "hyperliquid";
        let mut bridge = fixture("move/bridges/0");
        bridge["id"] = json!(format!("0192f1c2-3d4e-7f00-8a1b-{:012}", self.bridges.len() + 1));
        bridge["move_id"] = json!(MOVE);
        bridge["client_bridge_id"] = body["client_bridge_id"].clone();
        bridge["amount_in"] = body["amount_in"].clone();
        bridge["min_out"] = self.request["min_out"].clone();
        bridge["from"] = from;
        bridge["to"] = self.request["to"].clone();
        bridge["payer"] = self.request["payer"].clone();
        bridge["recipient"] = self.request["recipient"].clone();
        bridge["state"] = json!("prepared");
        bridge["nonce"] = if hypercore { json!((NOW * 1000).to_string()) } else { Value::Null };
        bridge["order"] = json!({
            "id": relay_order_id(&self.order).unwrap(),
            "data": self.order,
            "deadline": self.order["output"]["deadline"],
        });
        bridge["origin_txs"] = json!([]);
        self.bridges.push(bridge.clone());
        bridge
    }

    fn handle(&mut self, request: &Request) -> ResponseTemplate {
        let path = request.url.path().to_owned();
        let body: Value = request.body_json().unwrap_or(Value::Null);
        let route = format!("{} {path}", request.method.as_str());
        match route.as_str() {
            "GET /v1/moves/quote" => ok(self.quote.clone()),
            "POST /v1/moves" => ok(self.view()),
            _ if route == format!("GET /v1/moves/{MOVE}") => ok(self.view()),
            _ if route == format!("POST /v1/moves/{MOVE}/close") => {
                self.closed = Some(body);
                ok(self.view())
            }
            _ if route == format!("POST /v1/moves/{MOVE}/bridges") => {
                let known = self
                    .bridges
                    .iter()
                    .any(|bridge| bridge["client_bridge_id"] == body["client_bridge_id"]);
                if !known && !self.refusals.is_empty() {
                    return self.refusals.remove(0);
                }
                ok(self.bridge(&body))
            }
            _ if path.starts_with("/v1/direct") => {
                self.direct_route(request.method.as_str(), &path, &body)
            }
            _ => self.bridge_route(request.method.as_str(), &path, &body),
        }
    }

    /// A GMX withdrawal: created once, fenced by an empty report, confirmed by its hash.
    fn direct_route(&mut self, verb: &str, path: &str, body: &Value) -> ResponseTemplate {
        let escrow = deployments::contract(&VenueId::Gmx, EscrowEnvironment::Production).unwrap();
        match (verb, path) {
            ("POST", "/v1/direct") => {
                let created = self.direct.get_or_insert_with(|| {
                    let mut operation = body.clone();
                    operation["id"] = json!(DIRECT);
                    operation["chain_id"] = json!(ARBITRUM);
                    operation["escrow"] = json!(escrow);
                    operation["nonce"] = json!("0");
                    operation["deadline"] = json!((NOW + 600).to_string());
                    operation["state"] = json!("prepared");
                    operation["attempts"] = json!([]);
                    operation["tx_hash"] = Value::Null;
                    operation["received"] = Value::Null;
                    operation
                });
                return ok(created.clone());
            }
            ("GET", _) if path.ends_with("/funding") => {
                return ok(json!({
                    "owner": self.request["payer"], "token": ARBITRUM_USDC, "spender": escrow,
                    "balance": self.balance.to_string(), "allowance": "0", "block": 1,
                    "observed_at": time(0),
                }));
            }
            _ => {}
        }
        let Some(operation) = self.direct.as_mut().filter(|_| path.ends_with(DIRECT)) else {
            return api_error(404, "NOT_FOUND");
        };
        if verb == "POST" {
            if let Some(hash) = body.get("tx_hash") {
                operation["attempts"].as_array_mut().unwrap().push(hash.clone());
                operation["tx_hash"] = hash.clone();
                operation["received"] = operation["amount"].clone();
                operation["state"] = json!("confirmed");
            } else {
                operation["state"] = json!("pending");
            }
        }
        ok(operation.clone())
    }

    fn bridge_route(&mut self, verb: &str, path: &str, body: &Value) -> ResponseTemplate {
        let Some(rest) = path.strip_prefix("/v1/bridges/") else {
            return api_error(404, "NOT_FOUND");
        };
        let (id, action) = rest.split_once('/').unwrap_or((rest, ""));
        let Some(at) = self.bridges.iter().position(|bridge| bridge["id"] == id) else {
            return api_error(404, "NOT_FOUND");
        };
        match (verb, action) {
            ("GET", "") => {}
            ("GET", "funding") => {
                return ok(json!({
                    "owner": self.request["payer"], "token": ARBITRUM_USDC,
                    "spender": format!("{:#x}", relay::DEPOSITORY),
                    "balance": self.balance.to_string(), "allowance": self.allowance.to_string(),
                    "block": 1, "observed_at": time(0),
                }));
            }
            ("POST", "signatures") => self.bridges[at]["state"] = json!("submitting"),
            ("POST", "transactions") => {
                let bridge = &mut self.bridges[at];
                if let Some(hash) = body.get("tx_hash") {
                    bridge["origin_txs"].as_array_mut().unwrap().push(hash.clone());
                    bridge["state"] = json!("pending");
                } else if body.get("unsent").is_some() {
                    bridge["state"] = json!("failed");
                } else if bridge["state"] == "prepared" {
                    bridge["state"] = json!("submitting");
                    if self.fenced_elsewhere {
                        return ResponseTemplate::new(409).set_body_json(json!({"error": {
                            "code": "STATE_CONFLICT", "message": "fenced", "status": 409,
                            "reason": "fenced",
                        }}));
                    }
                } else {
                    return api_error(409, "STATE_CONFLICT");
                }
            }
            _ => return api_error(404, "NOT_FOUND"),
        }
        ok(self.bridges[at].clone())
    }
}

/// The host's wallet: what it is asked, and what it answers.
#[derive(Default)]
struct Wallet {
    sent: Mutex<Vec<Transaction>>,
    /// The deposit's outcome is lost: maybe sent, unknown to the SDK.
    lost: Mutex<bool>,
    /// Deposits declined before broadcast, in turn.
    declines: Mutex<usize>,
    /// GMX `transferOut`s declined before broadcast, in turn.
    declined_out: Mutex<usize>,
    /// What `status` answers.
    status: Mutex<Option<SendStatus>>,
    /// An approval raises the allowance there.
    api: Mutex<Option<Arc<Mutex<Api>>>>,
}

impl TransactionSender for Wallet {
    fn send(&self, transaction: Transaction) -> SendFuture<'_> {
        Box::pin(async move {
            let deposit = transaction.to == relay::DEPOSITORY;
            if !deposit && let Some(api) = self.api.lock().unwrap().as_ref() {
                api.lock().unwrap().allowance = U256::MAX;
            }
            let transfer_out = transaction.to == gmx::TRANSFER_ROUTER;
            self.sent.lock().unwrap().push(transaction);
            let mut declined_out = self.declined_out.lock().unwrap();
            if transfer_out && *declined_out > 0 {
                *declined_out -= 1;
                return Err(crate::SendError::NotSent("declined".into()));
            }
            drop(declined_out);
            let mut declines = self.declines.lock().unwrap();
            if deposit && *declines > 0 {
                *declines -= 1;
                return Err(crate::SendError::NotSent("declined".into()));
            }
            drop(declines);
            if deposit && *self.lost.lock().unwrap() {
                return Err(crate::SendError::Unknown("lost".into()));
            }
            Ok(HASH)
        })
    }

    fn status<'a>(&'a self, _: &'a str) -> StatusFuture<'a> {
        Box::pin(async move { Ok(self.status.lock().unwrap().unwrap_or(SendStatus::Unknown)) })
    }
}

/// `move.test.ts`'s `quote()`.
fn quote(lamarge: Option<Value>, bridge: Option<u64>, rest: u64, reason: Option<&str>) -> Value {
    let unavailable = if lamarge.is_some() { Value::Null } else { json!("no_liquidity") };
    json!({
        "lamarge_unavailable": unavailable,
        "lamarge": lamarge.unwrap_or(Value::Null),
        "bridge": bridge.map(|amount| json!({
            "provider": "relay", "amount_in": amount.to_string(), "amount_out": amount.to_string(),
            "minimum_out": amount.to_string(), "account_credit": null, "wallet_surplus": null,
            "eta_seconds": 10,
        })),
        "bridge_unavailable": reason,
        "unallocated": rest.to_string(),
        "unallocated_reason": reason,
    })
}

/// LaMarge's part the transfer mock fills: 1 USDC from Hyperliquid to Polymarket.
fn one_take() -> Value {
    let leg = json!({"from": "hyperliquid", "to": "polymarket", "amount_in": "1000000",
                     "amount_out": "1005000", "parts": 1, "taker_fee": "0", "taker_fee_bps": 0});
    json!({"budget": "1000000", "route": {
        "from": "hyperliquid", "to": "polymarket", "amount_in": "1000000", "amount_out": "1005000",
        "capped": false, "destination": "venue", "minimum_received": null, "legs": [leg],
    }})
}

fn arbitrum_wallet() -> Value {
    json!({"venue": "wallet", "chain_id": ARBITRUM, "token": ARBITRUM_USDC})
}

/// A move on the mock API: the harness, the server's state, the journal and the host's wallet.
struct Scenario {
    harness: Harness,
    api: Arc<Mutex<Api>>,
    journal: Journal,
    wallet: Arc<Wallet>,
    request: MoveFundsRequest,
}

impl Scenario {
    /// A move of `amount_in` for `min_out` from `from` to `to`, to the payer, as `quote` plans it,
    /// each bridge serving the vector `order`'s order; `levels` replaces the transfer mock's book.
    async fn new(
        (from, to): (Value, Value),
        (amount_in, min_out): (u64, u64),
        quote: Value,
        order: &str,
        levels: Option<Value>,
    ) -> Self {
        let harness = simulated(levels).await;
        let me = harness.address();
        let terms = MoveRequest::new(
            ClientId::new("move-a").unwrap(),
            serde_json::from_value(from).unwrap(),
            serde_json::from_value(to).unwrap(),
            Amount::from(amount_in),
            Amount::from(min_out),
            me,
            &MoveRecipient::Address(me),
        );
        let api = Arc::new(Mutex::new(Api {
            request: serde_json::to_value(&terms).unwrap(),
            quote,
            order: case(order, me)["order"].clone(),
            bridges: Vec::new(),
            transfers: Vec::new(),
            closed: None,
            refusals: Vec::new(),
            balance: U256::from(HUNDRED),
            allowance: U256::ZERO,
            fenced_elsewhere: false,
            direct: None,
        }));
        let shared = Arc::clone(&api);
        Mock::given(path_regex(r"^/v1/(moves|bridges|direct)"))
            .respond_with(move |request: &Request| shared.lock().unwrap().handle(request))
            .with_priority(1)
            .mount(&harness.server)
            .await;
        let wallet = Arc::new(Wallet::default());
        *wallet.api.lock().unwrap() = Some(Arc::clone(&api));
        let request = MoveFundsRequest::new(terms, MoveRecipient::Address(me))
            .unwrap()
            .poll_interval(Duration::from_millis(1))
            .arrival_timeout(Duration::from_millis(50));
        Self { harness, api, journal: Journal::new(MemoryStore::new()), wallet, request }
    }

    /// From `HyperCore` to the payer's Arbitrum USDC, 100 USDC for at least 90.
    async fn hypercore(quote: Value) -> Self {
        let from = json!({"venue": "hyperliquid"});
        let amounts = (HUNDRED, 90_000_000);
        Self::new((from, arbitrum_wallet()), amounts, quote, "hyperliquid_to_arbitrum", None).await
    }

    /// From the payer's Arbitrum USDC to Hyperliquid, 100 USDC for at least 90.
    async fn from_wallet(quote: Value) -> Self {
        let to = json!({"venue": "hyperliquid"});
        let amounts = (HUNDRED, 90_000_000);
        Self::new((arbitrum_wallet(), to), amounts, quote, "arbitrum_to_hyperliquid", None).await
    }

    /// From Hyperliquid to Polymarket, 1 USDC through LaMarge and 100 more, for as much.
    async fn lamarge(quote: Value, levels: Option<Value>) -> Self {
        let amount = HUNDRED + 1_000_000;
        let ends = (json!({"venue": "hyperliquid"}), json!({"venue": "polymarket"}));
        Self::new(ends, (amount, amount), quote, "hyperliquid_to_arbitrum", levels).await
    }

    async fn run(&self) -> Result<MoveOutcome> {
        let session = self.harness.session_with(self.journal.clone()).await;
        let sender: &dyn TransactionSender = self.wallet.as_ref();
        session.move_funds_with(&self.request, Some(sender)).await
    }

    fn edit(&self, edit: impl FnOnce(&mut Api)) {
        edit(&mut self.api.lock().unwrap());
    }

    fn closed(&self) -> Option<Value> {
        self.api.lock().unwrap().closed.clone()
    }

    fn bridges(&self) -> Vec<Value> {
        self.api.lock().unwrap().bridges.clone()
    }

    async fn count(&self, verb: &str, route: &str) -> usize {
        self.harness.requests(verb, route).await.len()
    }

    fn sent(&self) -> Vec<Transaction> {
        self.wallet.sent.lock().unwrap().clone()
    }

    async fn budget(&self) -> Value {
        let key = crate::store::scoped_key(
            crate::types::EscrowEnvironment::Production,
            self.harness.address(),
            "move:move-a:parts",
        );
        let bytes = self.journal.load(&key).await.unwrap().unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }
}

fn closed(outcome: Result<MoveOutcome>) -> Move {
    match outcome {
        Ok(MoveOutcome::Closed(found)) => *found,
        other => panic!("not closed: {other:?}"),
    }
}

fn waiting(outcome: Result<MoveOutcome>) -> MoveWait {
    match outcome {
        Ok(MoveOutcome::Pending { reason, .. }) => reason,
        other => panic!("not pending: {other:?}"),
    }
}

fn refusal(reason: &str) -> ResponseTemplate {
    ResponseTemplate::new(400).set_body_json(json!({"error": {
        "code": "VALIDATION_ERROR", "message": "no bridge", "status": 400, "reason": reason,
    }}))
}

fn closing(amount: u64, reason: &str) -> Value {
    json!({"unallocated": amount.to_string(), "unallocated_reason": reason})
}

/// "an endpoint maps to its account's venue, or the escrow venue on its wallet's chain".
#[test]
fn an_endpoint_maps_to_its_accounts_venue_or_the_escrow_venue_on_its_wallets_chain() {
    assert_eq!(move_venue(&MoveEndpoint::account(&VenueId::Lighter)), Some(VenueId::Lighter));
    let arbitrum = MoveEndpoint::wallet(ARBITRUM, ARBITRUM_USDC.parse().unwrap());
    assert_eq!(move_venue(&arbitrum), Some(VenueId::Gmx));
    assert_eq!(move_venue(&MoveEndpoint::wallet(137, Address::ZERO)), Some(VenueId::Polymarket));
    assert_eq!(move_venue(&MoveEndpoint::wallet(56, Address::ZERO)), None);
}

/// "to a Polygon wallet, the take's pUSD floor covers the native-USDC floor after the swap bound".
#[test]
fn a_wallet_delivery_on_polygon_covers_the_floor_after_the_swap_bound() {
    let part = U256::from(99_950_000u64);
    let floor = delivery_floor(&VenueId::Polymarket, part).unwrap();
    assert_eq!(floor, U256::from(100_000_000u64));
    assert!(fees::minimum_received(&VenueId::Polymarket, floor).unwrap() >= part);
    assert_eq!(delivery_floor(&VenueId::Gmx, U256::from(7u8)).unwrap(), U256::from(7u8));
}

/// "a move LaMarge cannot take goes whole through the bridge", from `HyperCore`: both payloads
/// rebuilt from the order and signed by the payer, once.
#[tokio::test]
async fn a_move_lamarge_cannot_take_goes_whole_through_a_hypercore_bridge() {
    let s = Scenario::hypercore(quote(None, Some(HUNDRED), 0, None)).await;
    let me = s.harness.address();
    let found = closed(s.run().await);
    assert_eq!(found.unallocated, Some(Amount::ZERO));
    let [bridge] = s.bridges().try_into().unwrap();
    assert_eq!(bridge["client_bridge_id"], format!("{MOVE}.b"));
    assert_eq!(bridge["state"], "submitting");
    assert_eq!(s.closed().unwrap(), json!({"unallocated": "0"}));

    let route = format!("/v1/bridges/{}/signatures", bridge["id"].as_str().unwrap());
    let [body] = s.harness.bodies("POST", &route).await.try_into().unwrap();
    let order: B256 = bridge["order"]["id"].as_str().unwrap().parse().unwrap();
    let nonce = u64::try_from(NOW * 1000).unwrap();
    let mapping = bridge_authorization(order, me, nonce);
    let send = bridge_send_asset(U256::from(HUNDRED), nonce).typed_data();
    for (field, typed) in [("authorization", mapping), ("send_asset", send)] {
        let item = json!({"signature": body[field]});
        assert_eq!(signer_of(&item, &typed.eip712_signing_hash().unwrap()), me, "{field}");
    }
    assert_eq!(s.harness.flow_signatures(), 2);
}

/// "a send declined after the mapping asks only for the send again"; "stored `HyperCore`
/// signatures are submitted again, never signed again".
#[tokio::test]
async fn a_send_declined_after_the_mapping_asks_only_for_the_send_again() {
    let s = Scenario::hypercore(quote(None, Some(HUNDRED), 0, None)).await;
    let nonce = u64::try_from(NOW * 1000).unwrap();
    let send = bridge_send_asset(U256::from(HUNDRED), nonce).typed_data();
    s.harness.signer().serve(&serde_json::to_value(&send).unwrap(), "send_asset");
    s.harness.decline("send_asset", 1);
    let sends =
        |s: &Scenario| s.harness.kinds().iter().filter(|kind| **kind == "send_asset").count();
    assert!(s.run().await.is_err());
    assert_eq!(
        (s.harness.flow_signatures(), sends(&s)),
        (2, 0),
        "the mapping, then the send declined"
    );
    let first = s.harness.flow_signatures();
    closed(s.run().await);
    assert_eq!(s.harness.flow_signatures(), first + 1, "only the send is asked again");
    assert_eq!(sends(&s), 1);

    // Closed: a later run signs, creates and closes nothing more.
    closed(s.run().await);
    assert_eq!(s.harness.flow_signatures(), first + 1);
    assert_eq!(s.count("POST", &format!("/v1/moves/{MOVE}/bridges")).await, 1);
    assert_eq!(s.count("POST", &format!("/v1/moves/{MOVE}/close")).await, 1);
}

/// "a wallet bridge approves only when short, fences, then sends and reports its deposit".
#[tokio::test]
async fn a_wallet_bridge_approves_when_short_fences_then_sends_and_reports_its_deposit() {
    let s = Scenario::from_wallet(quote(None, Some(HUNDRED), 0, None)).await;
    closed(s.run().await);
    let [approve, deposit] = s.sent().try_into().unwrap();
    assert_eq!(approve.to, ARBITRUM_USDC.parse::<Address>().unwrap());
    assert_eq!((approve.chain_id, deposit.chain_id), (ARBITRUM, ARBITRUM));
    assert_eq!(deposit.to, relay::DEPOSITORY);
    let [bridge] = s.bridges().try_into().unwrap();
    assert_eq!(bridge["origin_txs"], json!([format!("{HASH:#x}")]));
    let route = format!("/v1/bridges/{}/transactions", bridge["id"].as_str().unwrap());
    let bodies = s.harness.bodies("POST", &route).await;
    assert_eq!(bodies, vec![json!({}), json!({"tx_hash": format!("{HASH:#x}")})]);
    assert_eq!(s.harness.flow_signatures(), 0);

    closed(s.run().await);
    assert_eq!(s.sent().len(), 2, "nothing is sent again");
}

/// "a deposit whose outcome is unknown is never sent again".
#[tokio::test]
async fn a_deposit_of_unknown_outcome_is_never_asked_again() {
    let s = Scenario::from_wallet(quote(None, Some(HUNDRED), 0, None)).await;
    s.edit(|api| api.allowance = U256::MAX);
    *s.wallet.lost.lock().unwrap() = true;
    assert_eq!(waiting(s.run().await), MoveWait::Bridge);
    assert_eq!(waiting(s.run().await), MoveWait::Bridge);
    assert_eq!(s.sent().len(), 1);
    *s.wallet.status.lock().unwrap() = Some(SendStatus::Sent(HASH));
    closed(s.run().await);
    assert_eq!(s.sent().len(), 1);
    let [bridge] = s.bridges().try_into().unwrap();
    assert_eq!(bridge["origin_txs"], json!([format!("{HASH:#x}")]));
}

/// "funds not yet in the wallet wait, without sending a deposit that would revert".
#[tokio::test]
async fn funds_not_yet_in_the_wallet_wait_before_any_approval_or_fence() {
    let s = Scenario::from_wallet(quote(None, Some(HUNDRED), 0, None)).await;
    s.edit(|api| api.balance = U256::from(1u8));
    assert_eq!(waiting(s.run().await), MoveWait::Withdrawal);
    assert!(s.sent().is_empty());
    let [bridge] = s.bridges().try_into().unwrap();
    assert_eq!(bridge["state"], "prepared");
    assert!(s.closed().is_none());
}

/// "a provider that does not answer leaves the move open for a later run; a lasting refusal
/// closes it".
#[tokio::test]
async fn a_provider_that_does_not_answer_leaves_the_move_open_a_lasting_refusal_closes_it() {
    let s = Scenario::hypercore(quote(None, Some(HUNDRED), 0, None)).await;
    s.edit(|api| api.refusals = vec![api_error(503, "VENUE_UNAVAILABLE"), refusal("unsupported")]);
    assert_eq!(waiting(s.run().await), MoveWait::Bridge);
    assert!(s.closed().is_none());
    assert_eq!(s.budget().await["parts"], json!([]), "a bridge never created counts nothing");
    closed(s.run().await);
    assert_eq!(s.closed().unwrap(), closing(HUNDRED, "unsupported"));
    assert_eq!(s.harness.flow_signatures(), 0);
}

/// "what no part carries is closed as unallocated, with its reason", after LaMarge's part: one
/// take attached to the move, never paid twice.
#[tokio::test]
async fn what_no_part_carries_is_closed_as_unallocated_with_its_reason() {
    let quote = quote(Some(one_take()), None, HUNDRED, Some("insufficient_liquidity"));
    let s = Scenario::lamarge(quote, None).await;
    let found = closed(s.run().await);
    assert_eq!(found.unallocated, Some(Amount::from(HUNDRED)));
    assert_eq!(s.closed().unwrap(), closing(HUNDRED, "insufficient_liquidity"));
    assert!(s.bridges().is_empty());
    let [transfer] = s.harness.bodies("POST", "/v1/transfers").await.try_into().unwrap();
    assert_eq!(transfer["move_id"], MOVE);
    assert_eq!(transfer["client_transfer_id"], format!("{MOVE}.t"));
    assert_eq!(transfer["amount_in"], "1000000");
    assert_eq!(transfer["min_out"], "1000000");
    assert_eq!(s.harness.flow_signatures(), 1);

    closed(s.run().await);
    assert_eq!(s.harness.flow_signatures(), 1, "LaMarge is never paid twice");
}

/// "a bridge the preview could not quote is tried after LaMarge's part"; "a bridge the provider
/// has no route for leaves the rest unallocated after LaMarge's part".
#[tokio::test]
async fn a_bridge_the_preview_could_not_quote_is_tried_after_lamarges_part() {
    let quote = quote(Some(one_take()), None, HUNDRED, Some("provider_unavailable"));
    let s = Scenario::lamarge(quote, None).await;
    s.edit(|api| api.refusals = vec![refusal("insufficient_liquidity")]);
    closed(s.run().await);
    assert_eq!(s.closed().unwrap(), closing(HUNDRED, "insufficient_liquidity"));
    let route = format!("/v1/moves/{MOVE}/bridges");
    let [creation] = s.harness.bodies("POST", &route).await.try_into().unwrap();
    let asked = json!({"client_bridge_id": format!("{MOVE}.b"), "amount_in": HUNDRED.to_string()});
    assert_eq!(creation, asked);
    assert_eq!(s.harness.flow_signatures(), 1);
}

/// "an unpaid LaMarge part is bridged from the wallet its withdrawal reached; nothing more leaves
/// the origin": a part no book pays, never funded, is cancelled and frees its share.
#[tokio::test]
async fn an_unpaid_lamarge_part_frees_its_share_for_the_bridge() {
    let s =
        Scenario::lamarge(quote(Some(one_take()), Some(HUNDRED), 0, None), Some(json!([]))).await;
    s.edit(|api| {
        api.transfers = vec![json!({"id": T1, "final": false})];
        api.refusals = vec![refusal("unsupported")];
    });
    closed(s.run().await);
    assert_eq!(s.count("DELETE", &format!("/v1/transfers/{T1}")).await, 1);
    let route = format!("/v1/moves/{MOVE}/bridges");
    let [creation] = s.harness.bodies("POST", &route).await.try_into().unwrap();
    let all = (HUNDRED + 1_000_000).to_string();
    assert_eq!(creation["amount_in"], all, "LaMarge's freed share goes to the bridge too");
    assert_eq!(s.closed().unwrap(), closing(HUNDRED + 1_000_000, "unsupported"));
    assert_eq!(s.harness.flow_signatures(), 0);
    let parts = s.budget().await["parts"].clone();
    assert_eq!(parts[0]["name"], "lamarge");
    assert_eq!(parts[0]["amount"], "0");
}

/// "from a unified Hyperliquid account, a refused LaMarge take leaves its share unallocated,
/// never bridged from the origin".
#[tokio::test]
async fn no_bridge_leaves_a_blocked_origin_its_rest_unallocated() {
    let s = Scenario::hypercore(quote(None, None, HUNDRED, Some("hyperliquid_mode"))).await;
    closed(s.run().await);
    assert_eq!(s.closed().unwrap(), closing(HUNDRED, "hyperliquid_mode"));
    assert_eq!(s.count("POST", &format!("/v1/moves/{MOVE}/bridges")).await, 0);
}

/// No bridge leaves a venue settled inside itself (Lighter, Aster): the rest is unallocated.
#[tokio::test]
async fn a_venue_settled_inside_itself_leaves_the_rest_unallocated() {
    let ends = (json!({"venue": "aster"}), json!({"venue": "hyperliquid"}));
    let quote = quote(None, Some(HUNDRED), 0, None);
    let s = Scenario::new(ends, (HUNDRED, HUNDRED), quote, "arbitrum_to_hyperliquid", None).await;
    closed(s.run().await);
    assert_eq!(s.closed().unwrap(), closing(HUNDRED, "unsupported"));
    assert_eq!(s.count("POST", &format!("/v1/moves/{MOVE}/bridges")).await, 0);
}

/// "concurrent first reads of a move's plan share one quote, and other terms under its id are
/// refused".
#[tokio::test]
async fn concurrent_first_reads_share_one_quote_and_other_terms_are_refused() {
    let s = Scenario::hypercore(quote(None, Some(HUNDRED), 0, None)).await;
    let session = s.harness.session_with(s.journal.clone()).await;
    let (a, b) = tokio::join!(session.move_plan(&s.request), session.move_plan(&s.request));
    assert_eq!(a.unwrap(), b.unwrap());
    assert_eq!(s.count("GET", "/v1/moves/quote").await, 1);
    let me = s.harness.address();
    let terms = &s.request.terms;
    let other = MoveRequest::new(
        ClientId::new("move-a").unwrap(),
        terms.from.clone(),
        terms.to.clone(),
        Amount::from(HUNDRED + 1),
        terms.min_out,
        me,
        &MoveRecipient::Address(me),
    );
    let other = MoveFundsRequest::new(other, MoveRecipient::Address(me)).unwrap();
    assert!(matches!(session.move_plan(&other).await, Err(Error::InvalidRequest(_))));
}

/// A quote that does not add up to the move's `amount_in` creates nothing.
#[tokio::test]
async fn a_quote_that_does_not_add_up_creates_nothing() {
    let s = Scenario::hypercore(quote(None, Some(HUNDRED - 1), 0, None)).await;
    let Err(Error::Verification(error)) = s.run().await else { panic!("refused") };
    assert_eq!((error.kind, error.detail), (VerificationKind::Differs, details::MOVE_PLAN));
    assert_eq!(s.count("POST", "/v1/moves").await, 0);
}

/// A move answered with other terms goes no further.
#[tokio::test]
async fn a_move_answered_with_other_terms_goes_no_further() {
    let s = Scenario::hypercore(quote(None, Some(HUNDRED), 0, None)).await;
    s.edit(|api| api.request["min_out"] = json!("1"));
    let Err(Error::Verification(error)) = s.run().await else { panic!("refused") };
    assert_eq!((error.kind, error.detail), (VerificationKind::Changed, details::MOVE));
    assert!(s.bridges().is_empty());
}

/// "a Polymarket origin withdraws to the signer under the move, then bridges from that wallet",
/// from GMX: the payer's own `transferOut`, fenced and reported, then a bridge from its Arbitrum
/// wallet; a later run sends nothing again.
#[tokio::test]
async fn a_gmx_origin_sends_its_transfer_out_then_bridges_from_the_wallet() {
    let ends = (json!({"venue": "gmx"}), json!({"venue": "hyperliquid"}));
    let quote = quote(None, Some(HUNDRED), 0, None);
    let s =
        Scenario::new(ends, (HUNDRED, 90_000_000), quote, "arbitrum_to_hyperliquid", None).await;
    let me = s.harness.address();
    closed(s.run().await);
    assert_eq!(s.closed().unwrap(), json!({"unallocated": "0"}));

    let [created] = s.harness.bodies("POST", "/v1/direct").await.try_into().unwrap();
    assert_eq!(created["client_operation_id"], format!("{MOVE}.g"));
    assert_eq!(created["move_id"], MOVE);
    assert_eq!(
        (created["kind"].as_str(), created["venue"].as_str()),
        (Some("withdrawal"), Some("gmx"))
    );
    assert_eq!(created["owner"], json!(me));
    assert_eq!(created["recipient"], json!(me));
    assert_eq!(created["amount"], HUNDRED.to_string());
    let route = format!("/v1/direct/{DIRECT}");
    let reports = s.harness.bodies("POST", &route).await;
    assert_eq!(reports, vec![json!({}), json!({"tx_hash": format!("{HASH:#x}")})]);

    let [transfer_out, deposit] = s.sent().try_into().unwrap();
    assert_eq!((transfer_out.chain_id, transfer_out.to), (ARBITRUM, gmx::TRANSFER_ROUTER));
    assert_eq!(transfer_out.data, gmx::transfer_out(U256::from(HUNDRED)).to_vec());
    assert_eq!(deposit.to, relay::DEPOSITORY);
    let route = format!("/v1/moves/{MOVE}/bridges");
    let [bridge] = s.harness.bodies("POST", &route).await.try_into().unwrap();
    let wallet = json!({"chain_id": ARBITRUM, "token": ARBITRUM_USDC});
    assert_eq!(bridge["from_wallet"], wallet, "from the wallet the transferOut reached");

    closed(s.run().await);
    assert_eq!(s.sent().len(), 2, "nothing is sent again");
}

/// "a bridge beyond the move's amount is refused before it is quoted"; "a stuck bridge still
/// counts against the move's amount": every part counts until it is final with nothing debited,
/// and a wallet part only within what the move put there.
#[tokio::test]
async fn the_budget_counts_every_part_not_final_without_a_debit() {
    let s = Scenario::hypercore(quote(None, Some(HUNDRED), 0, None)).await;
    let session = s.harness.session_with(s.journal.clone()).await;
    let terms = &s.request.terms;
    let part = |name: &str, amount: u64, from| Part::new(name, Amount::from(amount), from);
    let half = HUNDRED / 2;
    budget::reserve(&session, terms, part("a", half, Source::Origin)).await.unwrap();
    budget::reserve(&session, terms, part("a", half, Source::Origin)).await.unwrap();
    budget::reserve(&session, terms, part("b", half, Source::Origin)).await.unwrap();
    let over = budget::reserve(&session, terms, part("c", 1, Source::Origin)).await;
    assert!(matches!(over, Err(Error::InvalidRequest(_))));
    budget::amend(&session, terms, "b", Amend::Release).await.unwrap();
    budget::reserve(&session, terms, part("c", 1, Source::Origin)).await.unwrap();
    let wallet = budget::reserve(&session, terms, part("w", 1, Source::Wallet)).await;
    assert!(matches!(wallet, Err(Error::InvalidRequest(_))));
    budget::amend(&session, terms, "a", Amend::Credit(Amount::from(5u64))).await.unwrap();
    budget::reserve(&session, terms, part("w", 5, Source::Wallet)).await.unwrap();
}

/// "a wallet refusal before broadcast reports the deposit unsent and frees the budget": the run
/// ends there, the move open; the next run asks once more, through a new bridge.
#[tokio::test]
async fn a_deposit_declined_before_broadcast_ends_the_run_and_the_next_asks_once_more() {
    let s = Scenario::from_wallet(quote(None, Some(HUNDRED), 0, None)).await;
    s.edit(|api| api.allowance = U256::MAX);
    *s.wallet.declines.lock().unwrap() = 1;
    assert_eq!(waiting(s.run().await), MoveWait::NotSent);
    assert_eq!(s.sent().len(), 1, "asked once");
    assert!(s.closed().is_none());
    let [bridge] = s.bridges().try_into().unwrap();
    assert_eq!(bridge["state"], "failed");
    let route = format!("/v1/bridges/{}/transactions", bridge["id"].as_str().unwrap());
    let bodies = s.harness.bodies("POST", &route).await;
    assert_eq!(bodies, vec![json!({}), json!({"unsent": true})]);

    closed(s.run().await);
    assert_eq!(s.sent().len(), 2);
    let [failed, sent] = s.bridges().try_into().unwrap();
    assert_eq!(failed["client_bridge_id"], format!("{MOVE}.b"));
    assert_eq!(sent["client_bridge_id"], format!("{MOVE}.b.1"));
    assert_eq!(s.closed().unwrap(), json!({"unallocated": "0"}));
}

/// "two stores racing for one deposit: only the store holding the fence sends": one that lost
/// the fence never prompts, nor says the deposit unsent.
#[tokio::test]
async fn a_store_that_lost_the_fence_never_sends_nor_says_unsent() {
    let s = Scenario::from_wallet(quote(None, Some(HUNDRED), 0, None)).await;
    s.edit(|api| {
        api.allowance = U256::MAX;
        api.fenced_elsewhere = true;
    });
    *s.wallet.status.lock().unwrap() = Some(SendStatus::NotSent);
    assert_eq!(waiting(s.run().await), MoveWait::Bridge);
    closed(s.run().await);
    assert!(s.sent().is_empty());
    let [bridge] = s.bridges().try_into().unwrap();
    assert_eq!(bridge["state"], "submitting");
    let route = format!("/v1/bridges/{}/transactions", bridge["id"].as_str().unwrap());
    assert_eq!(s.harness.bodies("POST", &route).await, vec![json!({})]);
}

/// "A GMX withdrawal has no application nonce; only its first fenced prompt may send it": one
/// declined ends the run and is never asked again; once it expires its part is released and the
/// rest closed unallocated.
#[tokio::test]
async fn a_declined_gmx_withdrawal_is_never_asked_again_and_released_once_expired() {
    let ends = (json!({"venue": "gmx"}), json!({"venue": "hyperliquid"}));
    let quote = quote(None, Some(HUNDRED), 0, None);
    let s =
        Scenario::new(ends, (HUNDRED, 90_000_000), quote, "arbitrum_to_hyperliquid", None).await;
    *s.wallet.declined_out.lock().unwrap() = 1;
    assert_eq!(waiting(s.run().await), MoveWait::NotSent);
    assert_eq!(waiting(s.run().await), MoveWait::Withdrawal);
    assert_eq!(s.sent().len(), 1, "never asked again");
    s.edit(|api| api.direct.as_mut().unwrap()["state"] = json!("expired"));
    closed(s.run().await);
    assert_eq!(s.closed().unwrap(), closing(HUNDRED, "withdrawal_refused"));
    assert!(s.bridges().is_empty());
    assert_eq!(s.sent().len(), 1);
}
