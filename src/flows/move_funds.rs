//! Moves run to their end: LaMarge's makers first, as far as they fill at the move's floor rate,
//! then an external bridge (Relay) for the rest, every part attached to one move and counted
//! against its `amount_in` in the journal, so that no fund is left between two parts.

use std::time::Duration;

use alloy_primitives::{Address, U256};
use serde::{Deserialize, Serialize};
use tokio::time::Instant;
use tracing::Instrument as _;

use self::{
    bridging::{BridgePart, Placed},
    budget::{Amend, Part, Source},
    direct::Withdrawn,
};
use super::{
    PendingReason, Recipient, RouteLeg, RouteOutcome, RouteRequest, TradeTerms, TransferOutcome,
    TransferRequest,
    conversion::ConversionTerms,
    records,
    transfer::{TransferOut, transfer_out_key, unsigned_reservation},
};
use crate::{
    Error, Result, SendStatus, Session, TransactionSender,
    api::{DirectRequest, MoveQuery, MoveRecipient, MoveRequest},
    deployments,
    model::{
        BridgeState, BridgeUnavailable, ConversionState, DirectState, FillState, Funding, Move,
        MoveEndpoint, MoveVenue, Route, TransferIntent, WithdrawalState,
    },
    protocol::{
        checks::details::{self, changed, differs, unsupported},
        fees, relay,
    },
    types::{Amount, ClientId, MoveId, TransferId, VenueId},
};

mod bridging;
mod budget;
mod direct;

/// How many bridges a leg tries in turn, each after one that ended with nothing debited.
const BRIDGE_ATTEMPTS: u32 = 4;

/// A move to run with [`Session::move_funds`]: its terms as created, and its recipient.
///
/// Persist the whole request (it is serializable) before the first call, then run that same
/// request again to resume: its plan, its parts and their signatures are journalled under the
/// move's `client_move_id`, and the same id with other terms is refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoveFundsRequest {
    terms: MoveRequest,
    recipient: MoveRecipient,
    poll_interval: Duration,
    arrival_timeout: Duration,
}

impl MoveFundsRequest {
    /// The move `terms` to `recipient`, the one `terms` was made with, reading every 2 s while
    /// funds, payments or approvals arrive, for up to 10 minutes each.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidRequest`] when `recipient` is not the move's.
    pub fn new(terms: MoveRequest, recipient: MoveRecipient) -> Result<Self> {
        if recipient.wire() != terms.recipient {
            return Err(Error::InvalidRequest("recipient is not the move's".into()));
        }
        Ok(Self {
            terms,
            recipient,
            poll_interval: Duration::from_secs(2),
            arrival_timeout: Duration::from_secs(600),
        })
    }

    /// Between reads while something arrives.
    #[must_use]
    pub const fn poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    /// How long to wait for each arrival before [`MoveOutcome::Pending`].
    #[must_use]
    pub const fn arrival_timeout(mut self, timeout: Duration) -> Self {
        self.arrival_timeout = timeout;
        self
    }

    /// The move's terms.
    pub const fn terms(&self) -> &MoveRequest {
        &self.terms
    }
}

/// The plan a move follows: its first quote, kept in the journal and read back by every later
/// run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct MovePlan {
    /// LaMarge's part, if its makers fill any.
    pub route: Option<Route>,
    /// What LaMarge's part takes from the origin, in its token's base units.
    pub budget: Amount,
    /// What neither part carries for a lasting reason, in the origin token's base units.
    pub unallocated: Amount,
    /// Why.
    pub reason: Option<String>,
    /// Why no bridge leaves the origin at all, whatever the amount.
    pub blocked: Option<BridgeUnavailable>,
}

/// How a move run ended.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum MoveOutcome {
    /// Every part placed and the move closed with what none carries: its parts settle on their
    /// own (`settling`), or did (`final`).
    Closed(Box<Move>),
    /// Nothing failed; run the same request again later.
    Pending {
        /// The move.
        move_id: MoveId,
        /// What it waits for.
        reason: MoveWait,
    },
}

/// What a pending move waits for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum MoveWait {
    /// LaMarge's payment from a wallet has not executed: a bridge from that wallet waits for it.
    Payment,
    /// The funds have not reached the payer's wallet, or are short of a bridge's deposit.
    Withdrawal,
    /// A bridge awaits its signatures' acceptance, its deposit's outcome, an approval or its
    /// expiry, or the provider is unavailable.
    Bridge,
    /// The route's second take cannot meet the move's floor yet.
    Route,
    /// The sender sent nothing (the owner declined, a signature timed out, gas was missing). A
    /// refused bridge deposit is asked again through a new bridge on the next run; a GMX
    /// withdrawal, which has no nonce, is never asked again: it is released once it expires.
    NotSent,
    /// LaMarge's transfer waits.
    Transfer(PendingReason),
}

