//! Maker credentials: create, rotate and delete.

use std::time::Duration;

use alloy_primitives::Address;
use chrono::{DateTime, Utc};

use super::Credentials;
use crate::{
    Error, Result,
    model::{
        CredentialCreated, CredentialInput, CredentialRotated, CredentialScope, RotationBody,
        expires_at,
    },
    protocol::checks::maker::check_created_credential,
    transport::{Call, ops},
    types::QuoteIdentityId,
};

/// A maker credential to create: a bearer bound to one quote identity and quote signer, never
/// able to move capital.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialRequest {
    name: String,
    quote_signer: Address,
    scopes: Vec<CredentialScope>,
    expires_in: Duration,
}

impl CredentialRequest {
    /// A credential named `name` (1 to 80 characters, not blank) for `quote_signer` (nonzero),
    /// valid for `expires_in` (1 second to 90 days), with scopes read, quote and cancel.
    pub fn new(name: impl Into<String>, quote_signer: Address, expires_in: Duration) -> Self {
        Self {
            name: name.into(),
            quote_signer,
            scopes: vec![CredentialScope::Read, CredentialScope::Quote, CredentialScope::Cancel],
            expires_in,
        }
    }

    /// Replaces the scopes; duplicates are dropped, the first occurrence kept.
    #[must_use]
    pub fn scopes(mut self, scopes: impl IntoIterator<Item = CredentialScope>) -> Self {
        self.scopes.clear();
        for scope in scopes {
            if !self.scopes.contains(&scope) {
                self.scopes.push(scope);
            }
        }
        self
    }

    /// The wire body at `now`, or `InvalidRequest` for an input the API refuses.
    fn body(&self, now: DateTime<Utc>) -> Result<CredentialInput> {
        let chars = self.name.chars().count();
        if self.name.trim().is_empty() || chars > 80 {
            return Err(Error::InvalidRequest("name must be 1 to 80 characters, not blank".into()));
        }
        if self.quote_signer.is_zero() {
            return Err(Error::InvalidRequest("quote_signer must be nonzero".into()));
        }
        if self.scopes.is_empty() || self.scopes.iter().any(CredentialScope::is_unknown) {
            return Err(Error::InvalidRequest("scopes must be some of read, quote, cancel".into()));
        }
        Ok(CredentialInput {
            name: self.name.clone(),
            quote_signer: self.quote_signer,
            scopes: self.scopes.clone(),
            expires_at: expires_at(now, self.expires_in)?,
        })
    }
}

