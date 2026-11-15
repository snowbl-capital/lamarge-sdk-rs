//! The take flow: reserve at market, check the payment, sign it, journal it, submit it.

use std::collections::BTreeSet;

use alloy_primitives::Address;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::Instrument as _;

use super::{
    Signatures, SignedItem, SignedRecord, canonical_digest, lighter_owner, records, request_digests,
};
use crate::{
    Error, Result, Session, deployments,
    error::ErrorCode,
    model::{
        Balance, Destination, Fill, FillState, Funding, Take, TakePreparation, TakeSignRequest,
        ToSignKind,
    },
    protocol::checks::{
        LighterOwners,
        details::{self, changed, differs, stale, unsupported},
        take::{
            Delivery, TakeIntent, TakeReads, aster_nonce, at_once_cutoff, check_take,
            lighter_accounts,
        },
        withdrawal::check_withdrawal,
    },
    transport::{Call, ops},
    types::{Amount, Bps, ClientId, MaxParts, SignatureBytes, TakeId, TransferId, VenueId},
};

/// Where a take or transfer delivers on `to`. Always stated by the caller: the SDK never takes
/// the recipient from the API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Recipient {
    /// Into this account, linked on `to` (for Lighter: the owner of the delivering account
    /// index).
    Account(Address),
    /// By plain transfer to this address on `to`'s escrow chain, in its wallet token.
    Wallet(Address),
}

impl Recipient {
    /// The address delivered to.
    pub(super) fn address(self) -> Address {
        match self {
            Self::Account(address) | Self::Wallet(address) => address,
        }
    }

    /// The API's name of where it delivers.
    pub(super) fn destination(self) -> Destination {
        match self {
            Self::Account(_) => Destination::Venue,
            Self::Wallet(_) => Destination::Wallet,
        }
    }
}

/// What a take or transfer trades. `amount_in` is in `from`'s base units, `min_out` in `to`'s
/// (see [`Amount::parse_venue`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TradeTerms {
    pub(super) from: VenueId,
    pub(super) to: VenueId,
    pub(super) amount_in: Amount,
    pub(super) min_out: Amount,
    pub(super) recipient: Recipient,
    pub(super) max_parts: MaxParts,
    pub(super) max_fee_bps: Option<Bps>,
    pub(super) funding: Funding,
}

impl TradeTerms {
    /// Pays at most `amount_in` on `from` for at least `min_out` on `to`, delivered to
    /// `recipient`; up to 8 parts, no fee accepted, funded by the venue account.
    pub fn new(
        from: VenueId,
        to: VenueId,
        amount_in: Amount,
        min_out: Amount,
        recipient: Recipient,
    ) -> Self {
        Self {
            from,
            to,
            amount_in,
            min_out,
            recipient,
            max_parts: MaxParts::MAX,
            max_fee_bps: None,
            funding: Funding::Venue,
        }
    }

    /// The most maker fills.
    #[must_use]
    pub fn max_parts(mut self, parts: MaxParts) -> Self {
        self.max_parts = parts;
        self
    }

    /// The highest taker fee accepted, as the book's `taker_fee_bps`; unset, no fee is accepted.
    #[must_use]
    pub fn max_fee_bps(mut self, bps: Bps) -> Self {
        self.max_fee_bps = Some(bps);
        self
    }

    /// Where the payment comes from.
    #[must_use]
    pub fn funding(mut self, funding: Funding) -> Self {
        self.funding = funding;
        self
    }

    pub(super) fn wallet_funding(&self) -> bool {
        self.funding == Funding::Wallet
    }
}

/// A take at market: `id` names it, so running the same request again finds the same take.
///
/// Persist the whole request (it is serializable) before the first call, then run that same
/// request again to resume: the same id with other terms is refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TakeRequest {
    id: ClientId,
    terms: TradeTerms,
}

impl TakeRequest {
    /// A take of `terms` named `id`.
    pub fn new(id: ClientId, terms: TradeTerms) -> Self {
        Self { id, terms }
    }
}

/// `POST /v1/takes` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TakeInput {
    /// Wallet funding from another address than the master; never set by this release.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) payer: Option<Address>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) funding: Option<Funding>,
    pub(crate) from: VenueId,
    pub(crate) to: VenueId,
    pub(crate) amount_in: Amount,
    pub(crate) min_out: Amount,
    /// A transfer's take: the transfer then owns the take's recovery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) transfer_intent_id: Option<TransferId>,
    pub(crate) client_take_id: ClientId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) max_parts: Option<MaxParts>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) max_fee_bps: Option<Bps>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) destination: Option<Destination>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) recipient: Option<Address>,
}

