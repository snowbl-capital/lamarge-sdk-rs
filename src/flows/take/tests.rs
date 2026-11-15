use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use serde_json::json;
use wiremock::{
    Mock, Request, ResponseTemplate,
    matchers::{method, path},
};

use super::*;
use crate::{
    error::{VerificationError, VerificationKind},
    protocol::lighter,
    store::{Journal, MemoryStore},
    testing::{FILL, MAKER, NOW, OTHER, edited, fixtures, harness::*, replaced},
};

/// The taker's Lighter account, and the maker's.
const PAYER_ACCOUNT: u64 = 42;
const MAKER_ACCOUNT: u64 = 731_033;

fn submissions_route() -> String {
    format!("/v1/takes/{FILL}/signatures")
}

/// A signed-in session whose address is linked on every venue (Lighter's account 42).
async fn taker() -> (Harness, Session) {
    taker_with(Journal::new(MemoryStore::new())).await
}

async fn taker_with(journal: Journal) -> (Harness, Session) {
    let harness = Harness::new().await;
    let session = harness.session_with(journal).await;
    let owner = harness.address();
    let rows: Vec<Value> = ["hyperliquid", "polymarket", "gmx", "aster"]
        .map(|venue| balance(venue, owner, None))
        .into_iter()
        .chain([balance("lighter", owner, Some("42"))])
        .collect();
    harness.mount("GET", "/v1/balances", vec![ok(Value::Array(rows))]).await;
    (harness, session)
}

/// Serves `prepared` (the fixtures' owner replaced) to `POST /v1/takes`, answers submissions
/// with `submitted` in turn and accepts cancellations.
async fn serve(harness: &Harness, prepared: Vec<Value>, submitted: Vec<ResponseTemplate>) {
    let prepared =
        prepared.iter().map(|p| ResponseTemplate::new(201).set_body_json(harness.owned(p)));
    harness.mount("POST", "/v1/takes", prepared.collect()).await;
    if !submitted.is_empty() {
        harness.mount("POST", &submissions_route(), submitted).await;
    }
    harness.mount("DELETE", &format!("/v1/takes/{FILL}"), vec![ResponseTemplate::new(204)]).await;
}

fn fills_of(prepared: &Value) -> ResponseTemplate {
    ok(json!({"fills": prepared["take"]["fills"]}))
}

fn request(
    from: VenueId,
    to: VenueId,
    amount_in: u128,
    min_out: u128,
    owner: Address,
) -> TakeRequest {
    let terms = TradeTerms::new(
        from,
        to,
        Amount::from(amount_in),
        Amount::from(min_out),
        Recipient::Account(owner),
    );
    TakeRequest::new(ClientId::new("take-1").unwrap(), terms)
}

/// [`fixtures::escrow_take`]'s request.
fn escrow_request(owner: Address) -> TakeRequest {
    request(VenueId::Hyperliquid, VenueId::Polymarket, 2_000_000, 2_000_000, owner)
}

fn lighter_request(owner: Address) -> TakeRequest {
    request(VenueId::Lighter, VenueId::Hyperliquid, 2_001_300, 2_000_000, owner)
}

fn aster_request(owner: Address) -> TakeRequest {
    request(VenueId::Aster, VenueId::Hyperliquid, 1_500_000_000_000_000_001, 1_000_000, owner)
}

/// An Aster take whose nonce is `ahead` seconds after [`NOW`].
fn aster_take(ahead: i64) -> Value {
    let nonce = ((NOW + ahead) * 1_000_000).to_string();
    fixtures::aster_take_of(&replaced(fixtures::aster_fields(), "nonce", &nonce))
}

async fn submissions(harness: &Harness) -> Vec<Value> {
    harness.bodies("POST", &submissions_route()).await
}

async fn cancellations(harness: &Harness) -> usize {
    harness.requests("DELETE", &format!("/v1/takes/{FILL}")).await.len()
}

async fn record(session: &Session) -> Option<Vec<u8>> {
    session.journal().load(&session.record_key(&format!("take:{FILL}"))).await.unwrap()
}

