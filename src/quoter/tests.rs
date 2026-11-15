use serde_json::{Value, json};
use wiremock::{
    Mock, ResponseTemplate,
    matchers::{header, method, path},
};

use super::*;
use crate::{
    testing::{
        harness::{Harness, venues},
        json::fixture,
    },
    types::EscrowEnvironment,
};

/// The fixture credential, its quote signer replaced by `quote_signer`.
fn credential(quote_signer: Address) -> MakerCredential {
    let mut created = fixture("credential_created");
    created["quote_signer"] = json!(quote_signer);
    serde_json::from_value::<CredentialCreated>(created).unwrap().into()
}

fn bearer(credential: &MakerCredential) -> String {
    format!("Bearer {}", credential.token.expose_secret())
}

#[tokio::test]
async fn a_quoter_reads_with_its_credential() {
    let harness = Harness::bare().await;
    harness.serve_venues(venues(EscrowEnvironment::Development)).await;
    let credential = credential(harness.address());
    for route in ["/v1/orders", "/v1/me/exposure"] {
        let body = if route == "/v1/orders" { json!([]) } else { fixture("exposure") };
        Mock::given(method("GET"))
            .and(path(route))
            .and(header("authorization", bearer(&credential).as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .expect(1)
            .mount(&harness.server)
            .await;
    }
    let quoter = harness.client.quoter(credential.clone(), harness.signer()).await.unwrap();
    assert_eq!(quoter.identity(), credential.identity);
    assert_eq!(quoter.identity().quote_signer, harness.address());
    assert_eq!(quoter.escrow_environment(), EscrowEnvironment::Development);
    assert!(quoter.orders().list(None).await.unwrap().is_empty());
    quoter.exposure().await.unwrap();
    // The events handle hands streams the same credential.
    let source = quoter.events().auth.expect("a quoter authenticates");
    assert_eq!(source.bearer().unwrap(), credential.token.expose_secret());
    assert!(quoter.fills().auth.is_some());
    // Nothing is signed to start quoting.
    assert_eq!(harness.signatures(), 0);
}

#[tokio::test]
async fn another_quote_key_is_refused_before_any_request() {
    let harness = Harness::bare().await;
    let other = alloy_signer_local::PrivateKeySigner::random().address();
    let error = harness.client.quoter(credential(other), harness.signer()).await.unwrap_err();
    assert!(matches!(error, Error::InvalidRequest(_)), "{error:?}");
    assert!(harness.server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_quoter_needs_a_reviewed_deployment() {
    let harness = Harness::bare().await;
    let mut venues = venues(EscrowEnvironment::Production);
    venues[1]["chain"]["token"] = json!("0x1111111111111111111111111111111111111111");
    harness.serve_venues(venues).await;
    let error = harness.client.quoter(credential(harness.address()), harness.signer()).await;
    assert!(matches!(error, Err(Error::Verification(_))), "{error:?}");
}

#[test]
fn credentials_persist_and_never_show_their_token() {
    let credential = credential(Address::repeat_byte(0x8d));
    let secret = credential.token.expose_secret().to_owned();
    let saved = serde_json::to_string(&credential).unwrap();
    assert!(saved.contains(&secret));
    assert_eq!(serde_json::from_str::<MakerCredential>(&saved).unwrap(), credential);
    let fields: Value = serde_json::from_str(&saved).unwrap();
    assert_eq!(fields["quote_signer"], json!(Address::repeat_byte(0x8d)), "flat, as created");
    let copy = MakerCredential::new(credential.identity, credential.token.clone());
    assert_eq!(copy, credential);
    let debug = format!("{credential:?}");
    assert!(debug.contains("CredentialToken(mk_****)") && !debug.contains(&secret), "{debug}");
}
