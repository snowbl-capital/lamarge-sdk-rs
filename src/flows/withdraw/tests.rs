use alloy_primitives::{Address, address};
use serde_json::{Value, json};
use wiremock::{
    Mock, ResponseTemplate,
    matchers::{method, path},
};

use super::*;
use crate::{
    error::VerificationKind,
    protocol::polymarket,
    store::{Journal, MemoryStore},
    testing::{MAKER, NOW, OTHER, harness::*, json::fixture, sample},
};

/// The session's Polymarket venue wallet, and another one.
const VENUE_WALLET: Address = address!("5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a");
const MOVED_WALLET: Address = address!("6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b");
const PREPARE: &str = "/v1/withdrawals/prepare";
const SUBMIT: &str = "/v1/withdrawals";
/// One USDT in base units.
const USDT: u128 = 1_000_000_000_000_000_000;

/// A Hyperliquid withdrawal of 12.5 USDC to the signer through the bridge, at [`NOW`].
fn hyperliquid_withdrawal() -> Value {
    sample("send_asset_withdrawal")
}

/// A Hyperliquid send of 5.25 USDC to `recipient`.
fn hyperliquid_send(recipient: Address) -> Value {
    let mut typed = sample("send_asset_send");
    typed["message"]["destination"] = json!(format!("{recipient:#x}"));
    typed
}

/// A Polymarket withdrawal of 10 pUSD from `owner` to `to`.
fn polymarket_withdrawal(owner: Address, to: Address) -> Value {
    let mut typed = sample("polymarket_withdraw");
    typed["message"]["account"] = json!(owner);
    typed["message"]["to"] = json!(to);
    typed
}

/// A Polymarket internal transfer of 250.5 pUSD from `owner` to `recipient`.
fn polymarket_send(owner: Address, recipient: Address) -> Value {
    let mut typed = sample("polymarket_internal_transfer");
    typed["op"]["args"]["account"] = json!(owner.to_checksum(None));
    typed["op"]["args"]["to"] = json!(recipient.to_checksum(None));
    let data = polymarket::InternalTransfer::read(&typed["op"]).unwrap().hash().unwrap();
    typed["message"]["data"] = json!(data);
    typed
}

/// An Aster send of 5 USDT from `user` to `recipient`, its nonce [`NOW`].
fn aster_send(user: Address, recipient: Address) -> Value {
    let fields = [
        ("asset", "USDT".to_owned()),
        ("amount", "5".to_owned()),
        ("toAddress", format!("{recipient:#x}")),
        ("clientTranId", format!("s{}", "ab".repeat(16))),
        ("nonce", (NOW * 1_000_000).to_string()),
        ("user", format!("{user:#x}")),
    ];
    let msg: Vec<String> = fields.iter().map(|(name, value)| format!("{name}={value}")).collect();
    let mut typed = sample("aster_transfer");
    typed["message"] = json!({"msg": msg.join("&")});
    typed
}

/// The server's record of a withdrawal on `venue` in `state`.
fn withdrawal(venue: &str, kind: &str, state: &str) -> Value {
    let mut row = fixture("transfer_page/data/0/withdrawal");
    row["venue"] = json!(venue);
    row["kind"] = json!(kind);
    row["state"] = json!(state);
    row["from_dex"] = json!(null);
    row
}

fn relayed() -> ResponseTemplate {
    ok(withdrawal("hyperliquid", "withdrawal", "relayed"))
}

fn request(id: &str, venue: VenueId, units: u128, to: WithdrawTo) -> WithdrawalRequest {
    WithdrawalRequest::new(ClientId::new(id).unwrap(), venue, Amount::from(units), to)
}

fn hyperliquid_request() -> WithdrawalRequest {
    request("w-1", VenueId::Hyperliquid, 12_500_000, WithdrawTo::Signer)
}

