//! Fill models: fills, their previews and history rows, and relayed transactions.

use alloy_primitives::{Address, B256};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{Destination, Funding};
use crate::types::{
    Amount, Bps, ClaimSecret, FillId, OrderId, Revision, TakeId, VenueId, open_enum,
};

open_enum! {
    /// Which side of a fill the reader is.
    pub enum FillRole {
        /// The fill's taker.
        Taker = "taker",
        /// The order's maker.
        Maker = "maker",
    }
}

open_enum! {
    /// A fill's lifecycle.
    pub enum FillState {
        /// No maker inventory is claimed yet.
        Prepared = "PREPARED",
        /// Admitted; inventory is held for the payment.
        Reserved = "RESERVED",
        /// The payment is locked (or executed on Lighter or Aster); delivery pending.
        Paid = "PAID",
        /// The payment is being made.
        Paying = "PAYING",
        /// The payment is locked.
        Locked = "LOCKED",
        /// Delivered; the payment is still locked.
        Claiming = "CLAIMING",
        /// Delivered and paid.
        Settled = "SETTLED",
        /// Never paid in time.
        Expired = "EXPIRED",
        /// Delivery abandoned; the payment is returning.
        Refunding = "REFUNDING",
        /// The payment returned to the taker.
        Refunded = "REFUNDED",
        /// The maker lost the fill.
        MakerLoss = "MAKER_LOSS",
        /// The taker lost the fill.
        TakerLoss = "TAKER_LOSS",
    }
}

open_enum! {
    /// The taker's payment lock on the paying venue's chain.
    pub enum PaymentLeg {
        /// Not locked.
        None = "NONE",
        /// Locked in the escrow.
        Locked = "LOCKED",
        /// Released to the maker (from Lighter or Aster: the transfer executed).
        Claimed = "CLAIMED",
        /// Returned to the taker.
        Refunded = "REFUNDED",
    }
}

open_enum! {
    /// The delivery on the receiving venue.
    pub enum MakerLeg {
        /// Not delivered.
        None = "NONE",
        /// Locked by the maker (fills settled before deliveries).
        Locked = "LOCKED",
        /// Delivered.
        Claimed = "CLAIMED",
        /// Returned to the maker (fills settled before deliveries).
        Refunded = "REFUNDED",
        /// Can no longer be delivered.
        Abandoned = "ABANDONED",
    }
}

open_enum! {
    /// Where the maker's claimed payment was released.
    pub enum PaymentDestination {
        /// Into the maker's venue account.
        Venue = "venue",
        /// Into the escrow's available balance.
        Escrow = "escrow",
    }
}

open_enum! {
    /// Where the taker's claimed output was released.
    pub enum MakerDestination {
        /// Into the taker's venue account.
        Venue = "venue",
        /// Into the escrow's available balance.
        Escrow = "escrow",
        /// Transferred to the taker's recipient wallet.
        Wallet = "wallet",
    }
}

open_enum! {
    /// What a relayed transaction does.
    pub enum TransactionKind {
        /// Funds an order.
        Fund = "fund",
        /// Closes an order.
        Close = "close",
        /// Locks a take's payment.
        Payment = "payment",
        /// Delivers a fill.
        Deliver = "deliver",
        /// Releases a payment to the maker.
        MakerClaim = "maker_claim",
        /// Returns a payment to the taker.
        RefundPayment = "refund_payment",
        /// Maker lock (fills settled before deliveries).
        MakerLock = "maker_lock",
        /// Taker claim (fills settled before deliveries).
        TakerClaim = "taker_claim",
        /// Maker refund (fills settled before deliveries).
        RefundMaker = "refund_maker",
    }
}

open_enum! {
    /// A relayed transaction's progress.
    pub enum TransactionStatus {
        /// No finalized receipt yet.
        Pending = "pending",
        /// Its nonce was taken by a replacement; still watched.
        Replaced = "replaced",
        /// Finalized.
        Mined = "mined",
        /// Reverted on chain.
        Reverted = "reverted",
        /// Never broadcast.
        Failed = "failed",
    }
}

