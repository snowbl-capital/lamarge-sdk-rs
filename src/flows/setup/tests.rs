use alloy_primitives::Signature;
use serde_json::{Value, json};
use wiremock::{
    Mock, Request, ResponseTemplate,
    matchers::{method, path},
};

use super::*;
use crate::{
    error::VerificationKind,
    store::{Journal, MemoryStore},
    testing::{NOW, OTHER, edited, fixtures, harness::*, json::fixture, replaced, sample},
};

const PERMIT_ID: &str = "0190f3a4-7b2c-7d10-8e4f-000000000003";
const WALLET_PERMIT: &str = "/v1/wallets/hyperliquid/permit";

/// Whether a request carries a signature (a submission, not a preparation).
fn signed(request: &Request) -> bool {
    request.body_json::<Value>().is_ok_and(|body| body.get("signature").is_some())
}

fn unsigned(request: &Request) -> bool {
    !signed(request)
}

/// A setup step row.
fn row(step: &str, status: &str) -> Value {
    json!({
        "step": step, "status": status, "reason": null, "expires_at": null,
        "needed_for": ["pay"], "to_sign": null, "hash": null,
    })
}

/// The signature a request body carries.
fn signature_of(request: &Request) -> Signature {
    request.body_json::<Value>().unwrap()["signature"].as_str().unwrap().parse().unwrap()
}

fn step_route(venue: &str, step: &str) -> String {
    format!("/v1/accounts/{venue}/setup/{step}")
}

/// Answers `venue`'s setup reads with `steps` (kind, status) lists in turn, the setup the
/// session's.
async fn reads(harness: &Harness, venue: &str, steps: &[&[(&str, &str)]]) {
    let owner = harness.address();
    let answers = steps.iter().map(|steps| {
        let rows: Vec<Value> = steps.iter().map(|(step, status)| row(step, status)).collect();
        let ready = steps.iter().all(|(_, status)| *status == "done");
        ok(json!({"venue": venue, "address": owner, "ready": ready, "steps": rows}))
    });
    harness.mount("GET", &format!("/v1/accounts/{venue}/setup"), answers.collect()).await;
}

/// Answers `route`'s preparations (`signed == false`) or submissions with `answers` in turn.
async fn posts(harness: &Harness, route: &str, signed_only: bool, answers: Vec<ResponseTemplate>) {
    let matcher = if signed_only { signed } else { unsigned };
    Mock::given(method("POST"))
        .and(path(route))
        .and(matcher)
        .respond_with(script(answers))
        .mount(&harness.server)
        .await;
}

/// The preparations and submissions `route` received.
async fn received(harness: &Harness, route: &str) -> (usize, Vec<Request>) {
    let (submitted, prepared): (Vec<_>, Vec<_>) =
        harness.requests("POST", route).await.into_iter().partition(signed);
    (prepared.len(), submitted)
}

/// Serves Lighter's balances (the account index, if any) and Lighter naming `owner` for the
/// account 42.
async fn lighter(harness: &Harness, index: Option<&str>, owner: Address) {
    let rows = json!([balance("lighter", harness.address(), index)]);
    harness.mount("GET", "/v1/balances", vec![ok(rows)]).await;
    harness.lighter_owner(42, owner).await;
}

/// Aster's refusal saying `message`.
fn rejected(message: &str) -> ResponseTemplate {
    ResponseTemplate::new(502).set_body_json(json!({"error": {
        "code": "VENUE_REJECTED", "message": message, "retryable": true,
    }}))
}

/// An Aster approval step under `nonce`.
fn approval(harness: &Harness, nonce: u64) -> ResponseTemplate {
    let fields = replaced(fixtures::approval_fields(), "nonce", &nonce.to_string());
    let mut step = fixtures::approval_step_of(&fixtures::approval_message(&fields));
    step["to_sign"]["ref"] = json!(nonce.to_string());
    ok(harness.owned(&step))
}

