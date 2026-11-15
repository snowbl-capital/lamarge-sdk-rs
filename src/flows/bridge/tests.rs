use alloy_primitives::{Address, B256, U256};
use serde_json::{Value, json};

use super::*;
use crate::{
    Error,
    error::VerificationKind,
    protocol::relay::relay_order_id,
    testing::{NOW, OTHER, harness::*, json::fixture},
    types::{ClientId, HyperliquidNonce, VenueId},
};

const VECTORS: &str =
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/vectors/relay.json"));

/// The vector `name`'s order paid by `payer`, and the bridge view serving it.
fn bridge(name: &str, payer: Address) -> (Value, Bridge) {
    let vectors: Value = serde_json::from_str(VECTORS).unwrap();
    let text = vectors["cases"][name]
        .to_string()
        .replace(&vectors["payer"].as_str().unwrap().to_lowercase(), &format!("{payer:#x}"));
    let text = text.replace(vectors["payer"].as_str().unwrap(), &format!("{payer:#x}"));
    let case: Value = serde_json::from_str(&text).unwrap();
    let mut view = fixture("move/bridges/0");
    view["order"] = json!({
        "id": relay_order_id(&case["order"]).unwrap(),
        "data": case["order"],
        "deadline": case["order"]["output"]["deadline"],
    });
    view["from"] = case["request"]["from"].clone();
    view["to"] = case["request"]["to"].clone();
    view["payer"] = json!(payer);
    view["state"] = json!("prepared");
    view["nonce"] = json!((NOW * 1000).to_string());
    (case, serde_json::from_value(view).unwrap())
}

fn terms(case: &Value, payer: Address, recipient: MoveRecipient) -> BridgeTerms {
    let request = &case["request"];
    let endpoint = |side: &str| serde_json::from_value(request[side].clone()).unwrap();
    let amount_in: Amount = request["amount_in"].as_str().unwrap().parse().unwrap();
    let id = ClientId::new("move-1").unwrap();
    let floor = Amount::from(1u64);
    let terms =
        MoveRequest::new(id, endpoint("from"), endpoint("to"), amount_in, floor, payer, &recipient);
    BridgeTerms::for_move(&terms, recipient, amount_in).unwrap()
}

#[tokio::test]
async fn a_hypercore_bridge_signs_the_mapping_and_the_send_for_the_payer() {
    let harness = Harness::new().await;
    let session = harness.session().await;
    let me = harness.address();
    let (case, bridge) = bridge("hyperliquid_to_arbitrum", me);
    let route = format!("/v1/bridges/{}/signatures", bridge.id);
    harness.mount("POST", &route, vec![ok(fixture("move/bridges/0"))]).await;
    let terms = terms(&case, me, MoveRecipient::Address(me));
    session.sign_bridge(&bridge, &terms).await.unwrap();

    let [body] = harness.bodies("POST", &route).await.try_into().unwrap();
    let nonce = bridge.nonce.unwrap().get();
    let mapping = relay::bridge_authorization(bridge.order.id, me, nonce);
    let send = relay::bridge_send_asset(U256::from(100_000_000u64), nonce).typed_data();
    for (field, typed) in [("authorization", mapping), ("send_asset", send)] {
        let item = json!({"signature": body[field]});
        assert_eq!(signer_of(&item, &typed.eip712_signing_hash().unwrap()), me, "{field}");
    }
    assert_eq!(harness.flow_signatures(), 2);
}

