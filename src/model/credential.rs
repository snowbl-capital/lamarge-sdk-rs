//! Maker credential models: answers, request bodies and the credential token.

use std::{fmt, str::FromStr, time::Duration};

use alloy_primitives::Address;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, de};

use crate::{
    Error, Result,
    error::InvalidCredentialToken,
    types::{QuoteIdentityId, open_enum},
};

/// The longest credential lifetime the API accepts.
const MAX_LIFETIME: Duration = Duration::from_hours(90 * 24);
/// The prefix of every credential token.
const TOKEN_PREFIX: &str = "mk_";

open_enum! {
    /// What a maker credential may do.
    pub enum CredentialScope {
        /// Read the identity's orders, fills and private events.
        Read = "read",
        /// Lease, prepare and publish quote lines.
        Quote = "quote",
        /// Pull quotes.
        Cancel = "cancel",
    }
}

/// A maker credential's bearer secret (`mk_…`); parse a saved one with [`str::parse`].
///
/// `Debug` shows only the prefix; [`expose_secret`](Self::expose_secret) gives the token to send
/// or store.
#[derive(Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct CredentialToken(String);

impl CredentialToken {
    /// The token.
    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for CredentialToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CredentialToken(mk_****)")
    }
}

impl FromStr for CredentialToken {
    type Err = InvalidCredentialToken;

    /// Accepts `mk_` followed by at least one character.
    fn from_str(token: &str) -> Result<Self, Self::Err> {
        if token.len() > TOKEN_PREFIX.len() && token.starts_with(TOKEN_PREFIX) {
            Ok(Self(token.to_owned()))
        } else {
            Err(InvalidCredentialToken)
        }
    }
}

impl<'de> Deserialize<'de> for CredentialToken {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // The error names no part of the token.
        String::deserialize(deserializer)?.parse().map_err(de::Error::custom)
    }
}

/// A new maker credential, with its token. The token is returned only once: store it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CredentialCreated {
    /// The credential's quote identity.
    pub id: QuoteIdentityId,
    /// Its name.
    pub name: String,
    /// The address that signs its quotes.
    pub quote_signer: Address,
    /// What it may do.
    pub scopes: Vec<CredentialScope>,
    /// When it stops working.
    pub expires_at: DateTime<Utc>,
    /// The bearer secret.
    pub token: CredentialToken,
}

/// A credential's replacement token. The token is returned only once: store it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CredentialRotated {
    /// The credential's quote identity.
    pub id: QuoteIdentityId,
    /// The new bearer secret; the previous one stops working.
    pub token: CredentialToken,
    /// When it stops working.
    pub expires_at: DateTime<Utc>,
}

/// `POST /v1/credentials` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CredentialInput {
    pub(crate) name: String,
    pub(crate) quote_signer: Address,
    pub(crate) scopes: Vec<CredentialScope>,
    /// Unix seconds.
    pub(crate) expires_at: u64,
}

/// `POST /v1/credentials/{id}/rotate` body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RotationBody {
    /// Unix seconds.
    pub(crate) expires_at: u64,
}

/// `now + expires_in` in whole Unix seconds, for a lifetime of 1 second to 90 days.
pub(crate) fn expires_at(now: DateTime<Utc>, expires_in: Duration) -> Result<u64> {
    let seconds = expires_in.as_secs();
    if seconds == 0 || expires_in > MAX_LIFETIME {
        return Err(Error::InvalidRequest("expires_in must be 1 second to 90 days".into()));
    }
    u64::try_from(now.timestamp())
        .ok()
        .and_then(|now| now.checked_add(seconds))
        .ok_or_else(|| Error::InvalidRequest("the clock is before 1970".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "mk_4f1c9a0e7b3d5f2a8c6e1b9d0a7f3c5e2b8d4a6f1c9e0b7d3a5f2c8e6b1d9a0f";

    #[test]
    fn token_requires_its_prefix_and_never_shows_in_debug() {
        assert_eq!("sk_abc".parse::<CredentialToken>(), Err(InvalidCredentialToken));
        assert_eq!("mk_".parse::<CredentialToken>(), Err(InvalidCredentialToken));
        let token: CredentialToken = TOKEN.parse().unwrap();
        assert_eq!(token.expose_secret(), TOKEN);
        assert_eq!(format!("{token:?}"), "CredentialToken(mk_****)");
        assert_eq!(serde_json::to_string(&token).unwrap(), format!("\"{TOKEN}\""));
        let error = serde_json::from_str::<CredentialToken>("\"secret-without-prefix\"")
            .unwrap_err()
            .to_string();
        assert!(!error.contains("secret-without-prefix"), "{error}");
    }

    #[test]
    fn created_and_rotated_debug_redact_the_token() {
        let created: CredentialCreated =
            serde_json::from_value(crate::testing::json::fixture("credential_created")).unwrap();
        let rotated: CredentialRotated =
            serde_json::from_value(crate::testing::json::fixture("credential_rotated")).unwrap();
        for (debug, token) in [
            (format!("{created:?}"), created.token.expose_secret()),
            (format!("{rotated:?}"), rotated.token.expose_secret()),
        ] {
            let secret = token.trim_start_matches("mk_");
            assert!(!debug.contains(secret), "{debug}");
            assert!(debug.contains("CredentialToken(mk_****)"), "{debug}");
        }
        assert!(format!("{created:?}").contains("quote_signer"));
    }
}
