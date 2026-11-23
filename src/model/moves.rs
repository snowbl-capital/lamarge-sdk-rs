//! Move models: a move's plan, its record and its bridges.

use alloy_primitives::{Address, B256};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use super::Route;
use crate::types::{
    Amount, Bps, BridgeId, ClientId, HyperliquidNonce, MoveId, VenueId, WithdrawalId, open_enum,
};

open_enum! {
    /// What a move or bridge endpoint is: a venue account, or a wallet token.
    pub enum MoveVenue {
        /// A Hyperliquid account (perps USDC).
        Hyperliquid = "hyperliquid",
        /// A Polymarket account.
        Polymarket = "polymarket",
        /// A GMX account.
        Gmx = "gmx",
        /// An Aster account.
        Aster = "aster",
        /// A Lighter account.
        Lighter = "lighter",
        /// A wallet token on a chain.
        Wallet = "wallet",
    }
}

/// An endpoint of a move or bridge: a venue account, or a wallet token on a chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct MoveEndpoint {
    /// The account's venue, or [`MoveVenue::Wallet`].
    pub venue: MoveVenue,
    /// A wallet's chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chain_id: Option<u64>,
    /// A wallet's token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<Address>,
}

impl MoveEndpoint {
    /// The account on `venue`.
    pub fn account(venue: &VenueId) -> Self {
        let Ok(venue) = venue.as_str().parse();
        Self { venue, chain_id: None, token: None }
    }

    /// `token` in a wallet on `chain_id`.
    pub fn wallet(chain_id: u64, token: Address) -> Self {
        Self { venue: MoveVenue::Wallet, chain_id: Some(chain_id), token: Some(token) }
    }
}

open_enum! {
    /// Why LaMarge's makers take no part of a move.
    pub enum LamargeUnavailable {
        /// No route between the endpoints.
        NoRoute = "no_route",
        /// No maker fills at the floor rate.
        NoLiquidity = "no_liquidity",
        /// The destination account cannot receive yet.
        NoDeposit = "no_deposit",
        /// The account is not admitted.
        NotAdmitted = "not_admitted",
        /// A venue or chain is unhealthy.
        Unhealthy = "unhealthy",
        /// The pair's fee is above `max_fee_bps`.
        FeeConsent = "fee_consent",
    }
}

open_enum! {
    /// Why no bridge carries the rest of a move.
    pub enum BridgeUnavailable {
        /// The provider lacks the liquidity.
        InsufficientLiquidity = "insufficient_liquidity",
        /// Below the provider's or the venue's minimum.
        BelowMinimum = "below_minimum",
        /// No bridge for these endpoints.
        Unsupported = "unsupported",
        /// A portfolio-margin Hyperliquid account, or one in an unsupported mode, cannot be the
        /// origin.
        HyperliquidMode = "hyperliquid_mode",
        /// The provider's guaranteed output misses the part's floor.
        Slippage = "slippage",
        /// The provider did not answer in time.
        ProviderUnavailable = "provider_unavailable",
        /// The deployment has no bridge provider.
        Disabled = "disabled",
    }
}

open_enum! {
    /// An external bridge provider.
    pub enum BridgeProvider {
        /// Relay (relay.link).
        Relay = "relay",
    }
}

open_enum! {
    /// A move's lifecycle.
    pub enum MoveState {
        /// Parts may still be added.
        Open = "open",
        /// Closed, with a part still running.
        Settling = "settling",
        /// Closed, every part final.
        Final = "final",
    }
}

open_enum! {
    /// A bridge's lifecycle.
    pub enum BridgeState {
        /// Quoted, nothing signed.
        Prepared = "prepared",
        /// Signatures received, or the deposit prompt fenced.
        Submitting = "submitting",
        /// The provider saw the deposit.
        Pending = "pending",
        /// Delivered.
        Completed = "completed",
        /// Refunded (`Bridge::refund` says what and where).
        Refunded = "refunded",
        /// Debited, neither filled nor refunded: contact the provider with `request_id`.
        Stuck = "stuck",
        /// Nothing debited.
        Failed = "failed",
        /// Nothing signed or fenced in time.
        Expired = "expired",
    }
}

/// A move's plan: what LaMarge fills now, what a bridge would carry of the rest. Reserves
/// nothing; each part is quoted again before signing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct MoveQuote {
    /// LaMarge's part.
    #[serde(deserialize_with = "super::required")]
    pub lamarge: Option<LamargePart>,
    /// Why LaMarge takes no part.
    #[serde(deserialize_with = "super::required")]
    pub lamarge_unavailable: Option<LamargeUnavailable>,
    /// The bridge's part, an estimate.
    #[serde(deserialize_with = "super::required")]
    pub bridge: Option<BridgeQuote>,
    /// Why no bridge carries the rest.
    #[serde(deserialize_with = "super::required")]
    pub bridge_unavailable: Option<BridgeUnavailable>,
    /// What neither part carries, in the origin token's base units.
    pub unallocated: Amount,
    /// Why.
    #[serde(deserialize_with = "super::required")]
    pub unallocated_reason: Option<String>,
}