/// Runs `request` against `prepared`, whose submission must never come, and returns its
/// refusal: nothing signed, submitted or saved, the take cancelled.
async fn refused(
    harness: &Harness,
    session: &Session,
    request: &TakeRequest,
    prepared: &Value,
) -> VerificationError {
    serve(harness, vec![prepared.clone()], vec![]).await;
    let Err(Error::Verification(error)) = session.take(request).await else {
        panic!("the take is refused");
    };
    assert_eq!(harness.flow_signatures(), 0, "{error:?}");
    assert!(submissions(harness).await.is_empty());
    assert!(record(session).await.is_none());
    assert_eq!(cancellations(harness).await, 1);
    error
}

#[tokio::test]
async fn a_take_from_hyperliquid_is_checked_signed_saved_and_submitted() {
    let (harness, session) = taker().await;
    let owner = harness.address();
    let prepared = harness.owned(&fixtures::escrow_take());
    serve(&harness, vec![prepared.clone()], vec![fills_of(&prepared)]).await;
    let fills = session.take(&escrow_request(owner)).await.unwrap();
    assert_eq!(fills.len(), 1);
    assert_eq!(fills[0].id.to_string(), FILL);
    assert_eq!(
        harness.bodies("POST", "/v1/takes").await,
        [json!({
            "funding": "venue", "from": "hyperliquid", "to": "polymarket",
            "amount_in": "2000000", "min_out": "2000000", "client_take_id": "take-1",
            "max_parts": 8,
        })]
    );
    let submitted = submissions(&harness).await;
    let [item] = submitted[0]["signatures"].as_array().unwrap().as_slice() else { panic!() };
    assert_eq!((&item["kind"], &item["fill_id"]), (&json!("pay_take"), &json!(FILL)));
    assert_eq!(signer_of(item, &digest(&prepared["to_sign"][0]["typed_data"])), owner);
    assert_eq!((submitted.len(), harness.flow_signatures()), (1, 1));
    assert!(record(&session).await.is_none(), "the record goes once the answer reads");
    assert_eq!(cancellations(&harness).await, 0);
}

#[tokio::test]
async fn a_take_with_nothing_to_sign_returns_its_fills() {
    let (harness, session) = taker().await;
    let mut paid = fixtures::escrow_take();
    paid["to_sign"] = json!([]);
    serve(&harness, vec![paid], vec![]).await;
    assert_eq!(session.take(&escrow_request(harness.address())).await.unwrap().len(), 1);
    assert_eq!(harness.flow_signatures(), 0);
    assert!(submissions(&harness).await.is_empty());
}

#[tokio::test]
async fn a_paid_take_is_never_signed_for_again() {
    // A Lighter payment is deduplicated by its nonce only: a fresh request for a paid take
    // would pay twice.
    let (harness, session) = taker().await;
    harness.lighter_owner(PAYER_ACCOUNT, harness.address()).await;
    harness.lighter_owner(MAKER_ACCOUNT, MAKER).await;
    let mut paid = fixtures::lighter_take();
    paid["take"]["fills"][0]["state"] = json!("PAID");
    serve(&harness, vec![paid], vec![]).await;
    let fills = session.take(&lighter_request(harness.address())).await.unwrap();
    assert_eq!(fills[0].state, FillState::Paid);
    assert_eq!(harness.flow_signatures(), 0);
    assert!(submissions(&harness).await.is_empty());
    assert_eq!(cancellations(&harness).await, 0);
}

#[tokio::test]
async fn options_reach_the_take_body() {
    let (harness, session) = taker().await;
    let mut paid = fixtures::escrow_take();
    paid["to_sign"] = json!([]);
    serve(&harness, vec![paid], vec![]).await;
    let terms = TradeTerms::new(
        VenueId::Hyperliquid,
        VenueId::Polymarket,
        Amount::from(5_u64),
        Amount::from(4_u64),
        Recipient::Wallet(OTHER),
    )
    .max_parts(MaxParts::new(3).unwrap())
    .max_fee_bps(Bps::new(5).unwrap());
    session.take(&TakeRequest::new(ClientId::new("take-1").unwrap(), terms)).await.unwrap();
    let body = &harness.bodies("POST", "/v1/takes").await[0];
    assert_eq!(
        (&body["max_parts"], &body["max_fee_bps"], &body["destination"], &body["recipient"]),
        (&json!(3), &json!(5), &json!("wallet"), &json!(OTHER))
    );
}

