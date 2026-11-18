//! Moves: a plan, then the record every part of a move attaches to.

use alloy_primitives::Address;
use serde::{Deserialize, Serialize};

use super::Moves;
use crate::{
    Result,
    model::{Move, MoveEndpoint, MoveQuote},
    transport::{Call, ops},
    types::{Amount, Bps, ClientId, MoveId},
};

/// Who a move or bridge delivers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum MoveRecipient {
    /// An address: a venue account's owner, or a wallet.
    Address(Address),
    /// A Lighter account and the owner Lighter's public API must report for it, read before
    /// anything is signed or sent to it.
    LighterAccount {
        /// The account index.
        index: u64,
        /// Its expected owner.
        owner: Address,
    },
}

impl MoveRecipient {
    /// As the API writes it: an address, or a Lighter account index.
    pub(crate) fn wire(&self) -> String {
        match self {
            Self::Address(address) => format!("{address:#x}"),
            Self::LighterAccount { index, .. } => index.to_string(),
        }
    }
}

/// What [`Moves::quote`] plans: `amount_in` from `from` to `to`, at least `min_out` over all
/// parts. Reserves nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoveQuery {
    from: MoveEndpoint,
    to: MoveEndpoint,
    amount_in: Amount,
    min_out: Amount,
    recipient: MoveRecipient,
    payer: Option<Address>,
    max_fee_bps: Option<Bps>,
}

impl MoveQuery {
    /// Plans `amount_in` (in `from`'s token base units) for at least `min_out` (in `to`'s) to
    /// `recipient`.
    pub fn new(
        from: MoveEndpoint,
        to: MoveEndpoint,
        amount_in: Amount,
        min_out: Amount,
        recipient: MoveRecipient,
    ) -> Self {
        Self { from, to, amount_in, min_out, recipient, payer: None, max_fee_bps: None }
    }

    /// The paying address; the server's default is the master address.
    #[must_use]
    pub fn payer(mut self, payer: Address) -> Self {
        self.payer = Some(payer);
        self
    }

    /// The highest taker fee LaMarge's part accepts; unset, it pays no fee.
    #[must_use]
    pub fn max_fee_bps(mut self, bps: Bps) -> Self {
        self.max_fee_bps = Some(bps);
        self
    }
}

/// `POST /v1/moves`: a move's terms, the one id every part attaches to.
///
/// Persist it before the first call: the same `client_move_id` and terms return the move.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoveRequest {
    pub(crate) client_move_id: ClientId,
    pub(crate) from: MoveEndpoint,
    pub(crate) to: MoveEndpoint,
    pub(crate) amount_in: Amount,
    pub(crate) min_out: Amount,
    pub(crate) payer: Address,
    pub(crate) recipient: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) max_fee_bps: Option<Bps>,
}

impl MoveRequest {
    /// A move named `id` of `amount_in` (in `from`'s token base units) from `payer`, delivering
    /// at least `min_out` (in `to`'s) to `recipient` over all its parts.
    pub fn new(
        id: ClientId,
        from: MoveEndpoint,
        to: MoveEndpoint,
        amount_in: Amount,
        min_out: Amount,
        payer: Address,
        recipient: &MoveRecipient,
    ) -> Self {
        Self {
            client_move_id: id,
            from,
            to,
            amount_in,
            min_out,
            payer,
            recipient: recipient.wire(),
            max_fee_bps: None,
        }
    }

    /// The highest taker fee LaMarge's part accepts.
    #[must_use]
    pub fn max_fee_bps(mut self, bps: Bps) -> Self {
        self.max_fee_bps = Some(bps);
        self
    }
}

#[derive(Serialize)]
struct CloseBody<'a> {
    unallocated: Amount,
    #[serde(skip_serializing_if = "Option::is_none")]
    unallocated_reason: Option<&'a str>,
}

