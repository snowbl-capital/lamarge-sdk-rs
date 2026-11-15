//! Strict EIP-712 typed-data parsing and the signed `sol!` schemas.
//!
//! Every `typed_data` the server prepares is parsed against the schema the check expects: exact
//! top-level, domain, type and message keys, exact field types and order, and integer, address
//! and byte values of their declared width. What is signed is never the server's JSON but a
//! payload rebuilt from the checked struct and a pinned domain ([`typed`]), whose digest must
//! equal the server's.

use std::{borrow::Cow, collections::BTreeMap};

use alloy_dyn_abi::{DynSolValue, TypedData};
use alloy_primitives::{Address, B256, U256};
use alloy_sol_types::{Eip712Domain, SolStruct, SolValue};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::checks::details::{self, differs};
use crate::{deployments, error::VerificationError};

/// The structs the SDK signs, as the escrow and the venues hash them.
mod schemas {
    alloy_sol_types::sol! {
        /// One quote of a quote set; `receive` is the EIP-712 field name (`receiveAmount` in
        /// Solidity).
        #[derive(Debug, PartialEq, Eq, serde::Serialize)]
        struct Quote {
            string venue;
            uint256 chainId;
            address token;
            address recipient;
            uint256 supply;
            uint256 receive;
            uint256 minFill;
        }

        /// A maker's signed quotes for one order.
        #[derive(Debug, PartialEq, Eq, serde::Serialize)]
        struct QuoteSet {
            address owner;
            bytes32 offerId;
            uint64 sequence;
            uint64 expiresAt;
            Quote[] quotes;
        }

        /// A maker's funding of an order.
        #[derive(Debug, PartialEq, Eq, serde::Serialize)]
        struct FundOffer {
            address owner;
            bytes32 offerId;
            uint256 amount;
            uint256 deposit;
            address quoteSigner;
            address operator;
            uint64 minQuoteSequence;
            uint256 nonce;
            uint256 deadline;
        }

        /// One payment of a take.
        #[derive(Debug, PartialEq, Eq, serde::Serialize)]
        struct LockPayment {
            address payer;
            bytes32 swapId;
            address recipient;
            uint256 amount;
            bytes32 hashlock;
            uint64 refundAt;
            uint256 deadline;
            bytes32 orderId;
            uint64 sequence;
            uint256 amountOut;
            uint256 receiveChainId;
            address receiveToken;
            address receiveEscrow;
            address receiveRecipient;
            uint32 dex;
        }

        /// A taker's payment of every fill of a take, after an optional deposit.
        #[derive(Debug, PartialEq, Eq, serde::Serialize)]
        struct PayTake {
            address payer;
            uint256 deposit;
            LockPayment[] payments;
        }

        /// An account's authorization to close its orders (checked by LaMarge only).
        #[derive(Debug, PartialEq, Eq, serde::Serialize)]
        struct CloseOrders {
            address account;
            bytes32[] orders;
            uint256 nonce;
            uint256 deadline;
        }

        /// An EIP-2612 permit.
        #[derive(Debug, PartialEq, Eq, serde::Serialize)]
        struct Permit {
            address owner;
            address spender;
            uint256 value;
            uint256 nonce;
            uint256 deadline;
        }

        /// A Polymarket withdrawal.
        #[derive(Debug, PartialEq, Eq, serde::Serialize)]
        struct Withdraw {
            address account;
            address token;
            uint256 amount;
            uint256 fee;
            address to;
            uint64 salt;
            uint64 ts;
        }

        /// A Polymarket operation: `data` hashes the operation the typed data's `op` names.
        #[derive(Debug, PartialEq, Eq, serde::Serialize)]
        struct Op {
            bytes32 data;
            uint64 salt;
            uint64 ts;
        }

        /// An Aster request: a query string.
        #[derive(Debug, PartialEq, Eq, serde::Serialize)]
        struct Message {
            string msg;
        }
    }
}

pub(crate) use schemas::*;

/// A primary type the SDK checks: its EIP-712 shape and how to read it from coerced values.
pub(crate) trait Schema: Sized {
    /// The primary type's name.
    const PRIMARY: &'static str;
    /// Whether the domain names a `verifyingContract` (Polymarket operations do not).
    const CONTRACT: bool = true;

