#![expect(clippy::items_after_statements, reason = "test-local types next to their use")]

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use wiremock::{
    Mock, ResponseTemplate,
    matchers::{header, method, path},
};

use super::*;
use crate::{
    error::VerificationKind,
    store::MemoryStore,
    testing::harness::*,
    types::{EscrowEnvironment, VenueId},
};

/// The signed-in user `GET /v1/me` answers, for `address`.
fn me(address: Address) -> Value {
    let mut me: Value = crate::testing::json::fixture("me");
    me["master_address"] = json!(address);
    me
}

/// Serves `GET /v1/me` for `address`, to requests carrying `token` only.
async fn serve_me(harness: &Harness, token: &str, address: Address) {
    Mock::given(method("GET"))
        .and(path("/v1/me"))
        .and(header("authorization", format!("Bearer {token}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(me(address)))
        .mount(&harness.server)
        .await;
}

/// Serves revocations of session `n`, by requests carrying `token`, expecting `times` of them.
async fn expect_revocation(harness: &Harness, n: usize, token: &str, times: u64) {
    Mock::given(method("DELETE"))
        .and(path(format!("/v1/auth/sessions/{}", session_id(n))))
        .and(header("authorization", format!("Bearer {token}")))
        .respond_with(ResponseTemplate::new(204))
        .expect(times)
        .mount(&harness.server)
        .await;
}

fn saved(session: &Session) -> SavedSession {
    session.export().expect("signed in")
}

#[tokio::test]
async fn sign_in_signs_the_checked_challenge_once() {
    let harness = Harness::new().await;
    let session = harness.session().await;
    // The server verified the signature over the text it issued.
    assert_eq!(harness.signatures(), 1);
    assert_eq!(session.address(), harness.address());
    assert_eq!(session.escrow_environment(), EscrowEnvironment::Production);
    let saved = saved(&session);
    assert_eq!(saved.token.expose_secret(), token(1));
    assert_eq!(saved.session_id.to_string(), session_id(1));
    assert_eq!(session.expires_at(), Some(saved.expires_at));
    let asked = harness.requests("POST", "/v1/auth/challenge").await;
    let body: Value = asked[0].body_json().unwrap();
    assert_eq!(body, json!({"address": harness.address()}));
    serve_me(&harness, &token(1), harness.address()).await;
    assert_eq!(session.account().me().await.unwrap().master_address, harness.address());
    // Every flow's future is `Send` (built, never polled).
    fn send<T: Send>(_: T) {}
    send(session.renew());
    send(session.sign_out());
    send(session.link_account(&VenueId::Hyperliquid));
    send(session.set_up_venue(&VenueId::Hyperliquid, None));
    send(session.authorize_wallet(&VenueId::Hyperliquid));
}

/// Signs in on `harness`, expecting a refusal of `kind` with nothing signed or verified.
async fn refused_without_signing(harness: &Harness, kind: VerificationKind) -> &'static str {
    let journal = Journal::new(MemoryStore::new());
    let error = harness.client.sign_in(harness.signer(), journal).await.unwrap_err();
    let Error::Verification(error) = error else { panic!("{error:?}") };
    assert_eq!(error.kind, kind);
    assert_eq!(harness.signatures(), 0);
    assert!(harness.requests("POST", "/v1/auth/verify").await.is_empty());
    error.detail
}

#[tokio::test]
async fn a_deployment_that_differs_from_the_pins_is_refused_before_signing() {
    let harness = Harness::bare().await;
    let mut venues = venues(EscrowEnvironment::Production);
    venues[0]["chain"]["escrow"] = json!("0x1111111111111111111111111111111111111111");
    harness.serve_venues(venues).await;
    harness.serve_sign_in().await;
    assert_eq!(
        refused_without_signing(&harness, VerificationKind::Deployment).await,
        details::ESCROW
    );
}

#[tokio::test]
async fn a_challenge_for_another_origin_or_address_is_refused_before_signing() {
    let other = "0x1111111111111111111111111111111111111111".parse().unwrap();
    for (origin, address) in [("https://evil.example", None), ("", Some(other))] {
        let harness = Harness::bare().await;
        harness.serve_venues(venues(EscrowEnvironment::Production)).await;
        let origin = if origin.is_empty() { harness.server.uri() } else { origin.to_owned() };
        harness
            .serve_challenges(move |asked, nonce, now| {
                challenge(&origin, address.unwrap_or(asked), nonce, now)
            })
            .await;
        assert_eq!(
            refused_without_signing(&harness, VerificationKind::Differs).await,
            details::CHALLENGE
        );
    }
    // Expired challenges are stale.
    let harness = Harness::bare().await;
    harness.serve_venues(venues(EscrowEnvironment::Production)).await;
    let origin = harness.server.uri();
    harness
        .serve_challenges(move |address, nonce, now| {
            challenge(&origin, address, nonce, now - chrono::Duration::minutes(15))
        })
        .await;
    assert_eq!(
        refused_without_signing(&harness, VerificationKind::Stale).await,
        details::CHALLENGE_WINDOW
    );
}

#[tokio::test]
async fn renew_swaps_the_credentials_of_every_clone_and_revokes_the_previous_session() {
    let harness = Harness::new().await;
    let session = harness.session().await;
    let clone = session.clone();
    // The previous session is revoked with the new token.
    expect_revocation(&harness, 1, &token(2), 1).await;
    session.renew().await.unwrap();
    assert_eq!(harness.signatures(), 2);
    let renewed = saved(&clone);
    assert_eq!(renewed.token.expose_secret(), token(2));
    assert_eq!(renewed.session_id.to_string(), session_id(2));
    serve_me(&harness, &token(2), harness.address()).await;
    clone.account().me().await.unwrap();
}

#[tokio::test]
async fn a_failed_renewal_keeps_the_current_session_and_revokes_the_issued_one() {
    let harness = Harness::bare().await;
    harness.serve_venues(venues(EscrowEnvironment::Production)).await;
    harness.serve_sign_in().await;
    let session = harness.session().await;
    let before = saved(&session);
    // The deployment now lists development's escrows.
    harness.server.reset().await;
    harness.serve_venues(venues(EscrowEnvironment::Development)).await;
    harness.serve_sign_in().await;
    // The session issued meanwhile is revoked with its own token.
    expect_revocation(&harness, 2, &token(2), 1).await;
    let error = session.renew().await.unwrap_err();
    assert!(
        matches!(&error, Error::Verification(e) if e.detail == details::ENVIRONMENT),
        "{error:?}"
    );
    assert_eq!(saved(&session), before);
    assert!(
        harness
            .requests("DELETE", &format!("/v1/auth/sessions/{}", session_id(1)))
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn renew_then_export_resumes_and_sign_out_revokes_the_renewed_session() {
    let harness = Harness::new().await;
    let journal = Journal::new(MemoryStore::new());
    let session = harness.session_with(journal.clone()).await;
    expect_revocation(&harness, 1, &token(2), 1).await;
    session.renew().await.unwrap();
    let exported = saved(&session);
    serve_me(&harness, &token(2), harness.address()).await;
    let resumed =
        harness.client.resume_session(exported.clone(), harness.signer(), journal).await.unwrap();
    // Resuming signs nothing.
    assert_eq!(harness.signatures(), 2);
    assert_eq!(resumed.export(), Some(exported));
    assert_eq!(resumed.escrow_environment(), EscrowEnvironment::Production);
    expect_revocation(&harness, 2, &token(2), 1).await;
    resumed.sign_out().await.unwrap();
    assert_eq!(resumed.export(), None);
}

#[tokio::test]
async fn signed_out_clones_fail_before_any_request() {
    let harness = Harness::new().await;
    let session = harness.session().await;
    let clone = session.clone();
    expect_revocation(&harness, 1, &token(1), 1).await;
    Mock::given(method("GET"))
        .and(path("/v1/me"))
        .respond_with(ResponseTemplate::new(200).set_body_json(me(harness.address())))
        .expect(0)
        .mount(&harness.server)
        .await;
    session.sign_out().await.unwrap();
    let signed_out = |result: Result<()>| matches!(result, Err(Error::SignedOut));
    assert!(signed_out(clone.account().me().await.map(drop)));
    assert!(signed_out(clone.renew().await));
    assert_eq!((clone.export(), clone.expires_at()), (None, None));
    // A repeat sends nothing.
    clone.sign_out().await.unwrap();
    assert_eq!(harness.requests("POST", "/v1/auth/challenge").await.len(), 1);
}

#[tokio::test]
async fn sign_out_signs_every_clone_out_even_when_the_revocation_fails() {
    let harness = Harness::new().await;
    let session = harness.session().await;
    Mock::given(method("DELETE"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&harness.server)
        .await;
    assert!(session.clone().sign_out().await.is_err());
    assert_eq!(session.export(), None);
}

#[tokio::test]
async fn a_saved_session_of_another_address_is_not_resumed() {
    let harness = Harness::new().await;
    let other = "0x1111111111111111111111111111111111111111".parse().unwrap();
    serve_me(&harness, "saved-token", other).await;
    let id = session_id(9).parse().unwrap();
    let saved = SavedSession::new("saved-token".to_owned().into(), id, harness.now());
    let error = harness
        .client
        .resume_session(saved, harness.signer(), MemoryStore::new())
        .await
        .unwrap_err();
    assert!(matches!(&error, Error::Verification(e) if e.detail == details::SIGNER), "{error:?}");
}

#[tokio::test]
async fn renewal_and_sign_out_reach_every_handle_and_wake_live_streams() {
    let harness = Harness::new().await;
    let session = harness.session().await;
    let shared = Arc::clone(session.events().auth.expect("a session sends its token"));
    let mut changes = shared.changes().expect("a session's token changes");
    assert_eq!(shared.bearer().unwrap(), token(1));
    expect_revocation(&harness, 1, &token(2), 1).await;
    session.renew().await.unwrap();
    assert_eq!(shared.bearer().unwrap(), token(2));
    assert!(changes.has_changed().unwrap(), "a renewal wakes the streams");
    changes.borrow_and_update();
    expect_revocation(&harness, 2, &token(2), 1).await;
    session.sign_out().await.unwrap();
    assert!(matches!(shared.bearer(), Err(Error::SignedOut)));
    assert!(changes.has_changed().unwrap(), "a sign-out wakes the streams");
}

#[test]
fn tokens_and_saved_sessions_never_show_the_token() {
    let saved: SavedSession = serde_json::from_value(json!({
        "token": token(1), "session_id": session_id(1), "expires_at": "2026-09-27T10:00:00Z",
    }))
    .unwrap();
    let debug = format!("{saved:?} {:?}", saved.token);
    assert!(debug.contains("SessionToken(****)"), "{debug}");
    assert!(!debug.contains(TOKEN_SENTINEL), "{debug}");
    // Persisted as served.
    assert_eq!(serde_json::to_value(&saved).unwrap()["token"], token(1));
    assert_eq!(SessionToken::from(token(1)), saved.token);
}

/// A `tracing` writer into a shared buffer.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl tracing_subscriber::fmt::MakeWriter<'_> for Capture {
    type Writer = Self;

    fn make_writer(&self) -> Self::Writer {
        self.clone()
    }
}

#[tokio::test]
async fn sign_in_renewal_and_sign_out_never_log_tokens_or_signatures() {
    // A second live dispatcher makes callsite registration consult this test's subscriber too
    // (see the transport's capture test).
    let _second = tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default());
    let capture = Capture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::FULL)
        .with_writer(capture.clone())
        .with_ansi(false)
        .finish();
    let _default = tracing::subscriber::set_default(subscriber);
    let harness = Harness::new().await;
    let session = harness.session().await;
    expect_revocation(&harness, 1, &token(2), 1).await;
    session.renew().await.unwrap();
    expect_revocation(&harness, 2, &token(2), 1).await;
    session.clone().sign_out().await.unwrap();
    let error = session.account().me().await.unwrap_err();
    let mut text = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    assert!(text.contains("lamarge.sign_in") && text.contains("postAuthVerify"), "{text}");
    text += &[format!("{session:?}"), error.to_string(), format!("{error:?}")].join(" ");
    let signatures: Vec<String> = harness
        .requests("POST", "/v1/auth/verify")
        .await
        .iter()
        .map(|request| {
            request.body_json::<Value>().unwrap()["signature"].as_str().unwrap()[2..].to_owned()
        })
        .collect();
    assert_eq!(signatures.len(), 2);
    for secret in signatures.iter().map(String::as_str).chain([TOKEN_SENTINEL]) {
        assert!(!text.contains(secret), "{secret} leaked:\n{text}");
    }
}

#[test]
fn sessions_are_send_sync_and_their_futures_send() {
    fn shared<T: Send + Sync + 'static>() {}
    fn send<T: Send>(_: T) {}
    shared::<Session>();
    shared::<SavedSession>();
    let client = Client::new(crate::Environment::Production).unwrap();
    let signer = alloy_signer_local::PrivateKeySigner::random();
    send(client.sign_in(signer.clone(), MemoryStore::new()));
    send(client.check_deployment());
    let saved: SavedSession = serde_json::from_value(json!({
        "token": "t", "session_id": session_id(1), "expires_at": "2026-09-27T10:00:00Z",
    }))
    .unwrap();
    send(client.resume_session(saved, signer, MemoryStore::new()));
}

#[tokio::test]
async fn check_deployment_resolves_the_environment_of_the_listed_escrows() {
    let harness = Harness::bare().await;
    harness.serve_venues(venues(EscrowEnvironment::Development)).await;
    assert_eq!(harness.client.check_deployment().await.unwrap(), EscrowEnvironment::Development);
    harness.server.reset().await;
    let mut venues = venues(EscrowEnvironment::Production);
    for venue in venues.as_array_mut().unwrap() {
        venue["enabled"] = json!(false);
    }
    harness.serve_venues(venues).await;
    let error = harness.client.check_deployment().await.unwrap_err();
    assert!(
        matches!(&error, Error::Verification(e) if e.detail == details::NO_ESCROW_VENUE),
        "{error:?}"
    );
}
