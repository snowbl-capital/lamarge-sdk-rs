//! Take models: preparations, signing instructions and results.

use alloy_primitives::{Address, B256};
use serde::{Deserialize, Deserializer, Serialize, de};
use serde_json::Value;
use uuid::Uuid;

use super::Fill;
use crate::types::{Amount, ClientId, SignatureBytes, TakeId, VenueId, open_enum};

open_enum! {
    /// Where a take delivers on its receiving venue.
    pub enum Destination {
        /// Into the taker's account linked there.
        Venue = "venue",
        /// By plain transfer to an address on the venue's escrow chain, in its wallet token.
        Wallet = "wallet",
    }
}

open_enum! {
    /// Where a payment's money comes from.
    pub enum Funding {
        /// The linked account on the paying venue, withdrawing a shortfall from its margin.
        Venue = "venue",
        /// Only the payer's wallet and available balance, in the escrow token.
        Wallet = "wallet",
    }
}

open_enum! {
    /// What a signing request authorizes; it names the expected signer.
    pub enum ToSignKind {
        /// A withdrawal out of venue margin (the linked owner).
        MarginMove = "margin_move",
        /// An order's funding (the linked owner).
        Fund = "fund",
        /// A maker's quote set (the quote signer).
        QuoteSet = "quote_set",
        /// Closing orders (the master).
        CloseOrders = "close_orders",
        /// A take's escrow payment (the linked owner).
        PayTake = "pay_take",
        /// An escrow available-balance withdrawal.
        EscrowWithdrawal = "escrow_withdrawal",
        /// A payment refund.
        RefundPayment = "refund_payment",
        /// A take paid from Lighter (owner message, `personal_sign`).
        LighterTransfer = "lighter_transfer",
        /// A take paid from Aster (the transfer request, EIP-712 at chain 1666).
        AsterTransfer = "aster_transfer",
    }
}

/// A token and an amount of it in base units.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TokenAmount {
    /// The token.
    pub token: Address,
    /// In `token` base units.
    pub amount: Amount,
}

/// One EIP-712 payload to sign (`eth_signTypedData_v4`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ToSign {
    /// What it authorizes.
    pub kind: ToSignKind,
    /// The address that must sign.
    pub signer: Address,
    /// The take id for `pay_take`; the fill id for `refund_payment`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fill_id: Option<Uuid>,
    /// The typed data, as the server prepared it; the SDK checks it before signing.
    pub typed_data: Value,
    /// LaMarge's attestation of a take's payment, when it attests it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attestation: Option<TakeAttestation>,
}

/// A take paid from Lighter: the owner message for this internal transfer, signed with
/// `personal_sign`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct LighterTransferItem {
    /// [`ToSignKind::LighterTransfer`].
    pub kind: ToSignKind,
    /// The taker's linked Lighter address.
    pub signer: Address,
    /// The take.
    pub fill_id: TakeId,
    /// The transfer the owner message describes.
    pub typed_data: LighterTransferTerms,
    /// LaMarge's attestation, when it attests the request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attestation: Option<TakeAttestation>,
}

/// The Lighter internal transfer LaMarge sends from the taker's account once every maker
/// accepted, signed by LaMarge's key at `api_key_index`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct LighterTransferTerms {
    /// LaMarge's key index on the account: 254 in production, 253 in development.
    pub api_key_index: u8,
    /// The taker's linked Lighter account.
    pub from_account_index: u64,
    /// The maker's linked Lighter account.
    pub to_account_index: u64,
    /// In USDC base units.
    pub amount: Amount,
    /// What Lighter charges the taker's account on top.
    pub usdc_fee: Amount,
    /// The fill id's 16 bytes then 16 zero bytes, as 64 lowercase hex digits.
    pub memo: String,
    /// The nonce of LaMarge's key the transfer uses.
    pub nonce: u64,
}

/// A take paid from Aster: the transfer request itself, EIP-712 at chain 1666.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AsterTransferItem {
    /// [`ToSignKind::AsterTransfer`].
    pub kind: ToSignKind,
    /// The taker's linked Aster address.
    pub signer: Address,
    /// The take.
    pub fill_id: TakeId,
    /// `Message{msg}` in the `AsterSignTransaction` domain, as the server prepared it.
    pub typed_data: Value,
    /// LaMarge's attestation, when it attests the request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attestation: Option<TakeAttestation>,
}

