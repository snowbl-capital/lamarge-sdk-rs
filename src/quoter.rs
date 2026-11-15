//! Quoters: a maker credential with its quote signing key.

use std::{
    fmt,
    sync::{Arc, Mutex},
    time::Duration,
};

use alloy_primitives::Address;
use serde::{Deserialize, Serialize};

pub use crate::model::credential::CredentialToken;
use crate::{
    Client, Error, Result, api,
    model::{CredentialCreated, Exposure},
    protocol::signing::{SigningKey, Verified},
    transport::{Auth, BearerToken, Inner},
    types::{EscrowEnvironment, QuoteIdentityId, SignatureBytes},
};

impl BearerToken for CredentialToken {
    fn bearer(&self) -> Result<String> {
        Ok(self.expose_secret().to_owned())
    }
}

/// A delegated quote identity: the credential's identity and the address that signs its quotes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub struct QuoteIdentity {
    /// The identity, as orders and fills name it.
    pub id: QuoteIdentityId,
    /// The address that signs its quotes.
    pub quote_signer: Address,
}

impl QuoteIdentity {
    /// The identity `id`, whose quotes `quote_signer` signs.
    pub fn new(id: QuoteIdentityId, quote_signer: Address) -> Self {
        Self { id, quote_signer }
    }
}

/// What a quoting host needs besides its quote key: the credential's identity and token.
/// Persistable; `Debug` redacts the token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct MakerCredential {
    /// The credential's quote identity and the address that signs its quotes.
    #[serde(flatten)]
    pub identity: QuoteIdentity,
    /// The bearer secret.
    pub token: CredentialToken,
}

impl MakerCredential {
    /// A saved credential.
    pub fn new(identity: QuoteIdentity, token: CredentialToken) -> Self {
        Self { identity, token }
    }
}

impl From<CredentialCreated> for MakerCredential {
    fn from(created: CredentialCreated) -> Self {
        Self::new(QuoteIdentity::new(created.id, created.quote_signer), created.token)
    }
}

/// A maker credential with its quote key: quotes, leases and cancels for one delegated identity,
/// and reads its orders, fills and private events. It never moves capital.
///
/// Cheap to clone; clones share the client and the key.
#[derive(Clone)]
pub struct Quoter {
    inner: Arc<QuoterInner>,
}

struct QuoterInner {
    client: Client,
    /// The credential's token, as every request's credential.
    bearer: Arc<dyn BearerToken>,
    identity: QuoteIdentity,
    key: SigningKey,
    environment: EscrowEnvironment,
    /// The round trip of the latest answered request and when it completed: lease telemetry.
    last_rtt: Mutex<Option<(Duration, tokio::time::Instant)>>,
}

impl Quoter {
    /// Checks that `key` is the credential's quote signer (before any request), then the
    /// deployment, whose environment the quoter keeps.
    pub(crate) async fn start(
        client: &Client,
        credential: MakerCredential,
        key: SigningKey,
    ) -> Result<Self> {
        if key.address() != credential.identity.quote_signer {
            return Err(Error::InvalidRequest(
                "the quote key is not the credential's quote signer".into(),
            ));
        }
        let environment = client.check_deployment().await?;
        let identity = credential.identity;
        Ok(Self {
            inner: Arc::new(QuoterInner {
                client: client.clone(),
                bearer: Arc::new(credential.token),
                identity,
                key,
                environment,
                last_rtt: Mutex::new(None),
            }),
        })
    }

    /// The identity it quotes for.
    pub fn identity(&self) -> QuoteIdentity {
        self.inner.identity
    }

    /// Funds the identity has committed to trading (`GET /v1/me/exposure`).
    pub async fn exposure(&self) -> Result<Exposure> {
        api::Account { inner: self.transport(), auth: self.auth() }.exposure().await
    }

    /// The escrow environment the deployment check resolved.
    pub fn escrow_environment(&self) -> EscrowEnvironment {
        self.inner.environment
    }

    /// The identity's orders.
    pub fn orders(&self) -> api::Orders<'_> {
        api::Orders { inner: self.transport(), auth: self.auth() }
    }

    /// The identity's fills.
    pub fn fills(&self) -> api::Fills<'_> {
        api::Fills { inner: self.transport(), auth: self.auth() }
    }

    /// The identity's private event journal.
    pub fn events(&self) -> api::Events<'_> {
        api::Events { inner: self.transport(), auth: self.auth() }
    }

    pub(crate) fn transport(&self) -> &Arc<Inner> {
        self.inner.client.inner()
    }

    /// The credential every request carries.
    #[expect(clippy::unnecessary_wraps, reason = "the credential as `Inner::send` takes it")]
    pub(crate) fn auth(&self) -> Auth<'_> {
        Some(&self.inner.bearer)
    }

    /// Signs `verified` with the quote key at the client's clock.
    pub(crate) async fn sign(&self, verified: &Verified) -> Result<SignatureBytes> {
        self.inner.key.sign(verified, &self.transport().clock).await
    }

    pub(crate) fn last_rtt(&self) -> &Mutex<Option<(Duration, tokio::time::Instant)>> {
        &self.inner.last_rtt
    }
}

impl fmt::Debug for Quoter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Quoter")
            .field("identity", &self.inner.identity)
            .field("environment", &self.inner.environment)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests;