    /// `encodeType` of the primary type: its fields, then the structs it references.
    fn encode_type() -> Cow<'static, str>;

    /// Reads the struct from the typed data's coerced message.
    fn decode(value: &DynSolValue) -> Option<Self>;
}

fn decode_sol<T: SolValue + From<<T::SolType as alloy_sol_types::SolType>::RustType>>(
    value: &DynSolValue,
) -> Option<T> {
    T::abi_decode(&value.abi_encode()).ok()
}

macro_rules! sol_schema {
    ($($schema:ident: $contract:literal),* $(,)?) => {$(
        impl Schema for $schema {
            const PRIMARY: &'static str = <$schema as SolStruct>::NAME;
            const CONTRACT: bool = $contract;

            fn encode_type() -> Cow<'static, str> {
                <$schema as SolStruct>::eip712_encode_type()
            }

            fn decode(value: &DynSolValue) -> Option<Self> {
                decode_sol(value)
            }
        }
    )*};
}

sol_schema!(
    QuoteSet: true,
    FundOffer: true,
    PayTake: true,
    CloseOrders: true,
    Permit: true,
    Withdraw: true,
    Message: true,
    Op: false,
);

/// A server `typed_data` read under its schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Parsed<T> {
    pub(crate) message: T,
    /// The digest of the server's typed data, as the server and the venue compute it.
    pub(crate) digest: B256,
}

/// Reads `raw` as typed data of schema `T`, refusing anything but its exact shape.
pub(crate) fn parse<T: Schema>(raw: &Value) -> Result<Parsed<T>, VerificationError> {
    parse_with(raw, false).map(|(parsed, _, _)| parsed)
}

/// [`parse`], signed in exactly `domain`.
pub(crate) fn parse_in<T: Schema>(
    raw: &Value,
    domain: &Eip712Domain,
) -> Result<Parsed<T>, VerificationError> {
    let (parsed, served, _) = parse_with(raw, false)?;
    if served == *domain { Ok(parsed) } else { Err(differs(details::DOMAIN)) }
}

/// Reads a Polymarket operation signed in `domain`: an [`Op`] whose typed data also carries the
/// unsigned `op` its `data` hashes, returned for its own check.
pub(crate) fn parse_operation<'a>(
    raw: &'a Value,
    domain: &Eip712Domain,
) -> Result<(Parsed<Op>, &'a Value), VerificationError> {
    let (parsed, served, op) = parse_with(raw, true)?;
    if served != *domain {
        return Err(differs(details::DOMAIN));
    }
    Ok((parsed, op.ok_or(differs(details::OP))?))
}

/// The parsed message, the served domain and, for an operation, its `op`.
type Read<'a, T> = (Parsed<T>, Eip712Domain, Option<&'a Value>);

fn parse_with<T: Schema>(raw: &Value, with_op: bool) -> Result<Read<'_, T>, VerificationError> {
    const KEYS: [&str; 4] = ["types", "primaryType", "domain", "message"];
    let object = raw.as_object().ok_or(differs(details::TYPED_DATA))?;
    let op = if with_op { object.get("op") } else { None };
    if object.len() != KEYS.len() + usize::from(op.is_some()) || !has_keys(object, &KEYS) {
        return Err(differs(details::TYPED_DATA));
    }
    let domain_types = domain_types(T::CONTRACT);
    let domain_keys: Vec<&str> = domain_types.iter().map(|(name, _)| *name).collect();
    let raw_domain = object["domain"].as_object().ok_or(differs(details::DOMAIN))?;
    if raw_domain.len() != domain_keys.len() || !has_keys(raw_domain, &domain_keys) {
        return Err(differs(details::DOMAIN));
    }
    if object["primaryType"] != T::PRIMARY {
        return Err(differs(details::PRIMARY_TYPE));
    }

    let encode_type = T::encode_type();
    let declared = raw_types(&object["types"]).ok_or(differs(details::TYPES))?;
    let mut names: Vec<&str> = struct_names(&encode_type).collect();
    names.push("EIP712Domain");
    if declared.len() != names.len()
        || !names.iter().all(|name| declared.contains_key(name))
        || declared["EIP712Domain"] != domain_types
    {
        return Err(differs(details::TYPES));
    }
    let typed = TypedData::deserialize(raw).map_err(|_| differs(details::TYPED_DATA))?;
    if typed.resolver.encode_type(T::PRIMARY).ok().as_deref() != Some(encode_type.as_ref()) {
        return Err(differs(details::TYPES));
    }

    if !fits(&declared, T::PRIMARY, &object["message"]) {
        return Err(differs(details::MESSAGE));
    }
    let value = typed.coerce().map_err(|_| differs(details::MESSAGE))?;
    let message = T::decode(&value).ok_or(differs(details::MESSAGE))?;
    let digest = typed.eip712_signing_hash().map_err(|_| differs(details::MESSAGE))?;
    Ok((Parsed { message, digest }, typed.domain, op))
}

