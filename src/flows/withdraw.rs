//! The withdrawal flow: withdrawals out of Hyperliquid and Polymarket, sends to another account
//! on Hyperliquid, Polymarket and Aster.

use alloy_primitives::Address;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::Instrument as _;

use super::{linked, records};
use crate::{
    Error, Result, Session,
    error::ErrorCode,
    model::{Withdrawal, WithdrawalState},
    protocol::{
        checks::{
            details::{self, changed, unsupported},
            withdrawal::{check_send, check_withdrawal, signed_at},
        },
        signing::Verified,
    },
    transport::{Call, ops},
    types::{Amount, ClientId, EscrowEnvironment, SignatureBytes, VenueId},
};

/// Where a withdrawal goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum WithdrawTo {
    /// Out of the venue to the session address (Hyperliquid: on HyperEVM; Polymarket: on
    /// Polygon).
    Signer,
    /// Out of Polymarket to this wallet (Deposit Wallet or Safe), which must be the account's
    /// linked venue wallet: the SDK never takes the destination from the API.
    VenueWallet(Address),
    /// A send to another account on the same venue; on Polymarket, its Perps owner (signer)
    /// address.
    Account(Address),
}

/// Exactly `amount` out of the session's account on `venue`: a withdrawal (Hyperliquid,
/// Polymarket) or a send to another account (Hyperliquid, Polymarket, Aster).
///
/// Persist the whole request (it is serializable) before the first call, then run that same
/// request again to resume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WithdrawalRequest {
    id: ClientId,
    venue: VenueId,
    amount: Amount,
    to: WithdrawTo,
}

impl WithdrawalRequest {
    /// A withdrawal named `id` of `amount` (in `venue`'s base units) to `to`. The same id
    /// finds this withdrawal again, never a second one.
    pub fn new(id: ClientId, venue: VenueId, amount: Amount, to: WithdrawTo) -> Self {
        Self { id, venue, amount, to }
    }
}

/// What a stored withdrawal was for: a stored record answers only the same request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Context {
    environment: EscrowEnvironment,
    venue: VenueId,
    amount: Amount,
    to: WithdrawTo,
    signer: Address,
}

/// A withdrawal's signature, saved before it is first submitted (`{ns}withdrawal:{id}`), then its
/// outcome.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct WithdrawalRecord {
    context: Context,
    /// As prepared, submitted exactly so.
    typed_data: Value,
    signature: SignatureBytes,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    result: Option<Withdrawal>,
    /// The venue refused it: the id is spent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    refused: Option<ErrorCode>,
}

#[derive(Serialize)]
struct PrepareBody<'a> {
    venue: &'a VenueId,
    amount: Amount,
    to: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    recipient: Option<Address>,
}

#[derive(Deserialize)]
struct Prepared {
    typed_data: Value,
}

#[derive(Serialize)]
struct SubmitBody<'a> {
    venue: &'a VenueId,
    typed_data: &'a Value,
    signature: SignatureBytes,
}

impl Session {
    /// Withdraws or sends exactly `amount` from the session's account on the request's venue:
    /// prepared, checked (to the destination asked for, without fee, its time within a minute),
    /// signed, saved to the journal, then relayed.
    ///
    /// The journal keeps the signature and the outcome under the request's id: running the same
    /// request again returns a final outcome (`relayed`) without any request, or submits the
    /// same signature again (checked again at its own signing time), never a second withdrawal.
    ///
    /// Supported: withdrawals out of Hyperliquid ([`WithdrawTo::Signer`]) and Polymarket
    /// ([`WithdrawTo::Signer`], [`WithdrawTo::VenueWallet`], the linked wallet), sends on
    /// Hyperliquid, Polymarket and Aster ([`WithdrawTo::Account`], another address than the
    /// session's).
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidRequest`] before any request for a zero amount, a send to the session's
    ///   own address or a destination the venue does not offer, or the same id with other terms;
    ///   `Verification(Unsupported)` for anything else this release does not support.
    /// - `Verification(..)` when the prepared withdrawal fails a check: nothing is signed.
    /// - `VENUE_REJECTED` or `WITHDRAWAL_EXPIRED` when the venue refused it, then
    ///   [`Error::WithdrawalRefused`] on every later run: use a new id.
    ///
    /// # Cancellation
    ///
    /// Dropping the future is safe: a saved signature is submitted by running the same request
    /// again, and an unsaved one is discarded.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn run(session: &lamarge::Session) -> Result<(), Box<dyn std::error::Error>> {
    /// use lamarge::flows::{WithdrawTo, WithdrawalRequest};
    /// use lamarge::types::{Amount, ClientId, VenueId};
    ///
    /// let amount = Amount::parse_venue(&VenueId::Hyperliquid, "25")?;
    /// let id = ClientId::new("out-1")?;
    /// let request = WithdrawalRequest::new(id, VenueId::Hyperliquid, amount, WithdrawTo::Signer);
    /// let withdrawal = session.withdraw(&request).await?;
    /// # Ok(()) }
    /// ```
    pub async fn withdraw(&self, request: &WithdrawalRequest) -> Result<Withdrawal> {
        let span =
            tracing::info_span!("lamarge.withdraw", id = %request.id, venue = %request.venue);
        run(self, request).instrument(span).await
    }
}

/// The API's name of `to`, and the send's recipient.
fn target(to: WithdrawTo) -> (&'static str, Option<Address>) {
    match to {
        WithdrawTo::Signer => ("signer", None),
        WithdrawTo::VenueWallet(_) => ("wallet", None),
        WithdrawTo::Account(recipient) => ("account", Some(recipient)),
    }
}

