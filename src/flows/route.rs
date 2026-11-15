//! Routes: a move through a venue between in two transfers, ported from the TypeScript SDK's
//! `route()`. The first names the route's end (`then`); the second pays what LaMarge leaves to
//! continue with on `via`. Where LaMarge can (the first take paying inside Lighter or Aster into
//! an escrow venue), and when asked, both takes are signed at once and committed together.

use std::time::Duration;

use alloy_primitives::{Address, U256};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::time::Instant;
use tracing::Instrument as _;

use super::{
    Recipient, Signatures, SignedItem, TradeTerms,
    conversion::ConversionTerms,
    records,
    take::{self, TakeInput},
    transfer::{
        Continues, FRESH_SECONDS, PendingReason, TransferOutcome, TransferRequest, prefunded,
        unsigned_reservation,
    },
};
use crate::{
    Error, Result, Session, TransactionSender, deployments,
    error::{ErrorCode, Reason},
    model::{
        Balance, Destination, Fill, FillState, Funding, GroupedRoute, GroupedRouteState,
        MakerDestination, MakerLeg, TakePreparation, TransferIntent,
    },
    protocol::checks::{
        details::{self, changed, differs},
        funding::check_wallet_capability,
    },
    transport::{Call, ops},
    types::{Amount, Bps, ClientId, MaxParts, MoveId, TakeId, TransferId, VenueId},
};

/// Least time left before `commit_by` to ask for another signature.
const REQUEST_MARGIN_SECONDS: i64 = 5;

/// Whether LaMarge can sign a route from `from` through `via` at once: the first take pays
/// inside Lighter or Aster, into an escrow venue.
pub fn groupable(from: &VenueId, via: &VenueId) -> bool {
    deployments::is_venue_settled(from)
        && !deployments::is_venue_settled(via)
        && deployments::chain(via).is_some()
}

/// The second take of a route, as planned: what it pays on `via`, the least it delivers on `to`,
/// and the highest taker fee it accepts (none without).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteLeg {
    amount_in: Amount,
    min_out: Amount,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_fee_bps: Option<Bps>,
}

impl RouteLeg {
    /// Pays `amount_in` on `via` for at least `min_out` on `to`, no fee accepted.
    pub const fn new(amount_in: Amount, min_out: Amount) -> Self {
        Self { amount_in, min_out, max_fee_bps: None }
    }

    /// Accepts a taker fee up to `bps`.
    #[must_use]
    pub const fn max_fee_bps(mut self, bps: Bps) -> Self {
        self.max_fee_bps = Some(bps);
        self
    }
}

/// A route from `from` through `via` to `to`: the first transfer pays `amount_in` for at least
/// `min_out` into the account on `via` (the session's, or the via session's), the second pays
/// what LaMarge leaves to continue with there, for the route's least in proportion, to
/// `recipient` on `to`.
///
/// Persist the whole request (it is serializable) before the first call, then run that same
/// request again to resume: the first transfer goes under `id`, the second under `id.2` (or a
/// successor's `renew:<id>`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteRequest {
    id: ClientId,
    from: VenueId,
    via: VenueId,
    to: VenueId,
    amount_in: Amount,
    min_out: Amount,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_fee_bps: Option<Bps>,
    then: RouteLeg,
    recipient: Recipient,
    funding: Funding,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    conversion: Option<ConversionTerms>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    move_id: Option<MoveId>,
    #[serde(default)]
    grouped: bool,
    #[serde(default)]
    keep_floor: bool,
    poll_interval: Duration,
    arrival_timeout: Duration,
}

impl RouteRequest {
    /// The route `id` from `from` through `via` to `to`, funded by the venue account, no fee
    /// accepted on its first take, run leg by leg, reading every 2 s while funds arrive, for up
    /// to 10 minutes.
    pub fn new(
        id: ClientId,
        (from, via, to): (VenueId, VenueId, VenueId),
        amount_in: Amount,
        min_out: Amount,
        then: RouteLeg,
        recipient: Recipient,
    ) -> Self {
        Self {
            id,
            from,
            via,
            to,
            amount_in,
            min_out,
            max_fee_bps: None,
            then,
            recipient,
            funding: Funding::Venue,
            conversion: None,
            move_id: None,
            grouped: false,
            keep_floor: false,
            poll_interval: Duration::from_secs(2),
            arrival_timeout: Duration::from_secs(600),
        }
    }

    /// Accepts a taker fee up to `bps` on the first take.
    #[must_use]
    pub const fn max_fee_bps(mut self, bps: Bps) -> Self {
        self.max_fee_bps = Some(bps);
        self
    }

    /// How the first transfer pays (default [`Funding::Venue`]); the second pays from `via`.
    #[must_use]
    pub fn funding(mut self, funding: Funding) -> Self {
        self.funding = funding;
        self
    }

    /// Funds the first transfer by a Polygon conversion ([`TransferRequest::conversion`]); run
    /// with a sender.
    #[must_use]
    pub const fn conversion(mut self, terms: ConversionTerms) -> Self {
        self.conversion = Some(terms);
        self
    }

    /// The move the route is LaMarge's part of, on its first transfer.
    #[must_use]
    pub const fn move_id(mut self, id: MoveId) -> Self {
        self.move_id = Some(id);
        self
    }