/// A signed-in session linked on every venue, Polymarket's venue wallets read as `wallets` in
/// turn; the withdrawal prepared as `typed`, its submissions answered with `submitted`.
async fn withdrawer(
    journal: Journal,
    wallets: &[Address],
    typed: Option<Value>,
    submitted: Vec<ResponseTemplate>,
) -> (Harness, Session) {
    let harness = Harness::new().await;
    let session = harness.session_with(journal).await;
    let owner = harness.address();
    let answers = wallets.iter().map(|wallet| {
        let wallet = format!("{wallet:#x}");
        let rows = ["hyperliquid", "aster"]
            .map(|venue| balance(venue, owner, None))
            .into_iter()
            .chain([balance("polymarket", owner, Some(&wallet))])
            .collect();
        ok(Value::Array(rows))
    });
    harness.mount("GET", "/v1/balances", answers.collect()).await;
    if let Some(typed) = typed {
        harness.mount("POST", PREPARE, vec![ok(json!({"typed_data": typed}))]).await;
    }
    if !submitted.is_empty() {
        harness.mount("POST", SUBMIT, submitted).await;
    }
    (harness, session)
}

async fn record(session: &Session, id: &str) -> Option<Value> {
    let key = session.record_key(&format!("withdrawal:{id}"));
    let bytes = session.journal().load(&key).await.unwrap()?;
    Some(serde_json::from_slice(&bytes).unwrap())
}

async fn count(harness: &Harness, verb: &str, route: &str) -> usize {
    harness.requests(verb, route).await.len()
}

#[tokio::test]
async fn withdrawals_and_sends_are_checked_signed_saved_and_relayed() {
    type Build = fn(Address) -> Value;
    let cases: [(WithdrawalRequest, Build, Value, &str); 6] = [
        (
            request("w-hl", VenueId::Hyperliquid, 12_500_000, WithdrawTo::Signer),
            |_| hyperliquid_withdrawal(),
            json!({"venue": "hyperliquid", "amount": "12500000", "to": "signer"}),
            "withdrawal",
        ),
        (
            request("w-pm", VenueId::Polymarket, 10_000_000, WithdrawTo::Signer),
            |owner| polymarket_withdrawal(owner, owner),
            json!({"venue": "polymarket", "amount": "10000000", "to": "signer"}),
            "withdrawal",
        ),
        (
            request(
                "w-pmw",
                VenueId::Polymarket,
                10_000_000,
                WithdrawTo::VenueWallet(VENUE_WALLET),
            ),
            |owner| polymarket_withdrawal(owner, VENUE_WALLET),
            json!({"venue": "polymarket", "amount": "10000000", "to": "wallet"}),
            "withdrawal",
        ),
        (
            request("s-hl", VenueId::Hyperliquid, 5_250_000, WithdrawTo::Account(MAKER)),
            |_| hyperliquid_send(MAKER),
            json!({"venue": "hyperliquid", "amount": "5250000", "to": "account", "recipient": MAKER}),
            "send",
        ),
        (
            request("s-pm", VenueId::Polymarket, 250_500_000, WithdrawTo::Account(MAKER)),
            |owner| polymarket_send(owner, MAKER),
            json!({"venue": "polymarket", "amount": "250500000", "to": "account", "recipient": MAKER}),
            "send",
        ),
        (
            request("s-as", VenueId::Aster, 5 * USDT, WithdrawTo::Account(MAKER)),
            |owner| aster_send(owner, MAKER),
            json!({
                "venue": "aster", "amount": (5 * USDT).to_string(), "to": "account",
                "recipient": MAKER,
            }),
            "send",
        ),
    ];
    for (request, build, body, kind) in cases {
        let venue = request.venue.as_str().to_owned();
        let answer = ok(withdrawal(&venue, kind, "relayed"));
        let journal = Journal::new(MemoryStore::new());
        let (harness, session) = withdrawer(journal, &[VENUE_WALLET], None, vec![answer]).await;
        let typed = build(harness.address());
        harness.mount("POST", PREPARE, vec![ok(json!({"typed_data": typed}))]).await;
        let result = session.withdraw(&request).await.unwrap();
        assert_eq!(result.state, WithdrawalState::Relayed);
        assert_eq!(harness.bodies("POST", PREPARE).await, [body], "{venue}");
        let submitted = &harness.bodies("POST", SUBMIT).await[0];
        assert_eq!((&submitted["venue"], &submitted["typed_data"]), (&json!(venue), &typed));
        let signer = signer_of(submitted, &digest(&typed));
        assert_eq!((signer, harness.flow_signatures()), (harness.address(), 1), "{venue}");
        let record = record(&session, request.id.as_str()).await.unwrap();
        assert_eq!(record["result"]["state"], "relayed");
        // A final outcome answers the same request again without any request.
        assert_eq!(session.withdraw(&request).await.unwrap(), result);
        assert_eq!(count(&harness, "GET", "/v1/balances").await, 1);
        assert_eq!(count(&harness, "POST", SUBMIT).await, 1);
    }
}