/// LaMarge's part of a [`MoveQuote`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct LamargePart {
    /// The route that fills it.
    pub route: Route,
    /// What it takes from the origin, in the origin token's base units (a Polygon native-USDC
    /// origin's conversion input).
    pub budget: Amount,
}

/// A bridge's part of a [`MoveQuote`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct BridgeQuote {
    /// The provider.
    pub provider: BridgeProvider,
    /// In the origin token's base units.
    pub amount_in: Amount,
    /// Expected, in the destination token's base units.
    pub amount_out: Amount,
    /// Guaranteed by the provider.
    pub minimum_out: Amount,
    /// The least a Polymarket, GMX or Aster account is credited; it is credited the whole output.
    #[serde(deserialize_with = "super::required")]
    pub account_credit: Option<Amount>,
    /// What reaches the recipient's wallet beside the account, expected: zero.
    #[serde(deserialize_with = "super::required")]
    pub wallet_surplus: Option<Amount>,
    /// Expected seconds to delivery.
    pub eta_seconds: u64,
}

/// A part attached to a move.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct MovePart {
    /// The transfer, withdrawal or direct operation.
    pub id: Uuid,
    /// Whether it moves nothing more.
    #[serde(rename = "final")]
    pub is_final: bool,
}

/// A move: its terms and every part attached to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Move {
    /// The move.
    pub id: MoveId,
    /// The caller's id.
    pub client_move_id: ClientId,
    /// The origin.
    pub from: MoveEndpoint,
    /// The destination.
    pub to: MoveEndpoint,
    /// Everything it takes from the origin, in the origin token's base units.
    pub amount_in: Amount,
    /// The least delivered over all parts, in the destination token's base units.
    pub min_out: Amount,
    /// Who pays.
    pub payer: Address,
    /// An address, or a Lighter account index.
    pub recipient: String,
    /// The highest taker fee LaMarge's part accepts.
    #[serde(deserialize_with = "super::required")]
    pub max_fee_bps: Option<Bps>,
    /// Root transfers (a route's continuation is its root's).
    pub transfers: Vec<MovePart>,
    /// Withdrawals feeding a bridge.
    pub withdrawals: Vec<MovePart>,
    /// GMX direct withdrawals feeding a bridge.
    pub direct: Vec<MovePart>,
    /// Bridges.
    pub bridges: Vec<Bridge>,
    /// When the client declared no more parts.
    #[serde(deserialize_with = "super::required")]
    pub closed_at: Option<DateTime<Utc>>,
    /// What no part carries, as declared on closing.
    #[serde(deserialize_with = "super::required")]
    pub unallocated: Option<Amount>,
    /// Why.
    #[serde(deserialize_with = "super::required")]
    pub unallocated_reason: Option<String>,
    /// Its lifecycle.
    pub state: MoveState,
    /// When it was created.
    pub created_at: DateTime<Utc>,
}

