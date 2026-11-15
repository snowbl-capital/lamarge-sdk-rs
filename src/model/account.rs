//! The session's account: profile, linked accounts, balances, statistics, exposure and escrow
//! orders.

use alloy_primitives::Address;
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use super::SetupStatus;
use crate::types::{
    Amount, BlockNumber, Decimal, OrderId, PortfolioRevision, QuoteIdentityId, SignedAmount,
    VenueId, open_enum,
};

open_enum! {
    /// Whether the user may make new commitments.
    pub enum AccessState {
        /// Allowed (admins always are).
        Granted = "granted",
        /// Not yet: invite mode, until a code is redeemed or an admin admits the user.
        Required = "required",
        /// Suspended.
        Suspended = "suspended",
    }
}

/// The signed-in user (`GET /v1/me`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Me {
    /// The address that signed in.
    pub master_address: Address,
    /// When the user was created.
    pub created_at: DateTime<Utc>,
    /// The linked venue accounts.
    pub accounts: Vec<LinkedAccount>,
    /// The user's own (browser) quote identity.
    pub quote_identity_id: QuoteIdentityId,
    /// Whether the user may make new commitments.
    pub access: AccessState,
    /// Whether the user is an admin.
    pub admin: bool,
}

/// A venue account linked to the user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct LinkedAccount {
    /// The account's setup steps as recorded.
    pub setup: Vec<SetupStatus>,
    /// The venue.
    pub venue: VenueId,
    /// The account's owner; signs its transfers.
    pub address: Address,
    /// Polymarket: the signer's Deposit Wallet or Safe; Lighter: the account index; `None`
    /// elsewhere.
    #[serde(deserialize_with = "super::required")]
    pub venue_wallet: Option<String>,
    /// When the account was linked.
    pub linked_at: DateTime<Utc>,
}

/// A linked account's funds on one venue (`GET /v1/balances`). Amounts are in the venue
/// token's base units.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Balance {
    /// The account's setup steps as recorded.
    pub setup: Vec<SetupStatus>,
    /// Funds on the venue, in its wallets and in the escrow, each counted once.
    pub holdings: Holdings,
    /// The portfolio change counter.
    pub portfolio: PortfolioObservation,
    /// The venue.
    pub venue: VenueId,
    /// The linked address.
    pub address: Address,
    /// The venue wallet (see [`LinkedAccount::venue_wallet`]).
    #[serde(deserialize_with = "super::required")]
    pub venue_wallet: Option<String>,
    /// The margin equity; `None` before the first read.
    #[serde(deserialize_with = "super::required")]
    pub equity: Option<SignedAmount>,
    /// What can leave the venue margin, less transfers since `read_at`.
    #[serde(deserialize_with = "super::required")]
    pub withdrawable: Option<Amount>,
    /// The estimated venue withdrawal fee; `None` when unavailable.
    #[serde(deserialize_with = "super::required")]
    pub withdrawal_fee: Option<Amount>,
    /// Source-token balance kept back for withdrawal gas, not part of the signed amount.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub withdrawal_reserve: Option<Amount>,
    /// Whether `withdrawable` is only an upper bound from the venue's public data.
    pub withdrawable_estimated: bool,
    /// The account mode on venues with several.
    #[serde(deserialize_with = "super::required")]
    pub mode: Option<String>,
    /// False while the venue knows no account for the address.
    pub has_account: bool,
    /// The venue token outside the margin, where the venue has one.
    #[serde(deserialize_with = "super::required")]
    pub wallet: Option<Amount>,
    /// A standard Hyperliquid account's trade.xyz balance; `None` elsewhere.
    #[serde(deserialize_with = "super::required")]
    pub xyz: Option<XyzBalance>,
    /// The linked address's token balance on the escrow chain.
    #[serde(deserialize_with = "super::required")]
    pub chain_balance: Option<Amount>,
    /// The escrow available balance, which takes pay from first.
    #[serde(deserialize_with = "super::required")]
    pub escrow_balance: Option<Amount>,
    /// Waiting at `deposit_address` to be swept into the available balance.
    #[serde(deserialize_with = "super::required")]
    pub deposit_balance: Option<Amount>,
    /// Tokens sent here become the available balance; `None` on Lighter and Aster.
    #[serde(deserialize_with = "super::required")]
    pub deposit_address: Option<Address>,
    /// The last successful venue read.
    #[serde(deserialize_with = "super::required")]
    pub read_at: Option<DateTime<Utc>>,
    /// Why the last venue read failed.
    #[serde(deserialize_with = "super::required")]
    pub error: Option<String>,
    /// The last successful chain read.
    #[serde(deserialize_with = "super::required")]
    pub chain_read_at: Option<DateTime<Utc>>,
    /// Why the last chain read failed.
    #[serde(deserialize_with = "super::required")]
    pub chain_error: Option<String>,
}