impl Moves<'_> {
    /// What LaMarge's makers fill now and what a bridge would carry of the rest, at the floor
    /// rate `min_out / amount_in`; reserves nothing.
    pub async fn quote(&self, query: &MoveQuery) -> Result<MoveQuote> {
        let mut call = Call::new(&ops::QUOTE_MOVE);
        for (venue, chain, token, endpoint) in [
            ("from", "from_chain_id", "from_token", &query.from),
            ("to", "to_chain_id", "to_token", &query.to),
        ] {
            call = call.query(venue, &endpoint.venue);
            if let Some(id) = endpoint.chain_id {
                call = call.query(chain, id);
            }
            if let Some(address) = endpoint.token {
                call = call.query(token, format_args!("{address:#x}"));
            }
        }
        call = call
            .query("amount_in", query.amount_in)
            .query("min_out", query.min_out)
            .query("recipient", query.recipient.wire());
        if let Some(payer) = query.payer {
            call = call.query("payer", format_args!("{payer:#x}"));
        }
        if let Some(bps) = query.max_fee_bps {
            call = call.query("max_fee_bps", bps.get());
        }
        self.inner.send(call, self.auth).await
    }

    /// Records a move's terms (idempotent by `client_move_id`); reserves and moves nothing.
    pub async fn create(&self, request: &MoveRequest) -> Result<Move> {
        self.inner.send(Call::new(&ops::CREATE_MOVE).json(request)?, self.auth).await
    }

    /// A move and its parts.
    pub async fn get(&self, id: MoveId) -> Result<Move> {
        self.inner.send(Call::new(&ops::GET_MOVE).segment(id), self.auth).await
    }

    /// Declares that no part will be added, with what no part carries (in the origin token's
    /// base units) and why. Idempotent with the same terms; refused while a bridge is prepared.
    pub async fn close(
        &self,
        id: MoveId,
        unallocated: Amount,
        reason: Option<&str>,
    ) -> Result<Move> {
        let body = CloseBody { unallocated, unallocated_reason: reason };
        let call = Call::new(&ops::CLOSE_MOVE).segment(id).json(&body)?;
        self.inner.send(call, self.auth).await
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::address;
    use serde_json::json;
    use wiremock::MockServer;

    use super::*;
    use crate::{
        testing::{
            http::{Expect, client, only_request, served, session},
            json::fixture,
        },
        types::VenueId,
    };

    const USDC: Address = address!("af88d065e77c8cc2239327c5edb3a432268e5831");
    const PAYER: Address = address!("000000000000000000000000000000000000dead");

    #[tokio::test]
    async fn every_call_sends_its_terms_with_the_session() {
        let server = MockServer::start().await;
        let client = client(&server);
        let moves = Moves { inner: client.inner(), auth: Some(session()) };
        let from = MoveEndpoint::account(&VenueId::Hyperliquid);
        let to = MoveEndpoint::wallet(42161, USDC);
        let lighter = MoveRecipient::LighterAccount { index: 3, owner: PAYER };
        let query = MoveQuery::new(
            from.clone(),
            to.clone(),
            Amount::from(105_000_000u64),
            Amount::from(102_900_000u64),
            lighter,
        );
        let bare = Expect::get("/v1/moves/quote", fixture("move_quote"))
            .query(&[
                ("from", "hyperliquid"),
                ("to", "wallet"),
                ("to_chain_id", "42161"),
                ("to_token", "0xaf88d065e77c8cc2239327c5edb3a432268e5831"),
                ("amount_in", "105000000"),
                ("min_out", "102900000"),
                ("recipient", "3"),
            ])
            .bearer();
        served(&server, bare, moves.quote(&query)).await.unwrap();
        let every = Expect::get("/v1/moves/quote", fixture("move_quote"))
            .query(&[
                ("from", "hyperliquid"),
                ("to", "wallet"),
                ("to_chain_id", "42161"),
                ("to_token", "0xaf88d065e77c8cc2239327c5edb3a432268e5831"),
                ("amount_in", "105000000"),
                ("min_out", "102900000"),
                ("recipient", "3"),
                ("payer", "0x000000000000000000000000000000000000dead"),
                ("max_fee_bps", "5"),
            ])
            .bearer();
        let query = query.payer(PAYER).max_fee_bps(Bps::new(5).unwrap());
        served(&server, every, moves.quote(&query)).await.unwrap();

        let request = MoveRequest::new(
            ClientId::new("move-1").unwrap(),
            from,
            to,
            Amount::from(105_000_000u64),
            Amount::from(102_900_000u64),
            PAYER,
            &MoveRecipient::Address(PAYER),
        )
        .max_fee_bps(Bps::new(5).unwrap());
        let created = Expect::post("/v1/moves", fixture("move")).bearer();
        let created = served(&server, created, moves.create(&request)).await.unwrap();
        let sent = only_request(&server).await.body_json::<serde_json::Value>().unwrap();
        assert_eq!(sent, fixture("move_request"));

        let one = format!("/v1/moves/{}", created.id);
        served(&server, Expect::get(&one, fixture("move")).bearer(), moves.get(created.id))
            .await
            .unwrap();
        let close = Expect::post(format!("{one}/close"), fixture("move")).bearer();
        served(&server, close, moves.close(created.id, Amount::ZERO, None)).await.unwrap();
        let sent = only_request(&server).await.body_json::<serde_json::Value>().unwrap();
        assert_eq!(sent, json!({"unallocated": "0"}));
    }
}
