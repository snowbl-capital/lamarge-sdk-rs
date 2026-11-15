//! Aster requests: query-string fields, transfer amounts and client ids, nonce windows, and
//! Aster's signing domain.
//!
//! Aster signs `Message{msg}` where `msg` is a query string; a transfer between accounts signs
//! it at chain 1666 (no such chain exists), approvals and withdrawals at BSC's 56.

use std::ops::RangeInclusive;

use alloy_primitives::{Address, U256};
use alloy_sol_types::Eip712Domain;

use super::eip712;
use crate::{deployments, types::FillId};

/// BSC: the chain of withdrawals and agent approvals.
pub(crate) const CHAIN_ID: u64 = 56;
/// The chain id of transfers between accounts.
pub(crate) const TRANSFER_CHAIN_ID: u64 = 1666;
/// The token Aster moves.
pub(crate) const TOKEN: &str = "USDT";
/// A take's transfer is signed while its nonce is at least this many seconds ahead.
pub(crate) const NONCE_AHEAD_SECONDS: i64 = 20;
/// A transfer's nonce may lead `now` by this much while its `pay_by` is sooner.
const NONCE_LEAD_SECONDS: i64 = 70;

const MICROS: i128 = 1_000_000;

/// `AsterSignTransaction`, version 1, at `chain_id`, the zero address as contract.
pub(crate) fn domain(chain_id: u64) -> Eip712Domain {
    eip712::domain("AsterSignTransaction", "1", chain_id, Some(Address::ZERO))
}

/// The values of `msg`'s `name=value` pairs, which must be exactly `names` in order.
pub(crate) fn fields<'a>(msg: &'a str, names: &[&str]) -> Option<Vec<&'a str>> {
    let pairs: Vec<&str> = msg.split('&').collect();
    if pairs.len() != names.len() {
        return None;
    }
    pairs
        .into_iter()
        .zip(names)
        .map(|(pair, name)| match pair.split_once('=') {
            Some((key, value)) if key == *name && !value.contains('=') => Some(value),
            _ => None,
        })
        .collect()
}

/// A nonce as Aster writes it: microseconds, 16 digits.
pub(crate) fn parse_nonce(text: &str) -> Option<u64> {
    if text.len() == 16 && text.bytes().all(|b| b.is_ascii_digit()) {
        text.parse().ok()
    } else {
        None
    }
}

/// Whether a microsecond `nonce` is within `skew` seconds of `now` (Unix seconds), exactly.
pub(crate) fn nonce_near(nonce: u64, now: i64, skew: i64) -> bool {
    (i128::from(nonce) - i128::from(now) * MICROS).abs() <= i128::from(skew) * MICROS
}

/// The microsecond nonces a take's transfer may carry: far enough ahead for LaMarge to relay it,
/// and no later than Aster accepts when relayed by `pay_by` (Unix seconds, as `now`).
pub(crate) fn transfer_nonce_window(now: i64, pay_by: i64) -> RangeInclusive<i128> {
    let earliest = i128::from(now) + i128::from(NONCE_AHEAD_SECONDS);
    let latest = (i128::from(now) + i128::from(NONCE_LEAD_SECONDS))
        .max(i128::from(pay_by) + i128::from(NONCE_AHEAD_SECONDS));
    earliest * MICROS..=latest * MICROS
}

/// The `clientTranId` of a fill's transfer: its id in 32 lowercase hex digits. Aster moves
/// nothing more for a repeated one.
pub(crate) fn client_id(fill: FillId) -> String {
    fill.as_uuid().simple().to_string()
}

/// A transfer between accounts, read from its `msg`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Transfer {
    /// Base units (18 decimals).
    pub(crate) amount: U256,
    pub(crate) to: Address,
    pub(crate) client_id: String,
    /// Microseconds.
    pub(crate) nonce: u64,
    pub(crate) user: Address,
}