#[tokio::test]
async fn a_take_from_lighter_signs_the_owner_message_of_accounts_lighter_names() {
    let (harness, session) = taker().await;
    let owner = harness.address();
    harness.lighter_owner(PAYER_ACCOUNT, owner).await;
    harness.lighter_owner(MAKER_ACCOUNT, MAKER).await;
    let prepared = fixtures::lighter_take();
    serve(&harness, vec![prepared.clone()], vec![fills_of(&prepared)]).await;
    session.take(&lighter_request(owner)).await.unwrap();
    let item = &submissions(&harness).await[0]["signatures"][0];
    assert_eq!((&item["kind"], &item["fill_id"]), (&json!("lighter_transfer"), &json!(FILL)));
    let message = lighter::transfer_message(
        &lighter::Transfer {
            nonce: 7,
            from: PAYER_ACCOUNT,
            from_route: lighter::COLLATERAL_ROUTE,
            api_key: 254,
            to: MAKER_ACCOUNT,
            to_route: lighter::COLLATERAL_ROUTE,
            asset: lighter::USDC_ASSET,
            amount: 2_001_300,
            fee: 0,
            memo: lighter::fill_memo(FILL.parse().unwrap()),
        },
        deployments::chain(&VenueId::Lighter).unwrap().id,
    );
    let signature: alloy_primitives::Signature =
        item["signature"].as_str().unwrap().parse().unwrap();
    let signer = signature.recover_address_from_msg(message).unwrap();
    assert_eq!((signer, harness.flow_signatures()), (owner, 1));
}

#[tokio::test]
async fn a_take_from_aster_signs_its_transfer_request() {
    let (harness, session) = taker().await;
    let owner = harness.address();
    let prepared = harness.owned(&fixtures::aster_take());
    serve(&harness, vec![prepared.clone()], vec![fills_of(&prepared)]).await;
    session.take(&aster_request(owner)).await.unwrap();
    let item = &submissions(&harness).await[0]["signatures"][0];
    assert_eq!(item["kind"], "aster_transfer");
    assert_eq!(signer_of(item, &digest(&prepared["to_sign"][0]["typed_data"])), owner);
}

/// An Aster take LaMarge must issue again, holding a request whose nonce is long gone.
fn reissued_take() -> Value {
    let mut held = aster_take(-300);
    held["aster_reissue"] = json!(true);
    held
}

/// The reissue answer: `prepared` with `matched`.
fn reissue_answer(harness: &Harness, prepared: &Value, matched: bool) -> ResponseTemplate {
    let mut answer = harness.owned(prepared);
    answer["matched"] = json!(matched);
    ok(answer)
}

fn reissue_route() -> String {
    format!("/v1/takes/{FILL}/aster-request")
}

#[tokio::test]
async fn an_aster_request_is_issued_again_right_before_signing() {
    let nonce = |ahead: i64| json!((NOW + ahead) * 1_000_000);
    for superseded in [false, true] {
        let (harness, session) = taker().await;
        let fresh = harness.owned(&aster_take(55));
        let mut answers = vec![reissue_answer(&harness, &fresh, true)];
        if superseded {
            answers.insert(0, reissue_answer(&harness, &aster_take(-100), false));
        }
        harness.mount("POST", &reissue_route(), answers).await;
        serve(&harness, vec![reissued_take()], vec![fills_of(&fresh)]).await;
        session.take(&aster_request(harness.address())).await.unwrap();
        let asked: Vec<Value> = harness
            .bodies("POST", &reissue_route())
            .await
            .iter()
            .map(|b| b["nonce"].clone())
            .collect();
        let expected = if superseded { vec![nonce(-300), nonce(-100)] } else { vec![nonce(-300)] };
        assert_eq!(asked, expected);
        let item = &submissions(&harness).await[0]["signatures"][0];
        let signed = signer_of(item, &digest(&fresh["to_sign"][0]["typed_data"]));
        assert_eq!((signed, harness.flow_signatures()), (harness.address(), 1));
    }
    // Superseded twice: a conflict, never signed.
    let (harness, session) = taker().await;
    let current = reissue_answer(&harness, &aster_take(-100), false);
    harness.mount("POST", &reissue_route(), vec![current]).await;
    let error =
        refused(&harness, &session, &aster_request(harness.address()), &reissued_take()).await;
    assert_eq!((error.kind, error.detail), (VerificationKind::Changed, details::ASTER_TRANSFER));
}

