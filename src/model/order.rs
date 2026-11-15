//! Order models: orders, their history rows, margin moves and quote cancellations.

use alloy_primitives::Address;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{QuoteSet, Transaction, WithdrawalState};
use crate::types::{
    Amount, ClientId, OrderId, PreparationId, QuoteIdentityId, Revision, VenueId, open_enum,
};

open_enum! {
    /// An order's lifecycle.
    pub enum OrderState {
        /// Its preparation is not signed yet.
        Preparing = "preparing",
        /// Its capital is leaving the venue margin.
        Moving = "moving",
        /// Its funding transaction is on its way.
        Funding = "funding",
        /// Funded; quotable.
        Live = "live",
        /// Its close is on its way.
        Closing = "closing",
        /// Closed.
        Closed = "closed",
        /// Its funding or close failed.
        Failed = "failed",
    }
}

open_enum! {
    /// Where an order's `held` amount is.
    pub enum HeldIn {
        /// Unlocked in the escrow.
        Escrow = "escrow",
        /// A Lighter or Aster pool's share of its account's balance.
        Venue = "venue",
    }
}

/// A move out of venue margin, relayed as a withdrawal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct MarginMove {
    /// The venue it leaves.
    pub venue: VenueId,
    /// In `venue` base units.
    pub amount: Amount,
    /// The venue's transfer reference, if any.
    #[serde(deserialize_with = "super::required")]
    pub venue_ref: Option<String>,
    /// Progress at the venue.
    pub state: WithdrawalState,
    /// Why it failed.
    #[serde(deserialize_with = "super::required")]
    pub error: Option<String>,
    /// When its state was last observed.
    pub at: DateTime<Utc>,
}

/// An amount on a venue, in its base units.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct VenueAmount {
    /// The venue.
    pub venue: VenueId,
    /// In `venue` base units.
    pub amount: Amount,
}

/// A maker order: a capital pool on one venue and its current quote set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Order {
    /// The preparation that funds it, while one does.
    #[serde(deserialize_with = "super::required")]
    pub preparation_id: Option<PreparationId>,
    /// Whether it is the current pool of its quote identity, venue and owner.
    pub capital_pool: bool,
    /// Where `held` is.
    pub held_in: HeldIn,
    /// The escrow's `offerId`.
    pub id: OrderId,
    /// The supply venue.
    pub venue: VenueId,
    /// The escrow offer's owner, the maker's linked address on `venue`.
    pub owner_address: Address,
    /// Its lifecycle.
    pub state: OrderState,
    /// Why the order, its funding or its last close failed.
    #[serde(deserialize_with = "super::required")]
    pub error: Option<String>,
    /// The funded amount; on Lighter or Aster, the cap deliveries consume.
    pub amount: Amount,
    /// What the order holds, where `held_in` says.
    pub held: Amount,
    /// Inside a venue, the last-read per-delivery fee; zero for an escrow order.
    pub delivery_fee: Amount,
    /// Newer state has a higher revision.
    pub revision: Revision,
    /// `held` less admitted fills, their delivery fees and withdrawals on their way.
    pub available_to_quote: Amount,
    /// Settled fills' payments per paying venue.
    pub received_by_venue: Vec<VenueAmount>,
    /// What the current sequence may still admit, capped by `available_to_quote`.
    pub quotable: Amount,
    /// Taken by settled fills, in `venue` base units.
    pub filled: Amount,
    /// The highest stored quote sequence; 0 without one.
    pub last_sequence: u64,
    /// The current quote set; check its `active`.
    #[serde(deserialize_with = "super::required")]
    pub quote_set: Option<QuoteSet>,
    /// Its move out of venue margin, if any.
    #[serde(deserialize_with = "super::required")]
    pub margin_move: Option<MarginMove>,
    /// Relayed transactions, oldest first.
    pub transactions: Vec<Transaction>,
    /// When it was created.
    pub created_at: DateTime<Utc>,
    /// When it last changed.
    pub updated_at: DateTime<Utc>,
    /// The creating preparation's `client_request_id`.
    pub client_order_id: ClientId,
    /// The quote identity it quotes for.
    pub quote_identity_id: QuoteIdentityId,
    /// The address signing its quotes.
    pub quote_signer: Address,
}

/// An order as order history lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct OrderHistoryRow {
    /// The escrow's `offerId`.
    pub id: OrderId,
    /// The supply venue.
    pub venue: VenueId,
    /// Its lifecycle.
    pub state: OrderState,
    /// The funded amount.
    pub amount: Amount,
    /// What the order holds, where `held_in` says.
    pub held: Amount,
    /// Where `held` is.
    pub held_in: HeldIn,
    /// What the current sequence may still admit.
    pub quotable: Amount,
    /// When it was created.
    pub created_at: DateTime<Utc>,
    /// When it last changed.
    pub updated_at: DateTime<Utc>,
    /// The creating preparation's `client_request_id`.
    pub client_order_id: ClientId,
    /// The quote identity it quotes for.
    pub quote_identity_id: QuoteIdentityId,
    /// The address signing its quotes.
    pub quote_signer: Address,
    /// Newer state has a higher revision.
    pub revision: Revision,
    /// The current quote sequence, if any.
    #[serde(deserialize_with = "super::required")]
    pub sequence: Option<u64>,
}

/// The answer to pulling an order's quotes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct QuoteCancellation {
    /// The order.
    pub order_id: OrderId,
    /// Advanced on every call.
    pub cancel_epoch: Revision,
}