impl TakeInput {
    pub(super) fn new(client_take_id: ClientId, terms: &TradeTerms) -> Self {
        let wallet = matches!(terms.recipient, Recipient::Wallet(_));
        Self {
            payer: None,
            funding: Some(terms.funding.clone()),
            from: terms.from.clone(),
            to: terms.to.clone(),
            amount_in: terms.amount_in,
            min_out: terms.min_out,
            transfer_intent_id: None,
            client_take_id,
            max_parts: Some(terms.max_parts),
            max_fee_bps: terms.max_fee_bps,
            destination: wallet.then_some(Destination::Wallet),
            recipient: wallet.then(|| terms.recipient.address()),
        }
    }
}

#[derive(Deserialize)]
struct TakeFills {
    fills: Vec<Fill>,
}

#[derive(Serialize)]
struct ReissueBody {
    nonce: u64,
}

/// `POST /v1/takes/{id}/aster-request` answer.
#[derive(Deserialize)]
struct Reissued {
    take: Take,
    #[serde(default)]
    aster_reissue: bool,
    to_sign: Vec<TakeSignRequest>,
    matched: bool,
}

/// A transfer's prepared withdrawal a take is signed at once with: checked and signed before
/// the payment, to the session address, and submitted with it as its `margin_move` item.
#[derive(Debug, Clone, Copy)]
pub(super) struct AtOnce<'a> {
    /// In `from` base units.
    pub(super) amount: Amount,
    pub(super) typed_data: &'a Value,
}

impl Session {
    /// Takes at market: reserves up to `max_parts` fills, checks the payment they ask for against
    /// `request`, signs it, saves the signature to the journal, then submits it. Returns the
    /// take's fills; nothing is signed when the take needs no signature or a fill is past its
    /// reservation (already paid). Running the same request again submits a saved signature
    /// again, never a second one.
    ///
    /// Checked before anything is signed:
    /// - the payer is the session address (the account linked on `from`, or the wallet), and a
    ///   [`Recipient::Account`] is the account linked on `to`;
    /// - every term of the payment is bound to `request`: amounts, parts, fee, recipient,
    ///   windows, this environment's escrow;
    /// - a take delivering into Lighter is to an account Lighter's own API says the recipient
    ///   owns;
    /// - a take paid from wallet funds has a fresh read of them covering `amount_in`.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidRequest`] before any request: a zero `amount_in`, a wallet recipient on
    ///   a venue without a wallet token, or wallet funding from Lighter or Aster;
    ///   `Verification(Unsupported)` for a venue or funding this release does not support.
    /// - `Verification(..)` when a check fails: nothing is signed and the take is cancelled.
    /// - [`Error::AsterRequestUncertain`] when the outcome of issuing an Aster request again is
    ///   unknown: run the same request again.
    /// - `TAKE_EXPIRED` when the take expired before its signature was accepted: take again with
    ///   a new id.
    ///
    /// # Cancellation
    ///
    /// Dropping the future is safe but not clean: a reserved, unsigned take stays until it
    /// expires; a saved signature is submitted by running the same request again.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn run(session: &lamarge::Session) -> Result<(), Box<dyn std::error::Error>> {
    /// use lamarge::flows::{Recipient, TakeRequest, TradeTerms};
    /// use lamarge::types::{Amount, ClientId, VenueId};
    ///
    /// let terms = TradeTerms::new(
    ///     VenueId::Hyperliquid,
    ///     VenueId::Polymarket,
    ///     Amount::parse_venue(&VenueId::Hyperliquid, "2")?,
    ///     Amount::parse_venue(&VenueId::Polymarket, "1.99")?,
    ///     Recipient::Account(session.address()),
    /// );
    /// let fills = session.take(&TakeRequest::new(ClientId::new("take-1")?, terms)).await?;
    /// # Ok(()) }
    /// ```
    pub async fn take(&self, request: &TakeRequest) -> Result<Vec<Fill>> {
        let span = tracing::info_span!("lamarge.take", client_take_id = %request.id);
        run(self, request).instrument(span).await
    }
}

async fn run(session: &Session, request: &TakeRequest) -> Result<Vec<Fill>> {
    let terms = &request.terms;
    validate(terms)?;
    // The whole take runs alone per request id across the journal's sessions, so a second run
    // reserves only once the first has submitted.
    let turn = session.record_key(&format!("take-request:{}", request.id));
    let _turn = session.journal().lock(&turn).await;
    let balances = session.account().balances().await?;
    let intent = intent(session, terms, &balances)?;
    let input = TakeInput::new(request.id.clone(), terms);
    take_input(session, &input, &intent).await
}