#[tokio::test]
async fn a_refused_withdrawal_spends_its_id() {
    for (status, code) in [(502, "VENUE_REJECTED"), (400, "WITHDRAWAL_EXPIRED")] {
        let journal = Journal::new(MemoryStore::new());
        let submitted = vec![api_error(status, code)];
        let typed = Some(hyperliquid_withdrawal());
        let (harness, session) = withdrawer(journal, &[VENUE_WALLET], typed, submitted).await;
        let error = session.withdraw(&hyperliquid_request()).await.unwrap_err();
        assert_eq!(error.code().map(ErrorCode::as_str), Some(code));
        let error = session.withdraw(&hyperliquid_request()).await.unwrap_err();
        let Error::WithdrawalRefused { id, code: refused } = error else { panic!("{error:?}") };
        assert_eq!((id.as_str(), refused.as_str()), ("w-1", code));
        assert_eq!(count(&harness, "POST", SUBMIT).await, 1);
        assert_eq!(count(&harness, "POST", PREPARE).await, 1);
    }
}

#[tokio::test]
async fn the_same_id_for_another_withdrawal_is_refused() {
    let journal = Journal::new(MemoryStore::new());
    let typed = Some(hyperliquid_withdrawal());
    let (harness, session) = withdrawer(journal, &[VENUE_WALLET], typed, vec![relayed()]).await;
    session.withdraw(&hyperliquid_request()).await.unwrap();
    for other in [
        request("w-1", VenueId::Hyperliquid, 12_500_001, WithdrawTo::Signer),
        request("w-1", VenueId::Hyperliquid, 12_500_000, WithdrawTo::Account(MAKER)),
        request("w-1", VenueId::Polymarket, 12_500_000, WithdrawTo::Signer),
    ] {
        let error = session.withdraw(&other).await.unwrap_err();
        assert!(matches!(error, Error::InvalidRequest(_)), "{error:?}");
    }
    assert_eq!(count(&harness, "GET", "/v1/balances").await, 1);
}

#[tokio::test]
async fn a_saved_signature_is_submitted_again_after_a_failed_submission() {
    let answers = vec![
        ResponseTemplate::new(503),
        ok(withdrawal("hyperliquid", "withdrawal", "submitting")),
        relayed(),
    ];
    let journal = Journal::new(MemoryStore::new());
    let typed = Some(hyperliquid_withdrawal());
    let (harness, session) = withdrawer(journal, &[VENUE_WALLET], typed, answers).await;
    let request = hyperliquid_request();
    assert_eq!(session.withdraw(&request).await.unwrap_err().status(), Some(503));
    // Not final: the next run submits again.
    assert_eq!(session.withdraw(&request).await.unwrap().state, WithdrawalState::Submitting);
    assert_eq!(session.withdraw(&request).await.unwrap().state, WithdrawalState::Relayed);
    let submitted = harness.requests("POST", SUBMIT).await;
    assert_eq!(submitted.len(), 3);
    assert!(submitted.iter().all(|request| request.body == submitted[0].body));
    assert_eq!((harness.flow_signatures(), count(&harness, "POST", PREPARE).await), (1, 1));
}