fn has_keys(object: &Map<String, Value>, keys: &[&str]) -> bool {
    keys.iter().all(|key| object.contains_key(*key))
}

fn domain_types(contract: bool) -> Vec<(&'static str, &'static str)> {
    let mut fields = vec![("name", "string"), ("version", "string"), ("chainId", "uint256")];
    if contract {
        fields.push(("verifyingContract", "address"));
    }
    fields
}

/// The struct names an `encodeType` declares: `A(…)B(…)` names `A` and `B`.
fn struct_names(encode_type: &str) -> impl Iterator<Item = &str> {
    encode_type.split_terminator(')').filter_map(|part| part.split_once('(').map(|(name, _)| name))
}

type Types<'a> = BTreeMap<&'a str, Vec<(&'a str, &'a str)>>;

/// `types` as names to `(field, type)` lists; `None` unless each field is exactly
/// `{"name": …, "type": …}`.
fn raw_types(value: &Value) -> Option<Types<'_>> {
    value
        .as_object()?
        .iter()
        .map(|(name, fields)| {
            let fields = fields
                .as_array()?
                .iter()
                .map(|field| {
                    let field = field.as_object()?;
                    (field.len() == 2).then_some(())?;
                    Some((field.get("name")?.as_str()?, field.get("type")?.as_str()?))
                })
                .collect::<Option<Vec<_>>>()?;
            Some((name.as_str(), fields))
        })
        .collect()
}

/// Whether `value` is of EIP-712 type `kind`: structs with exactly their fields, arrays of
/// fitting items, and leaves of the declared width.
fn fits(types: &Types<'_>, kind: &str, value: &Value) -> bool {
    if let Some(item) = kind.strip_suffix("[]") {
        return value.as_array().is_some_and(|items| items.iter().all(|v| fits(types, item, v)));
    }
    if let Some(fields) = types.get(kind) {
        return value.as_object().is_some_and(|object| {
            object.len() == fields.len()
                && fields.iter().all(|(name, field)| {
                    object.get(*name).is_some_and(|value| fits(types, field, value))
                })
        });
    }
    match kind {
        "address" => value.as_str().is_some_and(|text| hex_of_length(text, 20)),
        "string" => value.is_string(),
        "bool" => value.is_boolean(),
        _ => {
            if let Some(width) = kind.strip_prefix("bytes") {
                let width = width.parse::<usize>().ok().filter(|width| (1..=32).contains(width));
                width.is_some_and(|width| {
                    value.as_str().is_some_and(|text| hex_of_length(text, width))
                })
            } else if let Some(bits) = kind.strip_prefix("uint") {
                let bits = if bits.is_empty() { Some(256) } else { bits.parse::<usize>().ok() };
                bits.filter(|bits| bits % 8 == 0 && (8..=256).contains(bits))
                    .is_some_and(|bits| unsigned(value).is_some_and(|n| n.bit_len() <= bits))
            } else {
                false
            }
        }
    }
}

/// `0x` and `2 × bytes` hexadecimal digits.
fn hex_of_length(text: &str, bytes: usize) -> bool {
    text.strip_prefix("0x").is_some_and(|digits| {
        digits.len() == 2 * bytes && digits.bytes().all(|b| b.is_ascii_hexdigit())
    })
}

/// An address written as `0x` and 40 hexadecimal digits, in any case (no checksum required).
pub(crate) fn address_text(text: &str) -> Option<Address> {
    if hex_of_length(text, 20) { text.parse().ok() } else { None }
}