/// Reads `asset=USDT&amount=…&toAddress=…&clientTranId=…&nonce=…&user=…`, nothing else.
pub(crate) fn parse_transfer(msg: &str) -> Option<Transfer> {
    let names = ["asset", "amount", "toAddress", "clientTranId", "nonce", "user"];
    let values = fields(msg, &names)?;
    let [asset, amount, to, client_id, nonce, user] = values[..] else { return None };
    if asset != TOKEN {
        return None;
    }
    Some(Transfer {
        amount: deployments::decimal_units(amount, deployments::ASTER.decimals)?,
        to: eip712::address_text(to)?,
        client_id: client_id.to_owned(),
        nonce: parse_nonce(nonce)?,
        user: eip712::address_text(user)?,
    })
}

#[cfg(test)]
mod tests {
    use alloy_primitives::address;

    use super::*;

    const USDT: u128 = 1_000_000_000_000_000_000;

    #[test]
    fn fields_are_exactly_the_named_pairs_in_order() {
        assert_eq!(fields("a=1&b=2", &["a", "b"]), Some(vec!["1", "2"]));
        assert_eq!(fields("a=&b=2", &["a", "b"]), Some(vec!["", "2"]));
        assert_eq!(fields("b=2&a=1", &["a", "b"]), None);
        assert_eq!(fields("a=1&b=2&c=3", &["a", "b"]), None);
        assert_eq!(fields("a=1", &["a", "b"]), None);
        assert_eq!(fields("a=1&b", &["a", "b"]), None);
        assert_eq!(fields("a=1&b=2=3", &["a", "b"]), None);
    }

    #[test]
    fn client_ids_are_bare_lowercase_uuids() {
        let fill: FillId = "0198F0A2-1C3D-7E4F-8A9B-0C1D2E3F4A5B".parse().unwrap();
        assert_eq!(client_id(fill), "0198f0a21c3d7e4f8a9b0c1d2e3f4a5b");
    }

    #[test]
    fn nonces_are_sixteen_digit_microseconds() {
        assert_eq!(parse_nonce("1791220981000000"), Some(1_791_220_981_000_000));
        for text in ["179122098100000", "17912209810000000", "179122098100000a", "+791220981000000"]
        {
            assert_eq!(parse_nonce(text), None, "{text}");
        }
        let now = 1_791_220_981;
        let skew = 60;
        assert!(nonce_near(1_791_221_041_000_000, now, skew));
        assert!(nonce_near(1_791_220_921_000_000, now, skew));
        assert!(!nonce_near(1_791_221_041_000_001, now, skew));
        assert!(!nonce_near(1_791_220_920_999_999, now, skew));
    }

    #[test]
    fn a_takes_nonce_window_follows_its_pay_by() {
        let now = 1_000;
        assert_eq!(transfer_nonce_window(now, now + 45), 1_020_000_000..=1_070_000_000);
        // A grouped preparation extends `pay_by` and its nonce together.
        assert_eq!(transfer_nonce_window(now, now + 200), 1_020_000_000..=1_220_000_000);
    }

    #[test]
    fn a_transfer_reads_back_from_its_message() {
        let msg = "asset=USDT&amount=1.000001&toAddress=0x2222222222222222222222222222222222222222&clientTranId=0198f0a21c3d7e4f8a9b0c1d2e3f4a5b&nonce=1791221036000000&user=0x1111111111111111111111111111111111111111";
        assert_eq!(
            parse_transfer(msg),
            Some(Transfer {
                amount: U256::from(USDT + 1_000_000_000_000),
                to: address!("2222222222222222222222222222222222222222"),
                client_id: "0198f0a21c3d7e4f8a9b0c1d2e3f4a5b".to_owned(),
                nonce: 1_791_221_036_000_000,
                user: address!("1111111111111111111111111111111111111111"),
            })
        );
        for (from, to) in [
            ("asset=USDT", "asset=USDC"),
            ("amount=1.000001", "amount=1.0000001"),
            ("toAddress=0x2222222222222222222222222222222222222222", "toAddress=0x2222"),
            ("nonce=1791221036000000", "nonce=1791221036"),
            ("&user=", "&owner="),
        ] {
            assert_eq!(parse_transfer(&msg.replace(from, to)), None, "{to}");
        }
        assert_eq!(parse_transfer(&format!("{msg}&extra=1")), None);
    }
}
