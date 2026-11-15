//! The Polygon wallet conversion: native USDC converted into pUSD by the payer's own
//! transactions (an `approve` when the allowance is short, then the convert call) before a
//! wallet-funded transfer from Polymarket takes. Each transaction is journalled before it is
//! asked and once it has a hash, so it is never asked twice.

use alloy_primitives::{Address, B256, U256, address};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::records;
use crate::{
    Error, Result, SendError, SendStatus, Session, Transaction, TransactionSender,
    deployments::{self, POLYMARKET},
    error::VerificationError,
    model::{ConversionState, TransferIntent, WalletConversion},
    protocol::checks::{
        details::{self, changed, differs, stale, unsupported},
        funding::check_wallet_funding,
    },
    transport::{Call, ops},
    types::{Amount, VenueId},
};

/// The converter the escrow's conversion is pinned to.
const CONVERTER: Address = address!("2d31ff03f5da1c385a64d764389962c160e789f8");
/// The least native USDC a conversion converts: one cent.
const LEAST_CONVERTED: u64 = 10_000;
/// How far ahead a conversion's deadline may be: the server's 30 minutes and 15 seconds of skew.
const DEADLINE_WINDOW_SECONDS: i64 = 1_815;
/// `approve(address,uint256)`.
const APPROVE: [u8; 4] = [0x09, 0x5e, 0xa7, 0xb3];
/// The escrow's `convert(uint256 amountIn, uint256 minOut, uint256 nonce, uint256 deadline)`.
const CONVERT: [u8; 4] = [0x32, 0x0a, 0x91, 0xb1];

/// The native USDC on Polygon a conversion spends.
fn native_usdc() -> Address {
    POLYMARKET.wallet_token.unwrap_or(Address::ZERO)
}

/// A conversion funding a wallet-funded transfer from Polymarket: `amount_in` native USDC into
/// at least `min_out` pUSD, the transfer's `amount_in`. `min_out` must be at least 99.95 % of
/// `amount_in`, rounded up, and `amount_in` at least 0.01 USDC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversionTerms {
    amount_in: Amount,
    min_out: Amount,
}

impl ConversionTerms {
    /// Converts `amount_in` native USDC into at least `min_out` pUSD.
    pub const fn new(amount_in: Amount, min_out: Amount) -> Self {
        Self { amount_in, min_out }
    }

    /// The most native USDC whose conversion floor is `min_out` pUSD: `⌊min_out × 10 000 /
    /// 9 995⌋`. Use it to fund a transfer of `min_out` and quote what the owner spends.
    pub fn covering(min_out: Amount) -> Option<Self> {
        let bps = U256::from(deployments::WALLET_SWAP_MIN_BPS);
        let amount_in = min_out.get().checked_mul(U256::from(10_000u16))? / bps;
        Some(Self { amount_in: Amount::new(amount_in), min_out })
    }

    /// Native USDC converted, in base units.
    pub const fn amount_in(&self) -> Amount {
        self.amount_in
    }

    /// The least pUSD received, in base units.
    pub const fn min_out(&self) -> Amount {
        self.min_out
    }

    /// At least one cent, and a floor no lower than the escrow's.
    pub(super) fn valid(&self) -> bool {
        self.amount_in.get() >= U256::from(LEAST_CONVERTED)
            && floor(self.amount_in).is_some_and(|floor| self.min_out.get() >= floor)
    }
}

/// The least the escrow lets a conversion of `amount_in` return: `⌈amount_in × 9 995 / 10 000⌉`.
fn floor(amount_in: Amount) -> Option<U256> {
    let scaled = amount_in.get().checked_mul(U256::from(deployments::WALLET_SWAP_MIN_BPS))?;
    Some(scaled.div_ceil(U256::from(10_000u16)))
}

/// What a conversion will ask the sender for, read before anything is asked.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ConversionPreview {
    /// The chain the transactions are sent on (Polygon, 137).
    pub chain_id: u64,
    /// The address that sends them and pays their gas.
    pub payer: Address,
    /// Its native USDC balance, in base units.
    pub balance: Amount,
    /// Whether an `approve` is sent before the convert call: the escrow's allowance is short.
    pub approval: bool,
    /// How many transactions: 1, or 2 with the `approve`.
    pub transactions: u8,
    /// Whether the balance covers the conversion's `amount_in`.
    pub covered: bool,
}

