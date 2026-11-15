//! Escrow-chain wallets: funding capabilities and balances, the payment permit and receipts.

use alloy_primitives::{Address, B256};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::SignRequestKind;
use uuid::Uuid;

use crate::types::{Amount, VenueId};

/// Wallet funding on an escrow chain (`funding: wallet`): the token takes pay from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WalletFundingCapability {
    /// The other token accepted through a conversion (Polygon: native USDC); absent without one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversion: Option<WalletConversionCapability>,
    /// The escrow token.
    pub token: Address,
    /// Its symbol.
    pub symbol: String,
    /// Its decimals.
    pub decimals: u8,
}

/// A token converted into the escrow token before a wallet-funded take.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WalletConversionCapability {
    /// The token converted.
    pub token: Address,
    /// Its symbol.
    pub symbol: String,
    /// Its decimals.
    pub decimals: u8,
    /// The pinned converter.
    pub converter: Address,
    /// The least the conversion returns, in basis points of its input.
    pub min_out_bps: u32,
    /// The least amount converted, in `token` base units.
    pub minimum: Amount,
}

/// The session's wallet funds on a venue's escrow chain (`GET /v1/wallets/{venue}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WalletFunding {
    /// The convertible token's balance and allowance; absent without a conversion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversion: Option<ConversionBalance>,
    /// The venue.
    pub venue: VenueId,
    /// The escrow chain.
    pub chain_id: u64,
    /// The escrow.
    pub escrow: Address,
    /// The escrow token.
    pub token: Address,
    /// Its symbol.
    pub symbol: String,
    /// Its decimals.
    pub decimals: u8,
    /// The payer.
    pub address: Address,
    /// The payer's wallet balance, in `token` base units.
    pub wallet: Amount,
    /// The payer's escrow available balance.
    pub available: Amount,
    /// What unpaid takes will spend of `wallet`.
    pub committed_wallet: Amount,
    /// What unpaid takes will spend of `available`.
    pub committed_available: Amount,
    /// What a new take may spend of `wallet`.
    pub spendable_wallet: Amount,
    /// What a new take may spend of `available`.
    pub spendable_available: Amount,
    /// The block the balances were read at.
    pub block: u64,
    /// When they were read.
    pub observed_at: DateTime<Utc>,
    /// The escrow's permission to take payments from the wallet.
    pub payment_setup: WalletPermission,
}

/// The payer's balance and escrow allowance of a convertible token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ConversionBalance {
    /// The token.
    pub token: Address,
    /// The payer's balance, in its base units.
    pub balance: Amount,
    /// The allowance the converter has.
    pub allowance: Amount,
}

/// The escrow's permission to take payments from a wallet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WalletPermission {
    /// The wallet.
    pub address: Address,
    /// Whether the escrow may take payments from it.
    pub authorized: bool,
    /// Whether a permit is being relayed.
    pub pending: bool,
    /// The permit's relay transaction, while one is known.
    #[serde(deserialize_with = "super::required")]
    pub hash: Option<B256>,
    /// The permit to sign; `None` when none is prepared.
    #[serde(deserialize_with = "super::required")]
    pub permit: Option<WalletPermit>,
}

/// A prepared wallet permit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WalletPermit {
    /// The permit's reference; submit the signature with it.
    pub id: Uuid,
    /// What to sign.
    pub to_sign: PermitRequest,
}

/// The EIP-712 permit a wallet signs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PermitRequest {
    /// The request kind (`permit`).
    pub kind: SignRequestKind,
    /// Who signs.
    pub signer: Address,
    /// The typed data, as served.
    pub typed_data: Value,
}
