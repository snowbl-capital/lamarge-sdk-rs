use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use serde_json::{Value, json};
use wiremock::{
    Mock, Request, ResponseTemplate,
    matchers::{method, path, path_regex},
};

use super::*;
use crate::{
    error::VerificationKind,
    store::{Journal, MemoryStore},
    testing::{
        IDENTITY, MAKER, NOW, OPERATOR, OTHER, OWNER, POOL, edited, fixtures, harness::*, sample,
    },
    types::EscrowEnvironment,
};

const PREPARATION: &str = "0192f1c2-3d4e-7f00-8a1b-2c3d4e5f6a40";
const TEN: &str = "10000000";

fn identity() -> QuoteIdentity {
    QuoteIdentity::new(IDENTITY.parse().unwrap(), MAKER)
}

fn request(venue: &VenueId, funding: Funding) -> CapitalRequest {
    let id = ClientId::new("request-1").unwrap();
    CapitalRequest::new(id, venue.clone(), Amount::from(10_000_000u64), funding)
}

fn submit_path() -> String {
    format!("/v1/maker/preparations/{PREPARATION}/signatures")
}

/// `prepared` (a Hyperliquid fixture) moved to `venue`: its pool and its `FundOffer`'s domain.
fn on(venue: &VenueId, mut prepared: Value) -> Value {
    let chain = deployments::chain(venue).unwrap();
    let escrow = deployments::escrow(chain.id, EscrowEnvironment::Production).unwrap();
    prepared["venue"] = json!(venue);
    prepared["order"]["venue"] = json!(venue);
    for item in prepared["to_sign"].as_array_mut().unwrap() {
        if item["kind"] == "fund" {
            item["typed_data"]["domain"]["chainId"] = json!(chain.id);
            item["typed_data"]["domain"]["verifyingContract"] = json!(escrow);
        }
    }
    prepared
}

/// [`fixtures::margin_preparation`] on Polymarket: the margin move is a Polymarket withdrawal.
fn polymarket_margin() -> Value {
    let mut withdraw = sample("polymarket_withdraw");
    withdraw["message"]["account"] = json!(OWNER);
    withdraw["message"]["to"] = json!(OWNER);
    withdraw["message"]["amount"] = json!(TEN);
    withdraw["message"]["ts"] = json!(NOW);
    let mut prepared = on(&VenueId::Polymarket, fixtures::margin_preparation());
    prepared["to_sign"][0]["typed_data"] = withdraw;
    prepared
}

/// [`fixtures::pool`] live with 3 USDC available at sequence 4, and a preparation topping it up.
fn top_up() -> (Value, Value) {
    let live = edited(fixtures::pool(), |order| {
        order["state"] = json!("live");
        order["available_to_quote"] = json!("3000000");
        order["last_sequence"] = json!(4);
        order["revision"] = json!("7");
    });
    let prepared = edited(fixtures::maker_preparation(), |p| {
        p["order"] = live.clone();
        p["expected_revision"] = json!("7");
        p["to_sign"] = json!([fixtures::fund_item(TEN, TEN, 5)]);
        p["funding"]["existing_available"] = json!("3000000");
    });
    (live, prepared)
}

/// `prepared`, accepted.
fn accepted(prepared: &Value) -> Value {
    edited(prepared.clone(), |p| {
        p["state"] = json!("accepted");
        p["order"]["state"] = json!("funding");
        p["order"]["preparation_id"] = json!(PREPARATION);
    })
}

/// Serves the session's balances, `orders`, `prepared` (owned) for its venue, and accepts its
/// signatures.
async fn serve(harness: &Harness, orders: &[Value], prepared: &Value) {
    let prepared = harness.owned(prepared);
    let venue = prepared["venue"].as_str().unwrap().to_owned();
    let rows: Vec<Value> = ["hyperliquid", "polymarket", "gmx"]
        .into_iter()
        .map(|venue| balance(venue, harness.address(), None))
        .collect();
    harness.mount("GET", "/v1/balances", vec![ok(json!(rows))]).await;
    let orders: Vec<Value> = orders.iter().map(|order| harness.owned(order)).collect();
    harness.mount("GET", "/v1/orders", vec![ok(json!(orders))]).await;
    harness.mount("POST", &format!("/v1/maker/{venue}/prepare"), vec![ok(prepared.clone())]).await;
    harness.mount("POST", &submit_path(), vec![ok(accepted(&prepared))]).await;
}