    /// Requires the route to be signed at once: a route LaMarge cannot group, or that could not
    /// commit, ends in [`Error::RouteNotCommitted`]; going leg by leg is then the caller's call.
    #[must_use]
    pub const fn grouped(mut self, grouped: bool) -> Self {
        self.grouped = grouped;
        self
    }

    /// `then.min_out` is a floor: a second take paying less than `then.amount_in` still asks for
    /// all of it, never a share.
    #[must_use]
    pub const fn keep_floor(mut self, keep: bool) -> Self {
        self.keep_floor = keep;
        self
    }

    /// Between reads while funds arrive.
    #[must_use]
    pub const fn poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    /// How long to wait for each arrival before [`RouteOutcome::Pending`].
    #[must_use]
    pub const fn arrival_timeout(mut self, timeout: Duration) -> Self {
        self.arrival_timeout = timeout;
        self
    }

    /// The first take's terms: into `hub`'s account on `via`.
    fn first_terms(&self, hub: Address) -> TradeTerms {
        let terms = TradeTerms::new(
            self.from.clone(),
            self.via.clone(),
            self.amount_in,
            self.min_out,
            Recipient::Account(hub),
        )
        .funding(self.funding.clone());
        match self.max_fee_bps {
            Some(bps) => terms.max_fee_bps(bps),
            None => terms,
        }
    }

    fn continues(&self) -> Continues {
        let destination = self.recipient.destination();
        Continues {
            to: self.to.clone(),
            recipient: self.recipient.address(),
            destination: (destination == Destination::Wallet).then_some(destination),
            amount_in: self.then.amount_in,
            min_out: self.then.min_out,
            max_fee_bps: self.then.max_fee_bps,
        }
    }

    /// The least a second take paying `amount` may deliver: its share of `then.min_out`, or all
    /// of it with [`Self::keep_floor`].
    fn least_out(&self, amount: U256) -> Option<U256> {
        if self.keep_floor {
            return Some(self.then.min_out.get());
        }
        Some(self.then.min_out.get().checked_mul(amount)? / self.then.amount_in.get())
    }
}

/// How a route run ended.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum RouteOutcome {
    /// The first take delivered and the second, if any, ran: what was not taken waits on `via`.
    Settled {
        /// The first transfer, as last read.
        first: TransferIntent,
        /// The first take's fills.
        first_fills: Vec<Fill>,
        /// The second takes' fills: a grouped route's own, then a continuation's.
        second_fills: Vec<Fill>,
        /// The second transfer that ran, if any.
        second_transfer: Option<TransferId>,
        /// What LaMarge leaves to continue with on `via` (`continuable`), as last read.
        left: Option<Amount>,
    },
    /// Nothing failed; run the same request again later.
    Pending {
        /// The first transfer, as last read.
        first: TransferIntent,
        /// The second transfer, once there is one.
        second_transfer: Option<TransferId>,
        /// What it waits for.
        reason: PendingReason,
    },
}

/// Why a route asked to be signed at once was not committed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RouteNotCommitted {
    /// LaMarge cannot group it now: not eligible, the book or the accounts on `via` do not carry
    /// it, or too little time is left.
    Unavailable,
    /// `commit_by` passed before every signature was committed.
    Expired,
    /// The route was aborted, or its first transfer cancelled.
    Aborted,
}

impl std::fmt::Display for RouteNotCommitted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Unavailable => "unavailable",
            Self::Expired => "expired",
            Self::Aborted => "aborted",
        })
    }
}

/// `POST /v1/transfers` body of a route's first transfer.
#[derive(Serialize)]
struct FirstInput<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    move_id: Option<MoveId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    funding: Option<Funding>,
    client_transfer_id: &'a ClientId,
    from: &'a VenueId,
    to: &'a VenueId,
    amount_in: Amount,
    min_out: Amount,
    max_parts: MaxParts,
    payer: Address,
    recipient: Address,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_fee_bps: Option<Bps>,
    #[serde(skip_serializing_if = "Option::is_none")]
    conversion: Option<ConversionBody>,
    then: Continues,
}

#[derive(Serialize)]
struct ConversionBody {
    amount_in: Amount,
    min_out: Amount,
}

/// `POST /v1/transfers` body of a continuation.
#[derive(Serialize)]
struct SecondInput<'a> {
    client_transfer_id: &'a ClientId,
    from: &'a VenueId,
    to: &'a VenueId,
    amount_in: Amount,
    min_out: Amount,
    max_parts: MaxParts,
    payer: Address,
    recipient: Address,
    #[serde(skip_serializing_if = "Option::is_none")]
    destination: Option<Destination>,
    previous_transfer_id: TransferId,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_fee_bps: Option<Bps>,
}

#[derive(Serialize)]
struct PrepareBody {
    reissue: bool,
}

/// `POST /v1/transfers/{id}/grouped` answer.
#[derive(Deserialize)]
struct GroupedPreparation {
    route: GroupedRoute,
    first: TakePreparation,
    second: TakePreparation,
}

#[derive(Deserialize)]
struct TakeFills {
    take: Fills,
}

