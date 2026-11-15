//! The transfer flow: one take from `from` to `to`, funded from the venue account by a
//! withdrawal or from the escrow-chain wallet, recoverable from the journal.

use std::time::Duration;

use alloy_primitives::{Address, U256};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, de::Error as _};
use serde_json::{Value, json};
use tokio::time::Instant;
use tracing::Instrument as _;

use super::{
    Recipient, TradeTerms, linked, poll_until, records,
    take::{self, AtOnce, TakeInput},
};
use crate::{
    Error, Result, Session,
    book::{BookError, SweepTerms, delivery_floor},
    deployments,
    error::{ApiError, ErrorCode, Reason},
    model::{
        self, Balance, Destination, Fill, FillState, Funding, FundingPlan, FundingRequestKind,
        MakerLeg, PaymentLeg, PaymentSetup, StepStatus, TakePreparation, TransferIntent,
        Withdrawal, WithdrawalState,
    },
    protocol::checks::{
        details::{self, changed, differs, stale},
        funding::{PlanTerms, check_funding_plan, check_wallet_capability, check_wallet_funding},
        take::{TakeIntent, at_once_cutoff, signable_until},
        withdrawal::{check_withdrawal, signed_at},
    },
    transport::{Call, ops},
    types::{
        Amount, Bps, ClientId, MaxParts, MoveId, SetupStepKind, SignatureBytes, TakeId, TransferId,
        VenueId,
    },
};

/// How long a prepared, unsigned withdrawal may wait before its transfer is replaced.
const STALE_PREPARATION_SECONDS: i64 = 45;
/// How long a balance read stays fresh.
const FRESH_SECONDS: i64 = 120;

/// A transfer: `terms` traded by one take, funded first when the paying address lacks the
/// funds. `id` names the transfer only: its takes are named by the server, also after a
/// replacement.
///
/// Persist the whole request (it is serializable) before the first call, then run that same
/// request again to resume: the same id with other terms is refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferRequest {
    id: ClientId,
    terms: TradeTerms,
    poll_interval: Duration,
    arrival_timeout: Duration,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    move_id: Option<MoveId>,
}

impl TransferRequest {
    /// A transfer of `terms` named `id`, reading every 2 s while funds or a permit arrive, for
    /// up to 10 minutes each.
    pub fn new(id: ClientId, terms: TradeTerms) -> Self {
        Self {
            id,
            terms,
            poll_interval: Duration::from_secs(2),
            arrival_timeout: Duration::from_secs(600),
            move_id: None,
        }
    }

    /// Attaches the transfer to move `id` (its replacements too), from the move's origin and
    /// payer to its destination; the move's budget counts its take's `amount_in`.
    #[must_use]
    pub fn move_id(mut self, id: MoveId) -> Self {
        self.move_id = Some(id);
        self
    }

    /// Between reads while funds or a permit arrive.
    #[must_use]
    pub fn poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    /// How long to wait for funds to arrive, and then for a permit to confirm, before
    /// [`TransferOutcome::Pending`].
    #[must_use]
    pub fn arrival_timeout(mut self, timeout: Duration) -> Self {
        self.arrival_timeout = timeout;
        self
    }
}

/// How a transfer run ended.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum TransferOutcome {
    /// The take is signed and submitted: its fills, as the server answered them (a take signed
    /// at once with its withdrawal stays prepared until the funds arrive).
    Filled {
        /// The transfer, the replacement that took if it was replaced.
        intent: TransferIntent,
        /// The take's fills.
        fills: Vec<Fill>,
    },
    /// Nothing failed; run the same request again later.
    Pending {
        /// The transfer as last read.
        intent: TransferIntent,
        /// What it waits for.
        reason: PendingReason,
    },
}

/// What a pending transfer waits for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PendingReason {
    /// The funds have not reached the paying address within the arrival timeout.
    ArrivalUnconfirmed,
    /// The escrow's permit has not confirmed within the arrival timeout.
    AuthorizationUnconfirmed,
    /// The venue's answer to the withdrawal is unknown: it may have been sent. Never sign
    /// another.
    WithdrawalUnknown,
    /// The venue refused the withdrawal (`intent.withdrawal.error` says why).
    WithdrawalRefused,
}