/// One request a take asks to sign, by how its payment is made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
#[non_exhaustive]
pub enum TakeSignRequest {
    /// EIP-712 typed data (an escrow payment).
    Typed(ToSign),
    /// A Lighter owner message.
    Lighter(LighterTransferItem),
    /// An Aster transfer request.
    Aster(AsterTransferItem),
}

impl TakeSignRequest {
    /// What the request authorizes.
    pub fn kind(&self) -> &ToSignKind {
        match self {
            Self::Typed(item) => &item.kind,
            Self::Lighter(item) => &item.kind,
            Self::Aster(item) => &item.kind,
        }
    }
}

impl<'de> Deserialize<'de> for TakeSignRequest {
    /// Chooses the shape by `kind`, so a malformed Lighter or Aster item fails instead of being
    /// read as typed data.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        let kind = value
            .get("kind")
            .and_then(Value::as_str)
            .ok_or_else(|| de::Error::missing_field("kind"))?;
        let parsed = match kind.parse::<ToSignKind>() {
            Ok(ToSignKind::LighterTransfer) => serde_json::from_value(value).map(Self::Lighter),
            Ok(ToSignKind::AsterTransfer) => serde_json::from_value(value).map(Self::Aster),
            _ => serde_json::from_value(value).map(Self::Typed),
        };
        parsed.map_err(de::Error::custom)
    }
}

/// LaMarge's attester signature over what a take delivers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TakeAttestation {
    /// The attester's EIP-712 signature.
    pub signature: SignatureBytes,
    /// The attested terms.
    pub terms: AttestedTerms,
}

/// The terms of a [`TakeAttestation`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
#[non_exhaustive]
pub enum AttestedTerms {
    /// A wallet delivery's terms (they name a `destination`).
    Wallet(WalletTakeTerms),
    /// A venue delivery's terms.
    Venue(TakeTerms),
}

/// `TakeTerms`: what a venue delivery delivers, as LaMarge committed it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TakeTerms {
    /// The take id's 16 bytes then 16 zero bytes.
    pub take: B256,
    /// The paying venue.
    pub from: VenueId,
    /// The receiving venue.
    pub to: VenueId,
    /// The account linked on `to` the take delivers to.
    pub recipient: Address,
    /// The token `to` delivers.
    pub token: Address,
    /// What the take delivers in all, in `token` base units.
    #[serde(rename = "amountOut")]
    pub amount_out: Amount,
    /// Unix seconds: the payment is signed before then, or not at all.
    #[serde(rename = "payBy")]
    pub pay_by: u64,
}

/// `WalletTakeTerms`: what a wallet delivery delivers, as LaMarge committed it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WalletTakeTerms {
    /// The take id's 16 bytes then 16 zero bytes.
    pub take: B256,
    /// The paying venue.
    pub from: VenueId,
    /// The receiving venue.
    pub to: VenueId,
    /// The address the take transfers to.
    pub recipient: Address,
    /// The wallet token of `to`'s chain.
    pub token: Address,
    /// The least the recipient receives in all, in `token` base units.
    #[serde(rename = "amountOut")]
    pub amount_out: Amount,
    /// Unix seconds: the payment is signed before then, or not at all.
    #[serde(rename = "payBy")]
    pub pay_by: u64,
    /// [`Destination::Wallet`].
    pub destination: Destination,
}

/// A take: one reservation across up to eight maker fills.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Take {
    /// Where the payment comes from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub funding: Option<Funding>,
    /// For a wallet delivery, the least its recipient receives in all, in the wallet token.
    #[serde(deserialize_with = "super::required")]
    pub minimum_received: Option<TokenAmount>,
    /// The take (its first fill's id).
    pub id: TakeId,
    /// The caller's idempotency key.
    pub client_take_id: ClientId,
    /// What the taker pays, in the paying venue's base units.
    pub amount_in: Amount,
    /// What the taker receives, in the receiving venue's base units.
    pub amount_out: Amount,
    /// Its fills' taker fees.
    pub taker_fee: Amount,
    /// The fills.
    pub fills: Vec<Fill>,
}

