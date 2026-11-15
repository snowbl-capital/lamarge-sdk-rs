//! Hyperliquid `SendAsset` typed data: an SDK-owned schema, since its type name
//! (`HyperliquidTransaction:SendAsset`) is no Rust identifier for `sol!`.

use std::borrow::Cow;

use alloy_dyn_abi::{
    DynSolValue, TypedData,
    eip712::{PropertyDef, Resolver, TypeDef},
};
use alloy_primitives::Address;
use alloy_sol_types::Eip712Domain;
use serde_json::json;

use super::eip712::{self, Schema};

/// The primary type's name.
const PRIMARY: &str = "HyperliquidTransaction:SendAsset";
/// The primary type's fields, in order.
const FIELDS: [(&str, &str); 8] = [
    ("hyperliquidChain", "string"),
    ("destination", "string"),
    ("sourceDex", "string"),
    ("destinationDex", "string"),
    ("token", "string"),
    ("amount", "string"),
    ("fromSubAccount", "string"),
    ("nonce", "uint64"),
];
/// Hyperliquid accepts any signature chain; LaMarge uses HyperEVM's.
const SIGNATURE_CHAIN_ID: u64 = 999;

/// Hyperliquid's signing domain: `HyperliquidSignTransaction`, version 1, chain 999, the zero
/// address as contract.
pub(crate) fn domain() -> Eip712Domain {
    eip712::domain("HyperliquidSignTransaction", "1", SIGNATURE_CHAIN_ID, Some(Address::ZERO))
}

/// A `HyperCore` `sendAsset`: to the sender's own HyperEVM address through USDC's system address
/// (a withdrawal), or to another account (a send).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SendAsset {
    pub(crate) hyperliquid_chain: String,
    /// USDC's system address for a withdrawal, else the recipient, lowercase.
    pub(crate) destination: String,
    pub(crate) source_dex: String,
    pub(crate) destination_dex: String,
    pub(crate) token: String,
    /// Decimal USDC, as Hyperliquid reads it: `"5"`, `"5.25"`.
    pub(crate) amount: String,
    pub(crate) from_sub_account: String,
    /// Milliseconds.
    pub(crate) nonce: u64,
}

impl SendAsset {
    /// The payload a signer is given: these values under Hyperliquid's pinned domain.
    pub(crate) fn typed_data(&self) -> TypedData {
        let domain = domain();
        let mut resolver = Resolver::default();
        let props = |fields: &[(&str, &str)]| {
            fields.iter().map(|(name, kind)| PropertyDef::new_unchecked(*kind, *name)).collect()
        };
        let domain_fields = [
            ("name", "string"),
            ("version", "string"),
            ("chainId", "uint256"),
            ("verifyingContract", "address"),
        ];
        resolver.ingest(TypeDef::new_unchecked("EIP712Domain".to_owned(), props(&domain_fields)));
        resolver.ingest(TypeDef::new_unchecked(PRIMARY.to_owned(), props(&FIELDS)));
        TypedData {
            domain,
            resolver,
            primary_type: PRIMARY.to_owned(),
            message: json!({
                "hyperliquidChain": self.hyperliquid_chain,
                "destination": self.destination,
                "sourceDex": self.source_dex,
                "destinationDex": self.destination_dex,
                "token": self.token,
                "amount": self.amount,
                "fromSubAccount": self.from_sub_account,
                "nonce": self.nonce,
            }),
        }
    }
}

impl Schema for SendAsset {
    const PRIMARY: &'static str = PRIMARY;

    fn encode_type() -> Cow<'static, str> {
        let fields: Vec<String> =
            FIELDS.iter().map(|(name, kind)| format!("{kind} {name}")).collect();
        Cow::Owned(format!("{PRIMARY}({})", fields.join(",")))
    }

    fn decode(value: &DynSolValue) -> Option<Self> {
        let (_, _, values) = value.as_custom_struct()?;
        let [chain, destination, source_dex, destination_dex, token, amount, sub_account, nonce] =
            values
        else {
            return None;
        };
        let text = |value: &DynSolValue| value.as_str().map(str::to_owned);
        let (nonce, _) = nonce.as_uint()?;
        Some(Self {
            hyperliquid_chain: text(chain)?,
            destination: text(destination)?,
            source_dex: text(source_dex)?,
            destination_dex: text(destination_dex)?,
            token: text(token)?,
            amount: text(amount)?,
            from_sub_account: text(sub_account)?,
            nonce: u64::try_from(nonce).ok()?,
        })
    }
}

/// A `HyperCore` system address: `0x20`, then zeros up to a token index in its last five hex
/// digits, or HYPE's `0x2222…`. Funds sent there are bridged, never credited to an account.
pub(crate) fn is_system_address(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.starts_with(concat!("0x2", "0000000000", "0000000000", "0000000000", "0000"))
        || lower == format!("0x{}", "22".repeat(20))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deployments;

    fn withdrawal() -> SendAsset {
        SendAsset {
            hyperliquid_chain: "Mainnet".to_owned(),
            destination: format!("{:#x}", deployments::HYPERLIQUID_BRIDGE),
            source_dex: String::new(),
            destination_dex: "spot".to_owned(),
            token: deployments::HYPERLIQUID_USDC.to_owned(),
            amount: "12.5".to_owned(),
            from_sub_account: String::new(),
            nonce: 1_790_000_000_000,
        }
    }

    #[test]
    fn the_schema_is_hyperliquids() {
        assert_eq!(
            SendAsset::encode_type(),
            "HyperliquidTransaction:SendAsset(string hyperliquidChain,string destination,string sourceDex,string destinationDex,string token,string amount,string fromSubAccount,uint64 nonce)"
        );
        let typed = withdrawal().typed_data();
        assert_eq!(typed.encode_type().unwrap(), SendAsset::encode_type());
        assert_eq!(typed.resolver.encode_type("EIP712Domain").unwrap(), typed.domain.encode_type());
    }

    #[test]
    fn the_built_payload_reads_back_and_hashes_by_its_values() {
        let send = withdrawal();
        let typed = send.typed_data();
        let raw = serde_json::to_value(&typed).unwrap();
        let parsed = eip712::parse_in::<SendAsset>(&raw, &domain()).unwrap();
        assert_eq!(parsed.message, send);
        assert_eq!(parsed.digest, typed.eip712_signing_hash().unwrap());
        let more = SendAsset { amount: "12.6".to_owned(), ..send };
        assert_ne!(more.typed_data().eip712_signing_hash().unwrap(), parsed.digest);
    }

    #[test]
    fn system_addresses_are_recognised() {
        assert!(is_system_address("0x2000000000000000000000000000000000000000"));
        assert!(is_system_address("0x20000000000000000000000000000000000000c8"));
        assert!(is_system_address("0x2222222222222222222222222222222222222222"));
        assert!(!is_system_address("0x2000000000000000000000000000000000100000"));
        assert!(!is_system_address("0x84026d3beff123aab7dff957574d1362d6f880a5"));
        assert!(!is_system_address("0x2222222222222222222222222222222222222223"));
    }
}