/// Answers the first submission with `503`.
async fn lose_first_submission(harness: &Harness) {
    Mock::given(method("POST"))
        .and(path(submit_path()))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&harness.server)
        .await;
}

async fn record(session: &Session) -> Option<Vec<u8>> {
    session.journal().load(&session.record_key(&format!("maker:{PREPARATION}"))).await.unwrap()
}

/// Each source and venue: one signature per item, in order, by the session's address over the
/// served typed data, nothing left in the journal.
#[tokio::test]
async fn capital_is_checked_signed_and_submitted() {
    let (live, topped) = top_up();
    let cases = [
        (VenueId::Hyperliquid, Funding::Wallet, vec![], fixtures::maker_preparation()),
        (VenueId::Gmx, Funding::Wallet, vec![], on(&VenueId::Gmx, fixtures::maker_preparation())),
        (VenueId::Hyperliquid, Funding::Wallet, vec![live], topped),
        (VenueId::Hyperliquid, Funding::Venue, vec![], fixtures::margin_preparation()),
        (VenueId::Polymarket, Funding::Venue, vec![], polymarket_margin()),
    ];
    for (venue, funding, orders, prepared) in cases {
        let harness = Harness::new().await;
        let session = harness.session().await;
        serve(&harness, &orders, &prepared).await;
        let order = session
            .maker(OPERATOR)
            .add_capital(&request(&venue, funding.clone()), &identity())
            .await;
        let order = order.unwrap();
        assert_eq!(order.state, OrderState::Funding);
        assert_eq!(order.preparation_id, Some(PREPARATION.parse().unwrap()));
        let asked = harness.bodies("POST", &format!("/v1/maker/{venue}/prepare")).await;
        assert_eq!(
            asked,
            [json!({
                "client_request_id": "request-1", "quote_identity_id": IDENTITY, "amount": TEN,
                "funding_source": funding,
            })]
        );
        let submitted = harness.bodies("POST", &submit_path()).await;
        let items = submitted[0]["signatures"].as_array().unwrap();
        let to_sign = harness.owned(&prepared)["to_sign"].as_array().unwrap().clone();
        assert_eq!(items.len(), to_sign.len());
        for (item, asked) in items.iter().zip(&to_sign) {
            assert_eq!(item["kind"], asked["kind"]);
            assert_eq!(item.as_object().unwrap().len(), 2, "{item}");
            assert_eq!(signer_of(item, &digest(&asked["typed_data"])), harness.address());
        }
        assert_eq!(harness.flow_signatures(), to_sign.len(), "{venue}");
        assert_eq!(record(&session).await, None);
    }
}

/// One case per input the flow supplies to the checks: the pool it read, the operator and the
/// identity's quote signer. Each shows that nothing is signed, submitted or saved.
#[tokio::test]
async fn a_failed_check_signs_nothing() {
    let (live, topped) = top_up();
    let fund = |edit: fn(&mut Value)| {
        edited(fixtures::maker_preparation(), move |p| edit(&mut p["to_sign"][0]["typed_data"]))
    };
    let cases = [
        (
            vec![live],
            edited(topped, |p| p["order"]["available_to_quote"] = json!("2000000")),
            details::MAKER_CAPITAL,
        ),
        (vec![], fund(|t| t["message"]["operator"] = json!(MAKER)), details::FUND_OPERATOR),
        (
            vec![],
            fund(|t| t["message"]["quoteSigner"] = json!(OPERATOR)),
            details::FUND_QUOTE_SIGNER,
        ),
    ];
    for (orders, prepared, expected) in cases {
        let harness = Harness::new().await;
        let session = harness.session().await;
        serve(&harness, &orders, &prepared).await;
        let request = request(&VenueId::Hyperliquid, Funding::Wallet);
        let Err(Error::Verification(error)) =
            session.maker(OPERATOR).add_capital(&request, &identity()).await
        else {
            panic!("{expected} is refused");
        };
        assert_eq!(error.detail, expected);
        assert_eq!(harness.flow_signatures(), 0);
        assert!(harness.requests("POST", &submit_path()).await.is_empty());
        assert_eq!(record(&session).await, None);
    }
}

