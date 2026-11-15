//! API information and limits (`GET /v1/info`) and health (`GET /health`).

use alloy_primitives::Address;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::types::{Amount, BlockNumber, VenueId, open_enum};

open_enum! {
    /// Who may make new commitments.
    pub enum AccessMode {
        /// Everyone.
        Open = "open",
        /// Users granted access (see [`AccessState`](super::AccessState)).
        Invite = "invite",
    }
}

open_enum! {
    /// The server's overall health.
    pub enum HealthStatus {
        /// Every chain is ready.
        Ok = "ok",
        /// Something is not ready.
        Degraded = "degraded",
    }
}

/// This deployment's version, operator, limits, timing and chains (`GET /v1/info`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Info {
    /// The API version (`v1`).
    pub api_version: String,
    /// The server's time.
    pub server_time: DateTime<Utc>,
    /// The escrow operator of this deployment.
    pub operator: Address,
    /// Request and trading limits.
    pub limits: ApiLimits,
    /// Settlement timing.
    pub settlement: SettlementTiming,
    /// The escrow deployments.
    pub chains: Vec<DeploymentChain>,
    /// Who may make new commitments.
    pub access: AccessPolicy,
}

/// Who may make new commitments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AccessPolicy {
    /// Open, or by invitation.
    pub mode: AccessMode,
    /// The captcha site key of the access page; `None` without captcha.
    #[serde(deserialize_with = "super::required")]
    pub captcha_site_key: Option<String>,
}

/// Request and trading limits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ApiLimits {
    /// Requests per second per user's sessions or per credential, on authenticated routes.
    pub requests_per_second: u32,
    /// The request burst.
    pub request_burst: u32,
    /// The largest request body, in bytes.
    pub body_bytes: u64,
    /// The shortest quote lease, in seconds.
    pub lease_seconds_min: u64,
    /// The longest quote lease, in seconds.
    pub lease_seconds_max: u64,
    /// The longest quote validity, in seconds.
    pub quote_validity_seconds_max: u64,
    /// The most parts a take splits into.
    pub take_parts_max: u8,
    /// How long a take may wait for its payment, in seconds.
    pub pay_window_seconds: u64,
    /// The payment window of a take signed with its withdrawal, in seconds.
    pub route_pay_window_seconds: u64,
    /// Takes one taker may hold awaiting payment at once.
    pub max_unpaid_takes: u32,
    /// The most one fill takes from one level, in base units.
    pub fill_max: Amount,
}

/// Settlement timing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SettlementTiming {
    /// The maker's claim timeout, in seconds.
    pub maker_timeout_seconds: u64,
    /// The taker's refund timeout, in seconds.
    pub taker_timeout_seconds: u64,
    /// Deliveries on Lighter and Aster; `None` when neither is enabled.
    #[serde(deserialize_with = "super::required")]
    pub delivery: Option<DeliveryTiming>,
}

/// Timing of deliveries on Lighter and Aster.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct DeliveryTiming {
    /// `deliver_by` is `pay_by` plus this many seconds.
    pub window_seconds: u64,
    /// Seconds after `deliver_by` when an unreported paid fill's delivery is abandoned.
    pub abandon_after_seconds: u64,
    /// `report_by` is `t2` less this many seconds.
    pub report_margin_seconds: u64,
    /// The Lighter status a delivery needs; `None` without Lighter.
    #[serde(deserialize_with = "super::required")]
    pub min_status: Option<u8>,
}

/// An escrow deployment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct DeploymentChain {
    /// The venue it serves.
    pub venue: VenueId,
    /// Its chain.
    pub chain_id: u64,
    /// The escrow token.
    pub token: Address,
    /// Its symbol.
    pub symbol: String,
    /// Its decimals.
    pub decimals: u8,
    /// The escrow.
    pub escrow: Address,
    /// The block the escrow was deployed at.
    pub deployment_block: BlockNumber,
}

/// The server's health (`GET /health`); a 503 answer carries it too.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Health {
    /// Overall health.
    pub status: HealthStatus,
    /// Whether every chain is ready.
    pub ready: bool,
    /// The server's time.
    pub server_time: DateTime<Utc>,
    /// Relay transactions pending on all chains.
    pub pending_transactions: u64,
    /// Events committed but not yet given a cursor.
    pub journal_unpublished: u64,
    /// The age of the oldest of those, in seconds.
    #[serde(deserialize_with = "super::required")]
    pub journal_oldest_unpublished_seconds: Option<f64>,
    /// Per chain.
    pub chains: Vec<ChainHealth>,
}

/// One chain's health.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ChainHealth {
    /// The venue on the chain.
    pub venue: VenueId,
    /// The chain.
    pub chain_id: u64,
    /// Observations ready, no integrity mismatch, few and young pending transactions.
    pub ready: bool,
    /// The watcher follows the finalized head with enough operator gas.
    pub observations_ready: bool,
    /// The oldest pending relay transaction's age, in seconds.
    #[serde(deserialize_with = "super::required")]
    pub oldest_pending_seconds: Option<u64>,
    /// Relay transactions pending.
    pub pending_transactions: u64,
    /// When the last integrity check started.
    #[serde(deserialize_with = "super::required")]
    pub integrity_checked_at: Option<DateTime<Utc>>,
    /// When the last integrity check completed.
    #[serde(deserialize_with = "super::required")]
    pub integrity_completed_at: Option<DateTime<Utc>>,
    /// Why the last integrity check failed.
    #[serde(deserialize_with = "super::required")]
    pub integrity_error: Option<String>,
    /// A latched discrepancy; blocks admission until supervised verification.
    pub integrity_mismatch: bool,
    /// The watched block.
    #[serde(deserialize_with = "super::required")]
    pub block: Option<BlockNumber>,
    /// The finalized block.
    #[serde(deserialize_with = "super::required")]
    pub finalized_block: Option<BlockNumber>,
    /// When the watched block was read.
    #[serde(deserialize_with = "super::required")]
    pub checked_at: Option<DateTime<Utc>>,
    /// When the finalized block was read.
    #[serde(deserialize_with = "super::required")]
    pub finalized_at: Option<DateTime<Utc>>,
    /// The watcher's error.
    #[serde(deserialize_with = "super::required")]
    pub error: Option<String>,
    /// The operator's gas balance, in the chain's native base units.
    #[serde(deserialize_with = "super::required")]
    pub operator_gas: Option<Amount>,
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn chain() -> Value {
        crate::testing::json::fixture("health/chains/0")
    }

    #[test]
    fn block_numbers_are_decimal_strings() {
        let health: ChainHealth = serde_json::from_value(chain()).unwrap();
        let (block, finalized) = (health.block.map(BlockNumber::get), health.finalized_block);
        assert_eq!((block, finalized), (Some(18_100_000), Some(BlockNumber::new(18_099_990))));
        let text = serde_json::to_value(&health).unwrap();
        assert_eq!(text["block"], json!("18100000"));
        let mut unread = chain();
        unread["block"] = Value::Null;
        let unread: ChainHealth = serde_json::from_value(unread).unwrap();
        assert_eq!(unread.block, None);
        assert_eq!(serde_json::to_value(&unread).unwrap()["block"], Value::Null);
        for bad in [json!(18_100_000), json!("0x1"), json!("-1")] {
            let mut value = chain();
            value["block"] = bad;
            assert!(serde_json::from_value::<ChainHealth>(value).is_err());
        }
    }
}