/// What a wallet delivery's recipient received, as the escrow recorded it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WalletReceipt {
    /// The token received.
    pub token: Address,
    /// This fill's share, in `token` base units.
    pub amount: Amount,
    /// The escrow, or on Polygon the pinned converter.
    pub sender: Address,
}

/// A transaction LaMarge relayed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Transaction {
    /// What it does.
    pub kind: TransactionKind,
    /// The chain it was sent on.
    pub chain_id: u64,
    /// Its hash; `None` if never broadcast.
    #[serde(deserialize_with = "super::required")]
    pub hash: Option<String>,
    /// Its progress.
    pub status: TransactionStatus,
    /// Why it failed.
    #[serde(deserialize_with = "super::required")]
    pub error: Option<String>,
    /// When its status was last observed.
    pub at: DateTime<Utc>,
}

/// One order's share of a take, as its taker or maker sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Fill {
    /// Where the payment came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub funding: Option<Funding>,
    /// The fill.
    pub id: FillId,
    /// Its take.
    pub take_id: TakeId,
    /// The reader's side.
    pub role: FillRole,
    /// The maker's order.
    pub order_id: OrderId,
    /// The quote sequence it was accepted under.
    pub sequence: u64,
    /// Its quote's index in that set.
    pub quote_index: u32,
    /// Names the payment and its delivery.
    pub swap_id: B256,
    /// sha256 of the secret that releases the payment to the maker.
    pub hashlock: B256,
    /// The maker's secret once delivered: with it, anyone can claim the payment before `t2`.
    #[serde(deserialize_with = "super::required")]
    pub secret: Option<ClaimSecret>,
    /// The venue the taker pays on.
    pub from_venue: VenueId,
    /// The venue the taker receives on.
    pub to_venue: VenueId,
    /// What the taker receives, in `to_venue` base units.
    pub amount_out: Amount,
    /// What the taker pays, in `from_venue` base units, fee included when paid in that token.
    pub amount_in: Amount,
    /// The taker fee, in `taker_fee_venue` base units; zero without a fee.
    pub taker_fee: Amount,
    /// The taker fee rate fixed at acceptance.
    pub taker_fee_bps: Bps,
    /// The venue whose token carries the fee; `None` without a fee.
    #[serde(deserialize_with = "super::required")]
    pub taker_fee_venue: Option<VenueId>,
    /// What leaves the maker's order, in `to_venue` base units.
    pub maker_out: Amount,
    /// What the maker receives of the payment, in `from_venue` base units.
    pub maker_in: Amount,
    /// The taker's linked address on `from_venue`.
    pub payer: Address,
    /// The order's owner on `to_venue`.
    pub maker_owner: Address,
    /// The quote's payment recipient.
    pub maker_recipient: Address,
    /// The address receiving on `to_venue`.
    pub taker_recipient: Address,
    /// Into the linked account or to a wallet.
    pub destination: Destination,
    /// A wallet delivery's receipt; `None` before it and for a venue delivery.
    #[serde(deserialize_with = "super::required")]
    pub received: Option<WalletReceipt>,
    /// The payment lock.
    pub payment_leg: PaymentLeg,
    /// Where the maker's claimed payment was released; `None` until observed.
    #[serde(deserialize_with = "super::required")]
    pub payment_destination: Option<PaymentDestination>,
    /// Where the taker's claimed output was released; `None` until observed.
    #[serde(deserialize_with = "super::required")]
    pub maker_destination: Option<MakerDestination>,
    /// When the taker's signatures were accepted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signed_at: Option<DateTime<Utc>>,
    /// Block time of the finalized payment lock.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payment_locked_at: Option<DateTime<Utc>>,
    /// Block time of the finalized claim releasing the taker's output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maker_claimed_at: Option<DateTime<Utc>>,
    /// The delivery.
    pub maker_leg: MakerLeg,
    /// The fill's lifecycle.
    pub state: FillState,
    /// Why the fill expired or stopped.
    #[serde(deserialize_with = "super::required")]
    pub reason: Option<String>,
    /// The payment must lock by then.
    pub pay_by: DateTime<Utc>,
    /// On an escrow venue, the delivery deadline.
    pub t1: DateTime<Utc>,
    /// The payment refund time.
    pub t2: DateTime<Utc>,
    /// Delivery on Lighter or Aster: when the maker's transfer must execute.
    #[serde(deserialize_with = "super::required")]
    pub deliver_by: Option<DateTime<Utc>>,
    /// Delivery on Lighter or Aster: the deadline to report the transfer.
    #[serde(deserialize_with = "super::required")]
    pub report_by: Option<DateTime<Utc>>,
    /// The taker's account on a Lighter (index) or Aster (address) `to_venue`.
    #[serde(deserialize_with = "super::required")]
    pub delivery_to: Option<String>,
    /// The venue transaction accepted as delivery.
    #[serde(deserialize_with = "super::required")]
    pub delivery_hash: Option<String>,
    /// The venue fee charged to the maker on top of the delivery, in `to_venue` base units.
    pub delivery_fee: Amount,
    /// Deadline of the maker's acceptance, when one is asked.
    #[serde(deserialize_with = "super::required")]
    pub maker_accept_by: Option<DateTime<Utc>>,
    /// When the maker accepted the fill.
    #[serde(deserialize_with = "super::required")]
    pub maker_accepted_at: Option<DateTime<Utc>>,
    /// When the server verified the locked payment.
    #[serde(deserialize_with = "super::required")]
    pub payment_verified_at: Option<DateTime<Utc>>,
    /// When the fill was created.
    pub created_at: DateTime<Utc>,
    /// When the fill last changed.
    pub updated_at: DateTime<Utc>,
    /// When the fill settled.
    #[serde(deserialize_with = "super::required")]
    pub settled_at: Option<DateTime<Utc>>,
    /// Relayed transactions; each fill lists its take's payment.
    pub transactions: Vec<Transaction>,
}