/// An endpoint equal to another: the venue, and a wallet's chain and token.
fn same_endpoint(a: &MoveEndpoint, b: &MoveEndpoint) -> bool {
    a.venue == b.venue
        && (a.venue != MoveVenue::Wallet || (a.chain_id == b.chain_id && a.token == b.token))
}

/// An address, compared in any case, or a Lighter account index, compared exactly.
fn same_recipient(value: &str, recipient: &str) -> bool {
    if recipient.bytes().all(|byte| byte.is_ascii_digit()) {
        return value == recipient;
    }
    matches!((value.parse::<Address>(), recipient.parse::<Address>()), (Ok(a), Ok(b)) if a == b)
}

/// Whether an unpaid part's transfer is cancelled before its funds are counted: neither taken,
/// cancelled nor funded by a withdrawal, nor converting. LaMarge keeps a transfer whose
/// conversion is prepared or sent until the conversion resolves (it expires at its deadline).
fn dismissable(intent: &TransferIntent) -> bool {
    let converting = intent.conversion.as_ref().is_some_and(|conversion| {
        matches!(
            conversion.state.as_ref().unwrap_or(&ConversionState::Prepared),
            ConversionState::Prepared | ConversionState::Pending
        )
    });
    intent.take_id.is_none()
        && intent.cancelled_at.is_none()
        && intent.withdrawal.is_none()
        && !converting
}

/// The LaMarge venue an endpoint maps to: an account's own, or the escrow venue on a wallet's
/// chain (none on BSC).
fn move_venue(endpoint: &MoveEndpoint) -> Option<VenueId> {
    if endpoint.venue == MoveVenue::Wallet {
        return [VenueId::Hyperliquid, VenueId::Polymarket, VenueId::Gmx].into_iter().find(
            |venue| {
                deployments::chain(venue).is_some_and(|chain| Some(chain.id) == endpoint.chain_id)
            },
        );
    }
    let Ok(venue) = endpoint.venue.as_str().parse::<VenueId>();
    deployments::chain(&venue).is_some().then_some(venue)
}

/// The least a take to a wallet on `to` delivers so that its recipient receives `least`:
/// Polygon's swap bound inverted, rounded up.
fn delivery_floor(to: &VenueId, least: U256) -> Result<U256> {
    let out = if *to == VenueId::Polymarket {
        let bps = U256::from(deployments::WALLET_SWAP_MIN_BPS);
        least.checked_mul(U256::from(10_000u16)).ok_or(differs(details::AMOUNT))?.div_ceil(bps)
    } else {
        least
    };
    if fees::minimum_received(to, out).is_none_or(|received| received < least) {
        return Err(differs(details::AMOUNT).into());
    }
    Ok(out)
}

fn client_id(text: String) -> Result<ClientId> {
    ClientId::new(text).map_err(|_| differs(details::CLIENT_REQUEST_ID).into())
}

impl Session {
    /// The plan `request` follows: its first quote ([`Moves::quote`](crate::api::Moves::quote)),
    /// journalled under the move's id and read back afterwards. Concurrent first reads share one
    /// quote. A caller choosing signers from LaMarge's route reads it the same way beforehand.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidRequest`] when the move's id was planned with other terms;
    /// `Verification(Differs)` when the quote does not add up to the move's `amount_in`.
    pub async fn move_plan(&self, request: &MoveFundsRequest) -> Result<MovePlan> {
        let terms = &request.terms;
        let key = self.record_key(&format!("move:{}:plan", terms.client_move_id));
        let _turn = self.journal().lock(&key).await;
        let pinned = self.record_key(&format!("move:{}:request", terms.client_move_id));
        match records::load::<MoveRequest>(self.journal(), &pinned).await? {
            Some(saved) if saved != *terms => {
                let message = format!("move {} names another move", terms.client_move_id);
                return Err(Error::InvalidRequest(message.into()));
            }
            Some(_) => {}
            None => records::save(self.journal(), &pinned, terms, false).await?,
        }
        if let Some(plan) = records::load::<MovePlan>(self.journal(), &key).await? {
            return Ok(plan);
        }
        let plan = self.plan_for(request).await?;
        records::save(self.journal(), &key, &plan, false).await?;
        Ok(plan)
    }