#[tokio::test]
async fn link_account_links_the_session_address() {
    let harness = Harness::new().await;
    let session = harness.session().await;
    let mut linked = fixture("me/accounts/0");
    linked["venue"] = json!("hyperliquid");
    linked["address"] = json!(harness.address());
    let other = edited(linked.clone(), |l| l["address"] = json!(OTHER));
    harness.mount("POST", "/v1/accounts/link", vec![ok(linked), ok(other)]).await;
    let account = session.link_account(&VenueId::Hyperliquid).await.unwrap();
    assert_eq!(account.address, harness.address());
    assert_eq!(
        harness.bodies("POST", "/v1/accounts/link").await[0],
        json!({"venue": "hyperliquid", "address": harness.address()})
    );
    let Err(Error::Verification(error)) = session.link_account(&VenueId::Hyperliquid).await else {
        panic!("another address is refused");
    };
    assert_eq!(error.detail, details::ACCOUNT);
    let Err(Error::Verification(error)) = session.link_account(&"orderly".parse().unwrap()).await
    else {
        panic!("an unpinned venue is refused");
    };
    assert_eq!(error.kind, VerificationKind::Unsupported);
    assert_eq!(harness.requests("POST", "/v1/accounts/link").await.len(), 2);
}

#[tokio::test]
async fn a_permit_step_is_checked_signed_submitted_and_awaited() {
    let harness = Harness::new().await;
    let session = harness.session().await;
    let route = step_route("hyperliquid", "permit");
    let step = harness.owned(&fixtures::permit_step());
    reads(
        &harness,
        "hyperliquid",
        &[&[("permit", "required")], &[("permit", "pending")], &[("permit", "done")]],
    )
    .await;
    posts(&harness, &route, false, vec![ok(step.clone())]).await;
    posts(&harness, &route, true, vec![ok(row("permit", "pending"))]).await;
    assert!(session.set_up_venue(&VenueId::Hyperliquid, None).await.unwrap().ready);
    let (prepared, submitted) = received(&harness, &route).await;
    assert_eq!((prepared, submitted.len(), harness.flow_signatures()), (1, 1, 1));
    assert_eq!(submitted[0].body_json::<Value>().unwrap()["ref"], "permit-1");
    let typed = &step["to_sign"]["typed_data"];
    let signer = signature_of(&submitted[0]).recover_address_from_prehash(&digest(typed)).unwrap();
    assert_eq!(signer, harness.address());
}

#[tokio::test]
async fn a_stale_agent_approval_is_prepared_and_signed_once_more() {
    let route = step_route("aster", "agent");
    let first = u64::try_from(NOW).unwrap() * 1_000_000;
    let second = first + 1;
    // Stale once: prepared again under a new nonce and signed once more.
    let harness = Harness::new().await;
    let session = harness.session().await;
    reads(&harness, "aster", &[&[("agent", "required")], &[("agent", "done")]]).await;
    posts(&harness, &route, false, vec![approval(&harness, first), approval(&harness, second)])
        .await;
    posts(&harness, &route, true, vec![rejected("Aster: nonce expired"), ok(row("agent", "done"))])
        .await;
    session.set_up_venue(&VenueId::Aster, None).await.unwrap();
    let (prepared, submitted) = received(&harness, &route).await;
    let refs: Vec<Value> =
        submitted.iter().map(|r| r.body_json::<Value>().unwrap()["ref"].clone()).collect();
    assert_eq!(refs, [json!(first.to_string()), json!(second.to_string())]);
    assert_eq!((prepared, harness.flow_signatures()), (2, 2));
    // Stale twice, or refused otherwise: no third preparation, no second one.
    for (message, prepares) in [("Aster: nonce expired", 2), ("insufficient margin", 1)] {
        let harness = Harness::new().await;
        let session = harness.session().await;
        reads(&harness, "aster", &[&[("agent", "required")]]).await;
        posts(&harness, &route, false, vec![approval(&harness, first)]).await;
        posts(&harness, &route, true, vec![rejected(message)]).await;
        let error = session.set_up_venue(&VenueId::Aster, None).await.unwrap_err();
        assert_eq!(error.code(), Some(&ErrorCode::VenueRejected));
        let (prepared, submitted) = received(&harness, &route).await;
        assert_eq!(
            (prepared, submitted.len(), harness.flow_signatures()),
            (prepares, prepares, prepares)
        );
    }
}