#[tokio::test]
async fn invalid_or_unsupported_requests_send_nothing() {
    let harness = Harness::new().await;
    let session = harness.session().await;
    let maker = session.maker(OPERATOR);
    let id = || ClientId::new("request-1").unwrap();
    let zero = CapitalRequest::new(id(), VenueId::Hyperliquid, Amount::ZERO, Funding::Wallet);
    assert!(matches!(maker.add_capital(&zero, &identity()).await, Err(Error::InvalidRequest(_))));
    let unknown = request(&VenueId::Hyperliquid, "card".parse().unwrap());
    for (request, detail) in [
        (request(&VenueId::Lighter, Funding::Venue), details::VENUE),
        (request(&VenueId::Aster, Funding::Venue), details::VENUE),
        (request(&VenueId::Gmx, Funding::Venue), details::VENUE),
        (unknown, details::FUNDING),
    ] {
        let Err(Error::Verification(error)) = maker.add_capital(&request, &identity()).await else {
            panic!("{request:?} is unsupported");
        };
        assert_eq!((error.kind, error.detail), (VerificationKind::Unsupported, detail));
    }
    assert!(harness.received_only_auth().await);
}

#[tokio::test]
async fn an_account_linked_to_another_address_is_refused_before_preparing() {
    let harness = Harness::new().await;
    let session = harness.session().await;
    harness
        .mount("GET", "/v1/balances", vec![ok(json!([balance("hyperliquid", OTHER, None)]))])
        .await;
    harness.mount("GET", "/v1/orders", vec![ok(json!([]))]).await;
    let request = request(&VenueId::Hyperliquid, Funding::Wallet);
    let Err(Error::Verification(error)) =
        session.maker(OPERATOR).add_capital(&request, &identity()).await
    else {
        panic!("another linked address is refused");
    };
    assert_eq!((error.kind, error.detail), (VerificationKind::Changed, details::LINKED_ACCOUNT));
    assert!(harness.requests("POST", "/v1/maker/hyperliquid/prepare").await.is_empty());
}

#[tokio::test]
async fn an_accepted_preparation_returns_its_order_unsigned() {
    let harness = Harness::new().await;
    let session = harness.session().await;
    serve(&harness, &[], &accepted(&fixtures::maker_preparation())).await;
    let request = request(&VenueId::Hyperliquid, Funding::Wallet);
    let order = session.maker(OPERATOR).add_capital(&request, &identity()).await.unwrap();
    assert_eq!(order.state, OrderState::Funding);
    assert_eq!(harness.flow_signatures(), 0);
    assert!(harness.requests("POST", &submit_path()).await.is_empty());
}

#[tokio::test]
async fn an_ended_preparation_is_a_funding_failure() {
    let harness = Harness::new().await;
    let session = harness.session().await;
    let cancelled = edited(fixtures::maker_preparation(), |p| {
        p["state"] = json!("cancelled");
        p["order"]["error"] = json!("expired unsigned");
    });
    serve(&harness, &[], &cancelled).await;
    let request = request(&VenueId::Hyperliquid, Funding::Wallet);
    let error = session.maker(OPERATOR).add_capital(&request, &identity()).await.unwrap_err();
    let Error::FundingFailed { preparation, state, reason } = error else { panic!("{error:?}") };
    assert_eq!(preparation, PREPARATION.parse().unwrap());
    assert_eq!((state, reason.as_str()), (MakerPreparationState::Cancelled, "expired unsigned"));
}

/// The saved signatures are submitted again as they are, even once a margin move could no
/// longer be signed: only new signatures are bound to the clock.
#[tokio::test]
async fn an_unanswered_submission_is_resent_with_the_saved_signatures_later() {
    let harness = Harness::new().await;
    let session = harness.session().await;
    lose_first_submission(&harness).await;
    serve(&harness, &[], &fixtures::margin_preparation()).await;
    let maker = session.maker(OPERATOR);
    let request = request(&VenueId::Hyperliquid, Funding::Venue);
    assert_eq!(maker.add_capital(&request, &identity()).await.unwrap_err().status(), Some(503));
    assert!(record(&session).await.is_some());
    harness.later(90);
    maker.add_capital(&request, &identity()).await.unwrap();
    let submitted = harness.bodies("POST", &submit_path()).await;
    assert_eq!(submitted.len(), 2);
    assert_eq!(submitted[0], submitted[1]);
    assert_eq!(harness.flow_signatures(), 2, "each item signed once in all");
    assert_eq!(record(&session).await, None);
}