    /// The move's plan from its first quote: LaMarge's part, and what neither part carries for
    /// a lasting reason. A passing bridge refusal keeps the rest for the bridge, tried once
    /// LaMarge's part ran.
    async fn plan_for(&self, request: &MoveFundsRequest) -> Result<MovePlan> {
        let terms = &request.terms;
        let mut query = MoveQuery::new(
            terms.from.clone(),
            terms.to.clone(),
            terms.amount_in,
            terms.min_out,
            request.recipient,
        )
        .payer(terms.payer);
        if let Some(bps) = terms.max_fee_bps {
            query = query.max_fee_bps(bps);
        }
        let quote = self.moves().quote(&query).await?;
        let budget = quote.lamarge.as_ref().map_or(Amount::ZERO, |part| part.budget);
        let bridged = quote.bridge.as_ref().map_or(U256::ZERO, |bridge| bridge.amount_in.get());
        let total = budget
            .get()
            .checked_add(bridged)
            .and_then(|sum| sum.checked_add(quote.unallocated.get()));
        if total != Some(terms.amount_in.get()) {
            return Err(differs(details::MOVE_PLAN).into());
        }
        let refusal = quote.bridge_unavailable;
        let refused = refusal.as_ref().map(|reason| reason.as_str().to_owned());
        let passing = refusal.as_ref().is_some_and(|reason| !bridging::is_lasting(reason));
        let blocked = refusal.filter(|reason| {
            matches!(
                reason,
                BridgeUnavailable::HyperliquidMode
                    | BridgeUnavailable::Unsupported
                    | BridgeUnavailable::Disabled
            )
        });
        let reason = (!passing).then(|| quote.unallocated_reason.or(refused));
        Ok(MovePlan {
            route: quote.lamarge.map(|part| part.route),
            budget,
            unallocated: if passing { Amount::ZERO } else { quote.unallocated },
            reason: reason.flatten(),
            blocked,
        })
    }

    /// [`Self::move_funds_with`] without a transaction sender: a move whose parts send nothing
    /// from a wallet (a `HyperCore` bridge, LaMarge's part funded from a venue).
    pub async fn move_funds(&self, request: &MoveFundsRequest) -> Result<MoveOutcome> {
        self.move_funds_with(request, None).await
    }

    /// Moves `amount_in` from the move's origin to its destination: LaMarge's makers first, as far
    /// as they fill at the floor rate, then an external bridge for the rest, every part attached
    /// to one move ([`Moves::create`](crate::api::Moves::create)) and counted against its
    /// `amount_in` in the journal. The bridge starts once LaMarge's part ran, and once its
    /// payment executed when the bridge leaves a wallet. A Polymarket origin first withdraws the
    /// rest to the payer's wallet there, a GMX origin by the payer's own `transferOut` through
    /// `sender`. A LaMarge part that ends unpaid frees its share: what
    /// its funding withdrawal moved is bridged from the wallet, nothing more leaves the origin.
    /// What no part carries is declared when the move is closed, with its reason.
    ///
    /// `sender` sends the transactions of a wallet: a wallet bridge's `approve` and deposit, a
    /// Polygon conversion funding LaMarge's part, GMX's `transferOut`.
    ///
    /// Running the same request again resumes: the plan is the first one, LaMarge's part is
    /// followed and never paid twice, a bridge is found again by its id, its signatures
    /// submitted again and never signed anew, and a deposit of unknown outcome never asked
    /// again.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidRequest`] before any request when the move's payer is not the session's
    ///   address.
    /// - `Verification(Changed)` naming the term when the move, a part or a bridge is answered
    ///   with other terms than the request's.
    /// - As [`Self::transfer`] and [`Self::route`] for LaMarge's part.
    ///
    /// Waits that outlast the request's arrival timeout end in [`MoveOutcome::Pending`].
    pub async fn move_funds_with(
        &self,
        request: &MoveFundsRequest,
        sender: Option<&dyn TransactionSender>,
    ) -> Result<MoveOutcome> {
        let id = &request.terms.client_move_id;
        let span = tracing::info_span!("lamarge.move", client_move_id = %id);
        let run = MoveRun { session: self, request, sender };
        Box::pin(run.run().instrument(span)).await
    }
}