impl Session {
    /// Runs a transfer to its take: creates it (or resumes the one the journal names for the
    /// request's id), funds it, then reserves, checks, signs, saves and submits its take.
    ///
    /// Funding by `from`:
    /// - Hyperliquid or Polymarket margin: only the paying address's shortfall is withdrawn to
    ///   it, once the book pays `min_out`; the take is signed with the withdrawal when the
    ///   server plans so, else taken once the funds arrive.
    /// - Wallet funds ([`Funding::Wallet`]): nothing is withdrawn; a shortfall stops before
    ///   anything is signed, and the escrow's permit is signed first when needed.
    /// - Lighter or Aster: the take pays directly.
    ///
    /// Every answer is checked against `request` first: the transfer's terms, its funding plan,
    /// the withdrawal (to the session address, exactly the shortfall) and the take's payment.
    ///
    /// Running the same request again resumes: a submitted take returns its fills without any
    /// signature, a saved signature is submitted again, and a withdrawal is never sent twice. A
    /// transfer whose withdrawal went unsigned too long, or whose take expired unsigned, is
    /// replaced with the same terms; a replaced take is never funded again.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidRequest`] before any request for a combination the API refuses;
    ///   `Verification(Unsupported)` for GMX venue funding or a venue or funding this release
    ///   does not support.
    /// - `Verification(Changed)` naming the term when the transfer is answered with other terms
    ///   than `request`'s, or the wallet funds no longer cover it; `Verification(..)` when a
    ///   check fails, with nothing signed.
    ///
    /// Waits that outlast the request's arrival timeout, and a withdrawal refused or of unknown
    /// outcome, end in [`TransferOutcome::Pending`], not in an error.
    ///
    /// # Cancellation
    ///
    /// Dropping the future is safe but not clean: a reserved, unsigned take stays until it
    /// expires; the next run resumes from the journal.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn run(session: &lamarge::Session) -> Result<(), Box<dyn std::error::Error>> {
    /// use lamarge::flows::{Recipient, TradeTerms, TransferOutcome, TransferRequest};
    /// use lamarge::types::{Amount, ClientId, VenueId};
    ///
    /// let terms = TradeTerms::new(
    ///     VenueId::Hyperliquid,
    ///     VenueId::Polymarket,
    ///     Amount::parse_venue(&VenueId::Hyperliquid, "100")?,
    ///     Amount::parse_venue(&VenueId::Polymarket, "99.5")?,
    ///     Recipient::Account(session.address()),
    /// );
    /// let request = TransferRequest::new(ClientId::new("move-1")?, terms);
    /// match session.transfer(&request).await? {
    ///     TransferOutcome::Filled { fills, .. } => println!("{} fills", fills.len()),
    ///     TransferOutcome::Pending { reason, .. } => println!("pending: {reason:?}"),
    ///     _ => {}
    /// }
    /// # Ok(()) }
    /// ```
    pub async fn transfer(&self, request: &TransferRequest) -> Result<TransferOutcome> {
        let span = tracing::info_span!("lamarge.transfer", client_transfer_id = %request.id);
        run(self, request).instrument(span).await
    }
}

/// `POST /v1/transfers` body.
#[derive(Serialize)]
struct TransferInput<'a> {
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
    destination: Option<Destination>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_fee_bps: Option<Bps>,
}

/// The transfer a request last became (`{ns}transfer:{client_transfer_id}`), kept after it fills.
#[derive(Debug, Serialize, Deserialize)]
struct TransferPointer {
    intent: TransferId,
}

/// A transfer's withdrawal signature, saved before it is first submitted
/// (`{ns}transfer-withdrawal:{intent id}`), deleted once its outcome is known.
#[derive(Debug, Serialize, Deserialize)]
struct WithdrawalRecord {
    /// What it withdraws, in `from` base units.
    amount: Amount,
    typed_data: Value,
    signature: SignatureBytes,
}

/// A marker record: `renewal:{id}` (a replacement decided before the cancellation's answer) or
/// `funded:{id}` (a replacement whose funding was already sent).
#[derive(Debug, Serialize, Deserialize)]
struct Marker {}

#[derive(Serialize)]
struct PrepareBody {
    amount: Amount,
}

#[derive(Deserialize)]
struct Prepared {
    typed_data: Value,
}

#[derive(Serialize)]
struct SignatureBody {
    signature: SignatureBytes,
}

/// A reserved take as recovery reads it (`GET /v1/takes/{id}`), and whether all its fills are
/// unsigned reservations. A fill is one only when it states `signed_at: null`: a missing one is
/// unproven.
struct Recovered {
    prepared: TakePreparation,
    unsigned: bool,
}

impl<'de> Deserialize<'de> for Recovered {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        let stated: Vec<bool> = value["take"]["fills"].as_array().map_or_else(Vec::new, |fills| {
            fills.iter().map(|fill| fill.get("signed_at").is_some_and(Value::is_null)).collect()
        });
        let prepared = TakePreparation::deserialize(value).map_err(D::Error::custom)?;
        let fills = &prepared.take.fills;
        let unsigned = !fills.is_empty()
            && fills.iter().zip(&stated).all(|(fill, null)| *null && unsigned_reservation(fill));
        Ok(Self { prepared, unsigned })
    }
}

/// A reservation nothing was signed, paid or delivered for.
fn unsigned_reservation(fill: &Fill) -> bool {
    matches!(fill.state, FillState::Prepared | FillState::Reserved | FillState::Expired)
        && fill.payment_leg == PaymentLeg::None
        && matches!(fill.maker_leg, MakerLeg::None | MakerLeg::Abandoned)
        && fill.transactions.is_empty()
        && fill.delivery_hash.is_none()
        && fill.payment_locked_at.is_none()
        && fill.maker_claimed_at.is_none()
}

/// What the paying address holds for the take: `ready` now, `supplied` once a deposit address's
/// sweep lands; with the balances read.
struct Funds {
    ready: U256,
    supplied: U256,
    balances: Vec<Balance>,
}

/// Why a run stops: pending, or failed.
enum Halt {
    Pending(PendingReason),
    Failed(Error),
}

impl<E: Into<Error>> From<E> for Halt {
    fn from(error: E) -> Self {
        Self::Failed(error.into())
    }
}

type Step<T> = std::result::Result<T, Halt>;