#[derive(Deserialize)]
struct Fills {
    fills: Vec<Fill>,
}

/// The continuation a route last opened (`{ns}route-then:{route id}`).
#[derive(Debug, Serialize, Deserialize)]
struct Latest {
    transfer: TransferId,
}

/// Why a route run stops: pending, or failed.
enum Halt {
    Pending(Box<TransferIntent>, Option<TransferId>, PendingReason),
    Failed(Error),
}

/// A pending halt.
fn pending(first: TransferIntent, second: Option<TransferId>, reason: PendingReason) -> Halt {
    Halt::Pending(Box::new(first), second, reason)
}

impl<E: Into<Error>> From<E> for Halt {
    fn from(error: E) -> Self {
        Self::Failed(error.into())
    }
}

type Step<T> = std::result::Result<T, Halt>;

impl Session {
    /// Runs a route with the session paying on `from` and on `via`; see [`Self::route_with`].
    pub async fn route(&self, request: &RouteRequest) -> Result<RouteOutcome> {
        self.route_with(request, None, None).await
    }

    /// Runs a route: two transfers, each a [`Self::transfer`], the first naming the route's end,
    /// the second paying what LaMarge leaves to continue with on `via`, in whole venue steps,
    /// for the route's least in proportion, once a read of `via` made after the delivery shows
    /// it there. `via` is the session of the account linked on `via`, when another than this
    /// one: it receives the first take and pays the second. `sender` sends the first transfer's
    /// conversion.
    ///
    /// With [`RouteRequest::grouped`], a route whose first take pays inside a venue through an
    /// escrow venue ([`groupable`]) is signed at once: both takes are checked, signed back to
    /// back (each signature journalled) and committed together before `commit_by`; the second is
    /// then priced and signed before the first delivers, and paid once its funds arrive.
    ///
    /// Running the same request again resumes: a route LaMarge holds every signature of is
    /// followed to its end, a continuation already opened is followed and never opened again.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidRequest`] before any request for a route through `from` or `to`.
    /// - [`Error::RouteNotCommitted`] for a route asked to be signed at once that could not
    ///   commit; it is aborted first, no payment of it in force, its first take still prepared
    ///   to sign alone.
    /// - As [`Self::transfer`] for each transfer.
    pub async fn route_with(
        &self,
        request: &RouteRequest,
        via: Option<&Session>,
        sender: Option<&dyn TransactionSender>,
    ) -> Result<RouteOutcome> {
        let span = tracing::info_span!("lamarge.route", client_transfer_id = %request.id);
        let hub = via.unwrap_or(self);
        let run = Run { session: self, hub, request, sender };
        match Box::pin(run.run().instrument(span)).await {
            Ok(outcome) => Ok(outcome),
            Err(Halt::Pending(first, second_transfer, reason)) => {
                Ok(RouteOutcome::Pending { first: *first, second_transfer, reason })
            }
            Err(Halt::Failed(error)) => Err(error),
        }
    }
}

struct Run<'a> {
    session: &'a Session,
    hub: &'a Session,
    request: &'a RouteRequest,
    sender: Option<&'a dyn TransactionSender>,
}

/// What the first take delivered on `via`, when its last part was released, and whether it was
/// kept in the escrow's available balance there, as a grouped route's is.
struct Delivered {
    received: U256,
    released: Option<DateTime<Utc>>,
    kept: bool,
}