/// Saved signatures go again only for the same requests, asked in the same context.
#[tokio::test]
async fn saved_signatures_of_other_terms_are_not_submitted() {
    let prepared = fixtures::maker_preparation();
    let moved = edited(prepared.clone(), |p| {
        p["to_sign"][0]["typed_data"]["message"]["nonce"] = json!("5");
    });
    for (operator, served) in [(OTHER, &prepared), (OPERATOR, &moved)] {
        let harness = Harness::new().await;
        let session = harness.session().await;
        lose_first_submission(&harness).await;
        serve(&harness, &[], &prepared).await;
        let request = request(&VenueId::Hyperliquid, Funding::Wallet);
        session.maker(OPERATOR).add_capital(&request, &identity()).await.unwrap_err();
        harness.server.reset().await;
        serve(&harness, &[], served).await;
        let Err(Error::Verification(error)) =
            session.maker(operator).add_capital(&request, &identity()).await
        else {
            panic!("a saved signature of other terms is refused");
        };
        assert_eq!((error.kind, error.detail), (VerificationKind::Changed, details::DIGEST));
        assert_eq!(harness.flow_signatures(), 1);
        assert!(harness.requests("POST", &submit_path()).await.is_empty());
    }
}

/// Two sessions sharing a journal add the same capital concurrently: the second reads the
/// preparation once the first submitted it, and signs nothing.
#[tokio::test]
async fn sessions_sharing_a_journal_sign_a_preparation_once() {
    let harness = Harness::new().await;
    let journal = Journal::new(MemoryStore::new());
    let (first, second) =
        (harness.session_with(journal.clone()).await, harness.session_with(journal).await);
    let prepared = harness.owned(&fixtures::maker_preparation());
    serve(&harness, &[], &prepared).await;
    let submitted = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&submitted);
    let answered = accepted(&prepared);
    Mock::given(method("POST"))
        .and(path("/v1/maker/hyperliquid/prepare"))
        .respond_with(move |_: &Request| {
            ok(if seen.load(Ordering::SeqCst) > 0 { answered.clone() } else { prepared.clone() })
        })
        .with_priority(1)
        .mount(&harness.server)
        .await;
    let counted = Arc::clone(&submitted);
    let answered = accepted(&harness.owned(&fixtures::maker_preparation()));
    Mock::given(method("POST"))
        .and(path(submit_path()))
        .respond_with(move |_: &Request| {
            counted.fetch_add(1, Ordering::SeqCst);
            ok(answered.clone())
        })
        .with_priority(1)
        .mount(&harness.server)
        .await;
    let request = request(&VenueId::Hyperliquid, Funding::Wallet);
    let (a, b) = futures_util::future::join(
        first.maker(OPERATOR).add_capital(&request, &identity()),
        second.maker(OPERATOR).add_capital(&request, &identity()),
    )
    .await;
    assert_eq!(a.unwrap(), b.unwrap());
    assert_eq!((harness.flow_signatures(), submitted.load(Ordering::SeqCst)), (1, 1));
}

// Funding and order waits.

fn with_state(value: &Value, state: &str) -> Value {
    edited(value.clone(), |value| value["state"] = json!(state))
}

async fn serve_waits(harness: &Harness, preparations: Vec<Value>, orders: Vec<Value>) {
    let route = format!("/v1/maker/preparations/{PREPARATION}");
    harness.mount("GET", &route, preparations.into_iter().map(ok).collect()).await;
    harness.mount("GET", &format!("/v1/orders/{POOL}"), orders.into_iter().map(ok).collect()).await;
}

fn funding_order() -> Order {
    serde_json::from_value(accepted(&fixtures::maker_preparation())["order"].clone()).unwrap()
}

#[tokio::test]
async fn funding_is_followed_to_a_live_order() {
    let harness = Harness::new().await;
    let session = harness.session().await;
    let prepared = accepted(&fixtures::maker_preparation());
    let order = &prepared["order"];
    serve_waits(
        &harness,
        vec![prepared.clone(), with_state(&prepared, "complete")],
        vec![order.clone(), with_state(order, "live")],
    )
    .await;
    let maker = session.maker(OPERATOR);
    let live = maker.wait_for_funding(&funding_order(), Duration::from_secs(60)).await.unwrap();
    assert_eq!(live.state, OrderState::Live);
}

