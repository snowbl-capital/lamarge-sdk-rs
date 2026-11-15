//! Maker capital flows: adding capital, waiting for funding, closing orders.

use std::time::Duration;

use alloy_primitives::Address;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::time::Instant;
use tracing::Instrument as _;
use uuid::Uuid;

use super::{
    POLL, Signatures, SignedItem, SignedRecord, canonical_digest, linked, poll_until, records,
    request_digests,
};
use crate::{
    Error, QuoteIdentity, Result, Session, deployments,
    model::{
        Funding, FundingSource, MakerPreparation, MakerPreparationState, Order, OrderState, ToSign,
        ToSignKind,
    },
    protocol::checks::{
        details::{self, unsupported},
        maker::{self as check, MakerAuthority, check_close_orders, check_maker_intent},
    },
    transport::{Call, ops},
    types::{Amount, ClientId, OrderId, PreparationId, QuoteIdentityId, VenueId},
};

/// The most orders one close signs.
const CLOSE_CHUNK: usize = 50;

/// Capital to add to a delegated quote identity's pool on an escrow venue.
///
/// Persist the whole request (it is serializable) before the first call, then run that same
/// request again to resume the same preparation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapitalRequest {
    id: ClientId,
    venue: VenueId,
    amount: Amount,
    funding: Funding,
}

impl CapitalRequest {
    /// Capital named `id`: `amount` (in `venue` base units, see [`Amount::parse_venue`]) added
    /// to the pool on `venue`; what the pool's available balance does not cover comes from
    /// `funding`.
    pub fn new(id: ClientId, venue: VenueId, amount: Amount, funding: Funding) -> Self {
        Self { id, venue, amount, funding }
    }
}

/// The financial key's maker operations: capital in and out of pools quoted by delegated
/// identities. Get one with [`Session::maker`]; quoting belongs to a [`Quoter`](crate::Quoter).
#[derive(Debug, Clone, Copy)]
pub struct Maker<'a> {
    session: &'a Session,
    operator: Address,
}

impl Session {
    /// The maker operations of this session, funding pools that `operator` operates.
    /// `operator` comes from reviewed deployment configuration, never from the API.
    pub fn maker(&self, operator: Address) -> Maker<'_> {
        Maker { session: self, operator }
    }
}

/// `POST /v1/maker/{venue}/prepare` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct MakerInput {
    pub(crate) client_request_id: ClientId,
    pub(crate) quote_identity_id: QuoteIdentityId,
    pub(crate) amount: Amount,
    pub(crate) funding_source: FundingSource,
}

#[derive(Serialize)]
struct CloseBody<'a> {
    orders: &'a [OrderId],
}

/// A close to sign; `id` is `None` when no order needs a signature.
#[derive(Deserialize)]
struct ClosePreparation {
    id: Option<Uuid>,
    to_sign: Vec<ToSign>,
}

#[derive(Deserialize)]
struct ClosedOrders {
    orders: Vec<Order>,
}

