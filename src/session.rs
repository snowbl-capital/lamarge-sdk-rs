//! Signed-in sessions: SIWE sign-in, renewal, sign-out and the session's handles.

use std::{
    fmt,
    sync::{Arc, PoisonError, RwLock},
};

use alloy_primitives::Address;
use chrono::{DateTime, Utc};
use futures_util::future;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tracing::Instrument as _;

use crate::{
    Client, Error, Result, api,
    protocol::{
        checks::details::{self, deployment, differs},
        signing::SigningKey,
        siwe,
    },
    store::{self, Journal},
    transport::{Auth, BearerToken, Call, Inner, ops},
    types::{EscrowEnvironment, SessionId, SignatureBytes},
};

/// A session's bearer secret; wrap a saved one with [`From<String>`].
///
/// `Debug` shows `SessionToken(****)`; [`expose_secret`](Self::expose_secret) gives the token to
/// store.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionToken(String);

impl From<String> for SessionToken {
    fn from(token: String) -> Self {
        Self(token)
    }
}

impl SessionToken {
    /// The token.
    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SessionToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionToken(****)")
    }
}

impl BearerToken for SessionToken {
    fn bearer(&self) -> Result<String> {
        Ok(self.0.clone())
    }
}

impl SessionToken {
    /// The token as a request credential.
    fn to_auth(&self) -> Arc<dyn BearerToken> {
        Arc::new(self.clone())
    }
}

/// A session's credentials, to persist and resume later with
/// [`Client::resume_session`]. `Debug` redacts the token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SavedSession {
    /// The bearer secret.
    pub token: SessionToken,
    /// The session, as `DELETE /v1/auth/sessions/{id}` names it.
    pub session_id: SessionId,
    /// When the token stops working.
    pub expires_at: DateTime<Utc>,
}

impl SavedSession {
    /// Saved credentials, for example read back from separate columns.
    pub fn new(token: SessionToken, session_id: SessionId, expires_at: DateTime<Utc>) -> Self {
        Self { token, session_id, expires_at }
    }
}

/// A sign-in challenge (`POST /v1/auth/challenge`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct Challenge {
    message: String,
    nonce: String,
    expires_at: DateTime<Utc>,
}

#[derive(Serialize)]
struct ChallengeBody {
    address: Address,
}

#[derive(Serialize)]
struct VerifyBody<'a> {
    address: Address,
    nonce: &'a str,
    signature: SignatureBytes,
}

/// The credentials every clone of a session reads at each request: `None` once signed out.
struct Credentials {
    current: RwLock<Option<SavedSession>>,
    /// Bumped at every swap, so live event streams follow a renewal or a sign-out.
    changed: watch::Sender<()>,
}

impl Credentials {
    fn new(saved: SavedSession) -> Self {
        Self { current: RwLock::new(Some(saved)), changed: watch::Sender::new(()) }
    }

    fn get(&self) -> Option<SavedSession> {
        self.current.read().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Swaps in `next` as one value and wakes the streams; returns the previous credentials.
    fn replace(&self, next: Option<SavedSession>) -> Option<SavedSession> {
        let mut current = self.current.write().unwrap_or_else(PoisonError::into_inner);
        let previous = std::mem::replace(&mut *current, next);
        drop(current);
        self.changed.send_replace(());
        previous
    }
}

impl BearerToken for Credentials {
    fn bearer(&self) -> Result<String> {
        let credentials = self.current.read().unwrap_or_else(PoisonError::into_inner);
        credentials
            .as_ref()
            .map(|saved| saved.token.expose_secret().to_owned())
            .ok_or(Error::SignedOut)
    }