/// Refuses, before any request, what this release or the API does not do.
pub(super) fn validate(terms: &TradeTerms) -> Result<()> {
    let (Some(_), Some(to)) = (deployments::chain(&terms.from), deployments::chain(&terms.to))
    else {
        return Err(unsupported(details::VENUE).into());
    };
    if terms.amount_in.get().is_zero() {
        return Err(Error::InvalidRequest("amount_in must be positive".into()));
    }
    if terms.funding.is_unknown() {
        return Err(unsupported(details::FUNDING).into());
    }
    if matches!(terms.recipient, Recipient::Wallet(_)) && to.wallet_token.is_none() {
        return Err(Error::InvalidRequest(
            "a wallet recipient needs a receiving venue with a wallet token".into(),
        ));
    }
    if terms.wallet_funding() && deployments::is_venue_settled(&terms.from) {
        return Err(Error::InvalidRequest("wallet funding pays on an escrow venue only".into()));
    }
    Ok(())
}

/// The terms the take is checked against, from the request and a fresh read of the balances.
pub(super) fn intent(
    session: &Session,
    terms: &TradeTerms,
    balances: &[Balance],
) -> Result<TakeIntent> {
    let payer = session.address();
    let row = |venue: &VenueId| balances.iter().find(|row| row.venue == *venue);
    let wallet_funding = terms.wallet_funding();
    let mut payer_account = None;
    if !wallet_funding {
        let row = row(&terms.from).ok_or(changed(details::LINKED_ACCOUNT))?;
        if row.address != payer {
            return Err(changed(details::PAYER).into());
        }
        if terms.from == VenueId::Lighter {
            let index = row.venue_wallet.as_deref().and_then(|index| index.parse().ok());
            payer_account = Some(index.ok_or(changed(details::LINKED_ACCOUNT))?);
        }
    }
    let delivery = match terms.recipient {
        Recipient::Account(account) => {
            super::linked(balances, &terms.to, account)?;
            Delivery::Account(account)
        }
        Recipient::Wallet(address) => Delivery::Wallet(address),
    };
    Ok(TakeIntent {
        from: terms.from.clone(),
        to: terms.to.clone(),
        amount_in: terms.amount_in,
        min_out: terms.min_out,
        max_parts: terms.max_parts,
        max_fee_bps: terms.max_fee_bps,
        wallet_funding,
        delivery,
        payer,
        payer_account,
        at_once: false,
        route: false,
    })
}

/// Prepares `input`, then signs (or reuses the saved signatures) and submits.
pub(super) async fn take_input(
    session: &Session,
    input: &TakeInput,
    intent: &TakeIntent,
) -> Result<Vec<Fill>> {
    let call = Call::new(&ops::POST_TAKE).json(input)?;
    let prepared: TakePreparation = session.transport().send(call, session.auth()).await?;
    pay(session, input, intent, prepared, None).await
}

/// Signs `prepared` (or reuses the saved signatures) and submits; with `withdrawal`, signed at
/// once with it. A take past its reservation is returned as it is, never signed for again (a
/// Lighter payment with a fresh nonce would pay twice); a transfer follows it.
pub(super) async fn pay(
    session: &Session,
    input: &TakeInput,
    intent: &TakeIntent,
    prepared: TakePreparation,
    withdrawal: Option<AtOnce<'_>>,
) -> Result<Vec<Fill>> {
    let transfer = input.transfer_intent_id.is_some();
    if prepared.to_sign.is_empty() || !prepared.take.fills.iter().all(|fill| reserved(&fill.state))
    {
        return Ok(prepared.take.fills);
    }
    let id = prepared.take.id;
    tracing::info!(take_id = %id, "take reserved");
    let journal = session.journal();
    let key = take_key(session, id);
    let context = withdrawal.map(|withdrawal| canonical_digest(withdrawal.typed_data));
    let signatures = match records::load::<SignedRecord>(journal, &key).await? {
        Some(saved) => saved.reuse(&request_digests(&prepared.to_sign), context)?,
        None => match sign(session, prepared, intent, withdrawal, &key).await {
            Ok(items) => items,
            Err(error) => {
                // A transfer recovers its own takes, except one reserved for a withdrawal signed
                // with it; an uncertain Aster request is recovered by running the take again, so
                // its reservation stays.
                if (!transfer || withdrawal.is_some())
                    && !matches!(error, Error::AsterRequestUncertain)
                {
                    cancel(session, id).await;
                }
                return Err(error);
            }
        },
    };
    submit(session, id, &key, &signatures).await
}