async fn run(session: &Session, request: &TransferRequest) -> Result<TransferOutcome> {
    validate(request)?;
    let pointer = session.record_key(&format!("transfer:{}", request.id));
    // One run per request across the journal's sessions.
    let _turn = session.journal().lock(&pointer).await;
    let resumed = records::load::<TransferPointer>(session.journal(), &pointer).await?;
    let intent = if let Some(latest) = resumed {
        tracing::info!(transfer = %latest.intent, "resuming");
        read(session, latest.intent, true).await?
    } else {
        if request.terms.wallet_funding() {
            let venues = session.client().market().venues().await?;
            check_wallet_capability(&venues, &request.terms.from, session.escrow_environment())?;
        }
        post(session, request, &pointer, &request.id, false).await?
    };
    let mut run = Run { session, request, pointer, intent, authorizing: false };
    match run.start().await {
        Ok(fills) => Ok(TransferOutcome::Filled { intent: run.intent, fills }),
        Err(Halt::Pending(reason)) => {
            tracing::info!(?reason, "transfer pending");
            Ok(TransferOutcome::Pending { intent: run.intent, reason })
        }
        Err(Halt::Failed(error)) => Err(error),
    }
}

/// Creates the transfer under `client_id` (idempotent) and records it as the request's latest
/// under `pointer`; a replacement whose funding was already sent is marked so first, so a crash
/// between the two writes never leaves a pointer to a transfer that looks unfunded.
async fn post(
    session: &Session,
    request: &TransferRequest,
    pointer: &str,
    client_id: &ClientId,
    funded: bool,
) -> Result<TransferIntent> {
    let terms = &request.terms;
    let destination = terms.recipient.destination();
    let body = TransferInput {
        move_id: request.move_id,
        funding: terms.wallet_funding().then_some(Funding::Wallet),
        client_transfer_id: client_id,
        from: &terms.from,
        to: &terms.to,
        amount_in: terms.amount_in,
        min_out: terms.min_out,
        max_parts: terms.max_parts,
        payer: session.address(),
        recipient: terms.recipient.address(),
        destination: (destination == Destination::Wallet).then_some(destination),
        max_fee_bps: terms.max_fee_bps,
    };
    let call = Call::new(&ops::POST_TRANSFER).json(&body)?;
    let intent: TransferIntent = session.transport().send(call, session.auth()).await?;
    if intent.client_transfer_id != *client_id {
        return Err(differs(details::CLIENT_REQUEST_ID).into());
    }
    if funded {
        let key = session.record_key(&self::funded(intent.id));
        records::save(session.journal(), &key, &Marker {}, true).await?;
    }
    let latest = TransferPointer { intent: intent.id };
    records::save(session.journal(), pointer, &latest, true).await?;
    Ok(intent)
}

/// Transfer `id`, with its funding plan when `plan`.
async fn read(session: &Session, id: TransferId, plan: bool) -> Result<TransferIntent> {
    let transfers = session.transfers();
    let intent =
        if plan { transfers.get_with_funding_plan(id).await? } else { transfers.get(id).await? };
    if intent.id == id { Ok(intent) } else { Err(differs(details::CLIENT_REQUEST_ID).into()) }
}

/// Refuses, before any request, what this release or the API does not do.
fn validate(request: &TransferRequest) -> Result<()> {
    take::validate(&request.terms)?;
    let terms = &request.terms;
    if !terms.wallet_funding() && terms.from == VenueId::Gmx {
        // GMX margin leaves by a transaction the signer sends: not in this release.
        return Err(details::unsupported(details::VENUE).into());
    }
    Ok(())
}

struct Run<'a> {
    session: &'a Session,
    request: &'a TransferRequest,
    /// The journal key of the request's latest transfer.
    pointer: String,
    /// The transfer as last read.
    intent: TransferIntent,
    /// The permit of the plan is not final yet: the take waits for it.
    authorizing: bool,
}