#[tokio::test]
async fn an_unknown_reissue_outcome_keeps_the_reservation_and_signs_nothing() {
    for answer in [ResponseTemplate::new(503), ok(json!({"take": null}))] {
        let (harness, session) = taker().await;
        serve(&harness, vec![reissued_take()], vec![]).await;
        harness.mount("POST", &reissue_route(), vec![answer]).await;
        let error = session.take(&aster_request(harness.address())).await.unwrap_err();
        assert!(matches!(error, Error::AsterRequestUncertain), "{error:?}");
        assert_eq!((harness.flow_signatures(), cancellations(&harness).await), (0, 0));
        assert!(record(&session).await.is_none());
    }
    // LaMarge's refusal is definitive: the take is cancelled.
    let (harness, session) = taker().await;
    serve(&harness, vec![reissued_take()], vec![]).await;
    harness.mount("POST", &reissue_route(), vec![api_error(409, "TAKE_EXPIRED")]).await;
    let error = session.take(&aster_request(harness.address())).await.unwrap_err();
    assert_eq!(error.code(), Some(&ErrorCode::TakeExpired));
    assert_eq!((harness.flow_signatures(), cancellations(&harness).await), (0, 1));
}

/// One test per input the flow supplies to the checks: the payer, Lighter's owner reads and the
/// clock. Every check failure takes the same path, so each shows that nothing is signed,
/// submitted or saved and that the take is cancelled.
#[tokio::test]
async fn a_failed_check_signs_nothing_and_cancels_the_take() {
    // The payer is the session address.
    let (harness, session) = taker().await;
    let tampered = edited(fixtures::escrow_take(), |p| p["to_sign"][0]["signer"] = json!(OTHER));
    let error = refused(&harness, &session, &escrow_request(harness.address()), &tampered).await;
    assert_eq!(error.detail, details::PAYER);
    // Lighter says the paying account is someone else's.
    let (harness, session) = taker().await;
    harness.lighter_owner(PAYER_ACCOUNT, OTHER).await;
    harness.lighter_owner(MAKER_ACCOUNT, MAKER).await;
    let paying = lighter_request(harness.address());
    let error = refused(&harness, &session, &paying, &fixtures::lighter_take()).await;
    assert_eq!(error.detail, details::LIGHTER_PAYER_ACCOUNT);
    // Lighter says the delivering account is someone else's; the same take into the
    // recipient's account is paid.
    for (owner, paid) in [(OTHER, false), (Address::ZERO, true)] {
        let (harness, session) = taker().await;
        let owner = if owner.is_zero() { harness.address() } else { owner };
        harness.lighter_owner(731, owner).await;
        let request = request(
            VenueId::Hyperliquid,
            VenueId::Lighter,
            2_000_000,
            2_000_000,
            harness.address(),
        );
        let prepared = fixtures::lighter_delivery_take();
        if paid {
            serve(&harness, vec![prepared.clone()], vec![fills_of(&prepared)]).await;
            session.take(&request).await.unwrap();
            assert_eq!(harness.flow_signatures(), 1);
        } else {
            let error = refused(&harness, &session, &request, &prepared).await;
            assert_eq!(error.detail, details::DESTINATION_OWNER);
        }
    }
    // The Aster nonce must stay usable at the caller's clock.
    let (harness, session) = taker().await;
    let error =
        refused(&harness, &session, &aster_request(harness.address()), &aster_take(5)).await;
    assert_eq!(error.detail, details::ASTER_NONCE);
}