    fn changes(&self) -> Option<watch::Receiver<()>> {
        Some(self.changed.subscribe())
    }
}

/// A signed-in user: its signer, credentials and journal.
///
/// Cheap to clone; clones share the credentials, so a [`renew`](Self::renew) or
/// [`sign_out`](Self::sign_out) applies to every clone, requests in flight and event streams
/// included.
#[derive(Clone)]
pub struct Session {
    inner: Arc<SessionInner>,
}

struct SessionInner {
    client: Client,
    credentials: Arc<Credentials>,
    /// `credentials`, as every request's credential.
    bearer: Arc<dyn BearerToken>,
    /// Serializes `renew` and `sign_out`.
    auth_change: tokio::sync::Mutex<()>,
    key: SigningKey,
    journal: Journal,
    environment: EscrowEnvironment,
}

impl Session {
    /// Signs in with `key`: the deployment check and the challenge are asked together, the
    /// deployment decides first, then the challenge is checked, signed and verified.
    pub(crate) async fn sign_in(
        client: &Client,
        key: SigningKey,
        journal: Journal,
    ) -> Result<Self> {
        let span = tracing::info_span!("lamarge.sign_in");
        let (environment, saved) = authenticate(client, &key).instrument(span).await?;
        Ok(Self::new(client.clone(), key, journal, environment, saved))
    }

    /// Resumes `saved` after the deployment check, provided `GET /v1/me` names `key`'s address.
    pub(crate) async fn resume(
        client: &Client,
        saved: SavedSession,
        key: SigningKey,
        journal: Journal,
    ) -> Result<Self> {
        let environment = client.check_deployment().await?;
        let token = saved.token.to_auth();
        let account = api::Account { inner: client.inner(), auth: Some(&token) };
        if account.me().await?.master_address != key.address() {
            return Err(differs(details::SIGNER).into());
        }
        Ok(Self::new(client.clone(), key, journal, environment, saved))
    }

    fn new(
        client: Client,
        key: SigningKey,
        journal: Journal,
        environment: EscrowEnvironment,
        saved: SavedSession,
    ) -> Self {
        let credentials = Arc::new(Credentials::new(saved));
        Self {
            inner: Arc::new(SessionInner {
                client,
                bearer: credentials.clone(),
                credentials,
                auth_change: tokio::sync::Mutex::new(()),
                key,
                journal,
                environment,
            }),
        }
    }

    /// The signer's address: the user's master address.
    pub fn address(&self) -> Address {
        self.inner.key.address()
    }

    /// The escrow environment the deployment check resolved.
    pub fn escrow_environment(&self) -> EscrowEnvironment {
        self.inner.environment
    }

    /// When the token stops working; `None` once signed out.
    pub fn expires_at(&self) -> Option<DateTime<Utc>> {
        self.inner.credentials.get().map(|saved| saved.expires_at)
    }

    /// The client the session was opened with.
    pub fn client(&self) -> &Client {
        &self.inner.client
    }

    /// The credentials to persist (the token is a secret); `None` once signed out.
    pub fn export(&self) -> Option<SavedSession> {
        self.inner.credentials.get()
    }

    /// Signs in again with the same checks, swaps token, session id and expiry as one value for
    /// every clone, then revokes the previous session (best effort: a failed revocation is
    /// logged, the old token then lasts until it expires).
    ///
    /// On failure the current credentials stay; a session issued for another environment is
    /// revoked (best effort). A signed-out session fails with [`Error::SignedOut`] before any
    /// request.
    pub async fn renew(&self) -> Result<()> {
        let _turn = self.inner.auth_change.lock().await;
        let previous = self.inner.credentials.get().ok_or(Error::SignedOut)?;
        let span = tracing::info_span!("lamarge.renew");
        let (environment, renewed) =
            authenticate(&self.inner.client, &self.inner.key).instrument(span).await?;
        if environment != self.inner.environment {
            let revoke = Call::new(&ops::REVOKE_SESSION).segment(renewed.session_id);
            let auth = renewed.token.to_auth();
            if let Err(error) = self.transport().send::<()>(revoke, Some(&auth)).await {
                tracing::warn!(error = %error, "session of another environment not revoked");
            }
            return Err(deployment(details::ENVIRONMENT).into());
        }
        self.inner.credentials.replace(Some(renewed));
        let revoke = Call::new(&ops::REVOKE_SESSION).segment(previous.session_id);
        if let Err(error) = self.transport().send::<()>(revoke, self.auth()).await {
            tracing::warn!(error = %error, "previous session not revoked");
        }
        Ok(())
    }

    /// Revokes the session. Every clone is signed out before the revocation is sent: their
    /// requests then fail with [`Error::SignedOut`] before anything is sent, also when the
    /// revocation itself fails (the token then lasts until it expires). Repeats succeed.
    pub async fn sign_out(&self) -> Result<()> {
        let _turn = self.inner.auth_change.lock().await;
        let Some(current) = self.inner.credentials.replace(None) else { return Ok(()) };
        let revoke = Call::new(&ops::REVOKE_SESSION).segment(current.session_id);
        self.transport().send(revoke, Some(&current.token.to_auth())).await
    }

