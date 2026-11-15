//! Transfer models: intents, routes and funding plans.

use alloy_primitives::Address;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use super::{Destination, Dex, Funding, TokenAmount, Withdrawal};
use crate::{
    error::{ErrorCode, Reason},
    types::{
        Amount, Bps, ClientId, MaxParts, MoveId, SetupStepKind, TakeId, TransferId, VenueId,
        open_enum,
    },
};

/// The takes that would pay now: one, or two through another venue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Route {
    /// Where the payment comes from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub funding: Option<Funding>,
    /// Where the route delivers.
    pub destination: Destination,
    /// For a wallet destination, the least its recipient receives of the wallet token.
    #[serde(deserialize_with = "super::required")]
    pub minimum_received: Option<TokenAmount>,
    /// The paying venue.
    pub from: VenueId,
    /// The receiving venue.
    pub to: VenueId,
    /// What the first take pays, in `from` base units.
    pub amount_in: Amount,
    /// Whether the route spends less than asked beyond rounding.
    pub capped: bool,
    /// What the last take delivers, in `to` base units.
    pub amount_out: Amount,
    /// The takes in order.
    pub legs: Vec<RouteLeg>,
}

/// One take of a [`Route`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RouteLeg {
    /// The paying venue.
    pub from: VenueId,
    /// The receiving venue.
    pub to: VenueId,
    /// What it pays, in `from` base units.
    pub amount_in: Amount,
    /// What it delivers, in `to` base units.
    pub amount_out: Amount,
    /// The price levels it goes through.
    pub parts: u8,
    /// Its fee, included in its amounts: in `from` base units, or in `to` base units when
    /// paying from Lighter or Aster.
    pub taker_fee: Amount,
    /// The pair's fee now.
    pub taker_fee_bps: Bps,
}

open_enum! {
    /// Where a funding plan's withdrawal goes.
    pub enum WithdrawalDestination {
        /// To the signer.
        Signer = "signer",
        /// To the payer's deposit address, swept into the escrow's available balance.
        Escrow = "escrow",
    }
}

open_enum! {
    /// Whether the escrow may pull the payer's wallet token.
    pub enum PaymentSetup {
        /// Ready.
        Done = "done",
        /// A permit is submitted, not yet effective.
        Pending = "pending",
        /// A permit is needed first (the plan lists it).
        Required = "required",
    }
}

open_enum! {
    /// A wallet request a funding plan expects.
    pub enum FundingRequestKind {
        /// A token permit for the escrow.
        Permit = "permit",
        /// A withdrawal out of venue margin.
        MarginMove = "margin_move",
        /// GMX only: a transaction the signer sends itself.
        TransferOut = "transfer_out",
        /// The take's payment.
        PayTake = "pay_take",
    }
}

open_enum! {
    /// A grouped route's lifecycle.
    pub enum GroupedRouteState {
        /// Its takes hold no maker inventory; commit before `commit_by`.
        Prepared = "prepared",
        /// Committed; the operator runs the route.
        Committed = "committed",
        /// Never committed; its second take released.
        Aborted = "aborted",
        /// Committed, and nothing of it can move any more.
        Closed = "closed",
    }
}

open_enum! {
    /// A Polygon wallet conversion's progress.
    pub enum ConversionState {
        /// Prepared, not sent.
        Prepared = "prepared",
        /// Sent, not confirmed.
        Pending = "pending",
        /// Converted.
        Ready = "ready",
        /// Past its deadline.
        Expired = "expired",
    }
}

/// A transfer: one take from `from` to `to`, with its funding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TransferIntent {
    /// The move it belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub move_id: Option<MoveId>,
    /// The Polygon wallet conversion funding it, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversion: Option<WalletConversion>,
    /// Where the payment comes from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub funding: Option<Funding>,
    /// Where it delivers.
    pub destination: Destination,
    /// The transfer.
    pub id: TransferId,
    /// The caller's idempotency key.
    pub client_transfer_id: ClientId,
    /// The key its take uses: `transfer:<id>`.
    pub client_take_id: ClientId,
    /// The paying venue.
    pub from: VenueId,
    /// The receiving venue.
    pub to: VenueId,
    /// The most paid, in `from` base units.
    pub amount_in: Amount,
    /// The least received, in `to` base units.
    pub min_out: Amount,
    /// The most maker fills.
    pub max_parts: MaxParts,
    /// The highest taker fee accepted; `None` when it consented to no fee.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_fee_bps: Option<Bps>,
    /// The Hyperliquid balance its withdrawal leaves, when not the perps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_dex: Option<Dex>,
    /// The linked account on `from`, or with wallet funding the paying wallet.
    pub payer: Address,
    /// The receiving account, or with a wallet destination any address.
    pub recipient: Address,
    /// `from`'s chain.
    pub from_chain_id: u64,
    /// `to`'s chain.
    pub to_chain_id: u64,
    /// The prepared, unsigned withdrawal.
    #[serde(deserialize_with = "super::required")]
    pub withdrawal_preparation: Option<PreparedWithdrawal>,
    /// The submitted withdrawal.
    #[serde(deserialize_with = "super::required")]
    pub withdrawal: Option<Withdrawal>,
    /// The expected wallet requests, when computed.
    #[serde(deserialize_with = "super::required")]
    pub funding_plan: Option<FundingPlan>,
    /// Why `funding_plan` could not be computed when it was asked for.
    #[serde(deserialize_with = "super::required")]
    pub funding_plan_error: Option<FundingPlanError>,
    /// Its take, once made.
    #[serde(deserialize_with = "super::required")]
    pub take_id: Option<TakeId>,
    /// Whether every fill of its take is final; `None` without a take.
    #[serde(deserialize_with = "super::required")]
    pub take_final: Option<bool>,
    /// A route through `to`: where it ends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub then: Option<TransferThen>,
    /// The transfer this one continues.
    #[serde(deserialize_with = "super::required")]
    pub previous_transfer_id: Option<TransferId>,
    /// With `then`, what a continuation may still pay, in `to` base units.
    #[serde(deserialize_with = "super::required")]
    pub continuable: Option<Amount>,
    /// When it was dismissed.
    #[serde(deserialize_with = "super::required")]
    pub cancelled_at: Option<DateTime<Utc>>,
    /// When it was created.
    pub created_at: DateTime<Utc>,
    /// The grouped route it belongs to.
    #[serde(deserialize_with = "super::required")]
    pub grouped: Option<GroupedRoute>,
}