impl Run<'_> {
    #[expect(clippy::too_many_lines, reason = "the TypeScript route(), step by step")]
    async fn run(&self) -> Step<RouteOutcome> {
        let request = self.request;
        self.validate()?;
        let turn = self.session.record_key(&format!("route:{}", request.id));
        let _turn = self.session.journal().lock(&turn).await;
        if request.funding == Funding::Wallet {
            let venues = self.session.client().market().venues().await?;
            check_wallet_capability(&venues, &request.from, self.session.escrow_environment())?;
        }
        let mut first = self.post_first().await?;
        self.check_first(&first)?;
        // Signed at once where it can be: committed, it runs to its end with nothing left to sign.
        let held =
            first.grouped.as_ref().is_none_or(|route| route.state == GroupedRouteState::Prepared);
        if request.grouped
            && first.cancelled_at.is_none()
            && groupable(&request.from, &request.via)
            && held
        {
            self.sign_route(&first).await?;
            first = self.read(first.id).await?;
        }
        let grouped = first.grouped.clone().filter(|route| {
            matches!(route.state, GroupedRouteState::Committed | GroupedRouteState::Closed)
        });
        let mut fills = if let Some(route) = &grouped {
            let deadline = Instant::now() + request.arrival_timeout;
            while first
                .grouped
                .as_ref()
                .is_none_or(|route| route.state != GroupedRouteState::Closed)
            {
                if Instant::now() >= deadline {
                    let second = Some(route.second_transfer_id);
                    return Err(pending(first, second, PendingReason::ArrivalUnconfirmed));
                }
                tokio::time::sleep(request.poll_interval).await;
                first = self.read(first.id).await?;
            }
            self.fills(route.first_take_id).await?
        } else {
            if request.grouped {
                let aborted = first.cancelled_at.is_some()
                    || first
                        .grouped
                        .as_ref()
                        .is_some_and(|route| route.state == GroupedRouteState::Aborted);
                let reason = if aborted {
                    RouteNotCommitted::Aborted
                } else {
                    RouteNotCommitted::Unavailable
                };
                return Err(not_committed(&first, reason).into());
            }
            match Box::pin(self.first_transfer()).await? {
                TransferOutcome::Filled { intent, fills } => {
                    first = intent;
                    fills
                }
                TransferOutcome::Pending { intent, reason } => {
                    return Err(pending(intent, None, reason));
                }
            }
        };

        let take = fills.first().map(|fill| fill.take_id).or(first.take_id);
        let deadline = Instant::now() + request.arrival_timeout;
        let arrived = loop {
            if let Some(arrived) = delivered(&fills) {
                break arrived;
            }
            let Some(take) = take.filter(|_| Instant::now() < deadline) else {
                return Err(pending(first, None, PendingReason::ArrivalUnconfirmed));
            };
            tokio::time::sleep(request.poll_interval).await;
            fills = self.fills(take).await?;
        };
        // What the route's own second take paid, none of it a reservation released unsigned:
        // never continued itself, what it left goes on through a successor.
        let routed = match &grouped {
            Some(route) => self.read(route.second_transfer_id).await?.take_id,
            None => None,
        };
        let mut paid = match routed {
            Some(take) => self
                .fills(take)
                .await?
                .into_iter()
                .filter(|fill| !unsigned_reservation(fill))
                .collect(),
            None => Vec::new(),
        };
        let grouped_second = grouped.as_ref().map(|route| route.second_transfer_id);
        let settled = |first: TransferIntent, second_fills, second_transfer| {
            let left = first.continuable;
            RouteOutcome::Settled {
                first,
                first_fills: fills.clone(),
                second_fills,
                second_transfer,
                left,
            }
        };
        if arrived.received.is_zero() {
            return Ok(settled(first, paid, grouped_second));
        }

        // A continuation already opened is followed, never opened again: the one recorded, else
        // the route's latest, from the history.
        let latest_key = self.session.record_key(&format!("route-then:{}", request.id));
        let recorded = records::load::<Latest>(self.session.journal(), &latest_key).await?;
        let mut latest = match recorded {
            Some(latest) => Some(self.read(latest.transfer).await?),
            None => None,
        };
        if latest
            .as_ref()
            .is_none_or(|latest| latest.cancelled_at.is_some() || Some(latest.id) == grouped_second)
            && let Some(found) = self.last_continuation(&first).await?
        {
            latest = Some(found);
        }
        // A replacement of a take that expired unsigned goes on through the one it replaced.
        let second_id = continuation_id(&request.id)?;
        while let Some(current) = &latest {
            let Some(replaced) = current.client_transfer_id.as_str().strip_prefix("retry:") else {
                break;
            };
            if current.cancelled_at.is_some() || current.client_transfer_id == second_id {
                break;
            }
            let id: TransferId =
                replaced.parse().map_err(|_| differs(details::CLIENT_REQUEST_ID))?;
            latest = Some(self.read(id).await?);
        }
        if let Some(latest) = latest
            .as_ref()
            .filter(|latest| latest.cancelled_at.is_none() && Some(latest.id) != grouped_second)
        {
            let (second, more) = self.continuing(&first, latest, &arrived).await?;
            paid.extend(more);
            return Ok(settled(first, paid, Some(second)));
        }

        // What earlier continuations took or hold is not left; unknown while the venue between's
        // fee is, read until known.
        let left = loop {
            first = self.read(first.id).await?;
            if let Some(left) = first.continuable {
                break left;
            }
            if Instant::now() >= deadline {
                return Err(pending(first, grouped_second, PendingReason::ArrivalUnconfirmed));
            }
            tokio::time::sleep(request.poll_interval).await;
        };
        let step =
            deployments::chain(&request.via).map_or(U256::from(1u8), deployments::Chain::step);
        let amount = left.get() / step * step;
        // What is left never goes past what the first take delivered: more would pay from the
        // account's other funds.
        if amount > arrived.received {
            return Err(differs(details::AMOUNT).into());
        }
        // Less than a venue step, or a hundredth of the second take planned, is rounding: it
        // waits on `via`.
        let planned = first_then(&first)?.amount_in.get();
        let hundredfold = amount.checked_mul(U256::from(100u8));
        if amount.is_zero() || hundredfold.is_none_or(|value| value < planned) {
            return Ok(settled(first, paid, grouped_second));
        }
        let min_out = request.least_out(amount).ok_or(differs(details::AMOUNT_OUT))?;
        self.wait_for_funds(&first, amount, &arrived, deadline, grouped_second).await?;
        // A successor of the latest continuation goes under `renew:<id>`, with what is left now.
        let client_id = match &latest {
            Some(latest) => ClientId::new(format!("renew:{}", latest.id))
                .map_err(|_| differs(details::CLIENT_REQUEST_ID))?,
            None => second_id,
        };
        let destination = request.recipient.destination();
        let body = SecondInput {
            client_transfer_id: &client_id,
            from: &request.via,
            to: &request.to,
            amount_in: Amount::new(amount),
            min_out: Amount::new(min_out),
            max_parts: MaxParts::MAX,
            payer: self.hub.address(),
            recipient: request.recipient.address(),
            destination: (destination == Destination::Wallet).then_some(destination),
            previous_transfer_id: first.id,
            max_fee_bps: request.then.max_fee_bps,
        };
        let call = Call::new(&ops::POST_TRANSFER).json(&body)?;
        let second: TransferIntent = self.hub.transport().send(call, self.hub.auth()).await?;
        if second.client_transfer_id != client_id {
            return Err(differs(details::CLIENT_REQUEST_ID).into());
        }
        records::save(self.session.journal(), &latest_key, &Latest { transfer: second.id }, true)
            .await?;
        let (second, more) = self.continuing(&first, &second, &arrived).await?;
        paid.extend(more);
        Ok(settled(first, paid, Some(second)))
    }

    /// Refuses, before any request, a route through its own ends.
    fn validate(&self) -> Result<()> {
        let request = self.request;
        if request.via == request.from || request.via == request.to {
            return Err(Error::InvalidRequest("a route goes through a third venue".into()));
        }
        continuation_id(&request.id)?;
        take::validate(&request.first_terms(Address::ZERO))?;
        let then = TradeTerms::new(
            request.via.clone(),
            request.to.clone(),
            request.then.amount_in,
            request.then.min_out,
            request.recipient,
        );
        take::validate(&then)?;
        Ok(())
    }

    /// Creates the route's first transfer (idempotent by the request's id).
    async fn post_first(&self) -> Result<TransferIntent> {
        let request = self.request;
        let body = FirstInput {
            move_id: request.move_id,
            funding: (request.funding == Funding::Wallet).then_some(Funding::Wallet),
            client_transfer_id: &request.id,
            from: &request.from,
            to: &request.via,
            amount_in: request.amount_in,
            min_out: request.min_out,
            max_parts: MaxParts::MAX,
            payer: self.session.address(),
            recipient: self.hub.address(),
            max_fee_bps: request.max_fee_bps,
            conversion: request.conversion.map(|terms| ConversionBody {
                amount_in: terms.amount_in(),
                min_out: terms.min_out(),
            }),
            then: request.continues(),
        };
        let call = Call::new(&ops::POST_TRANSFER).json(&body)?;
        let intent: TransferIntent =
            self.session.transport().send(call, self.session.auth()).await?;
        if intent.client_transfer_id != request.id {
            return Err(differs(details::CLIENT_REQUEST_ID).into());
        }
        Ok(intent)
    }

    async fn read(&self, id: TransferId) -> Result<TransferIntent> {
        let intent = self.session.transfers().get(id).await?;
        if intent.id == id { Ok(intent) } else { Err(differs(details::CLIENT_REQUEST_ID).into()) }
    }

    async fn fills(&self, take: TakeId) -> Result<Vec<Fill>> {
        let call = Call::new(&ops::GET_TAKE).segment(take);
        let answer: TakeFills = self.session.transport().send(call, self.session.auth()).await?;
        Ok(answer.take.fills)
    }

    /// The first transfer is the one asked: into the via account, continuing to the route's end.
    fn check_first(&self, first: &TransferIntent) -> Result<()> {
        let request = self.request;
        let chain = |venue| deployments::chain(venue).map(|chain| chain.id);
        let continues = request.continues();
        let ok = first.from == request.from
            && first.to == request.via
            && Some(first.from_chain_id) == chain(&request.from)
            && Some(first.to_chain_id) == chain(&request.via)
            && first.amount_in == request.amount_in
            && first.min_out == request.min_out
            && first.max_fee_bps == request.max_fee_bps
            && first.from_dex.is_none()
            && first.payer == self.session.address()
            && first.recipient == self.hub.address()
            && first.destination == Destination::Venue
            && first.funding.clone().unwrap_or(Funding::Venue) == request.funding
            && first.then.as_ref().is_some_and(|then| {
                then.to == continues.to
                    && then.recipient == continues.recipient
                    && then.destination.clone().unwrap_or(Destination::Venue)
                        == request.recipient.destination()
                    && then.amount_in == continues.amount_in
                    && then.min_out == continues.min_out
                    && then.max_fee_bps == continues.max_fee_bps
            })
            && request.move_id.is_none_or(|id| first.move_id == Some(id));
        if ok { Ok(()) } else { Err(changed(details::FUNDING).into()) }
    }

    /// The first transfer, leg by leg, as any transfer: its take into the via account.
    async fn first_transfer(&self) -> Result<TransferOutcome> {
        let request = self.request;
        let mut transfer =
            TransferRequest::new(request.id.clone(), request.first_terms(self.hub.address()))
                .poll_interval(request.poll_interval)
                .arrival_timeout(request.arrival_timeout);
        if let Some(id) = request.move_id {
            transfer = transfer.move_id(id);
        }
        if let Some(terms) = request.conversion {
            transfer = transfer.conversion(terms);
        }
        transfer.then = Some(request.continues());
        match self.sender {
            Some(sender) => self.session.transfer_with(&transfer, sender).await,
            None => self.session.transfer(&transfer).await,
        }
    }

    /// Continues with `second`, checked against the route and what the first delivered, funded
    /// as LaMarge recorded it: a delivery kept in escrow pays from the available balance there,
    /// so nothing is withdrawn.
    async fn continuing(
        &self,
        first: &TransferIntent,
        second: &TransferIntent,
        arrived: &Delivered,
    ) -> Step<(TransferId, Vec<Fill>)> {
        let request = self.request;
        let floor = request.least_out(second.amount_in.get());
        let ok = second.previous_transfer_id == Some(first.id)
            && second.from == request.via
            && second.to == request.to
            && second.recipient == request.recipient.address()
            && second.destination == request.recipient.destination()
            && second.amount_in.get() <= arrived.received
            && floor.is_some_and(|floor| second.min_out.get() >= floor)
            && second.max_fee_bps == request.then.max_fee_bps;
        if !ok {
            return Err(changed(details::AMOUNT).into());
        }
        let latest_key = self.session.record_key(&format!("route-then:{}", request.id));
        records::save(self.session.journal(), &latest_key, &Latest { transfer: second.id }, true)
            .await?;
        // The TypeScript asks `wallet` for a kept delivery but follows the intent's own funding:
        // the continuation is posted without one, so it is the venue's.
        let funding = second.funding.clone().unwrap_or(Funding::Venue);
        let terms = TradeTerms::new(
            request.via.clone(),
            request.to.clone(),
            second.amount_in,
            second.min_out,
            request.recipient,
        )
        .funding(funding);
        let terms = match request.then.max_fee_bps {
            Some(bps) => terms.max_fee_bps(bps),
            None => terms,
        };
        let mut transfer = TransferRequest::new(second.client_transfer_id.clone(), terms)
            .poll_interval(request.poll_interval)
            .arrival_timeout(request.arrival_timeout);
        transfer.previous = Some(first.id);
        match Box::pin(self.hub.transfer(&transfer)).await? {
            TransferOutcome::Filled { intent, fills } => Ok((intent.id, fills)),
            TransferOutcome::Pending { intent, reason } => {
                Err(pending(first.clone(), Some(intent.id), reason))
            }
        }
    }

    /// The route's latest continuation, from the history.
    async fn last_continuation(&self, first: &TransferIntent) -> Result<Option<TransferIntent>> {
        let mut page = crate::api::PageRequest::first(100);
        loop {
            let found = self.session.transfers().history(&page).await?;
            if let Some(intent) =
                found.data.iter().find(|intent| intent.previous_transfer_id == Some(first.id))
            {
                return Ok(Some(intent.clone()));
            }
            let reached = found.data.iter().any(|intent| intent.id == first.id);
            match found.next_cursor {
                Some(cursor) if found.has_more && !reached => {
                    page = crate::api::PageRequest::first(100).after(cursor);
                }
                _ => return Ok(None),
            }
        }
    }

    /// The venue between credits the delivery after its release, the escrow a delivery kept
    /// there: the second transfer waits for a read that shows it.
    async fn wait_for_funds(
        &self,
        first: &TransferIntent,
        amount: U256,
        arrived: &Delivered,
        deadline: Instant,
        second: Option<TransferId>,
    ) -> Step<()> {
        let request = self.request;
        loop {
            let balances = self.hub.account().balances().await?;
            let row = balances
                .iter()
                .find(|row| row.venue == request.via)
                .filter(|row| row.address == self.hub.address())
                .ok_or(changed(details::LINKED_ACCOUNT))?;
            let now = self.hub.transport().now();
            let (held, read) = held(row, arrived.kept, now);
            let after = match (read, arrived.released) {
                (Some(read), Some(released)) => read > released,
                (Some(_), None) => true,
                _ => false,
            };
            if after && held.is_some_and(|held| held >= amount) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(pending(first.clone(), second, PendingReason::ArrivalUnconfirmed));
            }
            tokio::time::sleep(request.poll_interval).await;
        }
    }

    /// Prepares the route, checks and signs both takes, and commits them together. Any failure
    /// before the commit aborts the route; an uncertain commit is left to the next run.
    async fn sign_route(&self, first: &TransferIntent) -> Result<()> {
        let (session, request) = (self.session, self.request);
        let first_input = TakeInput {
            transfer_intent_id: Some(first.id),
            ..TakeInput::new(first.client_take_id.clone(), &request.first_terms(self.hub.address()))
        };
        if first.take_id.is_none() {
            let call = Call::new(&ops::POST_TAKE).json(&first_input)?;
            session.transport().send::<TakePreparation>(call, session.auth()).await?;
        }
        // Paid from Aster, the first take's request is issued again right before it is signed,
        // last: no wait to send it.
        let reissue = request.from == VenueId::Aster;
        let call = Call::new(&ops::PREPARE_GROUPED_ROUTE)
            .segment(first.id)
            .json(&PrepareBody { reissue })?;
        let preparation: GroupedPreparation =
            match session.transport().send(call, session.auth()).await {
                Ok(preparation) => preparation,
                Err(error) => {
                    return Err(match refusal(&error) {
                        Some(RouteNotCommitted::Expired) => {
                            not_committed(first, RouteNotCommitted::Expired)
                        }
                        Some(_) => not_committed(first, RouteNotCommitted::Unavailable),
                        None => error,
                    });
                }
            };
        let route = preparation.route.clone();
        match route.state {
            GroupedRouteState::Prepared => {}
            GroupedRouteState::Aborted => {
                self.forget(&route).await;
                return Err(not_committed_by(first.id, &route, RouteNotCommitted::Aborted));
            }
            // Committed by another run: followed.
            _ => return Ok(()),
        }
        let signed = match self.sign_both(first, preparation).await {
            Ok(signed) => signed,
            // An Aster request maybe issued again leaves the route to resume, its signatures kept.
            Err(Error::AsterRequestUncertain) => return Err(Error::AsterRequestUncertain),
            Err(error) => {
                self.abandon(first.id, &route).await;
                return Err(error);
            }
        };
        let signatures = match signed {
            Some(signatures) if session.transport().now() < route.commit_by => signatures,
            _ => {
                self.abandon(first.id, &route).await;
                return Err(not_committed_by(first.id, &route, RouteNotCommitted::Expired));
            }
        };
        let call = Call::new(&ops::COMMIT_GROUPED_ROUTE)
            .segment(first.id)
            .json(&Signatures { signatures: &signatures })?;
        match session.transport().send::<GroupedRoute>(call, session.auth()).await {
            Ok(committed)
                if matches!(
                    committed.state,
                    GroupedRouteState::Committed | GroupedRouteState::Closed
                ) =>
            {
                tracing::info!("route committed");
                self.forget(&route).await;
                Ok(())
            }
            Ok(_) => Err(differs(details::STATE).into()),
            Err(error) => {
                // A confirmed refusal ends this attempt; an uncertain outcome is recovered on the
                // next run.
                if let Some(reason) = refusal(&error) {
                    self.abandon(first.id, &route).await;
                    let reason = if reason == RouteNotCommitted::Expired {
                        reason
                    } else {
                        RouteNotCommitted::Aborted
                    };
                    return Err(not_committed_by(first.id, &route, reason));
                }
                if error.status() == Some(400) {
                    self.abandon(first.id, &route).await;
                }
                Err(error)
            }
        }
    }

    /// Both takes checked and signed, the shortest-lived request last: an Aster request LaMarge
    /// issues again right before it is signed, else the first take's. `None` when `commit_by` is
    /// too close to ask for another signature.
    async fn sign_both(
        &self,
        first: &TransferIntent,
        preparation: GroupedPreparation,
    ) -> Result<Option<Vec<SignedItem>>> {
        let request = self.request;
        let GroupedPreparation { route, first: first_take, second: second_take } = preparation;
        let second = self.read(route.second_transfer_id).await?;
        let ids = first_take.take.id == route.first_take_id
            && first.take_id.is_none_or(|id| id == route.first_take_id)
            && Some(second_take.take.id) == route.second_take_id
            && second.take_id == route.second_take_id;
        if !ids {
            return Err(differs(details::FILLS).into());
        }
        let delivers = first_take
            .take
            .fills
            .iter()
            .try_fold(U256::ZERO, |sum, fill| sum.checked_add(fill.amount_out.get()))
            .ok_or(differs(details::AMOUNT_OUT))?;
        check_route_intent(first, &second, self.hub.address(), delivers)?;
        let balances = self.session.account().balances().await?;
        let first_intent =
            take::intent(self.session, &request.first_terms(self.hub.address()), &balances)?;
        let then = TradeTerms::new(
            second.from.clone(),
            second.to.clone(),
            second.amount_in,
            second.min_out,
            request.recipient,
        );
        let then = match second.max_fee_bps {
            Some(bps) => then.max_fee_bps(bps),
            None => then,
        };
        let hub_balances = if std::ptr::eq(self.hub, self.session) {
            balances
        } else {
            self.hub.account().balances().await?
        };
        let mut second_intent = take::intent(self.hub, &then, &hub_balances)?;
        second_intent.route = true;

        let left = || {
            (route.commit_by - self.session.transport().now()).num_seconds()
                > REQUEST_MARGIN_SECONDS
        };
        let order = if first_take.aster_reissue { [false, true] } else { [true, false] };
        let (mut first_items, mut second_items) = (None, None);
        for first_step in order {
            if !left() {
                return Ok(None);
            }
            if first_step {
                first_items =
                    Some(take::sign_only(self.session, first_take.clone(), &first_intent).await?);
            } else {
                second_items =
                    Some(take::sign_only(self.hub, second_take.clone(), &second_intent).await?);
            }
        }
        let (Some(mut items), Some(second_items)) = (first_items, second_items) else {
            return Ok(None);
        };
        items.extend(second_items);
        Ok(Some(items))
    }

    /// Aborts a route not committed, best effort; its signatures go once the abort is confirmed.
    async fn abandon(&self, first: TransferId, route: &GroupedRoute) {
        // Released even when the run stops: the second reservation must not wait for its expiry.
        let call = Call::new(&ops::ABORT_GROUPED_ROUTE).segment(first);
        match self
            .session
            .transport()
            .send::<serde::de::IgnoredAny>(call, self.session.auth())
            .await
        {
            Ok(_) => self.forget(route).await,
            Err(error) => tracing::warn!(%error, "route not aborted; its signatures are kept"),
        }
    }

    /// Forgets both takes' saved signatures, best effort.
    async fn forget(&self, route: &GroupedRoute) {
        if let Err(error) = take::forget(self.session, route.first_take_id).await {
            tracing::warn!(%error, "a route's signature not forgotten");
        }
        if let Some(id) = route.second_take_id
            && let Err(error) = take::forget(self.hub, id).await
        {
            tracing::warn!(%error, "a route's signature not forgotten");
        }
    }
}