#[tokio::test]
async fn nothing_is_signed_for_an_order_its_id_or_terms_do_not_match() {
    let harness = Harness::new().await;
    let session = harness.session().await;
    let me = harness.address();
    let (case, bridge) = bridge("hyperliquid_to_arbitrum", me);
    let terms = terms(&case, me, MoveRecipient::Address(me));

    let mut other_id = bridge.clone();
    other_id.order.id = B256::repeat_byte(1);
    let refused = session.sign_bridge(&other_id, &terms).await.unwrap_err();
    assert!(matches!(refused, Error::Verification(e) if e.detail == details::ORDER_ID));

    let elsewhere = BridgeTerms { recipient: MoveRecipient::Address(OTHER), ..terms.clone() };
    let refused = session.sign_bridge(&bridge, &elsewhere).await.unwrap_err();
    assert!(matches!(refused, Error::Verification(e) if e.detail == details::ORDER_RECIPIENT));

    let other_payer = BridgeTerms { payer: OTHER, ..terms.clone() };
    let refused = session.sign_bridge(&bridge, &other_payer).await.unwrap_err();
    assert!(matches!(refused, Error::Verification(e) if e.detail == details::ORDER_REFUND));

    let mut no_nonce = bridge.clone();
    no_nonce.nonce = None;
    let refused = session.sign_bridge(&no_nonce, &terms).await.unwrap_err();
    assert!(matches!(refused, Error::Verification(e) if e.detail == details::NONCE));
    let mut stale_nonce = bridge.clone();
    stale_nonce.nonce = Some(HyperliquidNonce::new(1_794_657_763_758));
    let refused = session.sign_bridge(&stale_nonce, &terms).await.unwrap_err();
    assert!(matches!(refused, Error::Verification(e) if e.detail == details::NONCE));

    let refused = session.bridge_deposit(&bridge, &terms).await.unwrap_err();
    assert!(matches!(refused, Error::Verification(e) if e.kind == VerificationKind::Unsupported));
    assert_eq!(harness.flow_signatures(), 0);
    assert!(harness.received_only_auth().await);
}

#[tokio::test]
async fn a_wallet_bridge_returns_the_checked_deposit_after_reading_the_lighter_owner() {
    let harness = Harness::new().await;
    let session = harness.session().await;
    let me = harness.address();
    let (case, bridge) = bridge("arbitrum_to_lighter", me);
    let lighter = MoveRecipient::LighterAccount { index: 3, owner: me };
    harness.lighter_owner(3, me).await;
    let usdc = deployments_usdc();
    let route = format!("/v1/bridges/{}/funding", bridge.id);
    let funds = |allowance: &str, balance: &str, spender: Address| {
        let mut funds = fixture("bridge_funding");
        funds["owner"] = json!(me);
        funds["token"] = json!(usdc);
        funds["spender"] = json!(spender);
        funds["allowance"] = json!(allowance);
        funds["balance"] = json!(balance);
        ok(funds)
    };
    let answers = vec![
        funds("99999999", "100000000", relay::DEPOSITORY),
        funds("100000000", "100000000", relay::DEPOSITORY),
        funds("100000000", "99999999", relay::DEPOSITORY),
        funds("100000000", "100000000", OTHER),
        unavailable("chain_unread"),
        unavailable("chain_unavailable"),
        api_error(502, "VENUE_UNAVAILABLE"),
    ];
    harness.mount("GET", &route, answers).await;
    let terms_to = |recipient| terms(&case, me, recipient);
    let deposit = session.bridge_deposit(&bridge, &terms_to(lighter)).await.unwrap();
    assert_eq!((deposit.chain_id, deposit.token), (42161, usdc));
    let approve = deposit.approve.unwrap();
    assert_eq!((approve.to, deposit.deposit.to), (usdc, relay::DEPOSITORY));
    let expected = relay::bridge_transactions(
        bridge.order.id,
        relay::relay_endpoint(&MoveEndpoint::wallet(42161, usdc), true).unwrap(),
        me,
        U256::from(100_000_000u64),
    )
    .unwrap();
    assert_eq!(
        (approve.data, deposit.deposit.data.clone()),
        (expected.approve.clone(), expected.deposit)
    );
    let allowed = session.bridge_deposit(&bridge, &terms_to(lighter)).await.unwrap();
    assert_eq!((allowed.approve, allowed.deposit), (None, deposit.deposit));
    let short = session.bridge_deposit(&bridge, &terms_to(lighter)).await.unwrap_err();
    assert!(matches!(short, Error::Verification(e) if e.detail == details::WALLET_FUNDS));
    let elsewhere = session.bridge_deposit(&bridge, &terms_to(lighter)).await.unwrap_err();
    assert!(matches!(elsewhere, Error::Verification(e) if e.detail == details::BRIDGE_FUNDING));
    // An origin chain the server cannot read: the exact approve, the balance unchecked.
    let unread = session.bridge_deposit(&bridge, &terms_to(lighter)).await.unwrap();
    assert_eq!(unread.approve.map(|call| call.data), Some(expected.approve));
    assert_eq!(unread.funding, None);
    // A transient read failure, or one without a reason, is returned: retry later.
    for _ in 0..2 {
        let failed = session.bridge_deposit(&bridge, &terms_to(lighter)).await.unwrap_err();
        assert_eq!(failed.status(), Some(502));
    }

    let stranger = MoveRecipient::LighterAccount { index: 3, owner: OTHER };
    let refused = session.bridge_deposit(&bridge, &terms(&case, me, stranger)).await.unwrap_err();
    assert!(matches!(refused, Error::Verification(e) if e.detail == details::DESTINATION_OWNER));
    let refused = session.sign_bridge(&bridge, &terms(&case, me, lighter)).await.unwrap_err();
    assert!(matches!(refused, Error::Verification(e) if e.detail == details::BRIDGE_ORIGIN));
    assert_eq!(harness.flow_signatures(), 0);
}

