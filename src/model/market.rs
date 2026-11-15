//! Market data models: venues, books, markets, volume and trade proofs.

use alloy_primitives::{Address, B256, Bytes};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Destination, Funding, Transaction, WalletFundingCapability, WalletReceipt};
use crate::types::{Amount, Bps, FillId, OrderId, Price, VenueId, open_enum};

/// The executable quotes for paying on `from` and receiving on `to` (`GET /v1/books`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Book {
    /// The venue the taker pays on.
    pub from: VenueId,
    /// The venue the taker receives on.
    pub to: VenueId,
    /// The most one fill takes from one level, in `to` base units.
    pub max_fill: Amount,
    /// The pair's taker fee now; levels show the makers' prices without it.
    pub taker_fee_bps: Bps,
    /// The quotes, best price first.
    pub levels: Vec<Level>,
}

/// One quote of an order: the taker pays `receive` on `from` per `supply` on `to`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Level {
    /// The order.
    pub order_id: OrderId,
    /// The quote's position in the order's signed set.
    pub quote_index: u32,
    /// Price denominator and this quote's budget in the sequence, in `to` base units.
    pub supply: Amount,
    /// Price numerator, in `from` base units.
    pub receive: Amount,
    /// What the level can fill now, in `to` base units.
    pub available: Amount,
    /// Executable minimum, in `to` base units.
    pub min_fill: Amount,
    /// Signed on-chain settlement deadline, after `valid_until`.
    pub expires_at: DateTime<Utc>,
    /// When the quote was signed.
    pub quoted_at: DateTime<Utc>,
    /// The quote set's sequence.
    pub sequence: u64,
    /// Last admission time.
    pub valid_until: DateTime<Utc>,
}

open_enum! {
    /// How a venue settles the maker's side of a fill.
    pub enum Settlement {
        /// Locked in the escrow on the venue's chain.
        Escrow = "escrow",
        /// Delivered by the maker inside the venue (Lighter, Aster).
        Venue = "venue",
    }
}

open_enum! {
    /// How a margin move signs its amount.
    pub enum AmountFormat {
        /// Integer base units.
        BaseUnits = "base_units",
        /// Decimal tokens, such as `"5.25"`.
        Decimal = "decimal",
    }
}

open_enum! {
    /// Where a venue withdrawal may go.
    pub enum MarginDestination {
        /// The venue wallet.
        Wallet = "wallet",
        /// The signer.
        Signer = "signer",
        /// The escrow.
        Escrow = "escrow",
    }
}

/// A venue of this deployment (`GET /v1/venues`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Venue {
    /// The venue.
    pub id: VenueId,
    /// Its display name.
    pub name: String,
    /// Enabled and configured in this deployment.
    pub enabled: bool,
    /// How it settles; `None` for a disabled venue.
    #[serde(deserialize_with = "super::required")]
    pub settlement: Option<Settlement>,
    /// Its escrow or signing chain; `None` for a disabled venue.
    #[serde(deserialize_with = "super::required")]
    pub chain: Option<EscrowChain>,
    /// Its margin transfer format; `None` when disabled or without margin transfers.
    #[serde(deserialize_with = "super::required")]
    pub margin_move: Option<MarginMoveSpec>,
    /// The smallest amount the venue credits, in its base units; a smaller escrow release stays
    /// in escrow.
    #[serde(deserialize_with = "super::required")]
    pub release_minimum: Option<Amount>,
}

/// A venue's escrow chain, or for Lighter and Aster its signing terms.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct EscrowChain {
    /// Whether the escrow accepts direct deposits; absent means no.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direct_deposit: Option<bool>,
    /// Whether direct withdrawals from the venue account to its owner's wallet exist; absent
    /// means no.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direct_withdrawal: Option<bool>,
    /// Wallet funding in the escrow token; `None` (or absent) when unsupported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wallet_funding: Option<WalletFundingCapability>,
    /// What a wallet delivery pays on this chain; `None` where the escrow cannot deliver to a
    /// wallet.
    #[serde(deserialize_with = "super::required")]
    pub wallet: Option<WalletToken>,
    /// The chain id (999 HyperEVM, 137 Polygon, 42161 Arbitrum; 304 Lighter, 56 Aster).
    pub id: u64,
    /// The escrow (Lighter: its zkLighter contract; Aster: its vault).
    pub escrow: Address,
    /// The venue token.
    pub token: Address,
    /// The token's symbol.
    pub symbol: String,
    /// The token's decimals.
    pub decimals: u8,
    /// The token's EIP-2612 domain name and version; `None` on Lighter and Aster.
    #[serde(deserialize_with = "super::required")]
    pub permit: Option<PermitDomain>,
}

/// The token a wallet delivery pays.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WalletToken {
    /// The token.
    pub token: Address,
    /// Its symbol.
    pub symbol: String,
    /// Its decimals.
    pub decimals: u8,
    /// The least each part of a wallet take delivers, in this token's base units.
    pub minimum: Amount,
}

/// An EIP-2612 domain's name and version; the chain id and token complete it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PermitDomain {
    /// The domain name.
    pub name: String,
    /// The domain version.
    pub version: String,
}