impl Run<'_> {
    fn terms(&self) -> &TradeTerms {
        &self.request.terms
    }

    fn wallet_funding(&self) -> bool {
        self.terms().wallet_funding()
    }

    fn payer(&self) -> Address {
        self.session.address()
    }

    fn now(&self) -> DateTime<Utc> {
        self.session.transport().now()
    }

    fn key(&self, record: &str) -> String {
        self.session.record_key(record)
    }

    /// Resumes durable operations before new funding or payment; a short balance never repeats
    /// a withdrawal.
    async fn start(&mut self) -> Step<Vec<Fill>> {
        // A replacement decided before a lost cancellation answer is completed, never decided
        // again.
        let mut renewed_from = None;
        let id = self.intent.id;
        if self.intent.cancelled_at.is_some() && self.marked(&renewal(id)).await? {
            renewed_from = Some(id);
            self.replace(&format!("renew:{id}"), false).await?;
        } else {
            self.check_intent()?;
        }
        if let Some(id) = self.renew_stale().await? {
            renewed_from = Some(id);
        }
        self.renewed(renewed_from).await?;
        let funded = self.marked(&funded(self.intent.id)).await?;
        if let Some(fills) = self.recover_take().await? {
            return Ok(fills);
        }
        // A replacement of a take that expired unsigned after its funding: never funded again.
        if funded {
            return self.repriced(None).await;
        }
        // A replacement recovered above can hold a preparation as stale as the one it replaced.
        if let Some(id) = self.renew_stale().await? {
            self.renewed(Some(id)).await?;
        }
        if deployments::is_venue_settled(&self.intent.from) {
            tracing::info!("taking");
            return self.pay(None, None).await;
        }
        self.fund_and_take().await
    }

    /// Funds the take from the venue (a withdrawal) or checks the wallet's funds, waits for
    /// them and for the permit, then takes.
    async fn fund_and_take(&mut self) -> Step<Vec<Fill>> {
        self.check_withdrawal_state()?;
        let mut funds = self.read_funds().await?;
        // Wallet funds are read before the permit too: a transfer they cannot pay signs nothing.
        if self.wallet_funding() && self.intent.withdrawal.is_none() {
            covers(funds.as_ref(), self.intent.amount_in)?;
        }
        // The plan reads the escrow chain when answered; a balances read may predate a payment
        // that spent it.
        let plan = if self.intent.withdrawal.is_none() { self.fund().await? } else { None };
        let withdrawal = plan.as_ref().map_or(Amount::ZERO, |plan| plan.withdrawal_amount);
        let mut reread = false;
        if withdrawal != Amount::ZERO {
            // Margin leaves the venue only for a take the book can execute now.
            if self.intent.withdrawal_preparation.is_none() {
                self.review_quote().await?;
            }
            tracing::info!(amount = %withdrawal, "funding");
            let at_once = plan.as_ref().is_some_and(|plan| plan.signed_at_once);
            match self.withdraw(withdrawal, at_once, funds.as_ref()).await {
                Ok(Some(fills)) => return Ok(fills),
                Ok(None) => {}
                // An earlier withdrawal of the route is still arriving: nothing is withdrawn
                // again.
                Err(Halt::Failed(error))
                    if error.api().and_then(ApiError::reason) == Some(&Reason::FundsArriving) =>
                {
                    return Err(Halt::Pending(PendingReason::ArrivalUnconfirmed));
                }
                Err(halt) => return Err(halt),
            }
            self.check_withdrawal_state()?;
            // The plan read before is no sign of arrival: read again, with the plan once relayed.
            let relayed = self
                .intent
                .withdrawal
                .as_ref()
                .is_some_and(|withdrawal| withdrawal.state == WithdrawalState::Relayed);
            self.read_intent(relayed).await?;
            reread = true;
        }
        if self.intent.withdrawal.is_none() && funds.is_none() {
            return Err(stale(details::WALLET_FUNDS_OBSERVED_AT).into());
        }
        // Only the live funding plan proves arrival or detects funds spent since the read.
        let deadline = Instant::now() + self.request.arrival_timeout;
        if self.waiting(funds.as_ref())? {
            tracing::info!("waiting for the funds");
        }
        while self.waiting(funds.as_ref())? {
            if Instant::now() >= deadline {
                return Err(Halt::Pending(PendingReason::ArrivalUnconfirmed));
            }
            tokio::time::sleep(self.request.poll_interval).await;
            self.read_intent(true).await?;
            funds = self.read_funds().await?;
            if let Some(fills) = self.recover_take().await? {
                return Ok(fills);
            }
            self.check_withdrawal_state()?;
            reread = true;
        }
        if !reread {
            self.read_intent(plan.is_none()).await?;
            if let Some(fills) = self.recover_take().await? {
                return Ok(fills);
            }
        }
        if plan.is_none() && self.intent.funding_plan.is_some() {
            self.fund().await?;
        }
        // The permit gets its own wait, however long the funds took.
        if self.authorizing {
            let confirm_by = Instant::now() + self.request.arrival_timeout;
            let done = || async { Ok(self.permission_done().await?.then_some(())) };
            if poll_until(confirm_by, self.request.poll_interval, done).await?.is_none() {
                return Err(Halt::Pending(PendingReason::AuthorizationUnconfirmed));
            }
        }
        tracing::info!("taking");
        let funds = funds.ok_or(stale(details::WALLET_FUNDS_OBSERVED_AT))?;
        self.repriced(Some(funds.balances)).await
    }

    /// The withdrawal of `amount`: signed at once with the take when the plan says so (the
    /// take's fills, or `None` when another run submitted the withdrawal meanwhile), else
    /// submitted alone and recorded on the intent.
    async fn withdraw(
        &mut self,
        amount: Amount,
        at_once: bool,
        funds: Option<&Funds>,
    ) -> Step<Option<Vec<Fill>>> {
        let record = self.key(&withdrawal_key(self.intent.id));
        if at_once
            && records::load::<WithdrawalRecord>(self.session.journal(), &record).await?.is_none()
        {
            let (prepared, typed_data) = match &self.intent.withdrawal_preparation {
                Some(preparation) => (preparation.amount, preparation.typed_data.clone()),
                None => (amount, self.prepare_withdrawal(amount).await?),
            };
            if prepared != amount {
                return Err(changed(details::FUNDING_PLAN).into());
            }
            let balances = funds.map(|funds| funds.balances.as_slice());
            return self.take_at_once(amount, &typed_data, balances).await;
        }
        let withdrawal = self.withdraw_alone(amount, &record).await?;
        self.intent.withdrawal = Some(withdrawal);
        Ok(None)
    }

    /// The take prepared against the withdrawal prepared for it, then signed and submitted with
    /// it: the server sends the withdrawal, then the payment once its funds reach the wallet.
    async fn take_at_once(
        &mut self,
        amount: Amount,
        typed_data: &Value,
        balances: Option<&[Balance]>,
    ) -> Step<Option<Vec<Fill>>> {
        let payer = self.payer();
        check_withdrawal(typed_data, &self.intent.from, payer, payer, amount, self.now())?;
        let call = Call::new(&ops::POST_TAKE).json(&self.take_input())?;
        let prepared = match self.session.transport().send(call, self.session.auth()).await {
            Ok(prepared) => prepared,
            Err(error)
                if error.api().and_then(ApiError::reason) == Some(&Reason::WithdrawalSubmitted) =>
            {
                // Submitted meanwhile by another run: followed as any other withdrawal, never
                // sent again.
                if self.read_intent(false).await.is_ok() && self.intent.withdrawal.is_some() {
                    return Ok(None);
                }
                return Err(error.into());
            }
            Err(error) => return Err(error.into()),
        };
        tracing::info!("taking");
        self.sign_at_once(prepared, amount, typed_data, balances).await.map(Some)
    }

    /// Signs a take prepared against a withdrawal and submits both: a withdrawal refused, or of
    /// unknown outcome, never pays it.
    async fn sign_at_once(
        &mut self,
        prepared: TakePreparation,
        amount: Amount,
        typed_data: &Value,
        balances: Option<&[Balance]>,
    ) -> Step<Vec<Fill>> {
        let intent = self.take_intent(balances, true).await?;
        let withdrawal = AtOnce { amount, typed_data };
        let input = self.take_input();
        let fills = take::pay(self.session, &input, &intent, prepared, Some(withdrawal)).await?;
        self.read_intent(false).await?;
        self.check_withdrawal_state()?;
        Ok(fills)
    }

    /// The transfer's withdrawal alone, to the session address, for exactly `amount`; the saved
    /// signature goes once the outcome is known.
    async fn withdraw_alone(&self, amount: Amount, key: &str) -> Result<Withdrawal> {
        let journal = self.session.journal();
        let (from, payer, id) = (&self.intent.from, self.payer(), self.intent.id);
        let record = if let Some(saved) = records::load::<WithdrawalRecord>(journal, key).await? {
            // Checked again at its own signing time, for its own amount: the shortfall may have
            // moved since, never above what the transfer pays.
            if saved.amount > self.intent.amount_in {
                return Err(differs(details::AMOUNT).into());
            }
            let at = signed_at(&saved.typed_data)?;
            check_withdrawal(&saved.typed_data, from, payer, payer, saved.amount, at)?;
            tracing::info!("submitting the saved withdrawal");
            saved
        } else {
            let typed_data = match &self.intent.withdrawal_preparation {
                Some(preparation) => preparation.typed_data.clone(),
                None => self.prepare_withdrawal(amount).await?,
            };
            let verified = check_withdrawal(&typed_data, from, payer, payer, amount, self.now())?;
            let signature = self.session.sign(&verified).await?;
            let record = WithdrawalRecord { amount, typed_data, signature };
            records::save(journal, key, &record, false).await?;
            tracing::info!("withdrawal signed");
            record
        };
        let call = Call::new(&ops::SUBMIT_TRANSFER_WITHDRAWAL)
            .segment(id)
            .json(&SignatureBody { signature: record.signature })?;
        match self.session.transport().send::<Withdrawal>(call, self.session.auth()).await {
            Ok(withdrawal) => {
                // An unknown outcome keeps the signature: never another for it.
                if withdrawal.state != WithdrawalState::Uncertain {
                    records::delete(journal, key, true).await?;
                }
                tracing::info!(state = %withdrawal.state, "withdrawal submitted");
                Ok(withdrawal)
            }
            Err(error) if error.code() == Some(&ErrorCode::VenueRejected) => {
                records::delete(journal, key, true).await?;
                Err(error)
            }
            Err(error) => Err(error),
        }
    }

    async fn prepare_withdrawal(&self, amount: Amount) -> Result<Value> {
        let call = Call::new(&ops::PREPARE_TRANSFER_WITHDRAWAL)
            .segment(self.intent.id)
            .json(&PrepareBody { amount })?;
        let prepared: Prepared = self.session.transport().send(call, self.session.auth()).await?;
        Ok(prepared.typed_data)
    }

    /// The take already reserved, signed or paid, settled first; one that expired unsigned is
    /// replaced (`retry:{id}`), priced and taken again. `None` when there is no take.
    async fn recover_take(&mut self) -> Step<Option<Vec<Fill>>> {
        let mut renewed = false;
        while let Some(take_id) = self.intent.take_id {
            tracing::info!(take_id = %take_id, "recovering the take");
            let mut take = self.read_take(take_id).await?;
            let intent = &self.intent;
            let preparation = if intent.withdrawal.is_none() {
                intent
                    .withdrawal_preparation
                    .as_ref()
                    .map(|preparation| (preparation.amount, preparation.typed_data.clone()))
            } else {
                None
            };
            let fills = &take.prepared.take.fills;
            if take.unsigned && fills.iter().any(|fill| take::reserved(&fill.state)) {
                // Past what it can still be signed by, a withdrawal to send with it included,
                // it is replaced. Cancellation serializes with acceptance: a late or uncertain
                // signature is never replaced.
                let late = signable_until(&take.prepared)? <= self.now().timestamp()
                    || match &preparation {
                        Some((amount, typed_data)) => {
                            !self.sendable(*amount, typed_data, &take.prepared).await?
                        }
                        None => false,
                    };
                if late {
                    self.session.takes().cancel(take_id).await?;
                    take = self.read_take(take_id).await?;
                }
            }
            let fills = &take.prepared.take.fills;
            if !take.unsigned || !fills.iter().all(|fill| fill.state == FillState::Expired) {
                if take.prepared.to_sign.is_empty()
                    || !fills.iter().all(|fill| take::reserved(&fill.state))
                {
                    // Signed with its withdrawal: one refused or of unknown outcome never pays.
                    self.check_withdrawal_state()?;
                    return Ok(Some(take.prepared.take.fills));
                }
                return match preparation {
                    Some((amount, typed_data)) => {
                        self.sign_at_once(take.prepared, amount, &typed_data, None).await.map(Some)
                    }
                    None => self.pay(Some(take.prepared), None).await.map(Some),
                };
            }
            // A deterministic replacement survives a lost answer or a restart, with the same
            // terms. One whose withdrawal was never sent is funded again.
            renewed = preparation.is_none();
            self.replace(&format!("retry:{}", self.intent.id), renewed).await?;
        }
        // The original take was funded already: repricing never triggers another withdrawal.
        if renewed { self.repriced(None).await.map(Some) } else { Ok(None) }
    }

    /// Whether a take prepared against a withdrawal can still be submitted with it: in time for
    /// the server to send the withdrawal, and signed already or still signable in time.
    async fn sendable(
        &self,
        amount: Amount,
        typed_data: &Value,
        prepared: &TakePreparation,
    ) -> Result<bool> {
        let now = self.now();
        if at_once_cutoff(prepared)? <= now.timestamp() {
            return Ok(false);
        }
        if take::saved(self.session, prepared.take.id).await? {
            return Ok(true);
        }
        if !take::signable_at_once(prepared, now)? {
            return Ok(false);
        }
        let payer = self.payer();
        Ok(check_withdrawal(typed_data, &self.intent.from, payer, payer, amount, now).is_ok())
    }

    async fn read_take(&self, id: TakeId) -> Result<Recovered> {
        let call = Call::new(&ops::GET_TAKE).segment(id);
        self.session.transport().send(call, self.session.auth()).await
    }

    /// The transfer's take: from the current intent's ids, checked against the request.
    fn take_input(&self) -> TakeInput {
        let intent = &self.intent;
        TakeInput {
            transfer_intent_id: Some(intent.id),
            ..TakeInput::new(intent.client_take_id.clone(), self.terms())
        }
    }

    /// The terms the take is checked against, from `balances` or a fresh read of them.
    async fn take_intent(&self, balances: Option<&[Balance]>, at_once: bool) -> Result<TakeIntent> {
        let mut intent = if let Some(balances) = balances {
            take::intent(self.session, self.terms(), balances)?
        } else {
            let balances = self.session.account().balances().await?;
            take::intent(self.session, self.terms(), &balances)?
        };
        intent.at_once = at_once;
        Ok(intent)
    }

    /// Signs and submits the take: `prepared`, or a new reservation.
    async fn pay(
        &self,
        prepared: Option<TakePreparation>,
        balances: Option<&[Balance]>,
    ) -> Step<Vec<Fill>> {
        let intent = self.take_intent(balances, false).await?;
        let input = self.take_input();
        Ok(match prepared {
            Some(prepared) => take::pay(self.session, &input, &intent, prepared, None).await?,
            None => take::take_input(self.session, &input, &intent).await?,
        })
    }

    /// A take prepared again is priced first: a book that no longer pays its minimum takes
    /// nothing.
    async fn repriced(&self, balances: Option<Vec<Balance>>) -> Step<Vec<Fill>> {
        let balances = if let Some(balances) = balances {
            self.review_quote().await?;
            balances
        } else {
            let account = self.session.account();
            futures_util::future::try_join(self.review_quote(), account.balances()).await?.1
        };
        self.pay(None, Some(&balances)).await
    }

    /// The book pays the whole amount for at least `min_out`, above the delivery floor, within
    /// the accepted fee, without the session's own orders.
    async fn review_quote(&self) -> Result<()> {
        let terms = self.terms();
        let market = self.session.client().market();
        let (book, orders, venues) = futures_util::future::try_join3(
            market.book(&terms.from, &terms.to),
            self.session.orders().list(None),
            market.venues(),
        )
        .await?;
        let sweep = SweepTerms::new(terms.max_fee_bps)
            .max_parts(terms.max_parts)
            .floor(delivery_floor(&venues, &terms.to, &terms.recipient.destination()))
            .own(orders.into_iter().map(|order| order.id).collect());
        book.check(terms.amount_in, terms.min_out, &sweep, self.now())
            .map_err(|error: BookError| error.into())
    }

    /// The paying address's funds, with the balances; `None` for venue funding without a
    /// recent, successful chain read. The linked accounts must be the request's.
    async fn read_funds(&self) -> Result<Option<Funds>> {
        let balances = self.session.account().balances().await?;
        let terms = self.terms();
        let payer = self.payer();
        // A wallet delivery needs no account on `to`.
        if let Recipient::Account(account) = terms.recipient {
            linked(&balances, &terms.to, account)?;
        }
        if self.wallet_funding() {
            let account = self.session.account();
            let funds = account.wallet_funding(&terms.from, self.intent.take_id).await?;
            let environment = self.session.escrow_environment();
            let spendable =
                check_wallet_funding(&funds, &terms.from, payer, environment, self.now())?;
            return Ok(Some(Funds { ready: spendable, supplied: spendable, balances }));
        }
        let from = linked(&balances, &terms.from, payer)?;
        let Some(ready) = prefunded(from, self.now()) else { return Ok(None) };
        let supplied = ready.checked_add(sweepable(from)).ok_or(differs(details::WALLET_FUNDS))?;
        Ok(Some(Funds { ready, supplied, balances }))
    }

    /// Whether the take still waits for funds: a withdrawal not relayed or not yet seen by the
    /// plan, or a balance short of the amount. A plan asking for a withdrawal now is a change.
    fn waiting(&self, funds: Option<&Funds>) -> Result<bool> {
        let intent = &self.intent;
        if let Some(withdrawal) = &intent.withdrawal {
            return Ok(funds.is_none()
                || withdrawal.state != WithdrawalState::Relayed
                || intent.funding_plan.is_none());
        }
        if intent.funding_plan.as_ref().is_some_and(|plan| plan.withdrawal_amount != Amount::ZERO) {
            let detail =
                if self.wallet_funding() { details::WALLET_FUNDS } else { details::FUNDING_PLAN };
            return Err(changed(detail).into());
        }
        Ok(funds.is_none_or(|funds| funds.ready < intent.amount_in.get()))
    }

    /// Checks the plan, signs its permit when it asks for one, and reads the plan again before
    /// it decides a withdrawal. `None` once a withdrawal is recorded.
    async fn fund(&mut self) -> Result<Option<FundingPlan>> {
        let plan = self.check_plan()?;
        if !plan.requests.iter().any(|request| request.kind == FundingRequestKind::Permit) {
            return Ok(Some(plan));
        }
        tracing::info!("signing the permit");
        if self.wallet_funding() {
            // Every wallet signature is preceded by a fresh read covering the transfer.
            covers(self.read_funds().await?.as_ref(), self.intent.amount_in)?;
            self.session.authorize_wallet(&self.terms().from).await?;
        } else {
            let (venue, step) = (&self.terms().from, SetupStepKind::Permit);
            if self.session.run_step(venue, &step, None).await? == StepStatus::Required {
                return Err(Error::SetupIncomplete { venue: venue.clone(), step });
            }
        }
        // The permit waited for its signer: another run may have withdrawn meanwhile.
        self.read_intent(true).await?;
        if self.intent.withdrawal.is_some() { Ok(None) } else { self.check_plan().map(Some) }
    }

    /// The intent's funding plan, checked; a plan the server could not compute fails as its
    /// refusal would.
    fn check_plan(&mut self) -> Result<FundingPlan> {
        let intent = &self.intent;
        if intent.funding_plan.is_none()
            && let Some(refused) = &intent.funding_plan_error
        {
            return Err(Error::Api(Box::new(plan_refusal(refused))));
        }
        let terms = PlanTerms {
            from: intent.from.clone(),
            payer: self.payer(),
            amount_in: intent.amount_in,
            wallet_funding: self.wallet_funding(),
        };
        let environment = self.session.escrow_environment();
        let plan =
            check_funding_plan(intent.funding_plan.as_ref(), &terms, environment, self.now())?
                .clone();
        self.authorizing =
            matches!(plan.payment_setup, PaymentSetup::Required | PaymentSetup::Pending);
        Ok(plan)
    }

    /// Whether the escrow's permission is final: the wallet's authorization, or the account's
    /// permit step (one required again failed).
    async fn permission_done(&self) -> Result<bool> {
        let (venue, payer) = (&self.terms().from, self.payer());
        let account = self.session.account();
        if self.wallet_funding() {
            let funds = account.wallet_funding(venue, self.intent.take_id).await?;
            check_wallet_funding(
                &funds,
                venue,
                payer,
                self.session.escrow_environment(),
                self.now(),
            )?;
            return Ok(funds.payment_setup.authorized);
        }
        let setup = account.venue_setup(venue).await?;
        if setup.address != payer {
            return Err(differs(details::OWNER).into());
        }
        let permit = setup
            .steps
            .iter()
            .find(|row| row.step == SetupStepKind::Permit)
            .ok_or(differs(details::SETUP_STEP))?;
        match permit.status {
            StepStatus::Required => {
                Err(Error::SetupIncomplete { venue: venue.clone(), step: SetupStepKind::Permit })
            }
            StepStatus::Done => Ok(true),
            _ => Ok(false),
        }
    }

    /// A withdrawal refused or of unknown outcome stops the transfer, pending.
    fn check_withdrawal_state(&self) -> Step<()> {
        match self.intent.withdrawal.as_ref().map(|withdrawal| &withdrawal.state) {
            Some(WithdrawalState::Refused) => Err(Halt::Pending(PendingReason::WithdrawalRefused)),
            Some(WithdrawalState::Uncertain) => {
                Err(Halt::Pending(PendingReason::WithdrawalUnknown))
            }
            _ => Ok(()),
        }
    }

    /// A transfer whose withdrawal preparation went unsigned over 45 seconds is cancelled and
    /// replaced (`renew:{id}`); the id it replaced, if it did.
    async fn renew_stale(&mut self) -> Result<Option<TransferId>> {
        let intent = &self.intent;
        let stale = intent.withdrawal.is_none()
            && intent.take_id.is_none()
            && intent.withdrawal_preparation.as_ref().is_some_and(|preparation| {
                (self.now() - preparation.created_at).num_seconds() > STALE_PREPARATION_SECONDS
            });
        if !stale {
            return Ok(None);
        }
        let id = intent.id;
        tracing::info!(transfer = %id, "replacing a stale withdrawal preparation");
        // Cancellation serializes with acceptance; an uncertain withdrawal is never replaced.
        let marker = self.key(&renewal(id));
        records::save(self.session.journal(), &marker, &Marker {}, false).await?;
        if let Err(error) = self.session.transfers().cancel(id).await {
            // LaMarge's refusal (a 4xx other than 429) cancelled nothing: no replacement is
            // owed. Any other failure may hide a cancellation, so the marker stays.
            if error.status().is_some_and(|status| (400..500).contains(&status) && status != 429) {
                records::delete(self.session.journal(), &marker, false).await?;
            }
            return Err(error);
        }
        self.replace(&format!("renew:{id}"), false).await?;
        Ok(Some(id))
    }

    /// The replacement is recorded: the renewal marker of the transfer it replaced goes.
    async fn renewed(&self, from: Option<TransferId>) -> Result<()> {
        match from {
            Some(id) => {
                records::delete(self.session.journal(), &self.key(&renewal(id)), true).await
            }
            None => Ok(()),
        }
    }

    async fn marked(&self, record: &str) -> Result<bool> {
        let key = self.key(record);
        Ok(records::load::<Marker>(self.session.journal(), &key).await?.is_some())
    }

    /// A replacement repeats nothing: the same terms under a client id derived from the
    /// transfer it replaces; `funded` when the funding it replaces was already sent.
    async fn replace(&mut self, client_id: &str, funded: bool) -> Result<()> {
        let client_id =
            ClientId::new(client_id).map_err(|_| differs(details::CLIENT_REQUEST_ID))?;
        let intent = post(self.session, self.request, &self.pointer, &client_id, funded).await?;
        tracing::info!(transfer = %intent.id, "replaced");
        self.intent = intent;
        self.check_intent()
    }

    async fn read_intent(&mut self, plan: bool) -> Result<()> {
        let intent = read(self.session, self.intent.id, plan).await?;
        self.intent = intent;
        self.check_intent()
    }

    /// The transfer must be the request's, not cancelled: each term the request states, and
    /// none it does not (a route, a conversion, trade.xyz).
    fn check_intent(&self) -> Result<()> {
        let (intent, terms) = (&self.intent, self.terms());
        let chain = |venue| deployments::chain(venue).map(|chain| chain.id);
        let funding = if self.wallet_funding() { Funding::Wallet } else { Funding::Venue };
        let rules = [
            (intent.cancelled_at.is_none(), details::STATE),
            (
                intent.from == terms.from
                    && intent.to == terms.to
                    && Some(intent.from_chain_id) == chain(&terms.from)
                    && Some(intent.to_chain_id) == chain(&terms.to),
                details::VENUE,
            ),
            (intent.amount_in == terms.amount_in, details::AMOUNT_IN),
            (intent.min_out == terms.min_out, details::AMOUNT_OUT),
            (intent.max_parts == terms.max_parts, details::FILLS),
            (intent.max_fee_bps == terms.max_fee_bps, details::FEE_ABOVE_ACCEPTED),
            (intent.payer == self.payer(), details::PAYER),
            (intent.recipient == terms.recipient.address(), details::RECIPIENT),
            (
                intent.destination == terms.recipient.destination() && intent.then.is_none(),
                details::DESTINATION,
            ),
            (
                intent.funding.as_ref().unwrap_or(&Funding::Venue) == &funding
                    && intent.conversion.is_none(),
                details::FUNDING,
            ),
            (intent.from_dex.is_none(), details::SOURCE_DEX),
            (intent.previous_transfer_id.is_none(), details::CLIENT_REQUEST_ID),
            (intent.move_id == self.request.move_id, details::MOVE_ID),
        ];
        match rules.into_iter().find(|(ok, _)| !ok) {
            Some((_, detail)) => Err(changed(detail).into()),
            None => Ok(()),
        }
    }
}