/// A standard Hyperliquid account's trade.xyz (HIP-3 dex `xyz`) balance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct XyzBalance {
    /// The equity there.
    pub equity: SignedAmount,
    /// What can leave it, less transfers since `read_at`.
    pub withdrawable: Amount,
    /// trade.xyz's last read; `None` (with zero amounts) until the first.
    #[serde(deserialize_with = "super::required")]
    pub read_at: Option<DateTime<Utc>>,
}

/// An account's funds on a venue, in its wallets and in the escrow, each counted once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Holdings {
    /// Movable or committable now.
    pub available: Amount,
    /// `total` − `available`.
    pub locked: SignedAmount,
    /// What the account's unpaid takes will spend of its escrow-chain funds.
    pub committed: Amount,
    /// Unlocked escrow of the account's orders this deployment operates.
    pub orders: Amount,
    /// A Lighter or Aster pool's inventory: part of the equity, never added to it.
    pub pool: Amount,
    /// Legs of the account's fills locked in escrow.
    pub trades: Amount,
    /// Everything the account holds on the venue's side.
    pub total: SignedAmount,
}

/// A portfolio change counter: keep the prior observation while `revision` moves or `pending`
/// is true.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PortfolioObservation {
    /// The change counter.
    pub revision: PortfolioRevision,
    /// When the portfolio last changed.
    #[serde(deserialize_with = "super::required")]
    pub changed_at: Option<DateTime<Utc>>,
    /// Whether a change is being observed.
    pub pending: bool,
}

/// The user's trading statistics (`GET /v1/me/stats`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Stats {
    /// The user's results as a maker.
    pub maker: MakerResults,
    /// Per venue, by output venue.
    pub assets: Vec<StatsAsset>,
    /// Trades in the last 30 days.
    pub trades_30d: u64,
}

/// What the user's orders received less what they delivered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct MakerResults {
    /// Fills.
    pub fills: u64,
    /// Fills valued at the maker's marks.
    pub marked_fills: u64,
    /// Delivered, every token at 1 USD.
    pub volume_usd: Decimal,
    /// Profit and loss, in USD.
    pub pnl_usd: Decimal,
    /// `pnl_usd` over the delivered value; `None` without a delivery.
    #[serde(deserialize_with = "super::required")]
    pub pnl_bps: Option<Decimal>,
    /// By paying venue, then delivering venue.
    pub pairs: Vec<PairResult>,
    /// By UTC day a fill was taken, days without one left out.
    pub daily: Vec<DailyResult>,
}

/// A maker's results on one pair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PairResult {
    /// The paying venue.
    pub from: VenueId,
    /// The delivering venue.
    pub to: VenueId,
    /// Fills.
    pub fills: u64,
    /// Delivered, in `to` base units.
    pub delivered: Amount,
    /// Profit and loss, in USD.
    pub pnl_usd: Decimal,
    /// Profit and loss in basis points; `None` without a delivery.
    #[serde(deserialize_with = "super::required")]
    pub pnl_bps: Option<Decimal>,
}