/// A transfer's prepared withdrawal, not yet signed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PreparedWithdrawal {
    /// In `from` base units.
    pub amount: Amount,
    /// The typed data to sign, as the server prepared it; the SDK checks it before signing.
    pub typed_data: Value,
    /// When it was prepared.
    pub created_at: DateTime<Utc>,
}

/// The wallet requests a transfer expects from current balances and allowances; recheck each
/// preparation before signing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct FundingPlan {
    /// When the balances were read.
    pub observed_at: DateTime<Utc>,
    /// The paying venue.
    pub venue: VenueId,
    /// The paying address.
    pub owner: Address,
    /// The payment, in `venue` base units.
    pub amount: Amount,
    /// What the escrow pulls from the wallet.
    pub deposit: Amount,
    /// What leaves venue margin first.
    pub withdrawal_amount: Amount,
    /// Where that withdrawal goes, if one is needed.
    #[serde(deserialize_with = "super::required")]
    pub withdrawal_to: Option<WithdrawalDestination>,
    /// Whether the escrow may pull the wallet token.
    pub payment_setup: PaymentSetup,
    /// Whether the take is signed at once with the withdrawal, or after its funds arrive.
    pub signed_at_once: bool,
    /// The expected requests, in order.
    pub requests: Vec<FundingRequest>,
}

/// One request of a [`FundingPlan`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct FundingRequest {
    /// What it is.
    pub kind: FundingRequestKind,
    /// Who signs or sends it.
    pub signer: Address,
    /// The chain it is for.
    pub chain_id: u64,
}

/// Why a funding plan could not be computed, as the refusal would carry it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct FundingPlanError {
    /// The error code.
    pub code: ErrorCode,
    /// Human-readable; do not parse.
    pub message: String,
    /// The stable sub-case of `code`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<Reason>,
    /// With `SETUP_REQUIRED`: the venue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub venue: Option<VenueId>,
    /// With `SETUP_REQUIRED`: the step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<SetupStepKind>,
    /// The HTTP status the refusal would have.
    pub status: u16,
    /// Seconds after which the plan may be computed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after: Option<u64>,
}

/// The second transfer of a route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TransferThen {
    /// Where the route ends.
    pub to: VenueId,
    /// The receiving account, or with a wallet destination any address.
    pub recipient: Address,
    /// Where it delivers; the server's default is [`Destination::Venue`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination: Option<Destination>,
    /// What the second transfer pays, in the intermediate venue's base units.
    pub amount_in: Amount,
    /// The least accepted on `to` for that `amount_in`.
    pub min_out: Amount,
    /// The highest taker fee the second take accepts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_fee_bps: Option<Bps>,
    /// `to`'s chain, in an intent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chain_id: Option<u64>,
}

/// A route signed at once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct GroupedRoute {
    /// Its lifecycle.
    pub state: GroupedRouteState,
    /// The route's second transfer.
    pub second_transfer_id: TransferId,
    /// The first transfer's take.
    pub first_take_id: TakeId,
    /// The second transfer's take, once made.
    #[serde(deserialize_with = "super::required")]
    pub second_take_id: Option<TakeId>,
    /// Every signature must be committed before it.
    pub commit_by: DateTime<Utc>,
}

/// A Polygon wallet conversion of native USDC into pUSD, funding a transfer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WalletConversion {
    /// Native USDC converted, in base units.
    pub amount_in: Amount,
    /// The least pUSD received, in base units.
    pub min_out: Amount,
    /// The conversion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Uuid>,
    /// The chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chain_id: Option<u64>,
    /// The escrow funded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub escrow: Option<Address>,
    /// The paying wallet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payer: Option<Address>,
    /// The converter contract.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub converter: Option<Address>,
    /// The token converted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<Address>,
    /// The conversion's nonce.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nonce: Option<Amount>,
    /// The conversion's deadline, Unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline: Option<Amount>,
    /// The block the conversion was prepared at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prepared_block: Option<u64>,
    /// Its progress.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<ConversionState>,
    /// Transaction hashes sent for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempts: Option<Vec<String>>,
    /// The confirmed transaction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tx_hash: Option<String>,
    /// What it received.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub received: Option<String>,
    /// The confirming block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_number: Option<u64>,
}