#[tokio::test]
async fn a_saved_signature_is_submitted_again_after_a_failed_submission() {
    let (harness, session) = taker().await;
    let prepared = fixtures::escrow_take();
    serve(&harness, vec![prepared.clone()], vec![ResponseTemplate::new(503), fills_of(&prepared)])
        .await;
    let request = escrow_request(harness.address());
    assert_eq!(session.take(&request).await.unwrap_err().status(), Some(503));
    assert!(record(&session).await.is_some(), "kept for the next run");
    assert_eq!(cancellations(&harness).await, 0);
    session.take(&request).await.unwrap();
    let submitted = submissions(&harness).await;
    assert_eq!((submitted.len(), harness.flow_signatures()), (2, 1));
    assert_eq!(submitted[0], submitted[1]);
    assert!(record(&session).await.is_none());
}

#[tokio::test]
async fn a_take_dropped_after_its_signature_is_saved_submits_the_same_bytes() {
    let (harness, session) = taker().await;
    let prepared = fixtures::escrow_take();
    // The first submission hangs until the caller gives up.
    let gated = fills_of(&prepared).set_delay(Duration::from_secs(2));
    serve(&harness, vec![prepared.clone()], vec![gated, fills_of(&prepared)]).await;
    let request = escrow_request(harness.address());
    let first = tokio::time::timeout(Duration::from_millis(500), session.take(&request));
    assert!(first.await.is_err(), "dropped while submitting");
    assert!(record(&session).await.is_some());
    session.take(&request).await.unwrap();
    let submitted = submissions(&harness).await;
    assert_eq!((submitted.len(), harness.flow_signatures()), (2, 1));
    assert_eq!(submitted[0], submitted[1]);
}

#[tokio::test]
async fn a_signature_that_cannot_be_saved_is_never_submitted() {
    let (harness, session) = taker_with(Journal::new(FlakyStore::failing_save(1))).await;
    let prepared = fixtures::escrow_take();
    serve(&harness, vec![prepared.clone()], vec![fills_of(&prepared)]).await;
    let Err(Error::Store(error)) = session.take(&escrow_request(harness.address())).await else {
        panic!("the save fails")
    };
    assert!(!error.submitted());
    assert!(submissions(&harness).await.is_empty());
    assert_eq!(cancellations(&harness).await, 1);
}

#[tokio::test]
async fn a_record_that_cannot_be_deleted_after_submission_reports_it_submitted() {
    let (harness, session) = taker_with(Journal::new(FlakyStore::failing_delete(1))).await;
    let prepared = fixtures::escrow_take();
    let paid = edited(prepared.clone(), |p| p["to_sign"] = json!([]));
    serve(&harness, vec![prepared.clone(), paid], vec![fills_of(&prepared)]).await;
    let request = escrow_request(harness.address());
    let Err(Error::Store(error)) = session.take(&request).await else { panic!("the delete fails") };
    assert!(error.submitted());
    // The same request finds the take paid: its fills, nothing signed again.
    assert_eq!(session.take(&request).await.unwrap().len(), 1);
    assert_eq!((submissions(&harness).await.len(), harness.flow_signatures()), (1, 1));
}

#[tokio::test]
async fn a_saved_signature_never_goes_with_another_payment() {
    let (harness, session) = taker().await;
    let prepared = fixtures::escrow_take();
    let moved = edited(prepared.clone(), |p| {
        p["to_sign"][0]["typed_data"]["message"]["deposit"] = json!("1");
    });
    serve(&harness, vec![prepared, moved], vec![ResponseTemplate::new(503)]).await;
    let request = escrow_request(harness.address());
    session.take(&request).await.unwrap_err();
    let Err(Error::Verification(error)) = session.take(&request).await else { panic!() };
    assert_eq!(error.detail, details::DIGEST);
    assert_eq!((submissions(&harness).await.len(), harness.flow_signatures()), (1, 1));
}

