//! Venue account setup steps (`GET /v1/accounts/{venue}/setup`).

use alloy_primitives::{Address, B256};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::types::{SetupStepKind, SignatureBytes, VenueId, open_enum};

open_enum! {
    /// Where a setup step is.
    pub enum StepStatus {
        /// Done.
        Done = "done",
        /// Submitted, not yet shown by the venue (ten minutes at most).
        Pending = "pending",
        /// To do, or to do again.
        Required = "required",
    }
}

open_enum! {
    /// What a setup step is needed for.
    pub enum SetupUse {
        /// Making (quoting from the account).
        Make = "make",
        /// Paying takes from the account.
        Pay = "pay",
        /// Receiving deliveries into the account.
        Receive = "receive",
    }
}

open_enum! {
    /// What a setup or permit request asks the wallet to sign.
    pub enum SignRequestKind {
        /// An EIP-2612 token permit (EIP-712 `typed_data`).
        Permit = "permit",
        /// Aster's agent approval (EIP-712 `typed_data`, valid 60 seconds).
        AsterApproval = "aster_approval",
        /// Lighter's API key registration (`message`, signed with `personal_sign`).
        LighterApiKey = "lighter_api_key",
    }
}

/// A setup step as recorded, without reading the venue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SetupStatus {
    /// The step.
    pub step: SetupStepKind,
    /// Where it is.
    pub status: StepStatus,
    /// Why a step done before is required again.
    #[serde(deserialize_with = "super::required")]
    pub reason: Option<String>,
    /// When the step lapses (Aster's agent approval).
    #[serde(deserialize_with = "super::required")]
    pub expires_at: Option<DateTime<Utc>>,
    /// What the step is needed for.
    pub needed_for: Vec<SetupUse>,
}

/// A setup step with the request to sign once prepared.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SetupStep {
    /// The step.
    pub step: SetupStepKind,
    /// Where it is.
    pub status: StepStatus,
    /// Why a step done before is required again.
    #[serde(deserialize_with = "super::required")]
    pub reason: Option<String>,
    /// When the step lapses (Aster's agent approval).
    #[serde(deserialize_with = "super::required")]
    pub expires_at: Option<DateTime<Utc>>,
    /// What the step is needed for.
    pub needed_for: Vec<SetupUse>,
    /// The request to sign, returned by preparing a required step; submit it with its
    /// `reference`.
    #[serde(deserialize_with = "super::required")]
    pub to_sign: Option<SetupRequest>,
    /// The permit's relay transaction, while one is known.
    #[serde(deserialize_with = "super::required")]
    pub hash: Option<B256>,
}

/// What a setup step asks the wallet to sign.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SetupRequest {
    /// The request kind.
    pub kind: SignRequestKind,
    /// Who signs.
    pub signer: Address,
    /// The preparation's reference, sent back with the signature.
    #[serde(rename = "ref")]
    pub reference: String,
    /// EIP-712 typed data, as served (`permit`, `aster_approval`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub typed_data: Option<Value>,
    /// The text to sign with `personal_sign` (`lighter_api_key`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// When the request lapses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
    /// LaMarge's attestation that the agent or key is its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attestation: Option<SetupAttestation>,
}

/// LaMarge's attester signature of `SetupAttestation(address signer,bytes32 digest)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SetupAttestation {
    /// The attester's signature.
    pub signature: SignatureBytes,
}

/// A venue account's setup (`GET /v1/accounts/{venue}/setup`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct VenueSetup {
    /// The venue.
    pub venue: VenueId,
    /// The linked address.
    pub address: Address,
    /// Every step is done.
    pub ready: bool,
    /// The steps.
    pub steps: Vec<SetupStep>,
}