struct MoveRun<'a> {
    session: &'a Session,
    request: &'a MoveFundsRequest,
    sender: Option<&'a dyn TransactionSender>,
}

/// One bridge to place: its id, amount, and whether it leaves the payer's wallet.
struct Leg {
    id: ClientId,
    amount: U256,
    from_wallet: bool,
}

/// What LaMarge's part left in the payer's wallet once it ended unpaid: its share of the move is
/// then that alone, and it never runs again.
#[derive(Debug, Serialize, Deserialize)]
struct Freed {
    amount: Amount,
}

/// The transfer flow's pointer to the transfer it runs under a client id.
#[derive(Deserialize)]
struct Pointer {
    intent: TransferId,
}

/// What no part carries, and the first reason why.
struct Unplaced {
    amount: U256,
    reason: Option<String>,
}

impl Unplaced {
    fn add(&mut self, amount: U256, why: &str) {
        self.amount += amount;
        self.reason.get_or_insert_with(|| why.to_owned());
    }
}

/// Where one pass of a withdrawal to the payer's wallet left it.
enum Moved {
    Credited,
    Refused,
    /// Not final: a wait to end the run on, or none to read again.
    Waiting(Option<MoveWait>),
}

/// Why LaMarge's part stopped short of an outcome.
enum Stop {
    Wait(MoveWait),
    Failed(Error),
}