#[tokio::test]
async fn an_expired_take_forgets_its_signature() {
    let (harness, session) = taker().await;
    serve(&harness, vec![fixtures::escrow_take()], vec![api_error(409, "TAKE_EXPIRED")]).await;
    let error = session.take(&escrow_request(harness.address())).await.unwrap_err();
    assert_eq!(error.code(), Some(&ErrorCode::TakeExpired));
    assert!(record(&session).await.is_none());
}

#[tokio::test]
async fn sessions_sharing_a_journal_reserve_sign_and_submit_once() {
    let journal = Journal::new(MemoryStore::new());
    let (harness, session) = taker_with(journal.clone()).await;
    let other = harness.session_with(journal).await;
    let prepared = harness.owned(&fixtures::escrow_take());
    let paid_take = edited(prepared.clone(), |p| p["to_sign"] = json!([]));
    let paid = Arc::new(AtomicBool::new(false));
    let seen = Arc::clone(&paid);
    let reserved = prepared.clone();
    Mock::given(method("POST"))
        .and(path("/v1/takes"))
        .respond_with(move |_: &Request| {
            ok(if seen.load(Ordering::SeqCst) { paid_take.clone() } else { reserved.clone() })
        })
        .mount(&harness.server)
        .await;
    let fills = fills_of(&prepared);
    Mock::given(method("POST"))
        .and(path(submissions_route()))
        .respond_with(move |_: &Request| {
            paid.store(true, Ordering::SeqCst);
            fills.clone()
        })
        .mount(&harness.server)
        .await;
    let request = escrow_request(harness.address());
    let (first, second) =
        futures_util::future::join(session.take(&request), other.take(&request)).await;
    assert_eq!((first.unwrap().len(), second.unwrap().len()), (1, 1));
    assert_eq!((harness.flow_signatures(), submissions(&harness).await.len()), (1, 1));
}

#[tokio::test]
async fn a_wallet_funded_take_needs_fresh_funds_covering_it() {
    let wallet_take = edited(fixtures::escrow_take(), |p| {
        p["take"]["funding"] = json!("wallet");
        p["take"]["fills"][0]["funding"] = json!("wallet");
    });
    let wallet_request = |owner| {
        let terms = TradeTerms::new(
            VenueId::Hyperliquid,
            VenueId::Polymarket,
            Amount::from(2_000_000_u64),
            Amount::from(2_000_000_u64),
            Recipient::Account(owner),
        )
        .funding(Funding::Wallet);
        TakeRequest::new(ClientId::new("take-1").unwrap(), terms)
    };
    let funds_route = "/v1/wallets/hyperliquid";
    let (harness, session) = taker().await;
    serve(&harness, vec![wallet_take.clone()], vec![fills_of(&wallet_take)]).await;
    harness.mount("GET", funds_route, vec![ok(harness.owned(&fixtures::wallet_funds()))]).await;
    session.take(&wallet_request(harness.address())).await.unwrap();
    let reads = harness.requests("GET", funds_route).await;
    assert_eq!(reads[0].url.query(), Some(format!("take_id={FILL}").as_str()));
    assert_eq!(harness.bodies("POST", "/v1/takes").await[0]["funding"], "wallet");
    let short = |funds: &mut Value| {
        funds["spendable_wallet"] = json!("1000000");
        funds["spendable_available"] = json!("999999");
    };
    let stale = |funds: &mut Value| funds["observed_at"] = json!(crate::testing::time(-200));
    for (edit, expected) in [
        (&short as &dyn Fn(&mut Value), details::WALLET_FUNDS),
        (&stale, details::WALLET_FUNDS_OBSERVED_AT),
    ] {
        let (harness, session) = taker().await;
        let mut funds = harness.owned(&fixtures::wallet_funds());
        edit(&mut funds);
        harness.mount("GET", funds_route, vec![ok(funds)]).await;
        let request = wallet_request(harness.address());
        let error = refused(&harness, &session, &request, &wallet_take).await;
        assert_eq!(error.detail, expected);
    }
}