/// Refuses, before any request, what this release does not support or the API would refuse.
fn validate(session: &Session, request: &WithdrawalRequest) -> Result<()> {
    match (&request.venue, request.to) {
        (VenueId::Hyperliquid | VenueId::Polymarket, WithdrawTo::Signer)
        | (VenueId::Polymarket, WithdrawTo::VenueWallet(_))
        | (VenueId::Hyperliquid | VenueId::Polymarket | VenueId::Aster, WithdrawTo::Account(_)) => {
        }
        (VenueId::Hyperliquid, WithdrawTo::VenueWallet(_)) => {
            return Err(Error::InvalidRequest("Hyperliquid withdraws to the signer only".into()));
        }
        _ => return Err(unsupported(details::VENUE).into()),
    }
    if request.to == WithdrawTo::Account(session.address()) {
        return Err(Error::InvalidRequest("a send goes to another account".into()));
    }
    if request.amount == Amount::ZERO {
        return Err(Error::InvalidRequest("amount must be positive".into()));
    }
    Ok(())
}

async fn run(session: &Session, request: &WithdrawalRequest) -> Result<Withdrawal> {
    validate(session, request)?;
    let id = &request.id;
    let signer = session.address();
    let (to, recipient) = target(request.to);
    let journal = session.journal();
    let key = session.record_key(&format!("withdrawal:{id}"));
    let _turn = journal.lock(&key).await;
    let context = Context {
        environment: session.escrow_environment(),
        venue: request.venue.clone(),
        amount: request.amount,
        to: request.to,
        signer,
    };
    let stored = records::load::<WithdrawalRecord>(journal, &key).await?;
    if let Some(record) = &stored {
        if record.context != context {
            return Err(Error::InvalidRequest(
                format!("withdrawal {id} names another withdrawal").into(),
            ));
        }
        if let Some(code) = &record.refused {
            return Err(Error::WithdrawalRefused { id: id.clone(), code: code.clone() });
        }
        if let Some(result) = record.result.as_ref().filter(|result| is_final(&result.state)) {
            return Ok(result.clone());
        }
    }
    let destination = destination(session, request).await?;
    let record = if let Some(record) = stored {
        check(request, signer, destination, &record.typed_data, signed_at(&record.typed_data)?)?;
        tracing::info!("submitting the saved signature");
        record
    } else {
        let body = PrepareBody { venue: &request.venue, amount: request.amount, to, recipient };
        let call = Call::new(&ops::PREPARE_WITHDRAWAL).json(&body)?;
        let prepared: Prepared = session.transport().send(call, session.auth()).await?;
        let now = session.transport().now();
        let verified = check(request, signer, destination, &prepared.typed_data, now)?;
        let signature = session.sign(&verified).await?;
        let record = WithdrawalRecord {
            context,
            typed_data: prepared.typed_data,
            signature,
            result: None,
            refused: None,
        };
        records::save(journal, &key, &record, false).await?;
        tracing::info!("withdrawal signed");
        record
    };
    submit(session, request, &key, record).await
}

/// The withdrawal's destination, as the request names it, after a fresh read of the account
/// linked on the venue: the session address's, and its venue wallet the one the request names.
async fn destination(session: &Session, request: &WithdrawalRequest) -> Result<Address> {
    let balances = session.account().balances().await?;
    let row = linked(&balances, &request.venue, session.address())?;
    Ok(match request.to {
        WithdrawTo::Signer => session.address(),
        WithdrawTo::VenueWallet(wallet) => {
            let linked = row.venue_wallet.as_deref().and_then(|linked| linked.parse().ok());
            if linked != Some(wallet) {
                return Err(changed(details::LINKED_ACCOUNT).into());
            }
            wallet
        }
        WithdrawTo::Account(recipient) => recipient,
    })
}

/// Checks `typed` as `request`'s withdrawal or send to `destination`, signed at `at`.
fn check(
    request: &WithdrawalRequest,
    signer: Address,
    destination: Address,
    typed: &Value,
    at: DateTime<Utc>,
) -> Result<Verified> {
    let (venue, amount) = (&request.venue, request.amount);
    let checked = match request.to {
        WithdrawTo::Account(_) => check_send(typed, venue, signer, destination, amount, at),
        WithdrawTo::Signer | WithdrawTo::VenueWallet(_) => {
            check_withdrawal(typed, venue, signer, destination, amount, at)
        }
    };
    Ok(checked?)
}

/// Relays the saved signature and records the outcome: the result, or the venue's refusal.
async fn submit(
    session: &Session,
    request: &WithdrawalRequest,
    key: &str,
    mut record: WithdrawalRecord,
) -> Result<Withdrawal> {
    let journal = session.journal();
    let body = SubmitBody {
        venue: &request.venue,
        typed_data: &record.typed_data,
        signature: record.signature,
    };
    let call = Call::new(&ops::SUBMIT_WITHDRAWAL).json(&body)?;
    match session.transport().send::<Withdrawal>(call, session.auth()).await {
        Ok(result) => {
            record.result = Some(result.clone());
            records::save(journal, key, &record, true).await?;
            tracing::info!(state = %result.state, "withdrawal submitted");
            Ok(result)
        }
        Err(error) if refusal(&error) => {
            record.refused = error.code().cloned();
            records::save(journal, key, &record, true).await?;
            Err(error)
        }
        Err(error) => Err(error),
    }
}

/// An outcome running the request again cannot change.
fn is_final(state: &WithdrawalState) -> bool {
    matches!(state, WithdrawalState::Relayed | WithdrawalState::Refused)
}

/// The venue refused the signed withdrawal: it can never go.
fn refusal(error: &Error) -> bool {
    matches!(error.code(), Some(ErrorCode::VenueRejected | ErrorCode::WithdrawalExpired))
}

#[cfg(test)]
mod tests;