#[tokio::test]
async fn a_saved_signature_is_checked_again_before_it_is_submitted_again() {
    let journal = Journal::new(MemoryStore::new());
    let submitted = vec![ResponseTemplate::new(503)];
    let wallets = [VENUE_WALLET, MOVED_WALLET];
    let (harness, session) = withdrawer(journal, &wallets, None, submitted).await;
    let typed = polymarket_withdrawal(harness.address(), VENUE_WALLET);
    harness.mount("POST", PREPARE, vec![ok(json!({"typed_data": typed}))]).await;
    let request =
        request("w-1", VenueId::Polymarket, 10_000_000, WithdrawTo::VenueWallet(VENUE_WALLET));
    session.withdraw(&request).await.unwrap_err();
    // The venue wallet moved meanwhile: the saved withdrawal to the old one is not sent again.
    let Err(Error::Verification(error)) = session.withdraw(&request).await else { panic!() };
    assert_eq!(error.detail, details::LINKED_ACCOUNT);
    assert_eq!(count(&harness, "POST", SUBMIT).await, 1);
}

#[tokio::test]
async fn a_venue_wallet_withdrawal_goes_only_to_the_wallet_the_caller_names() {
    // The server names another wallet both as the linked one and as the destination.
    let journal = Journal::new(MemoryStore::new());
    let (harness, session) = withdrawer(journal, &[OTHER], None, vec![relayed()]).await;
    let typed = polymarket_withdrawal(harness.address(), OTHER);
    harness.mount("POST", PREPARE, vec![ok(json!({"typed_data": typed}))]).await;
    let request =
        request("w-1", VenueId::Polymarket, 10_000_000, WithdrawTo::VenueWallet(VENUE_WALLET));
    let Err(Error::Verification(error)) = session.withdraw(&request).await else { panic!() };
    assert_eq!((error.kind, error.detail), (VerificationKind::Changed, details::LINKED_ACCOUNT));
    assert_eq!(harness.flow_signatures(), 0);
    assert_eq!(count(&harness, "POST", SUBMIT).await, 0);
    assert!(record(&session, "w-1").await.is_none());
}

#[tokio::test]
async fn store_failures_tell_whether_the_withdrawal_was_submitted() {
    // The signature cannot be saved: nothing is submitted.
    let journal = Journal::new(FlakyStore::failing_save(1));
    let typed = Some(hyperliquid_withdrawal());
    let (harness, session) = withdrawer(journal, &[VENUE_WALLET], typed, vec![relayed()]).await;
    let Err(Error::Store(error)) = session.withdraw(&hyperliquid_request()).await else {
        panic!("the save fails")
    };
    assert!(!error.submitted());
    assert_eq!(count(&harness, "POST", SUBMIT).await, 0);
    // The outcome cannot be saved: submitted; the same id then returns the server's record.
    let journal = Journal::new(FlakyStore::failing_save(2));
    let typed = Some(hyperliquid_withdrawal());
    let (harness, session) = withdrawer(journal, &[VENUE_WALLET], typed, vec![relayed()]).await;
    let Err(Error::Store(error)) = session.withdraw(&hyperliquid_request()).await else {
        panic!("the save fails")
    };
    assert!(error.submitted());
    let result = session.withdraw(&hyperliquid_request()).await.unwrap();
    assert_eq!(result.state, WithdrawalState::Relayed);
    assert_eq!((harness.flow_signatures(), count(&harness, "POST", PREPARE).await), (1, 1));
}

#[tokio::test]
async fn sessions_sharing_a_journal_withdraw_once() {
    let journal = Journal::new(MemoryStore::new());
    let typed = Some(hyperliquid_withdrawal());
    let (harness, session) =
        withdrawer(journal.clone(), &[VENUE_WALLET], typed, vec![relayed()]).await;
    let other = harness.session_with(journal).await;
    let request = hyperliquid_request();
    let (first, second) =
        futures_util::future::join(session.withdraw(&request), other.withdraw(&request)).await;
    assert_eq!(first.unwrap(), second.unwrap());
    assert_eq!(harness.flow_signatures(), 1);
    assert_eq!(count(&harness, "POST", PREPARE).await, 1);
    assert_eq!(count(&harness, "POST", SUBMIT).await, 1);
}