impl Maker<'_> {
    /// Adds capital to `identity`'s pool on the request's venue, creating the pool on its first
    /// funding, and returns the order. Running the same request again submits the saved
    /// signatures again for the same preparation; a preparation already accepted returns its
    /// order without signing.
    ///
    /// Checked before anything is signed:
    /// - a `FundOffer` by the session's address of exactly the capital asked, to this
    ///   environment's escrow, with `identity`'s quote signer and the maker's operator;
    /// - a deposit of only what the pool's available balance does not cover, from the asked
    ///   source; for venue funding, one margin move of that deposit to the session's address;
    /// - no quote set: delegated identities only. Quotes signed before the funding stop at its
    ///   sequence floor, so the quoter publishes its lines again once the order is live
    ///   ([`wait_for_funding`](Self::wait_for_funding)).
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidRequest`] for a zero amount; `Verification(Unsupported)` for a venue
    ///   settled inside itself, venue funding on GMX or an unknown funding; both before any
    ///   request.
    /// - `Verification(Changed)` when the account linked on the venue is not the session's
    ///   address, or the pool's capital or quotes moved since it was read: review and run again.
    /// - [`Error::FundingFailed`] when the preparation of this id ended unsigned or failed: use
    ///   a new id.
    ///
    /// # Cancellation
    ///
    /// Dropping the future is safe: an unsigned preparation expires within the hour, a saved
    /// signature is submitted by the next run of the same request.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn run(
    /// #     session: &lamarge::Session,
    /// #     operator: lamarge::Address,
    /// #     identity: lamarge::QuoteIdentity,
    /// # ) -> Result<(), Box<dyn std::error::Error>> {
    /// use std::time::Duration;
    ///
    /// use lamarge::flows::CapitalRequest;
    /// use lamarge::model::Funding;
    /// use lamarge::types::{Amount, ClientId, VenueId};
    ///
    /// let amount = Amount::parse_venue(&VenueId::Hyperliquid, "1000")?;
    /// let id = ClientId::new("pool-1")?;
    /// let request = CapitalRequest::new(id, VenueId::Hyperliquid, amount, Funding::Wallet);
    /// let maker = session.maker(operator);
    /// let order = maker.add_capital(&request, &identity).await?;
    /// let live = maker.wait_for_funding(&order, Duration::from_secs(600)).await?;
    /// # Ok(()) }
    /// ```
    pub async fn add_capital(
        &self,
        request: &CapitalRequest,
        identity: &QuoteIdentity,
    ) -> Result<Order> {
        let span = tracing::info_span!(
            "lamarge.add_capital",
            venue = %request.venue,
            client_request_id = %request.id,
        );
        self.add(request, identity).instrument(span).await
    }

    async fn add(&self, request: &CapitalRequest, identity: &QuoteIdentity) -> Result<Order> {
        let venue = &request.venue;
        if request.amount.get().is_zero() {
            return Err(Error::InvalidRequest("the capital to add must be positive".into()));
        }
        if request.funding.is_unknown() {
            return Err(unsupported(details::FUNDING).into());
        }
        if deployments::chain(venue).is_none()
            || deployments::is_venue_settled(venue)
            || (*venue == VenueId::Gmx && request.funding == Funding::Venue)
        {
            return Err(unsupported(details::VENUE).into());
        }
        let session = self.session;
        let journal = session.journal();
        // The whole addition runs alone per request id across the journal's sessions, so a
        // second run reads the preparation only once the first has submitted it.
        let turn = session.record_key(&format!("maker-request:{}", request.id));
        let _turn = journal.lock(&turn).await;
        let (balances, orders) = futures_util::future::try_join(
            session.account().balances(),
            session.orders().list(None),
        )
        .await?;
        let owner = session.address();
        linked(&balances, venue, owner)?;
        let existing = orders.iter().find(|order| {
            order.capital_pool
                && order.venue == *venue
                && order.quote_identity_id == identity.id
                && order.owner_address == owner
        });
        let body = MakerInput {
            client_request_id: request.id.clone(),
            quote_identity_id: identity.id,
            amount: request.amount,
            funding_source: request.funding.clone(),
        };
        let call = Call::new(&ops::PREPARE_MAKER).segment(venue).json(&body)?;
        let prepared: MakerPreparation = session.transport().send(call, session.auth()).await?;
        match prepared.state {
            MakerPreparationState::Accepted | MakerPreparationState::Complete => {
                return Ok(prepared.order);
            }
            MakerPreparationState::Preparing => {}
            _ => return Err(ended(prepared)),
        }
        let key = session.record_key(&format!("maker:{}", prepared.id));
        // Saved signatures are replayed without the checks' clock, only in the same context.
        let context = Some(canonical_digest(&json!({
            "environment": session.escrow_environment(), "venue": venue, "request": body,
            "owner": owner, "quote_signer": identity.quote_signer, "operator": self.operator,
        })));
        let digests = request_digests(&prepared.to_sign);
        let items = if let Some(saved) = records::load::<SignedRecord>(journal, &key).await? {
            saved.reuse(&digests, context)?
        } else {
            let items = self.sign(request, identity, &prepared, existing).await?;
            let record = SignedRecord { digests, context, items };
            records::save(journal, &key, &record, false).await?;
            record.items
        };
        let call = Call::new(&ops::SUBMIT_MAKER_SIGNATURES)
            .segment(prepared.id)
            .json(&Signatures { signatures: &items })?;
        let accepted: MakerPreparation = session.transport().send(call, session.auth()).await?;
        records::delete(journal, &key, true).await?;
        tracing::info!(order = %accepted.order.id, "capital submitted");
        Ok(accepted.order)
    }

    /// Checks `prepared` against the request at the current time and signs each of its items.
    async fn sign(
        &self,
        request: &CapitalRequest,
        identity: &QuoteIdentity,
        prepared: &MakerPreparation,
        existing: Option<&Order>,
    ) -> Result<Vec<SignedItem>> {
        let session = self.session;
        let asked = check::CapitalRequest {
            venue: request.venue.clone(),
            amount: request.amount,
            client_request_id: request.id.clone(),
            funding_source: request.funding.clone(),
        };
        let authority = MakerAuthority {
            owner: session.address(),
            quote_signer: identity.quote_signer,
            quote_identity_id: identity.id,
            operator: self.operator,
            existing,
        };
        let now = session.transport().now();
        let verified =
            check_maker_intent(prepared, &asked, &authority, session.escrow_environment(), now)?;
        let mut items = Vec::with_capacity(verified.len());
        for (item, verified) in prepared.to_sign.iter().zip(&verified) {
            let signature = session.sign(verified).await?;
            items.push(SignedItem { kind: item.kind.clone(), fill_id: None, signature });
        }
        Ok(items)
    }

    /// A maker preparation.
    pub async fn preparation(&self, id: PreparationId) -> Result<MakerPreparation> {
        let call = Call::new(&ops::GET_MAKER_PREPARATION).segment(id);
        self.session.transport().send(call, self.session.auth()).await
    }

    /// Cancels an unsigned (`preparing`) preparation and restores its order; repeating returns
    /// it. Never retried.
    pub async fn cancel_preparation(&self, id: PreparationId) -> Result<MakerPreparation> {
        let call = Call::new(&ops::CANCEL_MAKER_PREPARATION).segment(id);
        self.session.transport().send(call, self.session.auth()).await
    }

    /// Waits for the funding [`add_capital`](Self::add_capital) submitted, then for the order
    /// to be `live`. The preparation, not the order, says whether the funding landed: a failed
    /// top-up returns the order to its prior state.
    ///
    /// # Errors
    ///
    /// [`Error::FundingFailed`] when the preparation is cancelled or fails,
    /// [`Error::OrderFailed`] when the order fails, [`Error::Timeout`] after `timeout` (the
    /// funding goes on: wait again or run `add_capital` again with the same id).
    pub async fn wait_for_funding(&self, order: &Order, timeout: Duration) -> Result<Order> {
        let deadline = Instant::now() + timeout;
        if let Some(id) = order.preparation_id {
            let funded = || async {
                let preparation = self.preparation(id).await?;
                match preparation.state {
                    MakerPreparationState::Complete => Ok(Some(())),
                    MakerPreparationState::Failed | MakerPreparationState::Cancelled => {
                        Err(ended(preparation))
                    }
                    _ => Ok(None),
                }
            };
            poll_until(deadline, POLL, funded).await?.ok_or(Error::Timeout("funding"))?;
        }
        self.poll_order(order.id, &[OrderState::Live], deadline).await
    }

    /// Waits until order `id` reaches one of `states`; for a funding, use
    /// [`wait_for_funding`](Self::wait_for_funding).
    ///
    /// # Errors
    ///
    /// [`Error::OrderFailed`] when the order fails (unless `states` holds `failed`),
    /// [`Error::Timeout`] after `timeout`.
    pub async fn wait_for_order(
        &self,
        id: OrderId,
        states: &[OrderState],
        timeout: Duration,
    ) -> Result<Order> {
        self.poll_order(id, states, Instant::now() + timeout).await
    }

    async fn poll_order(
        &self,
        id: OrderId,
        states: &[OrderState],
        deadline: Instant,
    ) -> Result<Order> {
        let reached = || async {
            let order = self.session.orders().get(id).await?;
            if states.contains(&order.state) {
                return Ok(Some(order));
            }
            if order.state == OrderState::Failed {
                return Err(Error::OrderFailed { order: id, reason: reason(order.error) });
            }
            Ok(None)
        };
        poll_until(deadline, POLL, reached).await?.ok_or(Error::Timeout("order"))
    }

    /// Closes `ids` into their owners' venue margin, 50 orders per `CloseOrders` signature,
    /// and returns the orders each close moved to `closing` (orders already closing are
    /// skipped). Each close is checked before it is signed: one `CloseOrders` in this
    /// environment's HyperEVM escrow domain, by and for the session's address, of distinct orders
    /// among those asked, within the hour.
    ///
    /// Closes are not journalled: after an error, run it again with the remaining ids.
    ///
    /// # Cancellation
    ///
    /// Dropping the future is safe: an unsigned close expires; a submitted one shows its orders
    /// `closing`.
    pub async fn close_orders(&self, ids: &[OrderId]) -> Result<Vec<Order>> {
        let span = tracing::info_span!("lamarge.close_orders", orders = ids.len());
        self.close(ids).instrument(span).await
    }

    async fn close(&self, ids: &[OrderId]) -> Result<Vec<Order>> {
        // Not journalled: the server prepares no close for orders already closing, and accepting
        // one only marks its orders closing, so a close signed again releases nothing twice.
        let session = self.session;
        let transport = session.transport();
        let mut closed = Vec::new();
        for chunk in ids.chunks(CLOSE_CHUNK) {
            let call = Call::new(&ops::PREPARE_CLOSE).json(&CloseBody { orders: chunk })?;
            let close: ClosePreparation = transport.send(call, session.auth()).await?;
            let Some(id) = close.id else { continue };
            let verified = check_close_orders(
                &close.to_sign,
                chunk,
                session.address(),
                session.escrow_environment(),
                transport.now(),
            )?;
            let signature = session.sign(&verified).await?;
            let items = [SignedItem { kind: ToSignKind::CloseOrders, fill_id: None, signature }];
            let call = Call::new(&ops::SUBMIT_CLOSE)
                .segment(id)
                .json(&Signatures { signatures: &items })?;
            let answer: ClosedOrders = transport.send(call, session.auth()).await?;
            tracing::info!(orders = answer.orders.len(), "close submitted");
            closed.extend(answer.orders);
        }
        Ok(closed)
    }
}

/// A preparation that ended without funding.
fn ended(preparation: MakerPreparation) -> Error {
    Error::FundingFailed {
        preparation: preparation.id,
        state: preparation.state,
        reason: reason(preparation.order.error),
    }
}

/// An order's error, or that it gave none.
fn reason(error: Option<String>) -> String {
    error.unwrap_or_else(|| "no reason given".to_owned())
}

#[cfg(test)]
mod tests;