#[tokio::test]
async fn a_failed_funding_names_its_reason() {
    let harness = Harness::new().await;
    let session = harness.session().await;
    let failed = edited(accepted(&fixtures::maker_preparation()), |p| {
        p["state"] = json!("failed");
        p["order"]["error"] = json!("deposit reverted");
    });
    serve_waits(&harness, vec![failed.clone()], vec![failed["order"].clone()]).await;
    let maker = session.maker(OPERATOR);
    let error =
        maker.wait_for_funding(&funding_order(), Duration::from_secs(60)).await.unwrap_err();
    let Error::FundingFailed { state, reason, .. } = error else { panic!("{error:?}") };
    assert_eq!((state, reason.as_str()), (MakerPreparationState::Failed, "deposit reverted"));
    assert!(harness.requests("GET", &format!("/v1/orders/{POOL}")).await.is_empty());
}

#[tokio::test]
async fn waits_end_at_their_timeout() {
    let harness = Harness::new().await;
    let session = harness.session().await;
    let prepared = accepted(&fixtures::maker_preparation());
    serve_waits(&harness, vec![prepared.clone()], vec![prepared["order"].clone()]).await;
    let maker = session.maker(OPERATOR);
    let error = maker.wait_for_funding(&funding_order(), Duration::ZERO).await.unwrap_err();
    assert!(matches!(error, Error::Timeout("funding")), "{error:?}");
    let id: OrderId = POOL.parse().unwrap();
    let error = maker.wait_for_order(id, &[OrderState::Live], Duration::ZERO).await.unwrap_err();
    assert!(matches!(error, Error::Timeout("order")), "{error:?}");
}

#[tokio::test]
async fn an_order_wait_returns_its_state_or_the_failure() {
    let harness = Harness::new().await;
    let session = harness.session().await;
    let order = fixtures::pool();
    let failed = edited(with_state(&order, "failed"), |o| o["error"] = json!("close reverted"));
    let answers = vec![with_state(&order, "closing"), with_state(&order, "closed"), failed];
    serve_waits(&harness, vec![], answers).await;
    let maker = session.maker(OPERATOR);
    let id: OrderId = POOL.parse().unwrap();
    let closed = maker.wait_for_order(id, &[OrderState::Closed], Duration::from_secs(60)).await;
    assert_eq!(closed.unwrap().state, OrderState::Closed);
    let error =
        maker.wait_for_order(id, &[OrderState::Live], Duration::from_secs(60)).await.unwrap_err();
    let Error::OrderFailed { order, reason } = error else { panic!("{error:?}") };
    assert_eq!((order, reason.as_str()), (id, "close reverted"));
    // An order without a funding in progress goes straight to the order.
    let mut live = funding_order();
    live.preparation_id = None;
    let error = maker.wait_for_funding(&live, Duration::from_secs(60)).await.unwrap_err();
    assert!(matches!(error, Error::OrderFailed { .. }), "{error:?}");
}

// Closes.

/// The close of `orders` by `account`, its deadline in an hour.
fn close_item(account: Address, orders: &[Value], signer: Address) -> Value {
    let mut typed = sample("close_orders");
    typed["message"]["account"] = json!(account);
    typed["message"]["orders"] = json!(orders);
    typed["message"]["deadline"] = json!((NOW + 3600).to_string());
    json!({"kind": "close_orders", "signer": signer, "typed_data": typed})
}

fn order_ids(count: usize) -> Vec<OrderId> {
    (0..count).map(|n| format!("0x{n:064x}").parse().unwrap()).collect()
}