#[tokio::test]
async fn a_transfers_take_is_left_to_the_transfer_on_failure() {
    let (harness, session) = taker().await;
    let owner = harness.address();
    let tampered =
        edited(fixtures::escrow_take(), |p| p["take"]["fills"][0]["payer"] = json!(OTHER));
    serve(&harness, vec![tampered], vec![]).await;
    let request = escrow_request(owner);
    let mut input = TakeInput::new(request.id.clone(), &request.terms);
    input.transfer_intent_id = Some("0192f1c2-3d4e-7f00-8a1b-2c3d4e5f6a40".parse().unwrap());
    let intent = TakeIntent {
        delivery: Delivery::Account(owner),
        payer: owner,
        ..fixtures::escrow_intent()
    };
    let Err(Error::Verification(error)) = take_input(&session, &input, &intent).await else {
        panic!("the take is refused");
    };
    assert_eq!(error.detail, details::FILLS_PAYER);
    assert!(harness.bodies("POST", "/v1/takes").await[0]["transfer_intent_id"].is_string());
    assert_eq!((harness.flow_signatures(), cancellations(&harness).await), (0, 0));
}

#[tokio::test]
async fn invalid_requests_fail_before_any_request() {
    let harness = Harness::new().await;
    let session = harness.session().await;
    let owner = harness.address();
    let terms = |from: VenueId, to: VenueId, recipient| {
        TradeTerms::new(from, to, Amount::from(1_u64), Amount::from(1_u64), recipient)
    };
    let zero = TradeTerms::new(
        VenueId::Hyperliquid,
        VenueId::Polymarket,
        Amount::ZERO,
        Amount::from(1_u64),
        Recipient::Account(owner),
    );
    let cases = [
        (zero, None),
        (terms(VenueId::Hyperliquid, VenueId::Lighter, Recipient::Wallet(owner)), None),
        (
            terms(VenueId::Aster, VenueId::Hyperliquid, Recipient::Account(owner))
                .funding(Funding::Wallet),
            None,
        ),
        (
            terms("orderly".parse().unwrap(), VenueId::Hyperliquid, Recipient::Account(owner)),
            Some(details::VENUE),
        ),
        (
            terms(VenueId::Hyperliquid, VenueId::Polymarket, Recipient::Account(owner))
                .funding("card".parse().unwrap()),
            Some(details::FUNDING),
        ),
    ];
    for (terms, unsupported) in cases {
        let error = session.take(&TakeRequest::new(ClientId::new("t").unwrap(), terms)).await;
        match (error.unwrap_err(), unsupported) {
            (Error::InvalidRequest(_), None) => {}
            (Error::Verification(error), Some(detail)) => {
                assert_eq!((error.kind, error.detail), (VerificationKind::Unsupported, detail));
            }
            (other, _) => panic!("{other:?}"),
        }
    }
    assert!(harness.received_only_auth().await);
}

#[tokio::test]
async fn the_payer_and_recipient_must_be_the_linked_accounts() {
    // `Address::ZERO` stands for the session address.
    for (rows, recipient, expected) in [
        (vec![("hyperliquid", OTHER), ("polymarket", Address::ZERO)], None, details::PAYER),
        (vec![("polymarket", Address::ZERO)], None, details::LINKED_ACCOUNT),
        (vec![("hyperliquid", Address::ZERO)], None, details::LINKED_ACCOUNT),
        (
            vec![("hyperliquid", Address::ZERO), ("polymarket", Address::ZERO)],
            Some(OTHER),
            details::LINKED_ACCOUNT,
        ),
    ] {
        let harness = Harness::new().await;
        let session = harness.session().await;
        let owner = harness.address();
        let rows: Vec<Value> = rows
            .into_iter()
            .map(|(venue, address)| {
                balance(venue, if address.is_zero() { owner } else { address }, None)
            })
            .collect();
        harness.mount("GET", "/v1/balances", vec![ok(Value::Array(rows))]).await;
        let request =
            request(VenueId::Hyperliquid, VenueId::Polymarket, 1, 1, recipient.unwrap_or(owner));
        let Err(Error::Verification(error)) = session.take(&request).await else { panic!() };
        assert_eq!((error.kind, error.detail), (VerificationKind::Changed, expected));
        assert!(harness.requests("POST", "/v1/takes").await.is_empty());
    }
}