impl Session {
    /// What converting `terms` from the session's wallet would ask the sender for, without
    /// asking: the chain, whether an `approve` comes first, and whether the balance covers it.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidRequest`] for terms the escrow refuses; `Verification(..)` when the
    /// conversion is unavailable or its pinned deployment differs.
    pub async fn conversion_preview(&self, terms: &ConversionTerms) -> Result<ConversionPreview> {
        if !terms.valid() {
            return Err(Error::InvalidRequest("conversion below the escrow's floor".into()));
        }
        let escrow = check_capability(self).await?;
        let funds = self.account().wallet_funding(&VenueId::Polymarket, None).await?;
        let balance = convertible(self, &funds, escrow)?;
        let approval = balance.allowance < terms.amount_in;
        Ok(ConversionPreview {
            chain_id: POLYMARKET.id,
            payer: self.address(),
            balance: balance.balance,
            approval,
            transactions: if approval { 2 } else { 1 },
            covered: balance.balance >= terms.amount_in,
        })
    }
}

/// The transfer's conversion is the request's: both or neither, its terms, from Polymarket's
/// wallet, its floor kept.
pub(super) fn check_conversion(
    intent: &TransferIntent,
    requested: Option<&ConversionTerms>,
) -> std::result::Result<(), VerificationError> {
    match (&intent.conversion, requested) {
        (None, None) => Ok(()),
        (Some(conversion), Some(requested)) => check_terms(intent, conversion, requested),
        _ => Err(changed(details::FUNDING)),
    }
}

fn check_terms(
    intent: &TransferIntent,
    conversion: &WalletConversion,
    requested: &ConversionTerms,
) -> std::result::Result<(), VerificationError> {
    let terms = ConversionTerms::new(conversion.amount_in, conversion.min_out);
    let ok = intent.from == VenueId::Polymarket
        && terms == *requested
        && intent.amount_in == conversion.min_out
        && terms.valid();
    if ok { Ok(()) } else { Err(changed(details::FUNDING)) }
}

/// Where a conversion stands after one pass.
pub(super) enum Progress {
    /// Converted: the take may pay.
    Ready,
    /// Sent or being confirmed: read again later.
    Waiting,
    /// The sender sent nothing (declined, timed out): running again asks again.
    NotSent,
}

/// A conversion's terms, journalled under `conversion:{id}:terms` and
/// `conversion-intent:{transfer id}`: a conversion answered with others stops.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Terms {
    id: Uuid,
    escrow: Address,
    payer: Address,
    amount_in: Amount,
    min_out: Amount,
    nonce: Amount,
    deadline: Amount,
}

/// A transaction asked of the sender: saved before asking, with its hash once returned;
/// `refused` once the sender said it sent nothing.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Asked {
    hash: Option<B256>,
    #[serde(default)]
    refused: bool,
}

#[derive(Serialize)]
struct Report {
    #[serde(skip_serializing_if = "Option::is_none")]
    tx_hash: Option<B256>,
}