    /// The account: profile, balances, statistics, venue setup and links.
    pub fn account(&self) -> api::Account<'_> {
        api::Account { inner: self.transport(), auth: self.auth() }
    }

    /// The session's transfers.
    pub fn transfers(&self) -> api::Transfers<'_> {
        api::Transfers { inner: self.transport(), auth: self.auth() }
    }

    /// The session's takes.
    pub fn takes(&self) -> api::Takes<'_> {
        api::Takes { inner: self.transport(), auth: self.auth() }
    }

    /// The session's fills.
    pub fn fills(&self) -> api::Fills<'_> {
        api::Fills { inner: self.transport(), auth: self.auth() }
    }

    /// The session's orders.
    pub fn orders(&self) -> api::Orders<'_> {
        api::Orders { inner: self.transport(), auth: self.auth() }
    }

    /// The session's withdrawals.
    pub fn withdrawals(&self) -> api::Withdrawals<'_> {
        api::Withdrawals { inner: self.transport(), auth: self.auth() }
    }

    /// The session's moves.
    pub fn moves(&self) -> api::Moves<'_> {
        api::Moves { inner: self.transport(), auth: self.auth() }
    }

    /// The session's external bridges.
    pub fn bridges(&self) -> api::Bridges<'_> {
        api::Bridges { inner: self.transport(), auth: self.auth() }
    }

    /// The session's direct operations.
    pub fn directs(&self) -> api::Directs<'_> {
        api::Directs { inner: self.transport(), auth: self.auth() }
    }

    /// The session's maker credentials.
    pub fn credentials(&self) -> api::Credentials<'_> {
        api::Credentials { inner: self.transport(), auth: self.auth() }
    }

    /// The private event journal.
    pub fn events(&self) -> api::Events<'_> {
        api::Events { inner: self.transport(), auth: self.auth() }
    }

    /// The session's own transfers and trades.
    pub fn explorer(&self) -> api::Explorer<'_> {
        api::Explorer { inner: self.transport(), auth: self.auth() }
    }

    pub(crate) fn transport(&self) -> &Arc<Inner> {
        self.inner.client.inner()
    }

    /// The session's credential, read at each request.
    #[expect(clippy::unnecessary_wraps, reason = "the credential as `Inner::send` takes it")]
    pub(crate) fn auth(&self) -> Auth<'_> {
        Some(&self.inner.bearer)
    }

    pub(crate) fn key(&self) -> &SigningKey {
        &self.inner.key
    }

    pub(crate) fn journal(&self) -> &Journal {
        &self.inner.journal
    }

    /// The journal key of `record`, namespaced by environment and signer.
    pub(crate) fn record_key(&self, record: &str) -> String {
        store::scoped_key(self.inner.environment, self.address(), record)
    }
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("address", &self.address())
            .field("environment", &self.inner.environment)
            .field("signed_in", &self.inner.credentials.get().is_some())
            .finish_non_exhaustive()
    }
}

/// SIWE for `key`: the deployment's environment and the new session's credentials.
async fn authenticate(
    client: &Client,
    key: &SigningKey,
) -> Result<(EscrowEnvironment, SavedSession)> {
    let inner = client.inner();
    let address = key.address();
    let ask = Call::new(&ops::AUTH_CHALLENGE).json(&ChallengeBody { address })?;
    let (environment, challenge) =
        future::join(client.check_deployment(), inner.send::<Challenge>(ask, None)).await;
    let environment = environment?;
    let challenge = challenge?;
    let verified = siwe::check_login_challenge(
        &challenge.message,
        &challenge.nonce,
        challenge.expires_at,
        address,
        &inner.origin,
        inner.now(),
    )?;
    let signature = key.sign(&verified, &inner.clock).await?;
    let body = VerifyBody { address, nonce: &challenge.nonce, signature };
    let saved = inner.send(Call::new(&ops::AUTH_VERIFY).json(&body)?, None).await?;
    tracing::info!("signed in");
    Ok((environment, saved))
}

#[cfg(test)]
mod tests;