/// One case per input the flow supplies to the checks: the clock, the linked venue wallet and
/// the session address. Each shows that nothing is signed, submitted or saved.
#[tokio::test]
async fn a_failed_check_signs_nothing() {
    type Build = fn(Address) -> Value;
    let cases: [(WithdrawalRequest, Build, &str); 3] = [
        (
            hyperliquid_request(),
            |_| {
                let mut typed = hyperliquid_withdrawal();
                typed["message"]["nonce"] = json!((NOW - 61) * 1000);
                typed
            },
            details::NONCE,
        ),
        (
            request("w-pm", VenueId::Polymarket, 10_000_000, WithdrawTo::VenueWallet(VENUE_WALLET)),
            |owner| polymarket_withdrawal(owner, OTHER),
            details::TO,
        ),
        (
            request("s-as", VenueId::Aster, 5 * USDT, WithdrawTo::Account(MAKER)),
            |_| aster_send(OTHER, MAKER),
            details::USER,
        ),
    ];
    for (request, build, expected) in cases {
        let journal = Journal::new(MemoryStore::new());
        let (harness, session) = withdrawer(journal, &[VENUE_WALLET], None, vec![]).await;
        let typed = build(harness.address());
        harness.mount("POST", PREPARE, vec![ok(json!({"typed_data": typed}))]).await;
        Mock::given(method("POST"))
            .and(path(SUBMIT))
            .respond_with(relayed())
            .expect(0)
            .mount(&harness.server)
            .await;
        let Err(Error::Verification(error)) = session.withdraw(&request).await else { panic!() };
        assert_eq!(error.detail, expected);
        assert_eq!(harness.flow_signatures(), 0);
        assert!(record(&session, request.id.as_str()).await.is_none());
    }
}

#[tokio::test]
async fn unsupported_withdrawals_fail_before_any_request() {
    let harness = Harness::new().await;
    let session = harness.session().await;
    let owner = harness.address();
    let unsupported = Some(VerificationKind::Unsupported);
    let cases = [
        (request("x", VenueId::Gmx, 1, WithdrawTo::Signer), unsupported),
        (request("x", VenueId::Aster, 1, WithdrawTo::Signer), unsupported),
        (request("x", VenueId::Lighter, 1, WithdrawTo::Account(MAKER)), unsupported),
        (request("x", VenueId::Hyperliquid, 1, WithdrawTo::VenueWallet(VENUE_WALLET)), None),
        (request("x", VenueId::Hyperliquid, 1, WithdrawTo::Account(owner)), None),
        (request("x", VenueId::Polymarket, 0, WithdrawTo::Signer), None),
    ];
    for (request, kind) in cases {
        match (session.withdraw(&request).await.unwrap_err(), kind) {
            (Error::InvalidRequest(_), None) => {}
            (Error::Verification(error), Some(kind)) => assert_eq!(error.kind, kind),
            (other, _) => panic!("{request:?}: {other:?}"),
        }
    }
    assert!(harness.received_only_auth().await);
}

#[tokio::test]
async fn the_account_withdrawn_from_must_be_the_sessions() {
    let harness = Harness::new().await;
    let session = harness.session().await;
    let rows =
        vec![balance("hyperliquid", OTHER, None), balance("polymarket", harness.address(), None)];
    harness.mount("GET", "/v1/balances", vec![ok(Value::Array(rows))]).await;
    for request in [
        request("w-1", VenueId::Hyperliquid, 1, WithdrawTo::Signer),
        request("w-2", VenueId::Polymarket, 1, WithdrawTo::VenueWallet(VENUE_WALLET)),
        request("w-3", VenueId::Aster, 1, WithdrawTo::Account(MAKER)),
    ] {
        let Err(Error::Verification(error)) = session.withdraw(&request).await else { panic!() };
        assert_eq!(
            (error.kind, error.detail),
            (VerificationKind::Changed, details::LINKED_ACCOUNT)
        );
    }
    assert_eq!(count(&harness, "POST", PREPARE).await, 0);
}