/// A maker's results on one UTC day.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct DailyResult {
    /// The day.
    pub day: NaiveDate,
    /// Fills taken that day.
    pub fills: u64,
    /// Profit and loss, in USD.
    pub pnl_usd: Decimal,
    /// Profit and loss since the first fill, in USD.
    pub cumulative_usd: Decimal,
}

/// The user's 30-day volume on one venue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct StatsAsset {
    /// The venue.
    pub venue: VenueId,
    /// The escrow chain; `None` on Lighter and Aster.
    #[serde(deserialize_with = "super::required")]
    pub chain_id: Option<u64>,
    /// The escrow token; `None` on Lighter and Aster.
    #[serde(deserialize_with = "super::required")]
    pub token: Option<Address>,
    /// The token's symbol.
    pub symbol: String,
    /// The token's decimals.
    pub decimals: u8,
    /// Volume, in base units.
    pub volume_30d: Amount,
    /// Volume as taker.
    pub taker_volume_30d: Amount,
    /// Volume as maker.
    pub maker_volume_30d: Amount,
    /// Trades.
    pub trades_30d: u64,
}

/// Funds committed to trading (`GET /v1/me/exposure`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Exposure {
    /// Per venue.
    pub assets: Vec<ExposureAsset>,
    /// The server's time.
    pub server_time: DateTime<Utc>,
}

/// Funds committed on one venue, in its base units.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ExposureAsset {
    /// The venue.
    pub venue: VenueId,
    /// The escrow chain; `None` on Lighter and Aster.
    #[serde(deserialize_with = "super::required")]
    pub chain_id: Option<u64>,
    /// The escrow token; `None` on Lighter and Aster.
    #[serde(deserialize_with = "super::required")]
    pub token: Option<Address>,
    /// The token's symbol.
    pub symbol: String,
    /// The token's decimals.
    pub decimals: u8,
    /// How current the chain reads are.
    #[serde(deserialize_with = "super::required")]
    pub chain_observation: Option<ChainObservation>,
    /// Orders' unlocked escrow, Lighter and Aster pools excluded.
    pub maker_escrow: Amount,
    /// A Lighter or Aster pool's transferable balance less deliveries since its read.
    pub maker_pool_inventory: Amount,
    /// Output of the maker's reserved or paid fills not yet delivered.
    pub maker_reserved: Amount,
    /// Output delivered with the payment still locked.
    pub maker_delivered_unpaid: Amount,
    /// Payments to the maker still locked.
    pub maker_payment_locked: Amount,
    /// The taker's payments still locked.
    pub taker_payment_locked: Amount,
}

/// The chain reads exposure is based on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ChainObservation {
    /// The watched block.
    pub block: BlockNumber,
    /// The finalized block.
    #[serde(deserialize_with = "super::required")]
    pub finalized_block: Option<BlockNumber>,
    /// When the watched block was read.
    pub checked_at: DateTime<Utc>,
    /// When the finalized block was read.
    #[serde(deserialize_with = "super::required")]
    pub finalized_at: Option<DateTime<Utc>>,
    /// Whether the observation is current; `None` without a finalized head.
    #[serde(deserialize_with = "super::required")]
    pub current: Option<bool>,
}

/// An order of a linked address in an escrow, with its unlocked funds (`GET /v1/escrow`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct EscrowOrder {
    /// The venue of the escrow.
    pub venue: VenueId,
    /// The order; zero is the owner's available balance.
    pub order_id: OrderId,
    /// The owner.
    pub owner: Address,
    /// Unlocked: what a close releases, in `venue` base units.
    pub available: Amount,
    /// The operator named at funding.
    pub operator: Address,
    /// Funded with this deployment's operator: a close can close it.
    pub managed: bool,
    /// Already counted in the balances' `holdings.orders` (order zero is `escrow_balance`).
    pub in_balances: bool,
    /// A withdrawal is pending.
    pub closing: bool,
}