/// One pass of the transfer's conversion: prepares it (idempotent), checks it, and sends what it
/// still needs.
pub(super) async fn convert(
    session: &Session,
    intent: &TransferIntent,
    requested: &ConversionTerms,
    sender: &dyn TransactionSender,
) -> Result<Progress> {
    let call = Call::new(&ops::PREPARE_TRANSFER_CONVERSION).segment(intent.id);
    let conversion: WalletConversion = session.transport().send(call, session.auth()).await?;
    check_terms(intent, &conversion, requested)?;
    let terms = pinned(session, &conversion)?;
    let journal = session.journal();
    let key = |record: &str| session.record_key(&format!("conversion:{}:{record}", terms.id));
    let _turn = journal.lock(&key("turn")).await;
    for record in [session.record_key(&format!("conversion-intent:{}", intent.id)), key("terms")] {
        if let Some(saved) = records::load::<Terms>(journal, &record).await?
            && saved != terms
        {
            return Err(differs(details::FUNDING).into());
        }
        records::save(journal, &record, &terms, false).await?;
    }
    let state = conversion.state.clone().unwrap_or(ConversionState::Prepared);
    match state {
        ConversionState::Ready => {
            let received =
                conversion.received.as_deref().and_then(|text| text.parse::<U256>().ok());
            return if received.is_some_and(|received| received >= terms.min_out.get())
                && conversion.tx_hash.is_some()
            {
                Ok(Progress::Ready)
            } else {
                Err(differs(details::FUNDING).into())
            };
        }
        ConversionState::Expired => return Err(stale(details::DEADLINE).into()),
        _ => {}
    }

    let ids = (
        format!("lamarge.conversion.{}.approve", terms.id),
        format!("lamarge.conversion.{}.convert", terms.id),
    );
    let sent_key = key("sent");
    let mut sent = records::load::<Asked>(journal, &sent_key).await?;
    if let Some(asked) = &mut sent
        && asked.hash.is_none()
        && !asked.refused
    {
        // Asked with no answer recorded: only the sender knows whether it went.
        match status(sender, &ids.1).await {
            SendStatus::Sent(hash) => asked.hash = Some(hash),
            SendStatus::NotSent => asked.refused = true,
            _ => return Ok(Progress::Waiting),
        }
        records::save(journal, &sent_key, asked, true).await?;
    }
    if let Some(hash) = sent.as_ref().and_then(|asked| asked.hash) {
        let attempts = conversion.attempts.as_deref().unwrap_or_default();
        if !attempts.iter().any(|attempt| attempt.parse::<B256>().is_ok_and(|seen| seen == hash)) {
            report(session, intent, Some(hash)).await?;
        }
        return Ok(Progress::Waiting);
    }
    let retry = sent.as_ref().is_some_and(|asked| asked.refused);
    if !retry && state == ConversionState::Pending {
        // Fenced by another client: it sends, this run follows.
        return Ok(Progress::Waiting);
    }
    let now = session.transport().now().timestamp();
    let deadline = i64::try_from(terms.deadline.get()).unwrap_or(i64::MAX);
    if !(state == ConversionState::Prepared || state == ConversionState::Pending)
        || deadline <= now
        || deadline > now + DEADLINE_WINDOW_SECONDS
    {
        return Err(stale(details::DEADLINE).into());
    }

    // The capability, the escrow and the balance, read fresh before anything is asked.
    if check_capability(session).await? != terms.escrow {
        return Err(differs(details::ESCROW).into());
    }
    let funds = session.account().wallet_funding(&VenueId::Polymarket, None).await?;
    let balance = convertible(session, &funds, terms.escrow)?;
    if balance.balance < terms.amount_in {
        return Err(changed(details::WALLET_FUNDS).into());
    }
    if balance.allowance < terms.amount_in {
        return approve(session, sender, &terms, &key("approval"), &ids.0).await;
    }
    let fence = state == ConversionState::Prepared;
    send_convert(session, intent, sender, &terms, &sent_key, ids.1, fence).await
}

/// The convert call, journalled before it is asked; fenced first while the conversion is only
/// prepared.
async fn send_convert(
    session: &Session,
    intent: &TransferIntent,
    sender: &dyn TransactionSender,
    terms: &Terms,
    sent_key: &str,
    id: String,
    fence: bool,
) -> Result<Progress> {
    let journal = session.journal();
    records::save(journal, sent_key, &Asked::default(), false).await?;
    if fence {
        // The fence comes before the prompt; after a lost answer, no prompt follows.
        let call = Call::new(&ops::FENCE_TRANSFER_CONVERSION)
            .segment(intent.id)
            .json(&Report { tx_hash: None })?;
        session.transport().send::<WalletConversion>(call, session.auth()).await?;
    }
    let mut data = CONVERT.to_vec();
    for word in [terms.amount_in, terms.min_out, terms.nonce, terms.deadline] {
        data.extend_from_slice(&word.get().to_be_bytes::<32>());
    }
    match sender.send(transaction(id, terms.escrow, data)).await {
        Ok(hash) => {
            records::save(journal, sent_key, &Asked { hash: Some(hash), refused: false }, true)
                .await?;
            report(session, intent, Some(hash)).await?;
            Ok(Progress::Waiting)
        }
        Err(SendError::NotSent(error)) => {
            tracing::info!(%error, "conversion not sent");
            records::save(journal, sent_key, &Asked { hash: None, refused: true }, true).await?;
            Ok(Progress::NotSent)
        }
        Err(error) => {
            tracing::warn!(%error, "conversion outcome unknown");
            Ok(Progress::Waiting)
        }
    }
}