/// Whether a fill is still a reservation: prepared or reserved, nothing paid.
pub(super) fn reserved(state: &FillState) -> bool {
    matches!(state, FillState::Prepared | FillState::Reserved)
}

/// Whether a take signed at once with a withdrawal can still be signed at `now`: the at-once
/// cutoff leaves its signing seconds.
pub(super) fn signable_at_once(prepared: &TakePreparation, now: DateTime<Utc>) -> Result<bool> {
    let signing = i64::try_from(deployments::AT_ONCE_SIGNING_SECONDS).unwrap_or(i64::MAX);
    Ok(at_once_cutoff(prepared)?.saturating_sub(signing) > now.timestamp())
}

/// The signatures of `prepared`, checked against `intent`, without submitting them: the saved
/// ones when they sign the same requests, else signed now and saved under the take.
pub(super) async fn sign_only(
    session: &Session,
    prepared: TakePreparation,
    intent: &TakeIntent,
) -> Result<Vec<SignedItem>> {
    let key = take_key(session, prepared.take.id);
    match records::load::<SignedRecord>(session.journal(), &key).await? {
        Some(saved) => saved.reuse(&request_digests(&prepared.to_sign), None),
        None => sign(session, prepared, intent, None, &key).await,
    }
}

/// Forgets the saved signatures of take `id`.
pub(super) async fn forget(session: &Session, id: TakeId) -> Result<()> {
    records::delete(session.journal(), &take_key(session, id), true).await
}

/// Whether the journal holds the signatures of take `id`.
pub(super) async fn saved(session: &Session, id: TakeId) -> Result<bool> {
    Ok(records::load::<SignedRecord>(session.journal(), &take_key(session, id)).await?.is_some())
}

fn take_key(session: &Session, id: TakeId) -> String {
    session.record_key(&format!("take:{id}"))
}

/// Checks the payment, signs it and saves the signature with the requests it signs. Signed at
/// once, the withdrawal is checked and signed first, each while the take can still be signed.
async fn sign(
    session: &Session,
    prepared: TakePreparation,
    intent: &TakeIntent,
    withdrawal: Option<AtOnce<'_>>,
    key: &str,
) -> Result<Vec<SignedItem>> {
    let moved = match withdrawal {
        Some(withdrawal) => {
            if intent.wallet_funding {
                return Err(differs(details::FUNDING).into());
            }
            let amount = withdrawal.amount.get();
            if amount.is_zero() || amount > intent.amount_in.get() {
                return Err(differs(details::AMOUNT).into());
            }
            in_time(&prepared, session.transport().now())?;
            let payer = intent.payer;
            let verified = check_withdrawal(
                withdrawal.typed_data,
                &intent.from,
                payer,
                payer,
                withdrawal.amount,
                session.transport().now(),
            )?;
            let signature = session.sign(&verified).await?;
            // The wallet may have held the withdrawal open: the take must still be signable.
            in_time(&prepared, session.transport().now())?;
            Some(signature)
        }
        None => None,
    };
    let mut owners = LighterOwners::new();
    read_owners(session, &mut owners, lighter_accounts(&prepared, intent)).await?;
    let funds = if intent.wallet_funding {
        Some(session.account().wallet_funding(&intent.from, Some(prepared.take.id)).await?)
    } else {
        None
    };
    let prepared = if prepared.aster_reissue {
        let fresh = reissue(session, &prepared).await?;
        read_owners(session, &mut owners, lighter_accounts(&fresh, intent)).await?;
        fresh
    } else {
        prepared
    };
    let reads = TakeReads { lighter_owners: &owners, wallet_funds: funds.as_ref() };
    let now = session.transport().now();
    let verified = check_take(&prepared, intent, reads, session.escrow_environment(), now)?;
    let item = prepared.to_sign.first().ok_or(changed(details::TO_SIGN))?;
    let signature = session.sign(&verified).await?;
    let mut items = vec![signed_item(item, signature)];
    if let Some(signature) = moved {
        items.push(SignedItem { kind: ToSignKind::MarginMove, fill_id: None, signature });
    }
    let record = SignedRecord {
        digests: request_digests(&prepared.to_sign),
        context: withdrawal.map(|withdrawal| canonical_digest(withdrawal.typed_data)),
        items,
    };
    records::save(session.journal(), key, &record, false).await?;
    tracing::info!("payment signed");
    Ok(record.items)
}