#[tokio::test]
async fn an_unavailable_venue_gets_the_same_submission_three_times() {
    let route = step_route("hyperliquid", "permit");
    let busy = || api_error(502, "VENUE_UNAVAILABLE");
    for (answers, succeeds) in
        [(vec![busy(), busy(), ok(row("permit", "done"))], true), (vec![busy()], false)]
    {
        let harness = Harness::new().await;
        let session = harness.session().await;
        reads(&harness, "hyperliquid", &[&[("permit", "required")], &[("permit", "done")]]).await;
        posts(&harness, &route, false, vec![ok(harness.owned(&fixtures::permit_step()))]).await;
        posts(&harness, &route, true, answers).await;
        let result = session.set_up_venue(&VenueId::Hyperliquid, None).await;
        assert_eq!(result.is_ok(), succeeds, "{result:?}");
        if !succeeds {
            assert_eq!(result.unwrap_err().code(), Some(&ErrorCode::VenueUnavailable));
        }
        let (prepared, submitted) = received(&harness, &route).await;
        assert_eq!((prepared, submitted.len(), harness.flow_signatures()), (1, 3, 1));
        assert!(submitted.iter().all(|request| request.body == submitted[0].body));
    }
}

#[tokio::test]
async fn a_lighter_key_registration_is_signed_for_an_account_lighter_says_is_ours() {
    let harness = Harness::new().await;
    let session = harness.session().await;
    let route = step_route("lighter", "api_key");
    lighter(&harness, Some("42"), harness.address()).await;
    reads(&harness, "lighter", &[&[("api_key", "required")], &[("api_key", "done")]]).await;
    let step = harness.owned(&fixtures::registration_step());
    posts(&harness, &route, false, vec![ok(step.clone())]).await;
    posts(&harness, &route, true, vec![ok(row("api_key", "pending")), ok(row("api_key", "done"))])
        .await;
    session.set_up_venue(&VenueId::Lighter, Some(&[SetupStepKind::ApiKey])).await.unwrap();
    let (_, submitted) = received(&harness, &route).await;
    let message = step["to_sign"]["message"].as_str().unwrap();
    let signer = signature_of(&submitted[0]).recover_address_from_msg(message).unwrap();
    assert_eq!((signer, harness.flow_signatures()), (harness.address(), 1));
}