/// The route's second transfer, as LaMarge made it, is the route's; `delivers` is what the
/// first take delivers on `via`.
fn check_route_intent(
    first: &TransferIntent,
    second: &TransferIntent,
    hub: Address,
    delivers: U256,
) -> Result<()> {
    let then = first_then(first)?;
    let same = second.previous_transfer_id == Some(first.id)
        && second.from == first.to
        && second.to == then.to
        && second.payer == hub
        && second.recipient == then.recipient
        && second.destination == then.destination.clone().unwrap_or(Destination::Venue)
        && second.max_fee_bps == then.max_fee_bps
        && second.withdrawal_preparation.is_none();
    if !same {
        return Err(changed(details::DESTINATION).into());
    }
    let (pays, planned) = (second.amount_in.get(), then.amount_in.get());
    let share = then.min_out.get().checked_mul(pays).map(|scaled| scaled / planned);
    let ok = !pays.is_zero()
        && pays <= delivers
        && pays.checked_mul(U256::from(100u8)).is_some_and(|hundred| hundred >= planned)
        && share.is_some_and(|share| second.min_out.get() >= share);
    if ok { Ok(()) } else { Err(changed(details::AMOUNT).into()) }
}

fn first_then(first: &TransferIntent) -> Result<&crate::model::TransferThen> {
    first.then.as_ref().ok_or_else(|| changed(details::DESTINATION).into())
}