/// The escrow's allowance, asked once: an `approve` sent or of unknown outcome is waited for
/// until the allowance shows it.
async fn approve(
    session: &Session,
    sender: &dyn TransactionSender,
    terms: &Terms,
    key: &str,
    id: &str,
) -> Result<Progress> {
    let journal = session.journal();
    if let Some(asked) = records::load::<Asked>(journal, key).await? {
        if asked.hash.is_some() {
            return Ok(Progress::Waiting);
        }
        match status(sender, id).await {
            SendStatus::Sent(hash) => {
                records::save(journal, key, &Asked { hash: Some(hash), refused: false }, true)
                    .await?;
                return Ok(Progress::Waiting);
            }
            SendStatus::NotSent => {}
            _ => return Ok(Progress::Waiting),
        }
    }
    records::save(journal, key, &Asked::default(), false).await?;
    let mut data = APPROVE.to_vec();
    data.extend_from_slice(&[0; 12]);
    data.extend_from_slice(terms.escrow.as_slice());
    data.extend_from_slice(&terms.amount_in.get().to_be_bytes::<32>());
    match sender.send(transaction(id.to_owned(), native_usdc(), data)).await {
        Ok(hash) => {
            records::save(journal, key, &Asked { hash: Some(hash), refused: false }, true).await?;
            Ok(Progress::Waiting)
        }
        Err(SendError::NotSent(error)) => {
            tracing::info!(%error, "approval not sent");
            records::delete(journal, key, true).await?;
            Ok(Progress::NotSent)
        }
        Err(error) => {
            tracing::warn!(%error, "approval outcome unknown");
            Ok(Progress::Waiting)
        }
    }
}

fn transaction(id: String, to: Address, data: Vec<u8>) -> Transaction {
    Transaction { id, chain_id: POLYMARKET.id, to, data, value: U256::ZERO }
}

/// What the sender knows of `id`; a failed lookup knows nothing.
async fn status(sender: &dyn TransactionSender, id: &str) -> SendStatus {
    sender.status(id).await.unwrap_or(SendStatus::Unknown)
}

async fn report(session: &Session, intent: &TransferIntent, hash: Option<B256>) -> Result<()> {
    let call = Call::new(&ops::REPORT_TRANSFER_CONVERSION)
        .segment(intent.id)
        .json(&Report { tx_hash: hash })?;
    session.transport().send::<WalletConversion>(call, session.auth()).await.map(drop)
}

/// The prepared conversion on the pinned deployment: Polygon, the environment's escrow, the
/// session's address, native USDC and the pinned converter.
fn pinned(session: &Session, conversion: &WalletConversion) -> Result<Terms> {
    let escrow = deployments::escrow(POLYMARKET.id, session.escrow_environment());
    let ok = conversion.chain_id == Some(POLYMARKET.id)
        && conversion.escrow.is_some()
        && conversion.escrow == escrow
        && conversion.payer == Some(session.address())
        && conversion.token == Some(native_usdc())
        && conversion.converter == Some(CONVERTER);
    let (Some(id), Some(escrow), Some(nonce), Some(deadline), true) =
        (conversion.id, conversion.escrow, conversion.nonce, conversion.deadline, ok)
    else {
        return Err(differs(details::FUNDING).into());
    };
    Ok(Terms {
        id,
        escrow,
        payer: session.address(),
        amount_in: conversion.amount_in,
        min_out: conversion.min_out,
        nonce,
        deadline,
    })
}

/// Polymarket's advertised conversion is the pinned one; its escrow.
async fn check_capability(session: &Session) -> Result<Address> {
    let venues = session.client().market().venues().await?;
    let chain = venues
        .iter()
        .find(|venue| venue.id == VenueId::Polymarket && venue.enabled)
        .and_then(|venue| venue.chain.as_ref())
        .ok_or(unsupported(details::VENUE))?;
    if deployments::escrow(POLYMARKET.id, session.escrow_environment()) != Some(chain.escrow) {
        return Err(differs(details::ESCROW).into());
    }
    let capability = chain
        .wallet_funding
        .as_ref()
        .and_then(|funding| funding.conversion.as_ref())
        .ok_or(unsupported(details::WALLET_FUNDING))?;
    let pinned = capability.converter == CONVERTER
        && capability.token == native_usdc()
        && capability.decimals == POLYMARKET.decimals
        && capability.min_out_bps == u32::from(deployments::WALLET_SWAP_MIN_BPS);
    if pinned { Ok(chain.escrow) } else { Err(differs(details::WALLET_FUNDING).into()) }
}

/// The native USDC balance and allowance of a fresh, checked wallet read on `escrow`.
fn convertible(
    session: &Session,
    funds: &crate::model::WalletFunding,
    escrow: Address,
) -> Result<crate::model::ConversionBalance> {
    let (venue, payer, environment) =
        (&VenueId::Polymarket, session.address(), session.escrow_environment());
    check_wallet_funding(funds, venue, payer, environment, session.transport().now())?;
    let balance = funds.conversion.as_ref().ok_or(unsupported(details::WALLET_FUNDING))?;
    if funds.escrow != escrow || balance.token != native_usdc() {
        return Err(differs(details::WALLET_FUNDS).into());
    }
    Ok(balance.clone())
}

#[cfg(test)]
mod tests;