impl Credentials<'_> {
    /// Creates a maker credential; its token is returned only once.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidRequest`], before anything is sent, for a request the API refuses.
    /// - [`Error::Verification`] when the answer does not echo the quote signer and scopes.
    pub async fn create(&self, request: &CredentialRequest) -> Result<CredentialCreated> {
        let body = request.body(self.inner.now())?;
        let created: CredentialCreated =
            self.inner.send(Call::new(&ops::CREATE_CREDENTIAL).json(&body)?, self.auth).await?;
        check_created_credential(&created, body.quote_signer, &body.scopes)?;
        Ok(created)
    }

    /// Replaces a credential's token, valid for `expires_in` (1 second to 90 days), keeping its
    /// identity and quote signer; the previous token stops working and the identity's quotes
    /// and lease end. Works on expired, not revoked, credentials.
    pub async fn rotate(
        &self,
        id: QuoteIdentityId,
        expires_in: Duration,
    ) -> Result<CredentialRotated> {
        let body = RotationBody { expires_at: expires_at(self.inner.now(), expires_in)? };
        let call = Call::new(&ops::ROTATE_CREDENTIAL).segment(id).json(&body)?;
        self.inner.send(call, self.auth).await
    }

    /// Revokes a credential and its identity for good, removing its quotes; reserved fills still
    /// settle. Repeats succeed.
    pub async fn delete(&self, id: QuoteIdentityId) -> Result<()> {
        self.inner.send(Call::new(&ops::DELETE_CREDENTIAL).segment(id), self.auth).await
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use wiremock::MockServer;

    use super::*;
    use crate::{
        error::VerificationKind,
        protocol::checks::details,
        testing::{
            NOW, at,
            http::{Expect, client, only_request, served, session},
            json::fixture,
        },
    };

    const DAY: Duration = Duration::from_hours(24);

    fn now() -> DateTime<Utc> {
        at(0)
    }

    fn signer() -> Address {
        fixture("credential_created")["quote_signer"].as_str().unwrap().parse().unwrap()
    }

    fn request() -> CredentialRequest {
        CredentialRequest::new("quoting bot", signer(), 30 * DAY)
    }

    async fn body(server: &MockServer) -> Value {
        serde_json::from_slice(&only_request(server).await.body).unwrap()
    }

    #[tokio::test]
    async fn create_sends_the_request_and_checks_the_echo() {
        let server = MockServer::start().await;
        let client = client(&server);
        let credentials = Credentials { inner: client.inner(), auth: Some(session()) };
        let created =
            Expect::post("/v1/credentials", fixture("credential_created")).status(201).bearer();
        let credential = served(&server, created, credentials.create(&request())).await.unwrap();
        assert!(credential.token.expose_secret().starts_with("mk_"));
        let expected = json!({
            "name": "quoting bot",
            "quote_signer": signer(),
            "scopes": ["read", "quote", "cancel"],
            "expires_at": NOW + 30 * 86_400,
        });
        assert_eq!(body(&server).await, expected);
        for (field, value, detail) in [
            ("quote_signer", json!(Address::repeat_byte(0x99)), details::CREDENTIAL_QUOTE_SIGNER),
            ("scopes", json!(["read", "quote"]), details::CREDENTIAL_SCOPES),
            ("scopes", json!(["quote", "read", "cancel"]), details::CREDENTIAL_SCOPES),
        ] {
            let mut answer = fixture("credential_created");
            answer[field] = value;
            let created = Expect::post("/v1/credentials", answer).bearer();
            let error = served(&server, created, credentials.create(&request())).await.unwrap_err();
            let Error::Verification(error) = error else { panic!("{error:?}") };
            assert_eq!((error.kind, error.detail), (VerificationKind::Differs, detail));
        }
    }

    #[tokio::test]
    async fn create_keeps_the_token_out_of_decode_errors() {
        let server = MockServer::start().await;
        let client = client(&server);
        let credentials = Credentials { inner: client.inner(), auth: Some(session()) };
        let mut answer = fixture("credential_created");
        answer["expires_at"] = json!("not a time");
        let created = Expect::post("/v1/credentials", answer.clone()).bearer();
        let error = served(&server, created, credentials.create(&request())).await.unwrap_err();
        let Error::Decode(decode) = &error else { panic!("{error:?}") };
        assert!(decode.excerpt().is_none());
        let secret = answer["token"].as_str().unwrap().trim_start_matches("mk_");
        assert!(!format!("{error:?} {error}").contains(secret));
    }

    #[tokio::test]
    async fn rotate_sends_the_new_expiry_and_delete_the_id() {
        let server = MockServer::start().await;
        let client = client(&server);
        let credentials = Credentials { inner: client.inner(), auth: Some(session()) };
        let rotated = fixture("credential_rotated");
        let id: QuoteIdentityId = rotated["id"].as_str().unwrap().parse().unwrap();
        let rotate = Expect::post(format!("/v1/credentials/{id}/rotate"), rotated).bearer();
        served(&server, rotate, credentials.rotate(id, 7 * DAY)).await.unwrap();
        assert_eq!(body(&server).await, json!({"expires_at": NOW + 7 * 86_400}));
        let delete = Expect::delete(format!("/v1/credentials/{id}"), None).bearer();
        served(&server, delete, credentials.delete(id)).await.unwrap();
    }

    #[test]
    fn request_body_defaults_scopes_and_drops_duplicates() {
        let body = request().body(now()).unwrap();
        assert_eq!(
            body.scopes,
            [CredentialScope::Read, CredentialScope::Quote, CredentialScope::Cancel]
        );
        let scoped = request().scopes([
            CredentialScope::Quote,
            CredentialScope::Read,
            CredentialScope::Quote,
        ]);
        assert_eq!(
            scoped.body(now()).unwrap().scopes,
            [CredentialScope::Quote, CredentialScope::Read]
        );
    }

    #[test]
    fn what_the_api_refuses_is_refused_before_sending() {
        let invalid = |request: CredentialRequest| {
            assert!(matches!(request.body(now()), Err(Error::InvalidRequest(_))), "{request:?}");
        };
        invalid(CredentialRequest::new("  ", signer(), DAY));
        invalid(CredentialRequest::new("n".repeat(81), signer(), DAY));
        invalid(CredentialRequest::new("bot", Address::ZERO, DAY));
        invalid(CredentialRequest::new("bot", signer(), DAY).scopes([]));
        invalid(CredentialRequest::new("bot", signer(), DAY).scopes(["admin".parse().unwrap()]));
        invalid(CredentialRequest::new("bot", signer(), Duration::from_millis(999)));
        invalid(CredentialRequest::new("bot", signer(), 91 * DAY));
        assert!(CredentialRequest::new("é".repeat(80), signer(), 90 * DAY).body(now()).is_ok());
        assert!(expires_at(now(), Duration::ZERO).is_err());
        assert!(expires_at(now(), 181 * DAY).is_err());
    }
}