/// `id.2`: the route's own continuation.
fn continuation_id(id: &ClientId) -> Result<ClientId> {
    ClientId::new(format!("{id}.2"))
        .map_err(|_| Error::InvalidRequest("the route's id leaves no room for `.2`".into()))
}

/// What the first take delivered, once its last part was released or none can deliver any more.
fn delivered(fills: &[Fill]) -> Option<Delivered> {
    let done = |fill: &Fill| {
        fill.maker_leg == MakerLeg::Claimed
            || matches!(
                fill.state,
                FillState::Settled
                    | FillState::Expired
                    | FillState::Refunded
                    | FillState::TakerLoss
                    | FillState::MakerLoss
            )
    };
    if !fills.iter().all(done) {
        return None;
    }
    let claimed: Vec<&Fill> =
        fills.iter().filter(|fill| fill.maker_leg == MakerLeg::Claimed).collect();
    let received =
        claimed.iter().try_fold(U256::ZERO, |sum, fill| sum.checked_add(fill.amount_out.get()))?;
    let released = claimed.iter().filter_map(|fill| fill.maker_claimed_at).max();
    let kept = !claimed.is_empty()
        && claimed.iter().all(|fill| fill.maker_destination == Some(MakerDestination::Escrow));
    Some(Delivered { received, released, kept })
}

