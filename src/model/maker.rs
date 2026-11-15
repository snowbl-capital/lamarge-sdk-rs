//! Maker capital models: preparations and their states.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{Funding, Order, ToSign};
use crate::types::{
    Amount, ClientId, PreparationId, QuoteIdentityId, Revision, VenueId, open_enum,
};

/// The API's `FundingSource`: the same values as [`Funding`], for added capital.
pub(crate) type FundingSource = Funding;

open_enum! {
    /// A maker preparation's lifecycle.
    pub enum MakerPreparationState {
        /// Awaiting its signatures.
        Preparing = "preparing",
        /// Signed; funding is on its way.
        Accepted = "accepted",
        /// Funded.
        Complete = "complete",
        /// Cancelled, or expired unsigned.
        Cancelled = "cancelled",
        /// Funding failed.
        Failed = "failed",
    }
}

/// What a maker preparation adds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct MakerIntent {
    /// Capital to add, in the venue's base units.
    pub amount: Amount,
    /// The identity the pool quotes for.
    pub quote_identity_id: QuoteIdentityId,
    /// Where the deposit, the capital the pool's available balance does not cover, comes from.
    pub funding_source: Funding,
}

/// How a maker preparation's capital is covered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct MakerFunding {
    /// What the pool already has available.
    pub existing_available: Amount,
    /// What comes from the wallet.
    pub wallet_amount: Amount,
    /// What comes out of venue margin.
    pub venue_amount: Amount,
    /// What comes from the escrow's idle balance.
    pub idle_amount: Amount,
}

/// A capital addition to a maker pool, with what it needs signed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct MakerPreparation {
    /// The preparation.
    pub id: PreparationId,
    /// The caller's idempotency key.
    pub client_request_id: ClientId,
    /// Its lifecycle.
    pub state: MakerPreparationState,
    /// The pool's venue.
    pub venue: VenueId,
    /// The order it funds.
    pub order: Order,
    /// What must be signed.
    pub to_sign: Vec<ToSign>,
    /// The quote sequence it signs, if any.
    #[serde(deserialize_with = "super::required")]
    pub sequence: Option<u64>,
    /// When it is cancelled if unsigned.
    pub expires_at: DateTime<Utc>,
    /// The order revision it expects.
    pub expected_revision: Revision,
    /// What it adds.
    pub intent: MakerIntent,
    /// How the capital is covered.
    pub funding: MakerFunding,
}
