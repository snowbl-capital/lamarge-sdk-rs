//! The explorer: public transfer receipts and pending trades.

use alloy_primitives::Address;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{Destination, FillState, Funding, TokenAmount, WalletReceipt};
use crate::types::{Amount, FillId, TakeId, VenueId, open_enum};

open_enum! {
    /// Whether a transfer reached its final destination.
    pub enum ExplorerTransferStatus {
        /// Delivered to the final destination with every settled execution proven.
        Completed = "completed",
        /// Settled, but not entirely as above.
        Partial = "partial",
        /// An execution is in flight, or a route's leg settled moments ago.
        Pending = "pending",
    }
}

/// A transfer's public receipt (`GET /v1/explorer/transfers`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ExplorerTransfer {
    /// The transfer's first take, stable across route continuations.
    pub id: TakeId,
    /// The paying venue.
    pub from: VenueId,
    /// The final destination; until an execution is proven, the first take's delivery venue.
    pub to: VenueId,
    /// Proven and signed in-flight payments, in `from` base units; `None` while none exists.
    #[serde(deserialize_with = "super::required")]
    pub amount_in: Option<Amount>,
    /// Proven and signed in-flight deliveries to `to`, in its base units; `None` while none
    /// exists.
    #[serde(deserialize_with = "super::required")]
    pub amount_out: Option<Amount>,
    /// Where the transfer delivers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination: Option<Destination>,
    /// Where its payments come from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub funding: Option<Funding>,
    /// Proven final wallet payouts; `None` if any is not reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub received: Option<TokenAmount>,
    /// When the transfer started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Utc>>,
    /// The latest signed active execution's state; `None` for a route between its legs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<FillState>,
    /// Whether it reached its final destination.
    pub status: ExplorerTransferStatus,
    /// The latest proven settlement, or the creation time without one.
    pub settled_at: DateTime<Utc>,
    /// The taker's master address; `None` until an execution is proven.
    #[serde(deserialize_with = "super::required")]
    pub taker_address: Option<Address>,
    /// The executions, each with a public trade proof.
    pub executions: Vec<ExplorerExecution>,
}

/// One execution (fill) of a public transfer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ExplorerExecution {
    /// The venue fee the payment also spent from the paying account, in `from` base units.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payment_fee: Option<Amount>,
    /// The execution; its trade proof is at `GET /v1/trades/{id}/proof`.
    pub id: FillId,
    /// The paying venue.
    pub from: VenueId,
    /// The receiving venue.
    pub to: VenueId,
    /// The payment, in `from` base units.
    pub amount_in: Amount,
    /// The delivery, in `to` base units.
    pub amount_out: Amount,
    /// Where it delivered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination: Option<Destination>,
    /// Where its payment came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub funding: Option<Funding>,
    /// A wallet delivery's receipt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub received: Option<WalletReceipt>,
    /// The maker's address.
    pub maker_address: Address,
    /// When it settled.
    pub settled_at: DateTime<Utc>,
}

/// A signed trade not yet final.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PendingTrade {
    /// Where the payment comes from; absent on older trades.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub funding: Option<Funding>,
    /// Where the trade delivers.
    pub destination: Destination,
    /// The trade (its fill).
    pub id: FillId,
    /// The paying venue.
    pub from: VenueId,
    /// The receiving venue.
    pub to: VenueId,
    /// The payment, in `from` base units.
    pub amount_in: Amount,
    /// The delivery, in `to` base units.
    pub amount_out: Amount,
    /// Its state, short of final.
    pub state: FillState,
    /// When it was created.
    pub created_at: DateTime<Utc>,
}