/// A JSON integer or a decimal string of digits, as the server writes integers.
fn unsigned(value: &Value) -> Option<U256> {
    match value {
        Value::Number(number) => number.as_u64().map(U256::from),
        Value::String(text)
            if (1..=78).contains(&text.len()) && text.bytes().all(|b| b.is_ascii_digit()) =>
        {
            U256::from_str_radix(text, 10).ok()
        }
        _ => None,
    }
}

/// An EIP-712 domain built from pinned values.
pub(crate) fn domain(
    name: &'static str,
    version: &'static str,
    chain_id: u64,
    contract: Option<Address>,
) -> Eip712Domain {
    Eip712Domain::new(
        Some(Cow::Borrowed(name)),
        Some(Cow::Borrowed(version)),
        Some(U256::from(chain_id)),
        contract,
        None,
    )
}

/// The escrow's domain (`LaMarge`, version 1) on `chain_id`: a pinned escrow, or a venue's
/// signing domain.
pub(crate) fn lamarge_domain(chain_id: u64, contract: Address) -> Eip712Domain {
    domain("LaMarge", "1", chain_id, Some(contract))
}

/// The permit domain of `chain`'s token, if it has one.
pub(crate) fn permit_domain(chain: &deployments::Chain) -> Option<Eip712Domain> {
    let permit = chain.permit?;
    Some(domain(permit.name, permit.version, chain.id, Some(chain.token)))
}