/// A fill as events and snapshots carry it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct FillPreview {
    /// Where the payment came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub funding: Option<Funding>,
    /// Into the linked account or to a wallet.
    pub destination: Destination,
    /// The fill.
    pub id: FillId,
    /// Its take.
    pub take_id: TakeId,
    /// The maker's order.
    pub order_id: OrderId,
    /// The quote sequence it was accepted under.
    pub sequence: u64,
    /// Its quote's index in that set.
    pub quote_index: u32,
    /// The fill's lifecycle.
    pub state: FillState,
    /// The payment lock.
    pub payment_leg: PaymentLeg,
    /// The delivery.
    pub maker_leg: MakerLeg,
    /// Where the maker's claimed payment was released; `None` until observed.
    #[serde(deserialize_with = "super::required")]
    pub payment_destination: Option<PaymentDestination>,
    /// Where the taker's claimed output was released; `None` until observed.
    #[serde(deserialize_with = "super::required")]
    pub maker_destination: Option<MakerDestination>,
    /// What the taker pays, in `from_venue` base units.
    pub amount_in: Amount,
    /// What the taker receives, in `to_venue` base units.
    pub amount_out: Amount,
    /// The taker fee, in `taker_fee_venue` base units.
    pub taker_fee: Amount,
    /// The taker fee rate.
    pub taker_fee_bps: Bps,
    /// The venue whose token carries the fee; `None` without a fee.
    #[serde(deserialize_with = "super::required")]
    pub taker_fee_venue: Option<VenueId>,
    /// What the maker receives of the payment.
    pub maker_in: Amount,
    /// What leaves the maker's order.
    pub maker_out: Amount,
    /// The venue the taker pays on.
    pub from_venue: VenueId,
    /// The venue the taker receives on.
    pub to_venue: VenueId,
    /// The payment must lock by then.
    pub pay_by: DateTime<Utc>,
    /// On an escrow venue, the delivery deadline.
    pub t1: DateTime<Utc>,
    /// The payment refund time.
    pub t2: DateTime<Utc>,
    /// Delivery on Lighter or Aster: when the maker's transfer must execute.
    #[serde(deserialize_with = "super::required")]
    pub deliver_by: Option<DateTime<Utc>>,
    /// Delivery on Lighter or Aster: the deadline to report the transfer.
    #[serde(deserialize_with = "super::required")]
    pub report_by: Option<DateTime<Utc>>,
    /// The taker's account on a Lighter (index) or Aster (address) `to_venue`.
    #[serde(deserialize_with = "super::required")]
    pub delivery_to: Option<String>,
    /// The venue transaction accepted as delivery.
    #[serde(deserialize_with = "super::required")]
    pub delivery_hash: Option<String>,
    /// The venue fee charged to the maker on top of the delivery.
    pub delivery_fee: Amount,
    /// Deadline of the maker's acceptance, when one is asked.
    #[serde(deserialize_with = "super::required")]
    pub maker_accept_by: Option<DateTime<Utc>>,
    /// When the maker accepted the fill.
    #[serde(deserialize_with = "super::required")]
    pub maker_accepted_at: Option<DateTime<Utc>>,
    /// When the server verified the locked payment.
    #[serde(deserialize_with = "super::required")]
    pub payment_verified_at: Option<DateTime<Utc>>,
    /// Why the fill expired or stopped.
    #[serde(deserialize_with = "super::required")]
    pub reason: Option<String>,
    /// The reader's side.
    pub role: FillRole,
}