/// The funding read's 502 with `reason`.
fn unavailable(reason: &str) -> wiremock::ResponseTemplate {
    wiremock::ResponseTemplate::new(502).set_body_json(json!({"error": {
        "code": "VENUE_UNAVAILABLE", "message": reason, "retryable": true, "reason": reason,
    }}))
}

fn deployments_usdc() -> Address {
    crate::deployments::chain(&VenueId::Gmx).unwrap().token
}

#[test]
fn terms_for_a_move_take_the_parts_share_of_its_floor() {
    let recipient = MoveRecipient::Address(OTHER);
    let usdt = crate::deployments::chain(&VenueId::Aster).unwrap().token;
    let request = MoveRequest::new(
        ClientId::new("move-1").unwrap(),
        MoveEndpoint::account(&VenueId::Hyperliquid),
        MoveEndpoint::account(&VenueId::Aster),
        Amount::from(3_000_000u64),
        Amount::from(2_000_000_000_000_000_001u128),
        OTHER,
        &recipient,
    );
    let terms = BridgeTerms::for_move(&request, recipient, Amount::from(1_000_000u64)).unwrap();
    // 1/3 of the floor, rounded up, credited to the Aster account exactly.
    let floor = Amount::from(666_666_666_666_666_667u128);
    assert_eq!((terms.min_out, terms.account_credit), (floor, Some(floor)));
    assert_eq!((terms.from.clone(), terms.payer), (request.from.clone(), OTHER));
    let wallet = terms.from_wallet(56, usdt);
    assert_eq!(wallet.from, MoveEndpoint::wallet(56, usdt));
    let lighter = MoveRecipient::LighterAccount { index: 3, owner: OTHER };
    assert!(BridgeTerms::for_move(&request, lighter, Amount::from(1u64)).is_err());
    assert!(BridgeTerms::for_move(&request, recipient, Amount::ZERO).is_err());
}

#[test]
fn a_polymarket_credit_is_at_least_its_deposit_minimum() {
    let recipient = MoveRecipient::Address(OTHER);
    let request = MoveRequest::new(
        ClientId::new("move-1").unwrap(),
        MoveEndpoint::account(&VenueId::Hyperliquid),
        MoveEndpoint::account(&VenueId::Polymarket),
        Amount::from(100_000_000u64),
        Amount::from(99_000_000u64),
        OTHER,
        &recipient,
    );
    let small = BridgeTerms::for_move(&request, recipient, Amount::from(2_000_000u64)).unwrap();
    assert_eq!(small.min_out, Amount::from(1_980_000u64));
    assert_eq!(small.account_credit, Some(Amount::from(5_000_000u64)));
    let large = BridgeTerms::for_move(&request, recipient, Amount::from(10_000_000u64)).unwrap();
    assert_eq!(large.account_credit, Some(Amount::from(9_900_000u64)));
}