/// A take and what it still needs signed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TakePreparation {
    /// The take.
    pub take: Take,
    /// Whether an Aster transfer request awaits signing and must be issued again right before
    /// the signature.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub aster_reissue: bool,
    /// Unsigned requests; empty once the signatures are accepted.
    pub to_sign: Vec<TakeSignRequest>,
}

/// A take as take history lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TakeHistoryRow {
    /// The take.
    pub id: TakeId,
    /// Its fills.
    pub fills: Vec<Fill>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn typed() -> Value {
        json!({"types": {}, "primaryType": "PayTake", "domain": {}, "message": {}})
    }

    #[test]
    fn sign_requests_are_chosen_by_kind() {
        let signer = format!("0x{}", "11".repeat(20));
        let take = "0192f1c2-3d4e-7f00-8a1b-000000000001";
        let escrow =
            json!({"kind": "pay_take", "signer": signer, "fill_id": take, "typed_data": typed()});
        assert!(matches!(serde_json::from_value(escrow).unwrap(), TakeSignRequest::Typed(_)));
        let aster = json!({"kind": "aster_transfer", "signer": signer, "fill_id": take, "typed_data": typed()});
        assert!(matches!(serde_json::from_value(aster).unwrap(), TakeSignRequest::Aster(_)));
        let terms = json!({
            "api_key_index": 254, "from_account_index": 1, "to_account_index": 2,
            "amount": "1000000", "usdc_fee": "0", "memo": "ab".repeat(32), "nonce": 3
        });
        let lighter = json!({"kind": "lighter_transfer", "signer": signer, "fill_id": take, "typed_data": terms});
        let parsed: TakeSignRequest = serde_json::from_value(lighter.clone()).unwrap();
        assert_eq!(parsed.kind(), &ToSignKind::LighterTransfer);
        assert_eq!(serde_json::to_value(&parsed).unwrap(), lighter);
        let unknown = json!({"kind": "future_kind", "signer": signer, "typed_data": typed()});
        let parsed: TakeSignRequest = serde_json::from_value(unknown).unwrap();
        assert!(parsed.kind().is_unknown());
    }

    #[test]
    fn a_malformed_lighter_item_is_not_read_as_typed_data() {
        let signer = format!("0x{}", "11".repeat(20));
        let item = json!({
            "kind": "lighter_transfer", "signer": signer,
            "fill_id": "0192f1c2-3d4e-7f00-8a1b-000000000001", "typed_data": typed()
        });
        assert!(serde_json::from_value::<TakeSignRequest>(item).is_err());
        assert!(serde_json::from_value::<TakeSignRequest>(json!({"signer": signer})).is_err());
    }

    #[test]
    fn attested_terms_are_told_apart_by_destination() {
        let venue = json!({
            "take": format!("0x{}", "ab".repeat(32)), "from": "hyperliquid", "to": "polymarket",
            "recipient": format!("0x{}", "11".repeat(20)), "token": format!("0x{}", "22".repeat(20)),
            "amountOut": "1000000", "payBy": 1_794_657_660_u64
        });
        assert!(matches!(
            serde_json::from_value(venue.clone()).unwrap(),
            AttestedTerms::Venue(TakeTerms { pay_by: 1_794_657_660, .. })
        ));
        let mut wallet = venue;
        wallet["destination"] = json!("wallet");
        assert!(matches!(serde_json::from_value(wallet).unwrap(), AttestedTerms::Wallet(_)));
    }

    #[test]
    fn aster_reissue_defaults_to_false_and_round_trips() {
        let mut preparation: TakePreparation =
            serde_json::from_value(crate::testing::json::fixture("take_preparation")).unwrap();
        assert!(preparation.aster_reissue);
        preparation.aster_reissue = false;
        let value = serde_json::to_value(&preparation).unwrap();
        assert!(value.get("aster_reissue").is_none());
        assert_eq!(serde_json::from_value::<TakePreparation>(value).unwrap(), preparation);
    }
}
