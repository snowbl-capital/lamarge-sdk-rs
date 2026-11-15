//! Settled trade history.

use alloy_primitives::Address;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{Destination, WalletReceipt};
use crate::types::{Amount, Bps, FillId, VenueId};

/// A settled trade (`GET /v1/trades/history`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TradeHistoryRow {
    /// Where the trade delivered.
    pub destination: Destination,
    /// A wallet delivery's receipt; `None` for a venue delivery.
    #[serde(deserialize_with = "super::required")]
    pub received: Option<WalletReceipt>,
    /// The trade (its fill); its proof is at `GET /v1/trades/{id}/proof`.
    pub id: FillId,
    /// The paying venue.
    pub from: VenueId,
    /// The receiving venue.
    pub to: VenueId,
    /// The payment, in `from` base units.
    pub amount_in: Amount,
    /// The delivery, in `to` base units.
    pub amount_out: Amount,
    /// The taker fee, in `taker_fee_venue` base units.
    pub taker_fee: Amount,
    /// The taker fee rate.
    pub taker_fee_bps: Bps,
    /// The venue whose token `taker_fee` is in.
    #[serde(deserialize_with = "super::required")]
    pub taker_fee_venue: Option<VenueId>,
    /// What the maker received of the payment.
    pub maker_in: Amount,
    /// What left the maker's order.
    pub maker_out: Amount,
    /// When it settled.
    pub settled_at: DateTime<Utc>,
    /// The taker's master address, not a linked venue address.
    pub taker_address: Address,
}
