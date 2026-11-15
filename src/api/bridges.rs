//! Bridges: the external part of a move, quoted by the server, checked and signed or sent by
//! the caller ([`Session::sign_bridge`](crate::Session::sign_bridge),
//! [`Session::bridge_deposit`](crate::Session::bridge_deposit)).

use alloy_primitives::{Address, B256};
use serde::{Deserialize, Serialize};

use super::Bridges;
use crate::{
    Error, Result,
    error::{ErrorCode, Reason},
    model::{Bridge, BridgeFunding},
    transport::{Call, ops},
    types::{Amount, BridgeId, ClientId, MoveId},
};

/// `POST /v1/moves/{id}/bridges`: `amount_in` of a move through the external bridge.
///
/// Persist it before the first call: the same `client_bridge_id` and terms return the bridge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BridgeRequest {
    client_bridge_id: ClientId,
    amount_in: Amount,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    from_wallet: Option<WalletSource>,
}

/// A wallet token a bridge starts from instead of the move's origin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalletSource {
    chain_id: u64,
    token: Address,
}

impl BridgeRequest {
    /// A bridge named `id` of `amount_in`, in the origin token's base units, from the move's
    /// origin.
    pub fn new(id: ClientId, amount_in: Amount) -> Self {
        Self { client_bridge_id: id, amount_in, from_wallet: None }
    }

    /// From the payer's `token` wallet on the origin chain `chain_id` instead (after a
    /// withdrawal there).
    #[must_use]
    pub fn from_wallet(mut self, chain_id: u64, token: Address) -> Self {
        self.from_wallet = Some(WalletSource { chain_id, token });
        self
    }
}

#[derive(Serialize)]
struct Report {
    #[serde(skip_serializing_if = "Option::is_none")]
    tx_hash: Option<B256>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    unsent: bool,
}

impl Bridges<'_> {
    /// Quotes the bridge and returns its order, unchecked: check it before signing or sending
    /// anything. The move's live bridges stay within its `amount_in`.
    pub async fn create(&self, move_id: MoveId, request: &BridgeRequest) -> Result<Bridge> {
        let call = Call::new(&ops::CREATE_BRIDGE).segment(move_id).json(request)?;
        self.inner.send(call, self.auth).await
    }

    /// A bridge.
    pub async fn get(&self, id: BridgeId) -> Result<Bridge> {
        self.inner.send(Call::new(&ops::GET_BRIDGE).segment(id), self.auth).await
    }

    /// A wallet-origin bridge's balance and allowance toward the depository, read fresh on the
    /// origin chain; refused for a `HyperCore` bridge.
    pub async fn funding(&self, id: BridgeId) -> Result<BridgeFunding> {
        self.inner.send(Call::new(&ops::GET_BRIDGE_FUNDING).segment(id), self.auth).await
    }

    /// Fences a wallet-origin bridge before the deposit prompt; sent once, never retried. After
    /// a lost answer, never prompt again.
    ///
    /// # Errors
    ///
    /// [`Error::DepositFenced`] when another client (or an earlier call) fenced it first: do not
    /// send the deposit.
    pub async fn fence(&self, id: BridgeId) -> Result<Bridge> {
        let body = Report { tx_hash: None, unsent: false };
        let call = Call::new(&ops::FENCE_BRIDGE).segment(id).json(&body)?;
        self.inner.send(call, self.auth).await.map_err(|error| {
            let fenced = error.api().is_some_and(|api| {
                *api.code() == ErrorCode::StateConflict && api.reason() == Some(&Reason::Fenced)
            });
            if fenced { Error::DepositFenced(id) } else { error }
        })
    }

    /// Reports a broadcast deposit, each replacement too (eight at most).
    pub async fn report_transaction(&self, id: BridgeId, tx_hash: B256) -> Result<Bridge> {
        self.report(id, &Report { tx_hash: Some(tx_hash), unsent: false }).await
    }

    /// States that the wallet refused before broadcasting, while no hash is reported: the
    /// bridge fails.
    pub async fn report_unsent(&self, id: BridgeId) -> Result<Bridge> {
        self.report(id, &Report { tx_hash: None, unsent: true }).await
    }

    async fn report(&self, id: BridgeId, body: &Report) -> Result<Bridge> {
        let call = Call::new(&ops::REPORT_BRIDGE_TRANSACTION).segment(id).json(body)?;
        self.inner.send(call, self.auth).await
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{address, b256};
    use serde_json::{Value, json};
    use wiremock::MockServer;

    use super::*;
    use crate::testing::{
        http::{Expect, client, only_request, served, session},
        json::fixture,
    };

    async fn sent(server: &MockServer) -> Value {
        only_request(server).await.body_json().unwrap()
    }

    #[tokio::test]
    async fn every_call_sends_its_terms_with_the_session() {
        let server = MockServer::start().await;
        let client = client(&server);
        let bridges = Bridges { inner: client.inner(), auth: Some(session()) };
        let answer = fixture("move/bridges/0");
        let bridge: Bridge = serde_json::from_value(answer.clone()).unwrap();
        let pusd = address!("c011a7e12a19f7b1f670d46f03b03f3342e82dfb");
        let request = BridgeRequest::new(
            ClientId::new("move-1.bridge-1").unwrap(),
            Amount::from(100_000_000u64),
        )
        .from_wallet(137, pusd);
        let create =
            Expect::post(format!("/v1/moves/{}/bridges", bridge.move_id), answer.clone()).bearer();
        served(&server, create, bridges.create(bridge.move_id, &request)).await.unwrap();
        assert_eq!(sent(&server).await, fixture("bridge_request"));

        let one = format!("/v1/bridges/{}", bridge.id);
        served(&server, Expect::get(&one, answer.clone()).bearer(), bridges.get(bridge.id))
            .await
            .unwrap();
        let funding = Expect::get(format!("{one}/funding"), fixture("bridge_funding")).bearer();
        served(&server, funding, bridges.funding(bridge.id)).await.unwrap();
        let hash = b256!("3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f");
        let transactions = format!("{one}/transactions");
        let expect = || Expect::post(&transactions, answer.clone()).bearer();
        served(&server, expect(), bridges.fence(bridge.id)).await.unwrap();
        assert_eq!(sent(&server).await, json!({}));
        let fenced = json!({"error": {"code": "STATE_CONFLICT", "message": "fenced",
            "retryable": false, "reason": "fenced"}});
        let conflict = Expect::post(&transactions, fenced).status(409).bearer();
        let refused = served(&server, conflict, bridges.fence(bridge.id)).await.unwrap_err();
        assert!(matches!(refused, Error::DepositFenced(id) if id == bridge.id));
        let other = json!({"error": {"code": "STATE_CONFLICT", "message": "expired",
            "retryable": false}});
        let conflict = Expect::post(&transactions, other).status(409).bearer();
        let refused = served(&server, conflict, bridges.fence(bridge.id)).await.unwrap_err();
        assert_eq!(refused.status(), Some(409));
        served(&server, expect(), bridges.report_transaction(bridge.id, hash)).await.unwrap();
        assert_eq!(sent(&server).await, json!({"tx_hash": hash}));
        served(&server, expect(), bridges.report_unsent(bridge.id)).await.unwrap();
        assert_eq!(sent(&server).await, json!({"unsent": true}));
    }
}
