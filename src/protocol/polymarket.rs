//! Polymarket operation encoding and hashing, and Polymarket's signing domains.
//!
//! A Polymarket wallet shows only an operation's hash (`Op.data`); the SDK rebuilds it from the
//! typed data's unsigned `op`, whose terms the checks read.

use alloy_primitives::{B256, keccak256};
use alloy_sol_types::Eip712Domain;
use serde_json::Value;

use super::eip712;
use crate::deployments;

/// Polygon.
const CHAIN_ID: u64 = 137;
/// The only operation a send signs.
pub(crate) const INTERNAL_TRANSFER: &str = "internalTransfer";

/// Polymarket's withdrawal domain.
pub(crate) fn withdraw_domain() -> Eip712Domain {
    eip712::domain("Polymarket", "1", CHAIN_ID, Some(deployments::POLYMARKET_WITHDRAW_CONTRACT))
}

/// Polymarket's operation domain: no `verifyingContract` (absent, not zero).
pub(crate) fn operation_domain() -> Eip712Domain {
    eip712::domain("Polymarket", "1", CHAIN_ID, None)
}

/// `MessagePack` of `[name, [args…]]` for the only shape an operation hashes: strings shorter
/// than 256 bytes, at most 15 arguments. `None` for any other.
pub(crate) fn msgpack(name: &str, args: &[&str]) -> Option<Vec<u8>> {
    fn string(out: &mut Vec<u8>, text: &str) -> Option<()> {
        let length = u8::try_from(text.len()).ok()?;
        if length < 32 {
            out.push(0xa0 | length);
        } else {
            out.extend([0xd9, length]);
        }
        out.extend(text.as_bytes());
        Some(())
    }
    let count = u8::try_from(args.len()).ok().filter(|count| *count < 16)?;
    let mut out = vec![0x92];
    string(&mut out, name)?;
    out.push(0x90 | count);
    for arg in args {
        string(&mut out, arg)?;
    }
    Some(out)
}

/// An operation's `data`: keccak256 of its `MessagePack`, over the exact strings sent.
pub(crate) fn op_hash(name: &str, args: &[&str]) -> Option<B256> {
    msgpack(name, args).map(keccak256)
}

/// The internal transfer an operation's `op` names, its strings exactly as sent (addresses
/// checksummed, the amount in decimal pUSD).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InternalTransfer {
    pub(crate) account: String,
    pub(crate) token: String,
    pub(crate) amount: String,
    pub(crate) to: String,
}

impl InternalTransfer {
    /// Reads `{"type": "internalTransfer", "args": {account, token, amount, to}}`, nothing else.
    pub(crate) fn read(op: &Value) -> Option<Self> {
        let op = op.as_object()?;
        if op.len() != 2 || op.get("type")?.as_str()? != INTERNAL_TRANSFER {
            return None;
        }
        let args = op.get("args")?.as_object()?;
        let text = |name: &str| args.get(name)?.as_str().map(str::to_owned);
        (args.len() == 4).then_some(())?;
        Some(Self {
            account: text("account")?,
            token: text("token")?,
            amount: text("amount")?,
            to: text("to")?,
        })
    }

    /// The `data` this transfer hashes to.
    pub(crate) fn hash(&self) -> Option<B256> {
        op_hash(INTERNAL_TRANSFER, &[&self.account, &self.token, &self.amount, &self.to])
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{b256, hex};
    use serde_json::json;

    use super::*;

    const ARGS: [&str; 4] = [
        "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266",
        "0xC011a7E12a19f7B1f670d46F03B03f3342E82DFB",
        "250.5",
        "0x9965507D1a55bcC2695C58ba16FB37d819B0A4dc",
    ];

    /// `@msgpack/msgpack` 3.1.2's encoding, as Polymarket's docs use it
    /// (`server/src/polymarket.rs`).
    #[test]
    fn msgpack_is_polymarkets_reference_encoding() {
        assert_eq!(
            hex::encode(msgpack(INTERNAL_TRANSFER, &ARGS).unwrap()),
            "92b0696e7465726e616c5472616e7366657294d92a307866333946643665353161616438384636463463653661423838323732373963666646623932323636d92a307843303131613745313261313966374231663637306434364630334230336633333432453832444642a53235302e35d92a307839393635353037443161353562634332363935433538626131364642333764383139423041346463"
        );
    }

    /// `sdk/src/send.test.ts`: Polymarket's encoder and viem give this hash; the server pins it.
    #[test]
    fn the_operation_hash_is_polymarkets() {
        let expected = b256!("116bb5d03ed73e04edd7c0ebfd0f9c37d9cf5dd00de629f8949cbdfc63e11825");
        assert_eq!(op_hash(INTERNAL_TRANSFER, &ARGS), Some(expected));
        // The exact strings are hashed: case and decimal form matter.
        let lower = ARGS[0].to_lowercase();
        assert_ne!(
            op_hash(INTERNAL_TRANSFER, &[&lower, ARGS[1], ARGS[2], ARGS[3]]),
            Some(expected)
        );
        assert_ne!(
            op_hash(INTERNAL_TRANSFER, &[ARGS[0], ARGS[1], "250.50", ARGS[3]]),
            Some(expected)
        );
    }

    #[test]
    fn msgpack_refuses_long_strings_and_many_arguments() {
        let long = "a".repeat(256);
        assert!(msgpack(INTERNAL_TRANSFER, &[long.as_str()]).is_none());
        let longest = "a".repeat(255);
        let encoded = msgpack("x", &[longest.as_str()]).unwrap();
        assert_eq!(&encoded[..5], &[0x92, 0xa1, b'x', 0x91, 0xd9]);
        assert!(msgpack("x", &["a"; 16]).is_none());
        assert_eq!(msgpack("x", &["a"; 15]).unwrap()[3], 0x9f);
    }

    #[test]
    fn an_op_is_read_only_as_an_internal_transfer() {
        let op = json!({"type": "internalTransfer", "args": {
            "account": ARGS[0], "token": ARGS[1], "amount": ARGS[2], "to": ARGS[3]}});
        let transfer = InternalTransfer::read(&op).unwrap();
        assert_eq!(transfer.amount, "250.5");
        assert_eq!(
            transfer.hash(),
            Some(b256!("116bb5d03ed73e04edd7c0ebfd0f9c37d9cf5dd00de629f8949cbdfc63e11825"))
        );
        let mut withdraw = op.clone();
        withdraw["type"] = json!("withdraw");
        let mut extra_arg = op.clone();
        extra_arg["args"]["memo"] = json!("x");
        let mut extra_key = op.clone();
        extra_key["note"] = json!("x");
        let mut number = op.clone();
        number["args"]["amount"] = json!(250.5);
        for refused in
            [withdraw, extra_arg, extra_key, number, json!([]), json!({"type": "internalTransfer"})]
        {
            assert_eq!(InternalTransfer::read(&refused), None, "{refused}");
        }
    }
}
