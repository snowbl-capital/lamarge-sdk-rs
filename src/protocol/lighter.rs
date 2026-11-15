//! Lighter L1 messages an owner signs with `personal_sign`, fill memos, and account owner
//! lookups on Lighter's public API.
//!
//! The texts are Lighter's (`server/src/lighter_sig`, checked against Lighter's Go vectors).

use alloy_primitives::{Address, hex};
use serde_json::Value;

use super::eip712;
use crate::types::FillId;

/// A taker's transfer moves USDC.
pub(crate) const USDC_ASSET: u8 = 3;
/// Between collateral routes.
pub(crate) const COLLATERAL_ROUTE: u8 = 0;

/// An internal transfer between Lighter accounts, as its owner message names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Transfer {
    pub(crate) nonce: u64,
    pub(crate) from: u64,
    pub(crate) from_route: u8,
    pub(crate) api_key: u8,
    pub(crate) to: u64,
    pub(crate) to_route: u8,
    pub(crate) asset: u8,
    /// USDC base units.
    pub(crate) amount: u64,
    pub(crate) fee: u64,
    pub(crate) memo: [u8; 32],
}

/// `0x` and sixteen hex digits, as Lighter writes integers in its messages.
fn word(value: impl Into<u64>) -> String {
    format!("{:#018x}", value.into())
}

/// The text an owner signs to authorize `transfer` on Lighter's chain `chain_id`.
pub(crate) fn transfer_message(transfer: &Transfer, chain_id: u64) -> String {
    format!(
        "Transfer\n\nnonce: {}\nfrom: {} (route {})\napi key: {}\nto: {} (route {})\nasset: {}\namount: {}\n\
         fee: {}\nchainId: {}\nmemo: {}\nOnly sign this message for a trusted client!",
        word(transfer.nonce),
        word(transfer.from),
        word(transfer.from_route),
        word(transfer.api_key),
        word(transfer.to),
        word(transfer.to_route),
        word(transfer.asset),
        word(transfer.amount),
        word(transfer.fee),
        word(chain_id),
        hex::encode(transfer.memo),
    )
}

/// The text an owner signs to register API key `api_key` with `public_key` on `account`.
pub(crate) fn registration_message(
    account: u64,
    api_key: u8,
    public_key: &[u8; 40],
    nonce: u64,
) -> String {
    format!(
        "Register Lighter Account\n\npubkey: 0x{}\nnonce: {}\naccount index: {}\napi key index: {}\n\
         Only sign this message for a trusted client!",
        hex::encode(public_key),
        word(nonce),
        word(account),
        word(api_key),
    )
}

/// The memo naming a fill: its id's 16 bytes, then 16 zero bytes.
pub(crate) fn fill_memo(fill: FillId) -> [u8; 32] {
    let mut memo = [0; 32];
    memo[..16].copy_from_slice(fill.as_uuid().as_bytes());
    memo
}

/// The path and query reading account `index` on Lighter's public API.
pub(crate) fn account_path(index: u64) -> String {
    format!("/api/v1/account?by=index&value={index}")
}

/// The owner of account `index` in Lighter's answer: exactly one account, with that index.
pub(crate) fn account_owner(answer: &Value, index: u64) -> Option<Address> {
    let [account] = answer.get("accounts")?.as_array()?.as_slice() else { return None };
    if account.get("index")?.as_u64()? != index {
        return None;
    }
    eip712::address_text(account.get("l1_address")?.as_str()?)
}

#[cfg(test)]
mod tests {
    use alloy_primitives::address;
    use serde_json::json;

    use super::*;

    /// Lighter's Go libraries generate these (`scripts/lighter-vectors`); the server's port and
    /// the TypeScript SDK check them too.
    const VECTORS: &str =
        include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/vectors/lighter.json"));

    fn vectors() -> Value {
        serde_json::from_str(VECTORS).unwrap()
    }

    fn number(case: &Value, name: &str) -> u64 {
        case[name].as_u64().unwrap()
    }

    fn small(case: &Value, name: &str) -> u8 {
        u8::try_from(number(case, name)).unwrap()
    }

    #[test]
    fn transfer_messages_are_lighters() {
        let cases = vectors()["transfers"].as_array().unwrap().clone();
        assert!(!cases.is_empty());
        for case in cases {
            let transfer = Transfer {
                nonce: number(&case, "nonce"),
                from: number(&case, "from"),
                from_route: small(&case, "from_route"),
                api_key: small(&case, "api_key"),
                to: number(&case, "to"),
                to_route: small(&case, "to_route"),
                asset: small(&case, "asset"),
                amount: number(&case, "amount"),
                fee: number(&case, "fee"),
                memo: hex::decode_to_array(case["memo"].as_str().unwrap()).unwrap(),
            };
            let chain_id = number(&case, "chain_id");
            assert_eq!(transfer_message(&transfer, chain_id), case["l1_body"].as_str().unwrap());
            let more = Transfer { amount: transfer.amount + 1, ..transfer };
            assert_ne!(transfer_message(&more, chain_id), case["l1_body"].as_str().unwrap());
        }
    }

    #[test]
    fn registration_messages_are_lighters() {
        let cases = vectors()["change_pub_keys"].as_array().unwrap().clone();
        assert!(!cases.is_empty());
        for case in cases {
            let public_key: [u8; 40] =
                hex::decode_to_array(case["pub_key"].as_str().unwrap()).unwrap();
            assert_eq!(
                registration_message(
                    number(&case, "account"),
                    small(&case, "api_key"),
                    &public_key,
                    number(&case, "nonce"),
                ),
                case["l1_body"].as_str().unwrap()
            );
        }
    }

    #[test]
    fn a_fill_memo_is_its_id_then_zeros() {
        let fill: FillId = "00010203-0405-0607-0809-0a0b0c0d0e0f".parse().unwrap();
        assert_eq!(
            hex::encode(fill_memo(fill)),
            format!("000102030405060708090a0b0c0d0e0f{}", "00".repeat(16))
        );
    }

    #[test]
    fn an_owner_is_read_for_that_index_only() {
        let owner = address!("00000000000000000000000000000000000000aa");
        let answer =
            |index: u64| json!({"code": 200, "accounts": [{"index": index, "l1_address": owner}]});
        assert_eq!(account_owner(&answer(7), 7), Some(owner));
        assert_eq!(account_owner(&answer(8), 7), None);
        assert_eq!(account_owner(&json!({"code": 21100}), 7), None);
        let two = json!({"accounts": [{"index": 7, "l1_address": owner}, {"index": 7, "l1_address": owner}]});
        assert_eq!(account_owner(&two, 7), None);
        let unreadable = json!({"accounts": [{"index": 7, "l1_address": "0x00aa"}]});
        assert_eq!(account_owner(&unreadable, 7), None);
        assert_eq!(account_path(7), "/api/v1/account?by=index&value=7");
    }
}