/// One case per input the flow supplies to the checks: the step asked, the setup's and Lighter's
/// owners, the linked Lighter account. Each shows that nothing is signed or submitted.
#[tokio::test]
async fn a_failed_setup_check_signs_nothing() {
    type Serve = fn(&Harness) -> (VenueId, Value, Option<(Option<&'static str>, bool)>);
    let cases: [(Serve, &str); 4] = [
        // The preparation answers another step than asked.
        (
            |h| {
                let step =
                    edited(h.owned(&fixtures::permit_step()), |s| s["step"] = json!("agent"));
                (VenueId::Hyperliquid, step, None)
            },
            details::SETUP_STEP,
        ),
        // Lighter says the account belongs to someone else.
        (
            |h| {
                (
                    VenueId::Lighter,
                    h.owned(&fixtures::registration_step()),
                    Some((Some("42"), false)),
                )
            },
            details::LIGHTER_OWNER,
        ),
        // No Lighter account is linked.
        (
            |h| (VenueId::Lighter, h.owned(&fixtures::registration_step()), Some((None, true))),
            details::LINKED_ACCOUNT,
        ),
        // The permit asks another address to sign.
        (
            |h| {
                let step = edited(h.owned(&fixtures::permit_step()), |s| {
                    s["to_sign"]["signer"] = json!(OTHER);
                });
                (VenueId::Hyperliquid, step, None)
            },
            details::TO_SIGN_SIGNER,
        ),
    ];
    for (serve, expected) in cases {
        let harness = Harness::new().await;
        let session = harness.session().await;
        let (venue, step, lighter_read) = serve(&harness);
        let kind = step["to_sign"]["kind"].as_str().unwrap();
        let name = if kind == "lighter_api_key" { "api_key" } else { "permit" };
        if let Some((index, ours)) = lighter_read {
            let owner = if ours { harness.address() } else { OTHER };
            lighter(&harness, index, owner).await;
        }
        reads(&harness, venue.as_str(), &[&[(name, "required")]]).await;
        let route = step_route(venue.as_str(), name);
        posts(&harness, &route, false, vec![ok(step)]).await;
        let Err(Error::Verification(error)) = session.set_up_venue(&venue, None).await else {
            panic!("{expected} is refused");
        };
        assert_eq!(error.detail, expected);
        assert_eq!(harness.flow_signatures(), 0);
        assert!(received(&harness, &route).await.1.is_empty());
    }
    // The setup is another address's: nothing is prepared.
    let harness = Harness::new().await;
    let session = harness.session().await;
    let setup = json!({"venue": "hyperliquid", "address": OTHER, "ready": false,
                       "steps": [row("permit", "required")]});
    harness.mount("GET", "/v1/accounts/hyperliquid/setup", vec![ok(setup)]).await;
    let Err(Error::Verification(error)) = session.set_up_venue(&VenueId::Hyperliquid, None).await
    else {
        panic!("another owner is refused");
    };
    assert_eq!(error.detail, details::OWNER);
    assert_eq!(received(&harness, &step_route("hyperliquid", "permit")).await.0, 0);
}

#[tokio::test]
async fn a_step_never_confirmed_is_incomplete() {
    let route = step_route("hyperliquid", "permit");
    // Pending past the confirmation wait, or answered `required` again.
    for (answer, later) in
        [("pending", &[("permit", "pending")]), ("required", &[("permit", "required")])]
    {
        let harness = Harness::new().await;
        let session = harness.session().await;
        reads(&harness, "hyperliquid", &[&[("permit", "required")], later]).await;
        posts(&harness, &route, false, vec![ok(harness.owned(&fixtures::permit_step()))]).await;
        posts(&harness, &route, true, vec![ok(row("permit", answer))]).await;
        let error = session.set_up_venue(&VenueId::Hyperliquid, None).await.unwrap_err();
        assert!(
            matches!(&error, Error::SetupIncomplete { step: SetupStepKind::Permit, .. }),
            "{error:?}"
        );
        assert_eq!(harness.flow_signatures(), 1);
    }
}

#[tokio::test]
async fn only_steps_asked_and_not_done_run() {
    let harness = Harness::new().await;
    let session = harness.session().await;
    reads(&harness, "hyperliquid", &[&[("permit", "required"), ("agent", "done")]]).await;
    // Agent is done and permit not asked: nothing is prepared, and the setup is complete for it.
    let setup = session.set_up_venue(&VenueId::Hyperliquid, Some(&[SetupStepKind::Agent])).await;
    assert!(!setup.unwrap().ready);
    assert!(harness.requests("POST", &step_route("hyperliquid", "permit")).await.is_empty());
    // A step asked and still required is reported once nothing more can be done.
    posts(
        &harness,
        &step_route("hyperliquid", "permit"),
        false,
        vec![ok(row("permit", "pending"))],
    )
    .await;
    let error = session.set_up_venue(&VenueId::Hyperliquid, Some(&[SetupStepKind::Permit])).await;
    assert!(matches!(error, Err(Error::SetupIncomplete { .. })), "{error:?}");
    assert_eq!(harness.flow_signatures(), 0);
}

// Wallet permits.

/// The wallet permission of the session: `authorized`/`pending` flags and, when neither, the
/// permit to sign.
fn permission(harness: &Harness, authorized: bool, pending: bool, permit: bool) -> Value {
    json!({
        "address": harness.address(), "authorized": authorized, "pending": pending, "hash": null,
        "permit": permit.then(|| harness.owned(&fixtures::wallet_permit())),
    })
}

/// A session on `journal` whose permit preparations and submissions are answered in turn.
async fn wallet(
    journal: Journal,
    prepared: impl Fn(&Harness) -> Vec<ResponseTemplate>,
    submitted: impl Fn(&Harness) -> Vec<ResponseTemplate>,
) -> (Harness, Session) {
    let harness = Harness::new().await;
    let session = harness.session_with(journal).await;
    posts(&harness, WALLET_PERMIT, false, prepared(&harness)).await;
    let submitted = submitted(&harness);
    if !submitted.is_empty() {
        posts(&harness, WALLET_PERMIT, true, submitted).await;
    }
    (harness, session)
}

fn to_sign(harness: &Harness) -> Vec<ResponseTemplate> {
    vec![ok(permission(harness, false, false, true))]
}

async fn permit_record(session: &Session) -> Option<Vec<u8>> {
    session.journal().load(&session.record_key("wallet-permit:hyperliquid")).await.unwrap()
}

#[tokio::test]
async fn a_wallet_permit_is_saved_before_it_is_submitted_and_deleted_after() {
    let queued = |h: &Harness| vec![ok(permission(h, false, true, false))];
    let (harness, session) = wallet(Journal::new(MemoryStore::new()), to_sign, queued).await;
    assert!(session.authorize_wallet(&VenueId::Hyperliquid).await.unwrap().pending);
    let (_, submitted) = received(&harness, WALLET_PERMIT).await;
    let body: Value = submitted[0].body_json().unwrap();
    assert_eq!((&body["payer"], &body["ref"]), (&json!(harness.address()), &json!(PERMIT_ID)));
    let typed = harness.owned(&sample("permit"));
    let signer = signature_of(&submitted[0]).recover_address_from_prehash(&digest(&typed)).unwrap();
    assert_eq!((signer, harness.flow_signatures()), (harness.address(), 1));
    assert_eq!(permit_record(&session).await, None);
    let prepared = harness.bodies("POST", WALLET_PERMIT).await;
    assert_eq!(prepared[0], json!({"payer": harness.address()}));
}

#[tokio::test]
async fn an_unanswered_permit_submission_is_resumed_with_the_saved_signature() {
    let answers =
        |h: &Harness| vec![ResponseTemplate::new(503), ok(permission(h, true, false, false))];
    let (harness, session) = wallet(Journal::new(MemoryStore::new()), to_sign, answers).await;
    let error = session.authorize_wallet(&VenueId::Hyperliquid).await.unwrap_err();
    assert_eq!(error.status(), Some(503));
    assert!(permit_record(&session).await.is_some());
    assert!(session.authorize_wallet(&VenueId::Hyperliquid).await.unwrap().authorized);
    let (_, submitted) = received(&harness, WALLET_PERMIT).await;
    assert_eq!(submitted.len(), 2);
    assert_eq!(submitted[0].body, submitted[1].body);
    assert_eq!(harness.flow_signatures(), 1);
    assert_eq!(permit_record(&session).await, None);
}

#[tokio::test]
async fn an_authorized_wallet_needs_no_signature_and_clears_its_record() {
    let authorized = |h: &Harness| vec![ok(permission(h, true, false, false))];
    let (harness, session) = wallet(Journal::new(MemoryStore::new()), authorized, |_| vec![]).await;
    let key = session.record_key("wallet-permit:hyperliquid");
    session.journal().save(&key, b"{\"v\":1}").await.unwrap();
    assert!(session.authorize_wallet(&VenueId::Hyperliquid).await.unwrap().authorized);
    assert_eq!(harness.flow_signatures(), 0);
    assert!(received(&harness, WALLET_PERMIT).await.1.is_empty());
    assert_eq!(permit_record(&session).await, None);
}

/// The answer's payer and the permit to sign come from the server: each refusal signs nothing.
#[tokio::test]
async fn a_failed_wallet_permit_check_signs_nothing() {
    type Answer = fn(&Harness) -> Vec<ResponseTemplate>;
    let cases: [(Answer, &str); 2] = [
        (
            |h| {
                vec![ok(edited(permission(h, false, false, true), |a| a["address"] = json!(OTHER)))]
            },
            details::PAYER,
        ),
        (|h| vec![ok(permission(h, false, false, false))], details::TO_SIGN),
    ];
    for (answer, expected) in cases {
        let (harness, session) = wallet(Journal::new(MemoryStore::new()), answer, |_| vec![]).await;
        let Err(Error::Verification(error)) = session.authorize_wallet(&VenueId::Hyperliquid).await
        else {
            panic!("{expected} is refused");
        };
        assert_eq!(error.detail, expected);
        assert_eq!(harness.flow_signatures(), 0);
        assert!(received(&harness, WALLET_PERMIT).await.1.is_empty());
        assert_eq!(permit_record(&session).await, None);
    }
}

#[tokio::test]
async fn a_saved_permit_signature_is_reused_only_for_the_same_terms() {
    let authorized = |h: &Harness| vec![ok(permission(h, true, false, false))];
    let (harness, session) = wallet(Journal::new(MemoryStore::new()), to_sign, authorized).await;
    // Saved for the same permit id, over other terms.
    let mut signature = [1; 65];
    signature[64] = 27;
    let saved = WalletPermitRecord {
        id: PERMIT_ID.parse().unwrap(),
        digest: B256::repeat_byte(9),
        signature: SignatureBytes::new(signature).unwrap(),
    };
    let key = session.record_key("wallet-permit:hyperliquid");
    records::save(session.journal(), &key, &saved, false).await.unwrap();
    let Err(Error::Verification(error)) = session.authorize_wallet(&VenueId::Hyperliquid).await
    else {
        panic!("a signature of other terms is refused");
    };
    assert_eq!((error.kind, error.detail), (VerificationKind::Changed, details::DIGEST));
    assert_eq!(harness.flow_signatures(), 0);
    assert!(received(&harness, WALLET_PERMIT).await.1.is_empty());
}

#[tokio::test]
async fn a_permit_neither_authorized_nor_queued_is_incomplete() {
    let neither = |h: &Harness| vec![ok(permission(h, false, false, false))];
    let (_harness, session) = wallet(Journal::new(MemoryStore::new()), to_sign, neither).await;
    let error = session.authorize_wallet(&VenueId::Hyperliquid).await.unwrap_err();
    assert!(
        matches!(&error, Error::SetupIncomplete { step: SetupStepKind::Permit, .. }),
        "{error:?}"
    );
    // Kept: the server may still take it.
    assert!(permit_record(&session).await.is_some());
}

#[tokio::test]
async fn a_permit_signature_that_cannot_be_saved_is_not_submitted() {
    let authorized = |h: &Harness| vec![ok(permission(h, true, false, false))];
    let journal = Journal::new(FlakyStore::failing_save(1));
    let (harness, session) = wallet(journal, to_sign, authorized).await;
    let Err(Error::Store(error)) = session.authorize_wallet(&VenueId::Hyperliquid).await else {
        panic!("the save fails")
    };
    assert!(!error.submitted());
    assert!(received(&harness, WALLET_PERMIT).await.1.is_empty());
}

#[tokio::test]
async fn wallet_permits_need_a_venue_with_wallet_funding() {
    let harness = Harness::new().await;
    let session = harness.session().await;
    for venue in [VenueId::Lighter, VenueId::Aster, "orderly".parse().unwrap()] {
        let Err(Error::Verification(error)) = session.authorize_wallet(&venue).await else {
            panic!("{venue} is unsupported");
        };
        assert_eq!(error.kind, VerificationKind::Unsupported);
    }
    assert!(harness.received_only_auth().await);
}

#[tokio::test]
async fn concurrent_authorizations_sharing_a_journal_sign_once() {
    let journal = Journal::new(MemoryStore::new());
    // The first submission is lost; the second session then resumes the saved signature.
    let answers =
        |h: &Harness| vec![ResponseTemplate::new(503), ok(permission(h, false, true, false))];
    let (harness, session) = wallet(journal.clone(), to_sign, answers).await;
    let other = harness.session_with(journal).await;
    let (first, second) = futures_util::future::join(
        session.authorize_wallet(&VenueId::Hyperliquid),
        other.authorize_wallet(&VenueId::Hyperliquid),
    )
    .await;
    assert!(first.is_ok() != second.is_ok(), "{first:?} {second:?}");
    assert_eq!(harness.flow_signatures(), 1);
    let (_, submitted) = received(&harness, WALLET_PERMIT).await;
    assert_eq!(submitted.len(), 2);
    assert_eq!(submitted[0].body, submitted[1].body);
}