/// A venue's margin transfer format: the typed data every withdrawal from its margin signs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct MarginMoveSpec {
    /// The signed `primaryType`.
    pub primary_type: String,
    /// The EIP-712 types, as served.
    pub types: Value,
    /// The EIP-712 domain, as served.
    pub domain: Value,
    /// Fields every transfer signs with these values.
    pub fixed: serde_json::Map<String, Value>,
    /// How the amount is signed.
    pub amount: AmountFormat,
    /// Where a withdrawal from this venue may go.
    pub destinations: Vec<MarginDestination>,
}

/// Prices, liquidity and volume per venue (`GET /v1/markets`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Markets {
    /// When the figures were computed.
    pub as_of: DateTime<Utc>,
    /// One entry per enabled venue.
    pub venues: Vec<MarketVenue>,
}

/// One venue's market figures.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct MarketVenue {
    /// The venue.
    pub venue: VenueId,
    /// Its token's symbol.
    pub symbol: String,
    /// Its token's decimals.
    pub decimals: u8,
    /// The best price makers pay for the venue's token.
    #[serde(deserialize_with = "super::required")]
    pub best_buy: Option<Price>,
    /// The best price makers sell the venue's token at.
    #[serde(deserialize_with = "super::required")]
    pub best_sell: Option<Price>,
    /// What the books show of the pools supplying the venue, in its base units.
    pub liquidity: Amount,
    /// Settled public trades delivered on the venue in the last 24 hours, in its base units.
    pub volume_24h: Amount,
}

/// A pair's trading volume over the last 24 hours (`GET /v1/volume`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Volume {
    /// One venue of the pair.
    pub a: VenueId,
    /// The other venue of the pair.
    pub b: VenueId,
    /// The volume, in `b` base units.
    pub volume_24h: Amount,
    /// The number of trades.
    pub trades_24h: u64,
}

/// A settled trade with everything signed for it (`GET /v1/trades/{id}/proof`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TradeProof {
    /// Where the payment came from; absent on older trades.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub funding: Option<Funding>,
    /// Where the trade delivered.
    pub destination: Destination,
    /// A wallet delivery's receipt; `None` for a venue delivery.
    #[serde(deserialize_with = "super::required")]
    pub received: Option<WalletReceipt>,
    /// The proof format: 2 for a trade with a taker fee.
    pub version: u8,
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
    /// Version 2: the fee the `swap_id` carries, in `taker_fee_venue` base units.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub taker_fee: Option<Amount>,
    /// Version 2: the venue whose token `taker_fee` is in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub taker_fee_venue: Option<VenueId>,
    /// The escrow swap id.
    pub swap_id: B256,
    /// The maker's order.
    pub order_id: OrderId,
    /// The priced quote set's sequence.
    pub sequence: u64,
    /// The priced quote's position in the maker's signed set.
    pub quote_index: u32,
    /// The order's owner.
    pub maker_owner: Address,
    /// Who paid.
    pub payer: Address,
    /// Where the payment went.
    pub maker_recipient: Address,
    /// Where the delivery went.
    pub taker_recipient: Address,
    /// When the trade settled.
    pub settled_at: DateTime<Utc>,
    /// The maker's signed quote set.
    pub maker: SignedAgreement,
    /// The owner's `FundOffer`; `None` for in-venue inventory.
    #[serde(deserialize_with = "super::required")]
    pub maker_authorization: Option<SignedAgreement>,
    /// The delivery to Lighter or Aster; `None` otherwise.
    #[serde(deserialize_with = "super::required")]
    pub delivery: Option<VenueDelivery>,
    /// The payer's `PayTake`; `None` for a take paid from Lighter or Aster.
    #[serde(deserialize_with = "super::required")]
    pub taker: Option<SignedAgreement>,
    /// The taker's executed transfer, for a take paid from Lighter or Aster; `None` otherwise.
    #[serde(deserialize_with = "super::required")]
    pub payment: Option<VenuePayment>,
    /// The escrow transactions of the trade.
    pub transactions: Vec<Transaction>,
}

/// EIP-712 typed data with its signature.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SignedAgreement {
    /// Who signed.
    pub signer: Address,
    /// What was signed, as served.
    pub typed_data: Value,
    /// The signature (an EOA signature, or on older proofs an ERC-1271 one).
    pub signature: Bytes,
    /// The EIP-712 digest of `typed_data`.
    pub digest: B256,
}

/// A fill's delivery to Lighter or Aster.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct VenueDelivery {
    /// The receiving venue.
    pub venue: VenueId,
    /// The Lighter transaction hash, or the Aster `transferId`.
    pub tx_hash: String,
    /// The taker's account on the venue: a Lighter index in decimal, an Aster address.
    #[serde(deserialize_with = "super::required")]
    pub account: Option<String>,
    /// When it executed.
    #[serde(deserialize_with = "super::required")]
    pub executed_at: Option<DateTime<Utc>>,
}

/// A take's payment from Lighter or Aster.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct VenuePayment {
    /// The paying venue.
    pub venue: VenueId,
    /// Lighter's transaction hash (without `0x`), or Aster's `transferId`.
    pub tx_hash: String,
    /// When it executed.
    #[serde(deserialize_with = "super::required")]
    pub executed_at: Option<DateTime<Utc>>,
    /// The transfer as sent, as served.
    pub tx_info: serde_json::Map<String, Value>,
}