/// A fill as fill history lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct FillHistoryRow {
    /// Where the payment came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub funding: Option<Funding>,
    /// The fill.
    pub id: FillId,
    /// Its take.
    pub take_id: TakeId,
    /// The reader's side.
    pub role: FillRole,
    /// The maker's order.
    pub order_id: OrderId,
    /// The quote sequence it was accepted under.
    pub sequence: u64,
    /// The venue the taker pays on.
    pub from_venue: VenueId,
    /// The venue the taker receives on.
    pub to_venue: VenueId,
    /// What the taker receives, in `to_venue` base units.
    pub amount_out: Amount,
    /// What the taker pays, in `from_venue` base units.
    pub amount_in: Amount,
    /// The taker fee, in `taker_fee_venue` base units.
    pub taker_fee: Amount,
    /// The taker fee rate.
    pub taker_fee_bps: Bps,
    /// The venue whose token carries the fee; `None` without a fee.
    #[serde(deserialize_with = "super::required")]
    pub taker_fee_venue: Option<VenueId>,
    /// What the maker receives of the payment.
    pub maker_in: Amount,
    /// What leaves the maker's order.
    pub maker_out: Amount,
    /// The payment lock.
    pub payment_leg: PaymentLeg,
    /// The delivery.
    pub maker_leg: MakerLeg,
    /// The fill's lifecycle.
    pub state: FillState,
    /// The payment must lock by then.
    pub pay_by: DateTime<Utc>,
    /// On an escrow venue, the delivery deadline.
    pub t1: DateTime<Utc>,
    /// The payment refund time.
    pub t2: DateTime<Utc>,
    /// When the fill was created.
    pub created_at: DateTime<Utc>,
    /// When the fill last changed.
    pub updated_at: DateTime<Utc>,
    /// When the fill settled.
    #[serde(deserialize_with = "super::required")]
    pub settled_at: Option<DateTime<Utc>>,
    /// Newer state has a higher revision.
    pub revision: Revision,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::TakePreparation;

    #[test]
    fn debug_never_shows_a_claim_secret_even_nested() {
        let mut fill: serde_json::Value =
            crate::testing::json::fixture("take_preparation/take/fills/0");
        fill["secret"] = format!("0x{}", "7a".repeat(32)).into();
        let mut preparation: serde_json::Value = crate::testing::json::fixture("take_preparation");
        preparation["take"]["fills"] = serde_json::json!([fill.clone()]);
        let fill: Fill = serde_json::from_value(fill).unwrap();
        let preparation: TakePreparation = serde_json::from_value(preparation).unwrap();
        assert_eq!(fill.secret.unwrap().expose_secret(), &B256::repeat_byte(0x7a));
        for debug in [format!("{fill:?}"), format!("{preparation:#?}")] {
            assert!(debug.contains("ClaimSecret(****)"), "{debug}");
            assert!(!debug.contains("7a7a") && !debug.contains("122, 122"), "{debug}");
        }
    }
}
