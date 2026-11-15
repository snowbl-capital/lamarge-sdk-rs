//! The check catalogue: one module per area, each a set of pure functions from a server answer,
//! the request, the deployment and the time to the payloads that may be signed.
//!
//! Nothing here performs I/O: what a flow reads first (Lighter account owners, wallet funds) is a
//! checker input, and a check refuses when an input it needs is missing.

use std::collections::BTreeMap;

use alloy_primitives::{Address, U256};

use self::details::{differs, stale};
use crate::error::VerificationError;

pub(crate) mod deployment;
pub(crate) mod details;
pub(crate) mod escrow;
pub(crate) mod funding;
pub(crate) mod lines;
pub(crate) mod maker;
pub(crate) mod setup;
pub(crate) mod take;
pub(crate) mod withdrawal;

/// Owners of Lighter accounts by index, as Lighter's public API answered (never LaMarge's record
/// of a link): an account whose owner a check needs and does not find here is refused.
pub(crate) type LighterOwners = BTreeMap<u64, Address>;

/// The most a signing time may differ from the clock, in seconds.
const SKEW_SECONDS: i64 = 60;

/// `u64` seconds as `i64`, saturating: such a time is beyond any window a check accepts.
fn signed(seconds: u64) -> i64 {
    i64::try_from(seconds).unwrap_or(i64::MAX)
}

/// The first rule that does not hold, as a [`differs`] failure of its detail.
fn differs_unless(
    rules: impl IntoIterator<Item = (bool, &'static str)>,
) -> Result<(), VerificationError> {
    rules.into_iter().find(|(ok, _)| !ok).map_or(Ok(()), |(_, detail)| Err(differs(detail)))
}

/// The checked sum of `values`, or `error` above `2^256 - 1`.
fn sum(
    values: impl IntoIterator<Item = U256>,
    error: VerificationError,
) -> Result<U256, VerificationError> {
    values.into_iter().try_fold(U256::ZERO, |total, value| total.checked_add(value).ok_or(error))
}

/// Whether `deadline` (Unix seconds) is after `now` and at most `seconds` (plus a 30 s clock
/// margin) ahead of it.
fn deadline_within(deadline: U256, now: i64, seconds: i64) -> bool {
    u64::try_from(deadline).is_ok_and(|deadline| {
        let deadline = signed(deadline);
        deadline > now && deadline <= now.saturating_add(seconds + 30)
    })
}

/// The signing bound of a deadline: 30 seconds before it.
fn deadline_bound(deadline: U256) -> i64 {
    u64::try_from(deadline).map_or(i64::MAX, signed).saturating_sub(30)
}

/// The signing bound of a payload signed at `signed` (Unix seconds): a stale `detail` unless
/// within [`SKEW_SECONDS`] of `now`, signable until that long after it.
fn near_clock(signed: i64, now: i64, detail: &'static str) -> Result<i64, VerificationError> {
    if signed.abs_diff(now) > SKEW_SECONDS.unsigned_abs() {
        return Err(stale(detail));
    }
    Ok(signed.saturating_add(SKEW_SECONDS))
}