/// Serves closes of what each request asks, its item as `item` builds it, recording each close's
/// orders by id; submissions answer those orders, `closing`.
async fn serve_closes(
    harness: &Harness,
    item: impl Fn(Address, &[Value]) -> Value + Send + Sync + 'static,
) {
    let owner = harness.address();
    let closes: Arc<Mutex<HashMap<String, Vec<Value>>>> = Arc::default();
    let served = Arc::clone(&closes);
    let count = AtomicUsize::new(0);
    Mock::given(method("POST"))
        .and(path("/v1/closes"))
        .respond_with(move |request: &Request| {
            let orders =
                request.body_json::<Value>().unwrap()["orders"].as_array().unwrap().clone();
            let id =
                format!("0192f1c2-3d4e-7f00-8a1b-{:012}", count.fetch_add(1, Ordering::SeqCst));
            served.lock().unwrap().insert(id.clone(), orders.clone());
            ok(json!({"id": id, "to_sign": [item(owner, &orders)]}))
        })
        .mount(&harness.server)
        .await;
    Mock::given(method("POST"))
        .and(path_regex("^/v1/closes/[^/]+/signatures$"))
        .respond_with(move |request: &Request| {
            let id = request.url.path().split('/').nth(3).unwrap().to_owned();
            let orders = closes.lock().unwrap()[&id].clone();
            let answered: Vec<Value> = orders
                .iter()
                .map(|id| {
                    edited(fixtures::pool(), |o| {
                        o["id"] = id.clone();
                        o["owner_address"] = json!(owner);
                        o["state"] = json!("closing");
                    })
                })
                .collect();
            ok(json!({"orders": answered}))
        })
        .mount(&harness.server)
        .await;
}

async fn close_submissions(harness: &Harness) -> Vec<Value> {
    let received = harness.server.received_requests().await.unwrap();
    received
        .iter()
        .filter(|r| r.url.path().starts_with("/v1/closes/"))
        .map(|r| r.body_json().unwrap())
        .collect()
}

#[tokio::test]
async fn closes_are_signed_fifty_orders_at_a_time() {
    let harness = Harness::new().await;
    let session = harness.session().await;
    serve_closes(&harness, |owner, orders| close_item(owner, orders, owner)).await;
    let ids = order_ids(120);
    let closed = session.maker(OPERATOR).close_orders(&ids).await.unwrap();
    assert_eq!(closed.iter().map(|order| order.id).collect::<Vec<_>>(), ids);
    assert!(closed.iter().all(|order| order.state == OrderState::Closing));
    let asked = harness.bodies("POST", "/v1/closes").await;
    let sizes: Vec<usize> = asked.iter().map(|b| b["orders"].as_array().unwrap().len()).collect();
    assert_eq!(sizes, [50, 50, 20]);
    let submitted = close_submissions(&harness).await;
    assert_eq!(submitted.len(), 3);
    for (body, asked) in submitted.iter().zip(&asked) {
        let [item] = body["signatures"].as_array().unwrap().as_slice() else { panic!("{body}") };
        assert_eq!(item["kind"], "close_orders");
        let orders = asked["orders"].as_array().unwrap();
        let typed = &close_item(harness.address(), orders, harness.address())["typed_data"];
        assert_eq!(signer_of(item, &digest(typed)), harness.address());
    }
    assert_eq!(harness.flow_signatures(), 3);
}

#[tokio::test]
async fn a_close_with_nothing_to_sign_is_skipped() {
    let harness = Harness::new().await;
    let session = harness.session().await;
    harness.mount("POST", "/v1/closes", vec![ok(json!({"id": null, "to_sign": []}))]).await;
    let maker = session.maker(OPERATOR);
    assert!(maker.close_orders(&order_ids(2)).await.unwrap().is_empty());
    assert!(maker.close_orders(&[]).await.unwrap().is_empty());
    assert_eq!(harness.requests("POST", "/v1/closes").await.len(), 1);
    assert_eq!(harness.flow_signatures(), 0);
}

/// One case per input the flow supplies: the session's account and the orders asked.
#[tokio::test]
async fn a_failed_close_check_signs_nothing() {
    type Item = fn(Address, &[Value]) -> Value;
    let cases: [(Item, &str); 2] = [
        (|owner, orders| close_item(OTHER, orders, owner), details::ACCOUNT),
        (
            |owner, orders| close_item(owner, &[orders, &[json!(POOL)]].concat(), owner),
            details::ORDERS,
        ),
    ];
    for (item, expected) in cases {
        let harness = Harness::new().await;
        let session = harness.session().await;
        serve_closes(&harness, item).await;
        let Err(Error::Verification(error)) =
            session.maker(OPERATOR).close_orders(&order_ids(1)).await
        else {
            panic!("{expected} is refused");
        };
        assert_eq!(error.detail, expected);
        assert!(close_submissions(&harness).await.is_empty());
        assert_eq!(harness.flow_signatures(), 0);
    }
}