/// An external bridge of part of a move. Check it with
/// [`Session::sign_bridge`](crate::Session::sign_bridge) or
/// [`Session::bridge_deposit`](crate::Session::bridge_deposit) before acting on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Bridge {
    /// The bridge.
    pub id: BridgeId,
    /// Its move.
    pub move_id: MoveId,
    /// The caller's id.
    pub client_bridge_id: ClientId,
    /// The provider.
    pub provider: BridgeProvider,
    /// The provider's request id, for its support.
    pub request_id: B256,
    /// Its lifecycle.
    pub state: BridgeState,
    /// The origin.
    pub from: MoveEndpoint,
    /// The destination.
    pub to: MoveEndpoint,
    /// In the origin token's base units.
    pub amount_in: Amount,
    /// Its floor, in the destination token's base units.
    pub min_out: Amount,
    /// Who pays.
    pub payer: Address,
    /// An address, or a Lighter account index.
    pub recipient: String,
    /// The provider's order, kept as served: the checks derive every payload from it.
    pub order: BridgeOrder,
    /// A `HyperCore` origin's nonce, shared by the nonce mapping and the send.
    #[serde(deserialize_with = "super::required")]
    pub nonce: Option<HyperliquidNonce>,
    /// A `HyperCore` origin's dex, which its send leaves from: `""` (perps) for a standard
    /// account, `spot` for a unified one. Absent from a server before it, which still reads.
    #[serde(default)]
    pub source_dex: Option<String>,
    /// The least a Polymarket, GMX or Aster account is credited; it is credited the whole output.
    #[serde(deserialize_with = "super::required")]
    pub account_credit: Option<Amount>,
    /// Delivered, once completed, in the destination token's base units.
    #[serde(deserialize_with = "super::required")]
    pub amount_out: Option<Amount>,
    /// What reached the recipient's wallet beside the account: zero.
    #[serde(deserialize_with = "super::required")]
    pub wallet_surplus: Option<Amount>,
    /// What the provider refunded, once refunded.
    #[serde(deserialize_with = "super::required")]
    pub refund: Option<BridgeRefund>,
    /// Origin transactions.
    pub origin_txs: Vec<String>,
    /// Destination transactions.
    pub destination_txs: Vec<String>,
    /// Refund transactions.
    pub refund_txs: Vec<String>,
    /// A `HyperCore` origin's send, relayed as a withdrawal.
    #[serde(deserialize_with = "super::required")]
    pub withdrawal_id: Option<WithdrawalId>,
    /// Why it failed.
    #[serde(deserialize_with = "super::required")]
    pub error: Option<String>,
    /// When an unsigned bridge expires.
    pub expires_at: DateTime<Utc>,
    /// When it was created.
    pub created_at: DateTime<Utc>,
    /// When it last changed.
    pub updated_at: DateTime<Utc>,
}

/// A bridge's order at its provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct BridgeOrder {
    /// The order's id, as the provider hashes it.
    pub id: B256,
    /// Relay's `orderData`, verbatim.
    pub data: Value,
    /// Unix seconds.
    pub deadline: u64,
}

/// A wallet-origin bridge's funds: the payer's token balance and allowance toward the
/// provider's depository, read fresh on the origin chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct BridgeFunding {
    /// The payer.
    pub owner: Address,
    /// The origin token.
    pub token: Address,
    /// The provider's depository.
    pub spender: Address,
    /// In the token's base units.
    pub balance: Amount,
    /// Toward `spender`, in the token's base units.
    pub allowance: Amount,
    /// The block read.
    pub block: u64,
    /// When it was read.
    pub observed_at: DateTime<Utc>,
}

/// What a bridge's provider refunded, and where.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct BridgeRefund {
    /// The chain.
    pub chain_id: u64,
    /// The token, as the provider names it.
    pub token: String,
    /// Who received it.
    pub recipient: String,
    /// In the token's base units.
    pub amount: Amount,
}

open_enum! {
    /// What a direct operation does.
    pub enum DirectKind {
        /// From the owner's wallet into its venue account.
        Deposit = "deposit",
        /// From the owner's GMX Account to its own address.
        Withdrawal = "withdrawal",
    }
}

open_enum! {
    /// A direct operation's lifecycle.
    pub enum DirectState {
        /// Recorded, nothing sent.
        Prepared = "prepared",
        /// Fenced or sent, not confirmed yet.
        Pending = "pending",
        /// Its transaction confirmed: `received` arrived.
        Confirmed = "confirmed",
        /// Its transaction reverted.
        Failed = "failed",
        /// Nothing confirmed by its deadline.
        Expired = "expired",
    }
}

/// An operation its owner sends itself, recorded and followed by LaMarge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct DirectOperation {
    /// The move it feeds a bridge for.
    #[serde(deserialize_with = "super::required")]
    pub move_id: Option<MoveId>,
    /// The operation.
    pub id: Uuid,
    /// The caller's id.
    pub client_operation_id: ClientId,
    /// The venue.
    pub venue: VenueId,
    /// What it does.
    pub kind: DirectKind,
    /// The chain it is sent on.
    pub chain_id: u64,
    /// The venue's escrow there.
    pub escrow: Address,
    /// Who sends it.
    pub owner: Address,
    /// Who receives.
    pub recipient: Address,
    /// The token moved.
    pub token: Address,
    /// In the token's base units.
    pub amount: Amount,
    /// The least received.
    pub min_out: Amount,
    /// Its nonce.
    pub nonce: Amount,
    /// Unix seconds.
    pub deadline: Amount,
    /// Its lifecycle.
    pub state: DirectState,
    /// Transaction hashes reported for it.
    pub attempts: Vec<String>,
    /// The confirmed transaction.
    #[serde(deserialize_with = "super::required")]
    pub tx_hash: Option<B256>,
    /// What it delivered, once confirmed.
    #[serde(deserialize_with = "super::required")]
    pub received: Option<Amount>,
}