/// The payload a signer is given: `message` under the pinned `domain`.
pub(crate) fn typed<T: SolStruct + Serialize>(message: &T, domain: Eip712Domain) -> TypedData {
    TypedData::from_struct(message, Some(domain))
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{address, b256};
    use serde_json::json;

    use super::*;
    use crate::{
        error::VerificationKind,
        protocol::{
            aster,
            hyperliquid::{self, SendAsset},
            polymarket,
        },
        testing::{edited, sample},
        types::{EscrowEnvironment, VenueId},
    };

    const SERVER: &str =
        include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/vectors/server-typed-data.json"));
    const DIGESTS: &str =
        include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/vectors/eip712.json"));

    fn vectors() -> (Map<String, Value>, Map<String, Value>) {
        let server: Value = serde_json::from_str(SERVER).unwrap();
        let digests: Value = serde_json::from_str(DIGESTS).unwrap();
        (server.as_object().unwrap().clone(), digests.as_object().unwrap().clone())
    }

    fn escrow(venue: &VenueId) -> Eip712Domain {
        let chain = deployments::chain(venue).unwrap();
        lamarge_domain(
            chain.id,
            deployments::escrow(chain.id, EscrowEnvironment::Production).unwrap(),
        )
    }

    /// Parses a vector, checks the server digest against viem's, and rebuilds it from the pins.
    fn rebuilds<T: Schema + SolStruct + Serialize>(
        raw: &Value,
        viem: B256,
        pinned: &Eip712Domain,
    ) -> Parsed<T> {
        let parsed = parse_in::<T>(raw, pinned).unwrap();
        assert_eq!(parsed.digest, viem);
        let rebuilt = typed(&parsed.message, pinned.clone());
        assert_eq!(rebuilt.eip712_signing_hash().unwrap(), viem);
        parsed
    }

    /// The server's own builders wrote `server-typed-data.json` (`server/src/eip712.rs`
    /// `sdk_typed_data`); viem, an independent implementation, hashed it into `eip712.json`.
    #[test]
    fn server_typed_data_parses_and_rebuilds_from_the_pins() {
        let (server, digests) = vectors();
        assert_eq!(
            server.keys().collect::<Vec<_>>(),
            digests.keys().collect::<Vec<_>>(),
            "refresh tests/vectors from LaMarge (CONTRIBUTING.md)"
        );
        let mut seen = 0;
        for (name, raw) in &server {
            let viem: B256 = digests[name].as_str().unwrap().parse().unwrap();
            match name.as_str() {
                "pay_take" => {
                    let parsed = rebuilds::<PayTake>(raw, viem, &escrow(&VenueId::Hyperliquid));
                    assert_eq!(parsed.message.payments.len(), 2);
                    assert_eq!(parsed.message.payments[1].dex, u32::MAX);
                }
                "fund_offer" => {
                    rebuilds::<FundOffer>(raw, viem, &escrow(&VenueId::Polymarket));
                }
                "close_orders" => {
                    rebuilds::<CloseOrders>(raw, viem, &escrow(&VenueId::Hyperliquid));
                }
                "quote_set" => {
                    rebuilds::<QuoteSet>(raw, viem, &escrow(&VenueId::Hyperliquid));
                }
                "quote_set_lighter" => {
                    rebuilds::<QuoteSet>(
                        raw,
                        viem,
                        &lamarge_domain(
                            deployments::LIGHTER.id,
                            deployments::venue_contract(&VenueId::Lighter).unwrap(),
                        ),
                    );
                }
                "permit" => {
                    let domain = permit_domain(&deployments::HYPERLIQUID).unwrap();
                    let parsed = rebuilds::<Permit>(raw, viem, &domain);
                    assert_eq!(parsed.message.value, deployments::UNLIMITED_ALLOWANCE);
                }
                "polymarket_withdraw" => {
                    rebuilds::<Withdraw>(raw, viem, &polymarket::withdraw_domain());
                }
                "polymarket_internal_transfer" => {
                    let (parsed, op) =
                        parse_operation(raw, &polymarket::operation_domain()).unwrap();
                    assert_eq!(parsed.digest, viem);
                    let op = polymarket::InternalTransfer::read(op).unwrap();
                    assert_eq!(op.hash(), Some(parsed.message.data));
                    let rebuilt = typed(&parsed.message, polymarket::operation_domain());
                    assert_eq!(rebuilt.eip712_signing_hash().unwrap(), viem);
                }
                "aster_transfer" => {
                    rebuilds::<Message>(raw, viem, &aster::domain(aster::TRANSFER_CHAIN_ID));
                }
                "aster_approval" => {
                    rebuilds::<Message>(raw, viem, &aster::domain(aster::CHAIN_ID));
                }
                "send_asset_withdrawal" | "send_asset_send" => {
                    let parsed = parse_in::<SendAsset>(raw, &hyperliquid::domain()).unwrap();
                    assert_eq!(parsed.digest, viem);
                    assert_eq!(parsed.message.typed_data().eip712_signing_hash().unwrap(), viem);
                }
                other => unreachable!("vector {other} has no schema in this test"),
            }
            seen += 1;
        }
        assert_eq!(seen, 12);
    }

    /// `server/src/quote_set.rs`'s vector, also checked by `contracts/test/LaMargeEscrow.t.sol`:
    /// signed by viem, recovered by the escrow.
    #[test]
    fn the_escrows_quote_set_vector_parses_and_recovers() {
        let offer = format!("0x6c0f{}01", "00".repeat(29));
        let raw = json!({
            "types": {
                "EIP712Domain": [
                    {"name": "name", "type": "string"}, {"name": "version", "type": "string"},
                    {"name": "chainId", "type": "uint256"}, {"name": "verifyingContract", "type": "address"}
                ],
                "QuoteSet": [
                    {"name": "owner", "type": "address"}, {"name": "offerId", "type": "bytes32"},
                    {"name": "sequence", "type": "uint64"}, {"name": "expiresAt", "type": "uint64"},
                    {"name": "quotes", "type": "Quote[]"}
                ],
                "Quote": [
                    {"name": "venue", "type": "string"}, {"name": "chainId", "type": "uint256"},
                    {"name": "token", "type": "address"}, {"name": "recipient", "type": "address"},
                    {"name": "supply", "type": "uint256"}, {"name": "receive", "type": "uint256"},
                    {"name": "minFill", "type": "uint256"}
                ]
            },
            "primaryType": "QuoteSet",
            "domain": {"name": "LaMarge", "version": "1", "chainId": 137,
                       "verifyingContract": "0xe5c0000000000000000000000000000000000001"},
            "message": {
                "owner": "0x84026d3beff123aab7dff957574d1362d6f880a5", "offerId": offer,
                "sequence": 1, "expiresAt": 1_790_500_000u64,
                "quotes": [{
                    "venue": "hyperliquid", "chainId": 999,
                    "token": "0xb88339cb7199b77e23db6e890353e22632ba630f",
                    "recipient": "0x84026d3beff123aab7dff957574d1362d6f880a5",
                    "supply": "500000000", "receive": "500250000", "minFill": "10000000"
                }]
            }
        });
        let digest = b256!("eaafbf7e8e5381f876288d5136db26cdb30b19d82ab6ef7d29245f5a972a36ef");
        let domain = lamarge_domain(137, address!("e5c0000000000000000000000000000000000001"));
        let parsed = rebuilds::<QuoteSet>(&raw, digest, &domain);
        assert_eq!(parsed.message.quotes[0].receive, U256::from(500_250_000u64));
        let signature: alloy_primitives::Signature = "0xd81d15f02e5cbfd4695138a5cb1a5ce6b52dce8c7d072a9d8b8536684adb0f51443a771a6b41d88d76e1ec0d28b802663a4ec4f4e84b487f40f93bcbb9d34e831b"
            .parse()
            .unwrap();
        assert_eq!(
            signature.recover_address_from_prehash(&parsed.digest).unwrap(),
            address!("2c7536e3605d9c16a7a3d7b1898e529396a65c23")
        );
        assert_eq!(
            QuoteSet::eip712_encode_type(),
            "QuoteSet(address owner,bytes32 offerId,uint64 sequence,uint64 expiresAt,Quote[] quotes)Quote(string venue,uint256 chainId,address token,address recipient,uint256 supply,uint256 receive,uint256 minFill)"
        );
    }

    fn refused<T: Schema + std::fmt::Debug>(raw: &Value) -> &'static str {
        let error = parse::<T>(raw).unwrap_err();
        assert_eq!(error.kind, VerificationKind::Differs);
        error.detail
    }

    /// A change to a vector's raw JSON.
    type Edit = fn(&mut Value);

    #[test]
    fn extra_or_missing_top_level_keys_are_refused() {
        let extra = edited(sample("pay_take"), |raw| raw["extra"] = json!(1));
        assert_eq!(refused::<PayTake>(&extra), details::TYPED_DATA);
        let missing = edited(sample("pay_take"), |raw| {
            raw.as_object_mut().unwrap().remove("message");
        });
        assert_eq!(refused::<PayTake>(&missing), details::TYPED_DATA);
        assert_eq!(refused::<PayTake>(&json!("typed data")), details::TYPED_DATA);
        // `op` belongs to Polymarket operations only.
        let op = edited(sample("pay_take"), |raw| raw["op"] = json!({}));
        assert_eq!(refused::<PayTake>(&op), details::TYPED_DATA);
    }

    #[test]
    fn a_salt_or_any_other_domain_key_is_refused() {
        let salt = edited(sample("pay_take"), |raw| {
            raw["domain"]["salt"] = json!(format!("0x{}", "00".repeat(32)));
        });
        assert_eq!(refused::<PayTake>(&salt), details::DOMAIN);
        let extra = edited(sample("fund_offer"), |raw| raw["domain"]["chain"] = json!("hyperevm"));
        assert_eq!(refused::<FundOffer>(&extra), details::DOMAIN);
        let missing = edited(sample("fund_offer"), |raw| {
            raw["domain"].as_object_mut().unwrap().remove("verifyingContract");
        });
        assert_eq!(refused::<FundOffer>(&missing), details::DOMAIN);
    }

    #[test]
    fn an_operation_domain_has_no_contract_and_needs_its_op() {
        let raw = sample("polymarket_internal_transfer");
        let domain = polymarket::operation_domain();
        let contract = edited(sample("polymarket_internal_transfer"), |raw| {
            raw["domain"]["verifyingContract"] =
                json!("0xdca4af75705dbb50f62437045aff9921947917d2");
        });
        assert_eq!(parse_operation(&contract, &domain).unwrap_err().detail, details::DOMAIN);
        let no_op = edited(sample("polymarket_internal_transfer"), |raw| {
            raw.as_object_mut().unwrap().remove("op");
        });
        assert_eq!(parse_operation(&no_op, &domain).unwrap_err().detail, details::OP);
        // Read as a plain schema, its `op` is an extra key.
        assert_eq!(refused::<Op>(&raw), details::TYPED_DATA);
        assert!(parse_operation(&raw, &domain).is_ok());
        // Signed in another domain.
        let withdraw = polymarket::withdraw_domain();
        assert_eq!(parse_operation(&raw, &withdraw).unwrap_err().detail, details::DOMAIN);
    }

    #[test]
    fn another_primary_type_is_refused() {
        let raw = sample("fund_offer");
        assert_eq!(refused::<PayTake>(&raw), details::PRIMARY_TYPE);
        let renamed = edited(sample("fund_offer"), |raw| raw["primaryType"] = json!("PayTake"));
        assert_eq!(refused::<FundOffer>(&renamed), details::PRIMARY_TYPE);
    }

    #[test]
    fn types_must_be_exactly_the_schema() {
        let cases: [(&str, Edit); 8] = [
            ("extra type", |raw| raw["types"]["Extra"] = json!([{"name": "a", "type": "uint256"}])),
            ("reordered field", |raw| {
                raw["types"]["LockPayment"].as_array_mut().unwrap().swap(0, 1);
            }),
            ("renamed field", |raw| raw["types"]["PayTake"][1]["name"] = json!("deposits")),
            ("retyped field", |raw| raw["types"]["LockPayment"][14]["type"] = json!("uint64")),
            ("missing field", |raw| {
                raw["types"]["PayTake"].as_array_mut().unwrap().pop();
            }),
            ("extra field key", |raw| raw["types"]["PayTake"][0]["note"] = json!("x")),
            ("domain type", |raw| raw["types"]["EIP712Domain"].as_array_mut().unwrap().swap(0, 1)),
            ("missing struct", |raw| {
                raw["types"].as_object_mut().unwrap().remove("LockPayment");
            }),
        ];
        for (case, edit) in cases {
            assert_eq!(
                refused::<PayTake>(&edited(sample("pay_take"), edit)),
                details::TYPES,
                "{case}"
            );
        }
    }

    #[test]
    fn message_keys_and_values_must_fit_the_schema() {
        let cases: [(&str, Edit); 10] = [
            ("extra key", |raw| raw["message"]["extra"] = json!("1")),
            ("missing key", |raw| {
                raw["message"].as_object_mut().unwrap().remove("deposit");
            }),
            ("extra nested key", |raw| raw["message"]["payments"][0]["extra"] = json!("1")),
            ("missing nested key", |raw| {
                raw["message"]["payments"][1].as_object_mut().unwrap().remove("dex");
            }),
            ("hex integer", |raw| raw["message"]["deposit"] = json!("0x10")),
            ("negative integer", |raw| raw["message"]["payments"][0]["dex"] = json!(-1)),
            ("integer above its width", |raw| {
                raw["message"]["payments"][0]["dex"] = json!(1u64 << 32);
            }),
            ("short bytes32", |raw| raw["message"]["payments"][0]["swapId"] = json!("0x1234")),
            ("bad address", |raw| raw["message"]["payer"] = json!("0x1234")),
            ("not an array", |raw| raw["message"]["payments"] = json!({})),
        ];
        for (case, edit) in cases {
            assert_eq!(
                refused::<PayTake>(&edited(sample("pay_take"), edit)),
                details::MESSAGE,
                "{case}"
            );
        }
        let float = edited(sample("quote_set"), |raw| raw["message"]["sequence"] = json!(1.5));
        assert_eq!(refused::<QuoteSet>(&float), details::MESSAGE);
        let text = edited(sample("aster_transfer"), |raw| raw["message"]["msg"] = json!(7));
        assert_eq!(refused::<Message>(&text), details::MESSAGE);
    }

    #[test]
    fn integers_as_numbers_or_decimal_strings_hash_alike() {
        let raw = sample("fund_offer");
        let as_numbers = edited(sample("fund_offer"), |raw| {
            raw["message"]["nonce"] =
                json!(raw["message"]["nonce"].as_str().unwrap().parse::<u64>().unwrap());
            raw["message"]["minQuoteSequence"] =
                json!(raw["message"]["minQuoteSequence"].to_string());
        });
        assert_eq!(parse::<FundOffer>(&raw).unwrap(), parse::<FundOffer>(&as_numbers).unwrap());
    }
}
