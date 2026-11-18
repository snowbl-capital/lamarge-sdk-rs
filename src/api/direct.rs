//! Direct operations: a GMX withdrawal its owner sends itself, recorded and followed by LaMarge,
//! which relays nothing there. [`Session::move_funds_with`](crate::Session::move_funds_with)
//! runs one for a move from GMX.

use alloy_primitives::{Address, B256};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::Directs;
use crate::{
    Result,
    model::{BridgeFunding, DirectKind, DirectOperation},
    transport::{Call, ops},
    types::{Amount, ClientId, MoveId, VenueId},
};

/// `POST /v1/direct`: an operation's terms, the same `client_operation_id` and terms finding it
/// again.
///
/// Persist it before the first call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) move_id: Option<MoveId>,
    pub(crate) client_operation_id: ClientId,
    pub(crate) venue: VenueId,
    pub(crate) kind: DirectKind,
    pub(crate) owner: Address,
    pub(crate) recipient: Address,
    pub(crate) token: Address,
    pub(crate) amount: Amount,
    pub(crate) min_out: Amount,
}

impl DirectRequest {
    /// `amount` of GMX's settlement token (Arbitrum USDC), out of `owner`'s GMX Account to
    /// `owner`'s own address on Arbitrum.
    pub fn gmx_withdrawal(id: ClientId, owner: Address, token: Address, amount: Amount) -> Self {
        Self {
            move_id: None,
            client_operation_id: id,
            venue: VenueId::Gmx,
            kind: DirectKind::Withdrawal,
            owner,
            recipient: owner,
            token,
            amount,
            min_out: amount,
        }
    }

    /// Attaches the withdrawal to move `id`, whose bridge it feeds.
    #[must_use]
    pub fn move_id(mut self, id: MoveId) -> Self {
        self.move_id = Some(id);
        self
    }
}

#[derive(Serialize)]
struct Report {
    #[serde(skip_serializing_if = "Option::is_none")]
    tx_hash: Option<B256>,
}

impl Directs<'_> {
    /// Records the operation (idempotent by `client_operation_id` and terms), unchecked: check it
    /// before sending anything.
    pub async fn create(&self, request: &DirectRequest) -> Result<DirectOperation> {
        self.inner.send(Call::new(&ops::CREATE_DIRECT).json(request)?, self.auth).await
    }

    /// An operation.
    pub async fn get(&self, id: Uuid) -> Result<DirectOperation> {
        self.inner.send(Call::new(&ops::GET_DIRECT).segment(id), self.auth).await
    }

    /// The owner's funds and allowance toward the operation's escrow, read fresh on its chain.
    pub async fn funding(&self, id: Uuid) -> Result<BridgeFunding> {
        self.inner.send(Call::new(&ops::GET_DIRECT_FUNDING).segment(id), self.auth).await
    }

    /// Fences the operation before its owner is asked to send it; sent once, never retried. After
    /// a lost answer, never ask again.
    pub async fn fence(&self, id: Uuid) -> Result<DirectOperation> {
        let call = Call::new(&ops::FENCE_DIRECT).segment(id).json(&Report { tx_hash: None })?;
        self.inner.send(call, self.auth).await
    }

    /// Reports a broadcast transaction of the operation.
    pub async fn report_transaction(&self, id: Uuid, tx_hash: B256) -> Result<DirectOperation> {
        let body = Report { tx_hash: Some(tx_hash) };
        let call = Call::new(&ops::REPORT_DIRECT).segment(id).json(&body)?;
        self.inner.send(call, self.auth).await
    }
}
