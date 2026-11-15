//! Test helpers shared across modules: the test clock, the server's typed-data vectors, JSON
//! edits and the parties and ids the fixtures use.

use alloy_primitives::{Address, address};
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::Value;

pub(crate) mod fixtures;
pub(crate) mod harness;
pub(crate) mod http;
pub(crate) mod json;

/// The test clock's start in Unix seconds, the server vectors' clock too.
pub(crate) const NOW: i64 = 1_790_000_000;

/// The taker, maker owner and master: linked everywhere, the session's signer.
pub(crate) const OWNER: Address = address!("84026d3beff123aab7dff957574d1362d6f880a5");
/// The maker's payment recipient and quote signer.
pub(crate) const MAKER: Address = address!("669ed91296e0098eb827d85eae6f2a60c5f14f8d");
/// The deployment's operator.
pub(crate) const OPERATOR: Address = address!("9965507d1a55bcc2695c58ba16fb37d819b0a4dc");
/// Anyone else.
pub(crate) const OTHER: Address = address!("1111111111111111111111111111111111111111");
/// The delegated quote identity.
pub(crate) const IDENTITY: &str = "0192f1c2-3d4e-7f00-8a1b-2c3d4e5f6a30";
/// A fill and its take.
pub(crate) const FILL: &str = "0192f0a1-b2c3-7d4e-8f90-a1b2c3d4e5f6";
/// The maker pool's order.
pub(crate) const ORDER: &str = "0x2222222222222222222222222222222222222222222222222222222222222222";
/// A swap id without fee tag.
pub(crate) const SWAP: &str = "0xabababababababababababababababababababababababababababababababab";
/// A fill's hashlock.
pub(crate) const HASHLOCK: &str =
    "0x1111111111111111111111111111111111111111111111111111111111111111";
/// A maker's pool.
pub(crate) const POOL: &str = "0x4444444444444444444444444444444444444444444444444444444444444444";

const SERVER: &str =
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/vectors/server-typed-data.json"));

/// `NOW + offset` seconds.
pub(crate) fn at(offset: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(NOW + offset, 0).unwrap()
}

/// `time` as the server writes timestamps.
pub(crate) fn stamp(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// `NOW + offset` seconds, as the server writes timestamps.
pub(crate) fn time(offset: i64) -> String {
    stamp(at(offset))
}

/// A server builder's typed data (`tests/vectors/server-typed-data.json`).
pub(crate) fn sample(name: &str) -> Value {
    let server: Value = serde_json::from_str(SERVER).unwrap();
    server[name].clone()
}

/// `value` after `edit`.
pub(crate) fn edited(mut value: Value, edit: impl FnOnce(&mut Value)) -> Value {
    edit(&mut value);
    value
}

/// `fields` (name, value) with `name`'s value replaced by `value`.
pub(crate) fn replaced(
    fields: Vec<(&'static str, String)>,
    name: &str,
    value: &str,
) -> Vec<(&'static str, String)> {
    fields
        .into_iter()
        .map(|(field, old)| (field, if field == name { value.to_owned() } else { old }))
        .collect()
}