/// The funds cover `amount`.
fn covers(funds: Option<&Funds>, amount: Amount) -> Result<()> {
    if funds.is_some_and(|funds| funds.supplied >= amount.get()) {
        Ok(())
    } else {
        Err(changed(details::WALLET_FUNDS).into())
    }
}

/// What a take pays without moving margin, from a recent, successful escrow-chain read: the
/// linked address's balance and its escrow available balance. `None` when unknown.
fn prefunded(row: &Balance, now: DateTime<Utc>) -> Option<U256> {
    let read_at = row.chain_read_at?;
    if row.chain_error.is_some() || (now - read_at).num_seconds() >= FRESH_SECONDS {
        return None;
    }
    let escrow = row.escrow_balance.unwrap_or(Amount::ZERO).get();
    row.chain_balance?.get().checked_add(escrow)
}

/// What the deposit address holds that the relayer sweeps without a withdrawal: at least one
/// whole unit of the venue's token, dust otherwise.
fn sweepable(row: &Balance) -> U256 {
    let held = row.deposit_balance.unwrap_or(Amount::ZERO).get();
    let minimum = row.venue.decimals().map(|decimals| U256::from(10u8).pow(U256::from(decimals)));
    if minimum.is_some_and(|minimum| held >= minimum) { held } else { U256::ZERO }
}

/// The refusal a funding plan error stands for, as the API would have answered it.
fn plan_refusal(refused: &model::FundingPlanError) -> ApiError {
    let envelope = json!({"error": {
        "code": refused.code, "message": refused.message, "reason": refused.reason,
        "venue": refused.venue, "step": refused.step,
    }});
    let body = serde_json::to_vec(&envelope).unwrap_or_default();
    let retry_after = refused.retry_after.map(Duration::from_secs);
    ApiError::from_answer(refused.status, &body, None, retry_after)
}

fn renewal(id: TransferId) -> String {
    format!("renewal:{id}")
}

fn funded(id: TransferId) -> String {
    format!("funded:{id}")
}

fn withdrawal_key(id: TransferId) -> String {
    format!("transfer-withdrawal:{id}")
}

#[cfg(test)]
mod tests;