impl MoveRun<'_> {
    fn terms(&self) -> &MoveRequest {
        &self.request.terms
    }

    fn key(&self, record: &str) -> String {
        self.session.record_key(&format!("move:{}:{record}", self.terms().client_move_id))
    }

    fn pending(id: MoveId, reason: MoveWait) -> MoveOutcome {
        MoveOutcome::Pending { move_id: id, reason }
    }

    async fn run(&self) -> Result<MoveOutcome> {
        let (session, terms) = (self.session, self.terms());
        if terms.payer != session.address() {
            return Err(Error::InvalidRequest("move payer differs from the session".into()));
        }
        let journal = session.journal();
        let _turn = journal.lock(&self.key("turn")).await;
        let plan = session.move_plan(self.request).await?;
        let mut current = self.check(session.moves().create(terms).await?)?;
        let move_id = current.id;
        // No LaMarge venue on the origin chain (BSC): the move is the bridge's alone.
        let from = move_venue(&terms.from);
        // The payer's wallet on the origin chain, which a Polymarket withdrawal or an unpaid
        // LaMarge part's funding reached.
        let wallet =
            from.as_ref().and_then(deployments::chain).map(|chain| (chain.id, chain.token));

        let freed_key = self.key("freed");
        let mut freed =
            records::load::<Freed>(journal, &freed_key).await?.map(|freed| freed.amount);
        if let (Some(route), None) = (&plan.route, freed) {
            if current.closed_at.is_none() {
                let part = Part::new("lamarge", plan.budget, Source::Origin);
                budget::reserve(session, terms, part).await?;
            }
            match self.lamarge(&plan, route, &current).await {
                Ok(()) => {}
                Err(Stop::Wait(reason)) => return Ok(Self::pending(move_id, reason)),
                Err(Stop::Failed(error)) => {
                    // An unpaid part keeps only what its funding withdrawal took from the
                    // origin, now in the wallet.
                    let withdrawn = match self.release(move_id, error).await {
                        Ok(withdrawn) => withdrawn,
                        Err(Error::QuoteUnavailable(_)) => {
                            return Ok(Self::pending(move_id, MoveWait::Route));
                        }
                        Err(error) => return Err(error),
                    };
                    budget::amend(session, terms, "lamarge", Amend::Freed(withdrawn)).await?;
                    records::save(journal, &freed_key, &Freed { amount: withdrawn }, false).await?;
                    freed = Some(withdrawn);
                }
            }
        }

        if current.closed_at.is_none() {
            let amount_in = terms.amount_in.get();
            let mut unplaced =
                Unplaced { amount: plan.unallocated.get(), reason: plan.reason.clone() };
            let unallocated = unplaced.amount;
            let lamarge = match (&plan.route, freed) {
                (None, _) => U256::ZERO,
                (Some(_), Some(withdrawn)) => withdrawn.get(),
                (Some(_), None) => plan.budget.get(),
            };
            let external = amount_in
                .checked_sub(unallocated)
                .and_then(|rest| rest.checked_sub(lamarge))
                .ok_or(differs(details::MOVE_PLAN))?;
            let recovered = freed.map_or(U256::ZERO, Amount::get);
            let leg = |suffix: &str, amount: U256, from_wallet: bool| -> Result<Leg> {
                Ok(Leg { id: client_id(format!("{move_id}.{suffix}"))?, amount, from_wallet })
            };
            let origin = &terms.from.venue;
            let mut legs = Vec::new();
            if matches!(origin, MoveVenue::Lighter | MoveVenue::Aster) {
                unplaced.add(external, BridgeUnavailable::Unsupported.as_str());
            } else if let Some(blocked) = &plan.blocked {
                // No bridge leaves this origin: a freed LaMarge share stays unallocated; only
                // what reached the wallet is bridged.
                unplaced.add(external, blocked.as_str());
                legs.push(leg("r", recovered, true)?);
            } else if matches!(origin, MoveVenue::Polymarket | MoveVenue::Gmx) {
                let moved = if external.is_zero() {
                    U256::ZERO
                } else {
                    match self.to_wallet(move_id, external).await? {
                        Ok(moved) => moved,
                        Err(wait) => return Ok(Self::pending(move_id, wait)),
                    }
                };
                if moved < external {
                    unplaced.add(external, "withdrawal_refused");
                }
                legs.push(leg("b", moved + recovered, true)?);
            } else {
                legs.push(leg("b", external, false)?);
                legs.push(leg("r", recovered, true)?);
            }
            let mut prepared = false;
            for leg in legs.iter().filter(|leg| !leg.amount.is_zero()) {
                match self.place(move_id, leg, wallet).await? {
                    Placed::Refused(why) => unplaced.add(leg.amount, &why),
                    Placed::Bridge(bridge) => prepared |= bridge.state == BridgeState::Prepared,
                    Placed::Waiting(wait) => return Ok(Self::pending(move_id, wait)),
                }
            }
            if prepared {
                return Ok(Self::pending(move_id, MoveWait::Bridge));
            }
            let why = unplaced.reason.filter(|_| !unplaced.amount.is_zero());
            let unallocated = Amount::new(unplaced.amount);
            let closed = session.moves().close(move_id, unallocated, why.as_deref()).await?;
            current = self.check(closed)?;
        }
        let read = self.check(session.moves().get(current.id).await?)?;
        Ok(MoveOutcome::Closed(Box::new(read)))
    }

    /// The move is the request's: its terms as created.
    fn check(&self, answer: Move) -> Result<Move> {
        let terms = self.terms();
        let ok = answer.client_move_id == terms.client_move_id
            && same_endpoint(&answer.from, &terms.from)
            && same_endpoint(&answer.to, &terms.to)
            && answer.amount_in == terms.amount_in
            && answer.min_out == terms.min_out
            && answer.payer == terms.payer
            && same_recipient(&answer.recipient, &terms.recipient)
            && answer.max_fee_bps == terms.max_fee_bps;
        if ok { Ok(answer) } else { Err(changed(details::MOVE).into()) }
    }

    /// LaMarge's part, through one take or a route through a venue between, its root attached to
    /// the move; with a wallet origin, run until its payment pulled the funds from there.
    #[expect(clippy::too_many_lines, reason = "the TypeScript runLamarge(), step by step")]
    async fn lamarge(
        &self,
        plan: &MovePlan,
        route: &Route,
        opened: &Move,
    ) -> std::result::Result<(), Stop> {
        let move_id = opened.id;
        let failed = |error: Error| Stop::Failed(error);
        let (session, terms) = (self.session, self.terms());
        let from =
            move_venue(&terms.from).ok_or_else(|| failed(differs(details::MOVE_PLAN).into()))?;
        let to = move_venue(&terms.to).ok_or_else(|| {
            failed(Error::InvalidRequest("no LaMarge venue on the destination chain".into()))
        })?;
        let native_usdc = deployments::POLYMARKET.wallet_token;
        let conversion = (terms.from.venue == MoveVenue::Wallet
            && terms.from.token.is_some()
            && terms.from.token == native_usdc)
            .then(|| ConversionTerms::new(plan.budget, route.amount_in));
        let wallet_destination = terms.to.venue == MoveVenue::Wallet;
        let wallet_token = deployments::chain(&to).and_then(|chain| chain.wallet_token);
        let ok = route.from == from
            && route.to == to
            && (conversion.is_some() || route.amount_in == plan.budget)
            && (!wallet_destination
                || (terms.to.token.is_some() && terms.to.token == wallet_token));
        if !ok {
            return Err(failed(differs(details::MOVE_PLAN).into()));
        }
        // Delivered to a Polygon wallet, the take's pUSD floor pays the part's native-USDC floor
        // after the escrow's swap bound.
        let part = relay::part_floor(plan.budget.get(), terms.min_out.get(), terms.amount_in.get())
            .ok_or_else(|| failed(differs(details::AMOUNT).into()))?;
        let least = if wallet_destination { delivery_floor(&to, part) } else { Ok(part) }
            .map_err(failed)?;
        let recipient = match self.request.recipient {
            MoveRecipient::LighterAccount { owner, .. } => Recipient::Account(owner),
            MoveRecipient::Address(address) if wallet_destination => Recipient::Wallet(address),
            MoveRecipient::Address(address) => Recipient::Account(address),
        };
        let funding =
            if terms.from.venue == MoveVenue::Wallet { Funding::Wallet } else { Funding::Venue };
        let id = client_id(format!("{move_id}.t")).map_err(failed)?;
        let (poll, arrival) = (self.request.poll_interval, self.request.arrival_timeout);
        let paid_by = match route.legs.as_slice() {
            [_] => {
                let mut trade =
                    TradeTerms::new(from, to, route.amount_in, Amount::new(least), recipient)
                        .funding(funding);
                if let Some(bps) = terms.max_fee_bps {
                    trade = trade.max_fee_bps(bps);
                }
                let mut transfer = TransferRequest::new(id, trade)
                    .move_id(move_id)
                    .poll_interval(poll)
                    .arrival_timeout(arrival);
                if let Some(conversion) = conversion {
                    transfer = transfer.conversion(conversion);
                }
                let outcome = match self.sender {
                    Some(sender) => session.transfer_with(&transfer, sender).await,
                    None => session.transfer(&transfer).await,
                };
                match outcome.map_err(failed)? {
                    TransferOutcome::Filled { intent, .. } => intent.id,
                    TransferOutcome::Pending { reason, .. } => {
                        return Err(Stop::Wait(MoveWait::Transfer(reason)));
                    }
                }
            }
            [first, second] => {
                // The first take's floor is its share of the part's, as the route's expected
                // output divides it; the second keeps the part's whole floor whatever the first
                // delivered.
                let first_least =
                    relay::part_floor(first.amount_out.get(), least, route.amount_out.get())
                        .ok_or_else(|| failed(differs(details::AMOUNT).into()))?;
                let mut then = RouteLeg::new(second.amount_in, Amount::new(least));
                if let Some(bps) = terms.max_fee_bps {
                    then = then.max_fee_bps(bps);
                }
                let mut ask = RouteRequest::new(
                    id,
                    (from, first.to.clone(), to),
                    route.amount_in,
                    Amount::new(first_least),
                    then,
                    recipient,
                )
                .funding(funding)
                .move_id(move_id)
                .keep_floor(true)
                .poll_interval(poll)
                .arrival_timeout(arrival);
                if let Some(bps) = terms.max_fee_bps {
                    ask = ask.max_fee_bps(bps);
                }
                if let Some(conversion) = conversion {
                    ask = ask.conversion(conversion);
                }
                match session.route_with(&ask, None, self.sender).await {
                    Ok(RouteOutcome::Settled { first, .. }) => first.id,
                    Ok(RouteOutcome::Pending { reason, .. }) => {
                        return Err(Stop::Wait(MoveWait::Transfer(reason)));
                    }
                    // Through `release` first: a first take no book pays frees the share; a second
                    // that cannot meet the floor yet keeps the move waiting.
                    Err(error) => return Err(failed(error)),
                }
            }
            _ => return Err(failed(differs(details::MOVE_PLAN).into())),
        };
        if terms.from.venue == MoveVenue::Hyperliquid || opened.closed_at.is_some() {
            return Ok(());
        }
        // A bridge from a wallet waits until LaMarge's payment has pulled its funds from there.
        let deadline = Instant::now() + arrival;
        loop {
            let intent = session.transfers().get(paid_by).await.map_err(failed)?;
            let paying = match intent.take_id {
                Some(take) => session.takes().get(take).await.map_err(failed)?.take.fills,
                None => Vec::new(),
            };
            if !paying
                .iter()
                .any(|fill| matches!(fill.state, FillState::Prepared | FillState::Reserved))
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(Stop::Wait(MoveWait::Payment));
            }
            tokio::time::sleep(self.request.poll_interval).await;
        }
    }

    /// What LaMarge's part moved to the payer's wallet, once every one of its transfers ended
    /// unpaid; otherwise `error` again. A transfer not yet funded nor taken is cancelled first.
    async fn release(&self, move_id: MoveId, error: Error) -> Result<Amount> {
        let session = self.session;
        let mut ids: Vec<TransferId> = session
            .moves()
            .get(move_id)
            .await?
            .transfers
            .iter()
            .map(|part| part.id.into())
            .collect();
        // The transfer the flow journalled, also before the move lists it.
        let pointer = session.record_key(&format!("transfer:{move_id}.t"));
        if let Some(known) = records::load::<Pointer>(session.journal(), &pointer).await?
            && !ids.contains(&known.intent)
        {
            ids.push(known.intent);
        }
        // Refused before a transfer existed: nothing of it can ever be signed.
        if ids.is_empty() {
            return if bridging::answered(&error) { Ok(Amount::ZERO) } else { Err(error) };
        }
        let mut moved = U256::ZERO;
        for id in ids {
            let mut intent = session.transfers().get(id).await?;
            if dismissable(&intent) {
                // A refusal or a lost answer cancels nothing this run counts on: the move ends on
                // the error that stopped it, not on the cancellation's.
                if let Err(cancel) = session.transfers().cancel(id).await {
                    tracing::warn!(transfer = %id, error = %cancel, "unpaid transfer not cancelled");
                    return Err(error);
                }
                intent = session.transfers().get(id).await?;
            }
            let ended = match intent.take_id {
                Some(_) => intent.take_final == Some(true),
                None => intent.cancelled_at.is_some(),
            };
            let settled = intent.withdrawal.as_ref().is_none_or(|withdrawal| {
                matches!(withdrawal.state, WithdrawalState::Relayed | WithdrawalState::Refused)
            });
            let converting = intent.conversion.as_ref().is_some_and(|conversion| {
                conversion.state.as_ref() != Some(&ConversionState::Expired)
            });
            let unsigned = match intent.take_id {
                Some(take) => {
                    session.takes().get(take).await?.take.fills.iter().all(unsigned_reservation)
                }
                None => true,
            };
            // A GMX `transferOut` counts what it moved once its funds arrived; one sent and not
            // seen arriving, or of unknown outcome, may still move them.
            let out = session.record_key(&transfer_out_key(id));
            let sent_out = match records::load::<TransferOut>(session.journal(), &out).await? {
                None => Some(U256::ZERO),
                Some(asked) if asked.arrived => Some(asked.amount.get()),
                Some(asked) if asked.hash.is_none() => match self.sender {
                    Some(sender) => match sender.status(&asked.id).await {
                        Ok(SendStatus::NotSent) => Some(U256::ZERO),
                        _ => None,
                    },
                    None => None,
                },
                Some(_) => None,
            };
            let Some(sent_out) = sent_out.filter(|_| ended && settled && !converting && unsigned)
            else {
                return Err(error);
            };
            if let Some(withdrawal) =
                intent.withdrawal.filter(|withdrawal| withdrawal.state == WithdrawalState::Relayed)
            {
                moved += withdrawal.amount.get();
            }
            moved += sent_out;
        }
        Ok(Amount::new(moved))
    }

    /// Moves `amount` from a Polymarket or GMX origin to the payer's wallet there, under the move:
    /// a Polymarket withdrawal LaMarge relays, or GMX's own `transferOut` the sender sends. What
    /// it credited once relayed or confirmed, zero when refused, failed or expired.
    async fn to_wallet(
        &self,
        move_id: MoveId,
        amount: U256,
    ) -> Result<std::result::Result<U256, MoveWait>> {
        let (session, terms) = (self.session, self.terms());
        let gmx = terms.from.venue == MoveVenue::Gmx;
        let name = if gmx { "gmx" } else { "withdrawal" };
        budget::reserve(session, terms, Part::new(name, Amount::new(amount), Source::Origin))
            .await?;
        let deadline = Instant::now() + self.request.arrival_timeout;
        loop {
            let moved = if gmx {
                self.gmx_withdrawal(move_id, amount).await?
            } else {
                self.polymarket_withdrawal(move_id, amount).await?
            };
            match moved {
                Moved::Credited => {
                    budget::amend(session, terms, name, Amend::Credit(Amount::new(amount))).await?;
                    return Ok(Ok(amount));
                }
                Moved::Refused => {
                    budget::amend(session, terms, name, Amend::Release).await?;
                    return Ok(Ok(U256::ZERO));
                }
                Moved::Waiting(Some(wait)) => return Ok(Err(wait)),
                Moved::Waiting(None) if Instant::now() >= deadline => {
                    return Ok(Err(MoveWait::Withdrawal));
                }
                Moved::Waiting(None) => tokio::time::sleep(self.request.poll_interval).await,
            }
        }
    }

    /// One pass of the Polymarket withdrawal `<move>.w` to the signer.
    async fn polymarket_withdrawal(&self, move_id: MoveId, amount: U256) -> Result<Moved> {
        let id = client_id(format!("{move_id}.w"))?;
        let withdrawal = super::WithdrawalRequest::new(
            id,
            VenueId::Polymarket,
            Amount::new(amount),
            super::WithdrawTo::Signer,
        )
        .move_id(move_id);
        let state = match self.session.withdraw(&withdrawal).await {
            Ok(withdrawal) => withdrawal.state,
            Err(Error::WithdrawalRefused { .. }) => WithdrawalState::Refused,
            Err(error)
                if matches!(
                    error.code(),
                    Some(
                        crate::error::ErrorCode::VenueRejected
                            | crate::error::ErrorCode::WithdrawalExpired
                    )
                ) =>
            {
                WithdrawalState::Refused
            }
            Err(error) => return Err(error),
        };
        Ok(match state {
            WithdrawalState::Relayed => Moved::Credited,
            WithdrawalState::Refused => Moved::Refused,
            _ => Moved::Waiting(None),
        })
    }

    /// One pass of the GMX withdrawal `<move>.g`, the payer's `transferOut` to its own address.
    async fn gmx_withdrawal(&self, move_id: MoveId, amount: U256) -> Result<Moved> {
        let token = deployments::GMX.token;
        let id = client_id(format!("{move_id}.g"))?;
        let request =
            DirectRequest::gmx_withdrawal(id, self.terms().payer, token, Amount::new(amount))
                .move_id(move_id);
        Ok(match direct::run(self.session, &request, self.sender).await? {
            Withdrawn::Operation(operation) => match operation.state {
                DirectState::Confirmed => Moved::Credited,
                DirectState::Failed | DirectState::Expired => Moved::Refused,
                _ => Moved::Waiting(None),
            },
            Withdrawn::Waiting(wait) => Moved::Waiting(Some(wait)),
        })
    }

    /// A bridge for `leg`, a new attempt after one that ended with nothing debited.
    async fn place(
        &self,
        move_id: MoveId,
        leg: &Leg,
        wallet: Option<(u64, Address)>,
    ) -> Result<Placed> {
        let from_wallet = if leg.from_wallet {
            Some(wallet.ok_or(unsupported(details::BRIDGE_ORIGIN))?)
        } else {
            None
        };
        for attempt in 0..BRIDGE_ATTEMPTS {
            let id = if attempt == 0 {
                leg.id.clone()
            } else {
                client_id(format!("{}.{attempt}", leg.id))?
            };
            let part = BridgePart {
                move_id,
                request: self.terms(),
                recipient: self.request.recipient,
                id,
                amount_in: Amount::new(leg.amount),
                from_wallet,
                poll_interval: self.request.poll_interval,
                arrival_timeout: self.request.arrival_timeout,
            };
            let mut placed = bridging::run(self.session, &part, self.sender).await?;
            // An unsigned quote past its deadline counts until the API reads it expired.
            if let Placed::Bridge(bridge) = &placed
                && bridge.state == BridgeState::Prepared
                && !bridging::in_time(bridge, self.session.transport().now().timestamp())
            {
                if !self.expired(bridge.id).await? {
                    return Ok(Placed::Waiting(MoveWait::Bridge));
                }
                placed = bridging::run(self.session, &part, self.sender).await?;
            }
            match &placed {
                Placed::Bridge(bridge) if bridging::is_dead(&bridge.state) => {}
                _ => return Ok(placed),
            }
        }
        Ok(Placed::Refused("bridge_failed".to_owned()))
    }

    /// Reads bridge `id` until it is final, within the arrival timeout.
    async fn expired(&self, id: crate::types::BridgeId) -> Result<bool> {
        let deadline = Instant::now() + self.request.arrival_timeout;
        loop {
            if bridging::is_final(&self.session.bridges().get(id).await?.state) {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            tokio::time::sleep(self.request.poll_interval).await;
        }
    }
}

#[cfg(test)]
mod tests;
