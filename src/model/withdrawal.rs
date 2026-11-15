//! Withdrawal models: withdrawals, their history rows and states.

use alloy_primitives::Address;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::types::{Amount, MoveId, Revision, VenueId, WithdrawalId, open_enum};

open_enum! {
    /// A withdrawal's progress at its venue.
    pub enum WithdrawalState {
        /// Waiting to be submitted.
        Queued = "queued",
        /// Being submitted; becomes `unknown` after a minute.
        Submitting = "submitting",
        /// `unknown`: maybe submitted. Never resend it.
        Uncertain = "unknown",
        /// Acknowledged by the venue; arrival unconfirmed.
        Relayed = "relayed",
        /// Rejected by the venue, or never started in time.
        Refused = "refused",
    }
}

open_enum! {
    /// Whether funds leave the venue or move to another account on it.
    pub enum WithdrawalKind {
        /// Out of the venue.
        Withdrawal = "withdrawal",
        /// To another account on the same venue.
        Send = "send",
    }
}

open_enum! {
    /// A Hyperliquid balance other than the perps margin.
    pub enum Dex {
        /// A standard account's trade.xyz balance.
        Xyz = "xyz",
    }
}

/// A withdrawal or same-venue send.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Withdrawal {
    /// The withdrawal.
    pub id: WithdrawalId,
    /// The move it feeds a bridge for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub move_id: Option<MoveId>,
    /// The venue it leaves.
    pub venue: VenueId,
    /// `Xyz` when it left a standard Hyperliquid account's trade.xyz balance.
    #[serde(deserialize_with = "super::required")]
    pub from_dex: Option<Dex>,
    /// Withdrawal or send.
    pub kind: WithdrawalKind,
    /// In `venue` base units; on Aster the venue fee included.
    pub amount: Amount,
    /// A send's recipient; a withdrawal's receiving address.
    pub destination: Address,
    /// Progress at the venue.
    pub state: WithdrawalState,
    /// The venue's withdrawal or transfer id once relayed (Polymarket, Aster).
    #[serde(deserialize_with = "super::required")]
    pub venue_ref: Option<String>,
    /// Why it failed.
    #[serde(deserialize_with = "super::required")]
    pub error: Option<String>,
    /// When it was prepared.
    pub created_at: DateTime<Utc>,
    /// When the venue acknowledged it.
    #[serde(deserialize_with = "super::required")]
    pub relayed_at: Option<DateTime<Utc>>,
}

/// A withdrawal as withdrawal history lists it, with its revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WithdrawalHistoryRow {
    /// The withdrawal.
    #[serde(flatten)]
    pub withdrawal: Withdrawal,
    /// Newer state has a higher revision.
    pub revision: Revision,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_state_is_a_known_variant() {
        let state: WithdrawalState = serde_json::from_str("\"unknown\"").unwrap();
        assert_eq!(state, WithdrawalState::Uncertain);
        assert!(!state.is_unknown());
        assert_eq!(serde_json::to_string(&state).unwrap(), "\"unknown\"");
        assert!(serde_json::from_str::<WithdrawalState>("\"lost\"").unwrap().is_unknown());
    }

    #[test]
    fn history_rows_flatten_their_withdrawal() {
        let value = crate::testing::json::fixture("withdrawal_history_page/data/0");
        let row: WithdrawalHistoryRow = serde_json::from_value(value.clone()).unwrap();
        let plain: Withdrawal = serde_json::from_value(value).unwrap();
        assert_eq!((row.withdrawal, row.revision), (plain, Revision::new(17)));
    }
}