/// What `row` holds for a continuation, and when that was read: the escrow balance for a
/// delivery kept there, else the withdrawable margin net of its fee, plus the escrow balance on
/// an escrow venue.
fn held(row: &Balance, kept: bool, now: DateTime<Utc>) -> (Option<U256>, Option<DateTime<Utc>>) {
    if kept {
        return (prefunded(row, now), row.chain_read_at);
    }
    let fresh = row.read_at.is_some_and(|at| (now - at).num_seconds() < FRESH_SECONDS);
    let margin = row.withdrawable.filter(|_| fresh).and_then(|withdrawable| {
        withdrawable.get().checked_sub(row.withdrawal_fee.unwrap_or(Amount::ZERO).get())
    });
    let escrow = if deployments::is_venue_settled(&row.venue) {
        U256::ZERO
    } else {
        prefunded(row, now).unwrap_or(U256::ZERO)
    };
    (margin.and_then(|margin| margin.checked_add(escrow)), row.read_at)
}

/// Whether `error` confirms no commit was accepted, and why.
fn refusal(error: &Error) -> Option<RouteNotCommitted> {
    let api = error.api()?;
    if error.code() == Some(&ErrorCode::TakeExpired) {
        return Some(RouteNotCommitted::Expired);
    }
    let unavailable = api.reason() == Some(&Reason::GroupedUnavailable)
        || (api.status() == 404 && error.code() == Some(&ErrorCode::NotFound));
    unavailable.then_some(RouteNotCommitted::Unavailable)
}

fn not_committed(first: &TransferIntent, reason: RouteNotCommitted) -> Error {
    let commit_by = first.grouped.as_ref().map(|route| route.commit_by);
    Error::RouteNotCommitted { transfer: first.id, reason, commit_by }
}

fn not_committed_by(first: TransferId, route: &GroupedRoute, reason: RouteNotCommitted) -> Error {
    Error::RouteNotCommitted { transfer: first, reason, commit_by: Some(route.commit_by) }
}

#[cfg(test)]
mod tests;