/// Refuses a take signed at once that can no longer be signed in time.
fn in_time(prepared: &TakePreparation, now: DateTime<Utc>) -> Result<()> {
    if signable_at_once(prepared, now)? {
        Ok(())
    } else {
        Err(stale(details::SIGNING_WINDOW).into())
    }
}

fn signed_item(item: &TakeSignRequest, signature: SignatureBytes) -> SignedItem {
    let fill_id = match item {
        TakeSignRequest::Typed(item) => item.fill_id,
        TakeSignRequest::Lighter(item) => Some(*item.fill_id.as_uuid()),
        TakeSignRequest::Aster(item) => Some(*item.fill_id.as_uuid()),
    };
    SignedItem { kind: item.kind().clone(), fill_id, signature }
}

/// Adds the owners Lighter's public API reports for `accounts` not yet known; an account it
/// does not answer for stays unknown, which the checks refuse.
async fn read_owners(
    session: &Session,
    owners: &mut LighterOwners,
    accounts: BTreeSet<u64>,
) -> Result<()> {
    for index in accounts {
        if !owners.contains_key(&index)
            && let Some(owner) = lighter_owner(session.transport(), index).await?
        {
            owners.insert(index, owner);
        }
    }
    Ok(())
}

/// Has LaMarge issue the held Aster request again, compare-and-set on its nonce: a superseded
/// one is answered with the current request, issued again once. Any other change is refused.
async fn reissue(session: &Session, held: &TakePreparation) -> Result<TakePreparation> {
    let id = held.take.id;
    let mut answer = reissue_once(session, id, held_nonce(&held.to_sign)?).await?;
    if !answer.matched {
        tracing::info!("aster request superseded; issuing the current one again");
        answer = reissue_once(session, id, held_nonce(&answer.to_sign)?).await?;
    }
    if !answer.matched || answer.take.id != id {
        return Err(changed(details::ASTER_TRANSFER).into());
    }
    Ok(TakePreparation {
        take: answer.take,
        aster_reissue: answer.aster_reissue,
        to_sign: answer.to_sign,
    })
}

fn held_nonce(to_sign: &[TakeSignRequest]) -> Result<u64> {
    match to_sign.first() {
        Some(TakeSignRequest::Aster(item)) => Ok(aster_nonce(item)?),
        _ => Err(changed(details::ASTER_TRANSFER).into()),
    }
}

/// One reissue call. Only LaMarge's refusal (a 4xx other than 429) is definitive: no answer, or
/// an unreadable one, may hide a request issued.
async fn reissue_once(session: &Session, id: TakeId, nonce: u64) -> Result<Reissued> {
    let call = Call::new(&ops::REISSUE_ASTER_REQUEST).segment(id).json(&ReissueBody { nonce })?;
    match session.transport().send(call, session.auth()).await {
        Ok(answer) => Ok(answer),
        Err(Error::Api(api)) if (400..500).contains(&api.status()) && api.status() != 429 => {
            Err(Error::Api(api))
        }
        Err(error) => {
            tracing::warn!(error = %error, "aster request outcome unknown");
            Err(Error::AsterRequestUncertain)
        }
    }
}

/// Submits the saved signatures; the record goes once the answer reads as fills, or once the
/// take expired (its signatures can never be used).
async fn submit(
    session: &Session,
    id: TakeId,
    key: &str,
    signatures: &[SignedItem],
) -> Result<Vec<Fill>> {
    let journal = session.journal();
    let call =
        Call::new(&ops::SUBMIT_TAKE_SIGNATURES).segment(id).json(&Signatures { signatures })?;
    match session.transport().send::<TakeFills>(call, session.auth()).await {
        Ok(answer) => {
            records::delete(journal, key, true).await?;
            tracing::info!(fills = answer.fills.len(), "take paid");
            Ok(answer.fills)
        }
        Err(error) if error.code() == Some(&ErrorCode::TakeExpired) => {
            if let Err(cleanup) = records::delete(journal, key, true).await {
                tracing::warn!(error = %cleanup, "expired take's signature not forgotten");
            }
            Err(error)
        }
        Err(error) => Err(error),
    }
}

/// Cancels a reserved take, best effort.
async fn cancel(session: &Session, id: TakeId) {
    if let Err(error) = session.takes().cancel(id).await {
        tracing::warn!(error = %error, "take not cancelled; it expires unpaid");
    }
}

#[cfg(test)]
mod tests;
