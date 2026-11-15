//! Take checks: escrow payments, Lighter and Aster payments, destinations and minimums.
//!
//! [`check_take`] is the one entry: it binds every fill and the one payment request to the
//! caller's terms, and whenever the take delivers into Lighter, to the owner Lighter itself
//! reports for the delivering account.

use std::collections::{BTreeSet, HashSet};

use alloy_primitives::{Address, B256, U256, hex};
use chrono::{DateTime, Utc};
use serde_json::Value;

use super::{
    LighterOwners,
    details::{self, at, changed, differs, stale, unsupported},
    differs_unless, escrow, funding, signed, sum,
};
use crate::{
    deployments,
    error::VerificationError,
    model::{
        AsterTransferItem, Destination, Fill, FillRole, Funding, LighterTransferItem, Take,
        TakePreparation, TakeSignRequest, ToSignKind, WalletFunding,
    },
    protocol::{
        aster,
        eip712::{self, Message, PayTake},
        fees, lighter,
        signing::Verified,
    },
    types::{Amount, Bps, EscrowEnvironment, MaxParts, VenueId},
};

/// The most seconds to a take's `pay_by`.
const PAY_WINDOW_SECONDS: i64 = 90;

/// Where a take delivers on its receiving venue, as the caller states it: never taken from the
/// API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Delivery {
    /// Into this account, linked on the receiving venue (for Lighter, the owner of the
    /// delivering account index).
    Account(Address),
    /// By plain transfer to this address on the receiving venue's escrow chain.
    Wallet(Address),
}

impl Delivery {
    fn recipient(self) -> Address {
        match self {
            Self::Account(address) | Self::Wallet(address) => address,
        }
    }

    fn wallet(self) -> bool {
        matches!(self, Self::Wallet(_))
    }
}

/// The caller's terms a take is checked against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TakeIntent {
    pub(crate) from: VenueId,
    pub(crate) to: VenueId,
    /// The most paid, in `from` base units.
    pub(crate) amount_in: Amount,
    /// The least received, in `to` base units.
    pub(crate) min_out: Amount,
    pub(crate) max_parts: MaxParts,
    /// `None`: no fee accepted.
    pub(crate) max_fee_bps: Option<Bps>,
    /// Paid from the escrow-chain wallet rather than the venue account.
    pub(crate) wallet_funding: bool,
    pub(crate) delivery: Delivery,
    /// The session address: the payer and signer.
    pub(crate) payer: Address,
    /// The payer's linked Lighter account index, for a take paid from Lighter.
    pub(crate) payer_account: Option<u64>,
    /// Signed at once with a transfer's withdrawal: a route's pay window, and signable only
    /// until the at-once cutoff leaves time to sign.
    pub(crate) at_once: bool,
}

/// What the flow read for the check: Lighter's owners of the accounts [`lighter_accounts`]
/// names, and for a take from wallet funds, a fresh read of them (excluding this take).
#[derive(Debug, Clone, Copy)]
pub(crate) struct TakeReads<'a> {
    pub(crate) lighter_owners: &'a LighterOwners,
    pub(crate) wallet_funds: Option<&'a WalletFunding>,
}

/// Checks a prepared take (`POST /v1/takes`) against `intent` at `now` and returns its one
/// payment to sign: a `PayTake` from an escrow venue, the Lighter owner message of a take paid
/// from Lighter, or the Aster transfer request of a take paid from Aster.
pub(crate) fn check_take(
    prepared: &TakePreparation,
    intent: &TakeIntent,
    reads: TakeReads<'_>,
    environment: EscrowEnvironment,
    now: DateTime<Utc>,
) -> Result<Verified, VerificationError> {
    let take = &prepared.take;
    if intent.delivery.wallet() && wallet_token(&intent.to).is_none() {
        return Err(unsupported(details::DESTINATION));
    }
    let funding = if intent.wallet_funding { Funding::Wallet } else { Funding::Venue };
    let funded = |value: &Option<Funding>| value.as_ref().unwrap_or(&Funding::Venue) == &funding;
    if !funded(&take.funding) || !take.fills.iter().all(|fill| funded(&fill.funding)) {
        return Err(differs(details::FUNDING));
    }
    let kind = payment_kind(&intent.from)?;
    if !prepared.to_sign.iter().all(|item| *item.kind() == kind) {
        return Err(differs(details::TO_SIGN_KIND));
    }
    check_minimum_received(take, intent)?;
    if intent.to == VenueId::Lighter {
        check_lighter_destination(&take.fills, intent.delivery.recipient(), reads.lighter_owners)?;
    }
    if intent.wallet_funding {
        let funds = reads.wallet_funds.ok_or(changed(details::WALLET_FUNDS))?;
        let spendable =
            funding::check_wallet_funding(funds, &intent.from, intent.payer, environment, now)?;
        if spendable < intent.amount_in.get() {
            return Err(changed(details::WALLET_FUNDS));
        }
    }
    let now = now.timestamp();
    match &intent.from {
        VenueId::Lighter => {
            check_lighter_take(prepared, intent, reads.lighter_owners, environment, now)
        }
        VenueId::Aster => check_aster_take(prepared, intent, now),
        _ => check_escrow_take(prepared, intent, environment, now),
    }
}

/// The Lighter account indexes whose owners [`check_take`] needs: a delivery into Lighter's
/// account, and a Lighter payment's two accounts.
pub(crate) fn lighter_accounts(prepared: &TakePreparation, intent: &TakeIntent) -> BTreeSet<u64> {
    let mut indexes = BTreeSet::new();
    if intent.to == VenueId::Lighter {
        let delivered = prepared.take.fills.iter().filter_map(|fill| fill.delivery_to.as_deref());
        indexes.extend(delivered.filter_map(account_index));
    }
    for item in &prepared.to_sign {
        if let TakeSignRequest::Lighter(item) = item {
            indexes.extend([item.typed_data.from_account_index, item.typed_data.to_account_index]);
        }
    }
    indexes
}

/// The nonce, in microseconds, of a take's Aster transfer request, as it must be named to have
/// LaMarge issue the request again.
pub(crate) fn aster_nonce(item: &AsterTransferItem) -> Result<u64, VerificationError> {
    Ok(aster_transfer(&item.typed_data)?.1.nonce)
}

/// Until when, in Unix seconds, a reserved take can still be signed: its earliest `pay_by`,
/// sooner for an Aster transfer whose nonce must stay ahead, unless LaMarge issues that request
/// again right before it is signed (`aster_reissue`).
pub(crate) fn signable_until(prepared: &TakePreparation) -> Result<i64, VerificationError> {
    let mut until = prepared.take.fills.iter().map(|fill| fill.pay_by.timestamp()).min();
    if !prepared.aster_reissue {
        for item in &prepared.to_sign {
            if let TakeSignRequest::Aster(item) = item {
                let ahead = nonce_seconds(aster_nonce(item)?) - aster::NONCE_AHEAD_SECONDS;
                until = Some(until.map_or(ahead, |until| until.min(ahead)));
            }
        }
    }
    until.ok_or(differs(details::FILLS))
}

/// The server's submission cutoff, in Unix seconds, for a take signed at once with its
/// withdrawal.
pub(crate) fn at_once_cutoff(prepared: &TakePreparation) -> Result<i64, VerificationError> {
    Ok(signable_until(prepared)? - signed(deployments::AT_ONCE_SEND_MARGIN_SECONDS))
}

/// The last second a take may be signed: before its [`signable_until`], or signed at once, while
/// the at-once cutoff leaves its signing seconds.
fn signing_bound(
    prepared: &TakePreparation,
    intent: &TakeIntent,
) -> Result<i64, VerificationError> {
    let bound = if intent.at_once {
        at_once_cutoff(prepared)? - signed(deployments::AT_ONCE_SIGNING_SECONDS)
    } else {
        signable_until(prepared)?
    };
    Ok(bound - 1)
}

fn nonce_seconds(nonce: u64) -> i64 {
    signed(nonce / 1_000_000)
}

/// The request kind paying a take from `from`.
fn payment_kind(from: &VenueId) -> Result<ToSignKind, VerificationError> {
    match from {
        VenueId::Lighter => Ok(ToSignKind::LighterTransfer),
        VenueId::Aster => Ok(ToSignKind::AsterTransfer),
        VenueId::Hyperliquid | VenueId::Polymarket | VenueId::Gmx => Ok(ToSignKind::PayTake),
        VenueId::Unknown(_) => Err(unsupported(details::VENUE)),
    }
}

fn wallet_token(venue: &VenueId) -> Option<Address> {
    deployments::chain(venue)?.wallet_token
}

fn account_index(text: &str) -> Option<u64> {
    if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) {
        text.parse().ok()
    } else {
        None
    }
}

/// A wallet take's `minimum_received` is the least its recipient gets of the wallet token for
/// all it delivers, as the escrow bounds it; a venue take's is none.
fn check_minimum_received(take: &Take, intent: &TakeIntent) -> Result<(), VerificationError> {
    let error = differs(details::MINIMUM_RECEIVED);
    let expected = if intent.delivery.wallet() {
        let total = sum(take.fills.iter().map(|fill| fill.amount_out.get()), error)?;
        let least = fees::minimum_received(&intent.to, total).ok_or(error)?;
        Some((wallet_token(&intent.to).ok_or(error)?, least))
    } else {
        None
    };
    let served = take.minimum_received.as_ref().map(|least| (least.token, least.amount.get()));
    if served == expected { Ok(()) } else { Err(error) }
}

/// Every fill delivering into Lighter names one account index, and Lighter says its owner is the
/// recipient.
fn check_lighter_destination(
    fills: &[Fill],
    recipient: Address,
    owners: &LighterOwners,
) -> Result<(), VerificationError> {
    let indexes: HashSet<Option<u64>> =
        fills.iter().map(|fill| fill.delivery_to.as_deref().and_then(account_index)).collect();
    let mut indexes = indexes.into_iter();
    let (Some(Some(index)), None) = (indexes.next(), indexes.next()) else {
        return Err(differs(details::FILLS_DELIVERY_TO));
    };
    if owners.get(&index) == Some(&recipient) {
        Ok(())
    } else {
        Err(differs(details::DESTINATION_OWNER))
    }
}

/// What every fill of a take must say: the taker's, on the asked venues, paid by the payer,
/// delivered to the recipient as asked, under a swap id of that kind, for a positive amount.
fn check_fill(fill: &Fill, intent: &TakeIntent) -> Result<(), VerificationError> {
    let wallet = intent.delivery.wallet();
    let destination = if wallet { Destination::Wallet } else { Destination::Venue };
    differs_unless([
        (fill.role == FillRole::Taker, details::FILLS_ROLE),
        (fill.from_venue == intent.from, details::FILLS_FROM_VENUE),
        (fill.to_venue == intent.to, details::FILLS_TO_VENUE),
        (fill.payer == intent.payer, details::FILLS_PAYER),
        (fill.taker_recipient == intent.delivery.recipient(), details::FILLS_TAKER_RECIPIENT),
        (fill.destination == destination, details::FILLS_DESTINATION),
        (fees::swap_fee(&fill.swap_id).wallet == wallet, details::FILLS_SWAP_ID),
        (!fill.amount_in.get().is_zero(), details::FILLS_AMOUNT_IN),
    ])
}

/// Delivered inside a venue, a fill goes to the taker's own account there: its Aster address,
/// or a Lighter index (whose owner only Lighter confirms); through an escrow, to none.
fn check_delivery(fill: &Fill, recipient: Address) -> Result<(), VerificationError> {
    let to = fill.delivery_to.as_deref();
    let ok = match fill.to_venue {
        VenueId::Aster => to.and_then(eip712::address_text) == Some(recipient),
        VenueId::Lighter => to.and_then(account_index).is_some(),
        _ => to.is_none(),
    };
    if ok { Ok(()) } else { Err(differs(details::FILLS_DELIVERY_TO)) }
}

/// `pay_by` within the pay window, `t1` after it within the maker's timeout.
fn check_windows(fill: &Fill, now: i64, pay_window: i64) -> Result<(), VerificationError> {
    let (pay_by, t1) = (fill.pay_by.timestamp(), fill.t1.timestamp());
    if pay_by <= now || pay_by > now + pay_window {
        return Err(stale(details::FILLS_PAY_BY));
    }
    if t1 <= pay_by || t1 > now + signed(deployments::MAKER_TIMEOUT_SECONDS) + 30 {
        return Err(differs(details::FILLS_T1));
    }
    Ok(())
}

/// A take paid from an escrow venue: one `PayTake` in `environment`'s escrow on `from`, paying
/// every fill exactly, all within the caller's amounts, parts, fee, recipient and windows.
fn check_escrow_take(
    prepared: &TakePreparation,
    intent: &TakeIntent,
    environment: EscrowEnvironment,
    now: i64,
) -> Result<Verified, VerificationError> {
    let fills = &prepared.take.fills;
    if fills.is_empty() || fills.len() > usize::from(intent.max_parts.get()) {
        return Err(differs(details::FILLS));
    }
    if fills.iter().map(|fill| fill.id).collect::<HashSet<_>>().len() != fills.len() {
        return Err(differs(details::FILLS_ID));
    }
    if fills.iter().map(|fill| fill.swap_id).collect::<HashSet<_>>().len() != fills.len() {
        return Err(differs(details::FILLS_SWAP_ID));
    }
    let total_in = sum(fills.iter().map(|fill| fill.amount_in.get()), differs(details::AMOUNT_IN))?;
    let total_out =
        sum(fills.iter().map(|fill| fill.amount_out.get()), differs(details::AMOUNT_OUT))?;
    if total_in.is_zero() || total_in > intent.amount_in.get() {
        return Err(differs(details::AMOUNT_IN));
    }
    if total_out < intent.min_out.get() {
        return Err(differs(details::AMOUNT_OUT));
    }
    let [TakeSignRequest::Typed(item)] = &prepared.to_sign[..] else {
        return Err(differs(details::TO_SIGN));
    };
    let (parsed, domain) =
        escrow::check_escrow_typed::<PayTake>(&item.typed_data, &intent.from, environment)?;
    let pay = &parsed.message;
    if item.signer != intent.payer || pay.payer != intent.payer {
        return Err(differs(details::PAYER));
    }
    if !item.fill_id.is_some_and(|id| fills.iter().any(|fill| *fill.id.as_uuid() == id)) {
        return Err(differs(details::FILL_ID));
    }
    if pay.payments.len() != fills.len() {
        return Err(differs(details::PAYMENTS));
    }
    let receive = Receive::of(&intent.to, intent.delivery.wallet(), environment)?;
    let pay_window = if intent.at_once {
        signed(deployments::ROUTE_PAY_WINDOW_SECONDS)
    } else {
        PAY_WINDOW_SECONDS
    };
    for (index, fill) in fills.iter().enumerate() {
        check_paid_fill(fill, pay, intent, &receive, now, pay_window)
            .map_err(|error| at(error, index))?;
    }
    if pay.deposit > total_in {
        return Err(differs(details::DEPOSIT));
    }
    let not_after = signing_bound(prepared, intent)?;
    Verified::typed(eip712::typed(pay, domain), parsed.digest, intent.payer, Some(not_after))
}

/// Where a payment's delivery must be received: the receiving chain, its token (the wallet token
/// for a wallet delivery) and escrow.
struct Receive {
    chain_id: U256,
    token: Address,
    escrow: Address,
}

impl Receive {
    fn of(
        to: &VenueId,
        wallet: bool,
        environment: EscrowEnvironment,
    ) -> Result<Self, VerificationError> {
        let chain = deployments::chain(to).ok_or(unsupported(details::VENUE))?;
        let token = if wallet { chain.wallet_token } else { Some(chain.token) };
        Ok(Self {
            chain_id: U256::from(chain.id),
            token: token.ok_or(unsupported(details::DESTINATION))?,
            escrow: deployments::contract(to, environment).ok_or(unsupported(details::VENUE))?,
        })
    }
}

/// One fill of an escrow take and its one payment in `pay`.
fn check_paid_fill(
    fill: &Fill,
    pay: &PayTake,
    intent: &TakeIntent,
    receive: &Receive,
    now: i64,
    pay_window: i64,
) -> Result<(), VerificationError> {
    check_fill(fill, intent)?;
    if fill.amount_out.get().is_zero() {
        return Err(differs(details::FILLS_AMOUNT_OUT));
    }
    check_delivery(fill, intent.delivery.recipient())?;
    check_windows(fill, now, pay_window)?;
    let (t1, t2) = (fill.t1.timestamp(), fill.t2.timestamp());
    if t2 <= t1 || t2 > now + signed(deployments::TAKER_TIMEOUT_SECONDS) + 30 {
        return Err(differs(details::FILLS_T2));
    }
    let mut matching = pay.payments.iter().filter(|payment| payment.swapId == fill.swap_id);
    let (Some(payment), None) = (matching.next(), matching.next()) else {
        return Err(differs(details::PAYMENTS_SWAP_ID));
    };
    let (amount_in, amount_out) = (fill.amount_in.get(), fill.amount_out.get());
    let dex = payment.dex == 0 || intent.from == VenueId::Hyperliquid && payment.dex == u32::MAX;
    differs_unless([
        (payment.payer == intent.payer, details::PAYMENTS_PAYER),
        (payment.recipient == fill.maker_recipient, details::PAYMENTS_RECIPIENT),
        (payment.hashlock == fill.hashlock, details::PAYMENTS_HASHLOCK),
        (payment.amount == amount_in, details::PAYMENTS_AMOUNT),
    ])?;
    // The fee rides on the payment, which the claim splits: the maker's price is the rest.
    fees::check_fee(
        &fill.swap_id,
        fill.taker_fee.get(),
        amount_in,
        amount_out,
        false,
        intent.max_fee_bps,
    )?;
    differs_unless([
        (i64::try_from(payment.refundAt).ok() == Some(t2), details::PAYMENTS_REFUND_AT),
        (
            payment.deadline == U256::try_from(fill.pay_by.timestamp()).unwrap_or(U256::MAX),
            details::PAYMENTS_DEADLINE,
        ),
        (dex, details::PAYMENTS_DEX),
        (payment.orderId == *fill.order_id.as_b256(), details::PAYMENTS_ORDER_ID),
        (payment.sequence == fill.sequence, details::PAYMENTS_SEQUENCE),
        (payment.amountOut == amount_out, details::PAYMENTS_AMOUNT_OUT),
        (
            payment.receiveRecipient == intent.delivery.recipient(),
            details::PAYMENTS_RECEIVE_RECIPIENT,
        ),
        (
            payment.receiveChainId == receive.chain_id
                && payment.receiveToken == receive.token
                && payment.receiveEscrow == receive.escrow,
            details::PAYMENTS_RECEIVE,
        ),
    ])
}

/// A take paid inside its venue: exactly one fill, paid by one request of the payer, within the
/// caller's amounts and fee (taken on delivery), delivered as asked, in the pay window.
fn check_venue_fill<'a>(
    prepared: &'a TakePreparation,
    intent: &TakeIntent,
    signer: Address,
    fill_id: &uuid::Uuid,
    now: i64,
) -> Result<&'a Fill, VerificationError> {
    let ([fill], [_]) = (&prepared.take.fills[..], &prepared.to_sign[..]) else {
        return Err(differs(details::TO_SIGN));
    };
    if signer != intent.payer {
        return Err(differs(details::TO_SIGN_SIGNER));
    }
    check_fill(fill, intent)?;
    if fill.id.as_uuid() != fill_id {
        return Err(differs(details::FILL_ID));
    }
    if fill.amount_in > intent.amount_in {
        return Err(differs(details::FILLS_AMOUNT_IN));
    }
    if fill.amount_out < intent.min_out {
        return Err(differs(details::FILLS_AMOUNT_OUT));
    }
    fees::check_fee(
        &fill.swap_id,
        fill.taker_fee.get(),
        fill.amount_in.get(),
        fill.amount_out.get(),
        true,
        intent.max_fee_bps,
    )?;
    check_delivery(fill, intent.delivery.recipient())?;
    check_windows(fill, now, PAY_WINDOW_SECONDS)?;
    Ok(fill)
}

/// A take paid from Lighter: the owner message of one internal transfer of exactly the fill's
/// amount from the payer's linked account (Lighter says the payer owns it) to the maker's (Lighter
/// says the maker's recipient owns it), named by the fill's memo, by `environment`'s LaMarge key,
/// with Lighter's fee at most 1 USDC.
fn check_lighter_take(
    prepared: &TakePreparation,
    intent: &TakeIntent,
    owners: &LighterOwners,
    environment: EscrowEnvironment,
    now: i64,
) -> Result<Verified, VerificationError> {
    let Some(TakeSignRequest::Lighter(LighterTransferItem {
        signer,
        fill_id,
        typed_data: terms,
        ..
    })) = prepared.to_sign.first()
    else {
        return Err(differs(details::TO_SIGN));
    };
    let fill = check_venue_fill(prepared, intent, *signer, fill_id.as_uuid(), now)?;
    let memo = lighter::fill_memo(fill.id);
    if Some(terms.from_account_index) != intent.payer_account
        || terms.amount != fill.amount_in
        || terms.memo != hex::encode(memo)
    {
        return Err(differs(details::LIGHTER_TRANSFER));
    }
    // Pinned to the environment: another environment's key is another registration.
    if terms.api_key_index != deployments::slots(environment).lighter_key_index {
        return Err(differs(details::LIGHTER_KEY));
    }
    if terms.usdc_fee.get() > deployments::LIGHTER_MAX_TAKER_FEE {
        return Err(differs(details::LIGHTER_FEE));
    }
    if owners.get(&terms.to_account_index) != Some(&fill.maker_recipient) {
        return Err(differs(details::LIGHTER_MAKER_ACCOUNT));
    }
    if owners.get(&terms.from_account_index) != Some(&intent.payer) {
        return Err(differs(details::LIGHTER_PAYER_ACCOUNT));
    }
    let units = |amount: Amount| {
        u64::try_from(amount.get()).map_err(|_| differs(details::LIGHTER_TRANSFER))
    };
    let transfer = lighter::Transfer {
        nonce: terms.nonce,
        from: terms.from_account_index,
        from_route: lighter::COLLATERAL_ROUTE,
        api_key: terms.api_key_index,
        to: terms.to_account_index,
        to_route: lighter::COLLATERAL_ROUTE,
        asset: lighter::USDC_ASSET,
        amount: units(terms.amount)?,
        fee: units(terms.usdc_fee)?,
        memo,
    };
    let not_after = signing_bound(prepared, intent)?;
    let message = lighter::transfer_message(&transfer, deployments::LIGHTER.id);
    Ok(Verified::text(message, intent.payer, Some(not_after)))
}

/// A take paid from Aster: the payer's transfer request (`Message` at chain 1666) of the fill's
/// amount, as Aster carries it in six decimals rounded up, to the maker's recipient under the
/// fill's id, its nonce far enough ahead for LaMarge to relay it and within Aster's window when
/// relayed by `pay_by`, signable only while that nonce stays ahead.
fn check_aster_take(
    prepared: &TakePreparation,
    intent: &TakeIntent,
    now: i64,
) -> Result<Verified, VerificationError> {
    let Some(TakeSignRequest::Aster(item)) = prepared.to_sign.first() else {
        return Err(differs(details::TO_SIGN));
    };
    let fill = check_venue_fill(prepared, intent, item.signer, item.fill_id.as_uuid(), now)?;
    let (msg, transfer, digest) = aster_transfer(&item.typed_data)?;
    let owed = fill.amount_in.get();
    let amount = transfer.amount >= owed && transfer.amount - owed < deployments::ASTER.step();
    if !amount
        || transfer.to != fill.maker_recipient
        || transfer.user != intent.payer
        || transfer.client_id != aster::client_id(fill.id)
    {
        return Err(differs(details::ASTER_TRANSFER));
    }
    let window = aster::transfer_nonce_window(now, fill.pay_by.timestamp());
    if !window.contains(&i128::from(transfer.nonce)) {
        return Err(stale(details::ASTER_NONCE));
    }
    // Whether or not LaMarge may issue the request again, this one is signed only while its
    // nonce stays ahead.
    let ahead = nonce_seconds(transfer.nonce) - aster::NONCE_AHEAD_SECONDS;
    let not_after = signing_bound(prepared, intent)?.min(ahead - 1);
    let domain = aster::domain(aster::TRANSFER_CHAIN_ID);
    let message = Message { msg };
    Verified::typed(eip712::typed(&message, domain), digest, intent.payer, Some(not_after))
}

/// An Aster transfer request between accounts (`Message` at chain 1666): its `msg`, the transfer
/// it reads as, and its digest.
fn aster_transfer(raw: &Value) -> Result<(String, aster::Transfer, B256), VerificationError> {
    let parsed = eip712::parse_in::<Message>(raw, &aster::domain(aster::TRANSFER_CHAIN_ID))?;
    let msg = parsed.message.msg;
    let transfer = aster::parse_transfer(&msg).ok_or(differs(details::ASTER_TRANSFER))?;
    Ok((msg, transfer, parsed.digest))
}

#[cfg(test)]
mod tests {
    use alloy_signer_local::PrivateKeySigner;
    use serde_json::{Value, json};

    use super::*;
    use crate::{
        error::VerificationKind,
        testing::{
            self, MAKER, NOW, OTHER, OWNER, at, edited,
            fixtures::{
                self, BYBIT, UNKNOWN_FILL, escrow_take, filled, lock, paid, paying, payment,
                requested,
            },
            replaced, time,
        },
    };

    fn none() -> LighterOwners {
        LighterOwners::new()
    }

    fn check(prepared: &Value, intent: &TakeIntent) -> Result<Verified, VerificationError> {
        fixtures::take_check(prepared, intent, &none(), None)
    }

    fn refused(prepared: &Value, intent: &TakeIntent) -> VerificationError {
        check(prepared, intent).unwrap_err()
    }

    #[test]
    fn an_escrow_take_signs_its_one_payment_rebuilt_from_the_pins() {
        let prepared = escrow_take();
        let verified = check(&prepared, &fixtures::escrow_intent()).unwrap();
        let served = eip712::parse::<PayTake>(&prepared["to_sign"][0]["typed_data"]).unwrap();
        assert_eq!(verified.digest(), served.digest);
        // Signable until a second before `pay_by`.
        assert_eq!(verified.not_after(), Some(NOW + 44));
    }

    /// `intents.test.ts` "take validates total spend, slippage, one payment per fill, deadlines and
    /// destination".
    #[test]
    fn a_take_binds_spend_slippage_payments_deadlines_and_destination() {
        let intent = fixtures::escrow_intent();
        let item = escrow_take()["to_sign"][0].clone();
        let lock = escrow_take()["to_sign"][0]["typed_data"]["message"]["payments"][0].clone();
        let types = |p: &mut Value| {
            let types = &mut p["to_sign"][0]["typed_data"]["types"]["LockPayment"];
            types.as_array_mut().unwrap().retain(|field| field["name"] != "receiveRecipient");
        };
        let cases: Vec<(Value, &str)> = vec![
            (edited(escrow_take(), |p| p["to_sign"] = json!([item, item])), details::TO_SIGN),
            (paying("payments", json!([lock, lock])), details::PAYMENTS),
            (paying("payments", json!([])), details::PAYMENTS),
            (paying("payer", json!(OTHER)), details::PAYER),
            (filled("amount_in", json!("2000001")), details::AMOUNT_IN),
            (filled("amount_out", json!("1999999")), details::AMOUNT_OUT),
            (filled("taker_recipient", json!(OTHER)), details::FILLS_TAKER_RECIPIENT),
            (filled("delivery_to", json!("42")), details::FILLS_DELIVERY_TO),
            (filled("role", json!("maker")), details::FILLS_ROLE),
            (filled("from_venue", json!("gmx")), details::FILLS_FROM_VENUE),
            (filled("to_venue", json!("gmx")), details::FILLS_TO_VENUE),
            (filled("payer", json!(OTHER)), details::FILLS_PAYER),
            (filled("pay_by", json!(time(0))), details::FILLS_PAY_BY),
            (filled("pay_by", json!(time(91))), details::FILLS_PAY_BY),
            (filled("t1", json!(time(45))), details::FILLS_T1),
            (filled("t1", json!(time(3631))), details::FILLS_T1),
            (filled("t2", json!(time(3600))), details::FILLS_T2),
            (filled("funding", json!("wallet")), details::FUNDING),
            (paid("note", json!("hidden")), details::MESSAGE),
            (requested("kind", json!("margin_move")), details::TO_SIGN_KIND),
            (requested("fill_id", json!(UNKNOWN_FILL)), details::FILL_ID),
            (requested("signer", json!(OTHER)), details::PAYER),
            (
                edited(escrow_take(), |p| {
                    fixtures::object(&mut p["to_sign"][0]).remove("fill_id");
                }),
                details::FILL_ID,
            ),
            (edited(escrow_take(), types), details::TYPES),
            (edited(escrow_take(), |p| p["take"]["funding"] = json!("wallet")), details::FUNDING),
            (
                edited(escrow_take(), |p| {
                    p["take"]["minimum_received"] = json!({"token": OTHER, "amount": "1"});
                }),
                details::MINIMUM_RECEIVED,
            ),
        ];
        for (index, (prepared, detail)) in cases.iter().enumerate() {
            assert_eq!(refused(prepared, &intent).detail, *detail, "case {index}");
        }
        // Delivered from an escrow offer, T1 may be two minutes after pay_by; T2 a day ahead.
        assert!(check(&filled("t1", json!(time(165))), &intent).is_ok());
        let day = edited(escrow_take(), |p| {
            p["take"]["fills"][0]["t2"] = json!(time(86_100));
            payment(p)["refundAt"] = json!(NOW + 86_100);
        });
        assert!(check(&day, &intent).is_ok());
        let later = edited(escrow_take(), |p| {
            p["take"]["fills"][0]["t2"] = json!(time(86_460));
            payment(p)["refundAt"] = json!(NOW + 86_460);
        });
        assert_eq!(refused(&later, &intent).detail, details::FILLS_T2);
    }

    /// `intents.test.ts` "one take signature binds both amounts, destination, quote and payment of
    /// every fill before signing".
    #[test]
    fn one_signature_binds_every_payment_term() {
        let intent = fixtures::escrow_intent();
        let retired = "0xea5ba9e1c0f5dde572bc08f3af6a3e8b9db73734";
        let cases: [(&str, Value, &str); 16] = [
            ("amountOut", json!("2000001"), details::PAYMENTS_AMOUNT_OUT),
            ("amount", json!("1"), details::PAYMENTS_AMOUNT),
            ("receiveRecipient", json!(MAKER), details::PAYMENTS_RECEIVE_RECIPIENT),
            ("receiveChainId", json!("999"), details::PAYMENTS_RECEIVE),
            ("receiveToken", json!(OWNER), details::PAYMENTS_RECEIVE),
            ("receiveEscrow", json!(OWNER), details::PAYMENTS_RECEIVE),
            ("receiveEscrow", json!(retired), details::PAYMENTS_RECEIVE),
            ("hashlock", json!(testing::ORDER), details::PAYMENTS_HASHLOCK),
            ("recipient", json!(OTHER), details::PAYMENTS_RECIPIENT),
            ("refundAt", json!(NOW + 86_400), details::PAYMENTS_REFUND_AT),
            ("orderId", json!(testing::HASHLOCK), details::PAYMENTS_ORDER_ID),
            ("sequence", json!(8), details::PAYMENTS_SEQUENCE),
            ("swapId", json!(testing::HASHLOCK), details::PAYMENTS_SWAP_ID),
            ("deadline", json!((NOW + 60).to_string()), details::PAYMENTS_DEADLINE),
            ("payer", json!(OTHER), details::PAYMENTS_PAYER),
            ("dex", json!(1), details::PAYMENTS_DEX),
        ];
        for (field, value, detail) in cases {
            let error = refused(&paid(field, value.clone()), &intent);
            assert_eq!((error.detail, error.index), (detail, Some(0)), "{field} = {value}");
        }
        // Hyperliquid alone pays from spot.
        let spot = paid("dex", json!(u32::MAX));
        let polymarket = edited(spot.clone(), |p| {
            p["to_sign"][0]["typed_data"]["domain"]["chainId"] = json!(137);
        });
        assert!(check(&spot, &intent).is_ok());
        let from_polymarket =
            TakeIntent { from: VenueId::Polymarket, to: VenueId::Hyperliquid, ..intent.clone() };
        let polymarket = edited(polymarket, |p| {
            p["take"]["fills"][0]["from_venue"] = json!("polymarket");
            p["take"]["fills"][0]["to_venue"] = json!("hyperliquid");
            *payment(p) = lock(&p["take"]["fills"][0]);
            payment(p)["dex"] = json!(u32::MAX);
        });
        assert_eq!(refused(&polymarket, &from_polymarket).detail, details::PAYMENTS_DEX);
    }

    /// `intents.test.ts` "a take pays and is delivered in one environment's escrows": here only
    /// the client's own environment's.
    #[test]
    fn a_take_pays_and_is_delivered_in_the_environments_escrows() {
        let development =
            |chain: u64| deployments::escrow(chain, EscrowEnvironment::Development).unwrap();
        let prepared = edited(escrow_take(), |p| {
            p["to_sign"][0]["typed_data"]["domain"]["verifyingContract"] = json!(development(999));
            payment(p)["receiveEscrow"] = json!(development(137));
        });
        let dev = |prepared: &Value| {
            let prepared: TakePreparation = fixtures::model(prepared);
            let reads = TakeReads { lighter_owners: &none(), wallet_funds: None };
            check_take(
                &prepared,
                &fixtures::escrow_intent(),
                reads,
                EscrowEnvironment::Development,
                fixtures::now(),
            )
        };
        assert!(dev(&prepared).is_ok());
        assert_eq!(refused(&prepared, &fixtures::escrow_intent()).detail, details::DOMAIN);
        let mixed = edited(prepared, |p| payment(p)["receiveEscrow"] = json!(fixtures::ESCROW_HL));
        assert_eq!(dev(&mixed).unwrap_err().detail, details::PAYMENTS_RECEIVE);
    }

    /// `intents.test.ts` "take signing respects the caller's limit on parts".
    #[test]
    fn a_take_respects_the_callers_limit_on_parts() {
        let first = fixtures::fill("hyperliquid", "polymarket", "2000000", "2000000");
        let mut second = first.clone();
        second["id"] = json!("0192f0a1-b2c3-7d4e-8f90-000000000002");
        second["swap_id"] = json!(format!("0x{}", "cc".repeat(32)));
        let both = fixtures::pay_take(vec![lock(&first), lock(&second)], "0");
        let prepared = fixtures::preparation(
            vec![first.clone(), second.clone()],
            vec![fixtures::pay_item(both)],
        );
        let two = TakeIntent {
            max_parts: MaxParts::new(2).unwrap(),
            ..fixtures::intent("hyperliquid", "polymarket", 4_000_000, 4_000_000)
        };
        assert!(check(&prepared, &two).is_ok());
        let one = TakeIntent { max_parts: MaxParts::new(1).unwrap(), ..two.clone() };
        assert_eq!(refused(&prepared, &one).detail, details::FILLS);
        let same_lock = fixtures::pay_take(vec![lock(&first), lock(&first)], "0");
        let doubled = fixtures::preparation(
            vec![first.clone(), second.clone()],
            vec![fixtures::pay_item(same_lock)],
        );
        assert_eq!(refused(&doubled, &two).detail, details::PAYMENTS_SWAP_ID);
        let short = fixtures::preparation(
            vec![first.clone(), second],
            vec![fixtures::pay_item(fixtures::pay_take(vec![lock(&first)], "0"))],
        );
        assert_eq!(refused(&short, &two).detail, details::PAYMENTS);
        let twice = fixtures::preparation(
            vec![first.clone(), first.clone()],
            vec![fixtures::pay_item(fixtures::pay_take(vec![lock(&first), lock(&first)], "0"))],
        );
        assert_eq!(refused(&twice, &two).detail, details::FILLS_ID);
        let mut swapped = first.clone();
        swapped["id"] = json!("0192f0a1-b2c3-7d4e-8f90-000000000003");
        let same_swap = fixtures::preparation(vec![first.clone(), swapped], vec![]);
        assert_eq!(refused(&same_swap, &two).detail, details::FILLS_SWAP_ID);
    }

    /// `intents.test.ts` "a take deposits only its payer's shortfall with a reusable allowance".
    #[test]
    fn a_take_deposits_only_its_payers_shortfall_and_signs_no_permit() {
        let intent = fixtures::escrow_intent();
        let deposit = |amount: &str| paying("deposit", json!(amount));
        assert!(check(&deposit("400000"), &intent).is_ok());
        assert!(check(&deposit("2000000"), &intent).is_ok());
        assert_eq!(refused(&deposit("2000001"), &intent).detail, details::DEPOSIT);
        let permit =
            json!({"kind": "permit", "signer": OWNER, "typed_data": testing::sample("permit")});
        let with_permit = edited(deposit("400000"), |p| {
            let item = p["to_sign"][0].clone();
            p["to_sign"] = json!([permit, item]);
        });
        assert_eq!(refused(&with_permit, &intent).detail, details::TO_SIGN_KIND);
    }

    /// `intents.test.ts` "a take's fee rides on its payment, as its swap id carries it, within the
    /// taker's consent".
    #[test]
    fn a_takes_fee_rides_on_its_payment_within_consent() {
        let swap = format!("0xde65f326692b00000000000000000bb8{}", "cc".repeat(16));
        let charged = |fee: &str| {
            let mut fill = fixtures::fill("hyperliquid", "polymarket", "1003000", "1005000");
            fill["swap_id"] = json!(swap);
            fill["taker_fee"] = json!(fee);
            fixtures::preparation(
                vec![fill.clone()],
                vec![fixtures::pay_item(fixtures::pay_take(vec![lock(&fill)], "0"))],
            )
        };
        let consent = |bps: Option<u16>| TakeIntent {
            max_fee_bps: bps.map(|bps| Bps::new(bps).unwrap()),
            ..fixtures::intent("hyperliquid", "polymarket", 1_003_000, 1_000_000)
        };
        assert!(check(&charged("3000"), &consent(Some(30))).is_ok());
        assert_eq!(
            refused(&charged("3000"), &consent(Some(29))).detail,
            details::FEE_ABOVE_ACCEPTED
        );
        assert_eq!(refused(&charged("3000"), &consent(None)).detail, details::FEE_ABOVE_ACCEPTED);
        assert_eq!(refused(&charged("0"), &consent(Some(30))).detail, details::FILLS_TAKER_FEE);
    }

    /// `intents.test.ts` "a wallet take pays the address it names, in the wallet token, under a
    /// wallet swap id".
    #[test]
    fn a_wallet_take_pays_the_address_it_names() {
        let intent = fixtures::wallet_intent();
        assert!(check(&fixtures::wallet_take(), &intent).is_ok());
        let pm_token = deployments::chain(&VenueId::Polymarket).unwrap().token;
        let cases: Vec<(Value, &str)> = vec![
            (
                edited(fixtures::wallet_take(), |p| {
                    p["take"]["fills"][0]["destination"] = json!("venue");
                }),
                details::FILLS_DESTINATION,
            ),
            (
                edited(fixtures::wallet_take(), |p| {
                    p["take"]["fills"][0]["swap_id"] = json!(testing::SWAP);
                    payment(p)["swapId"] = json!(testing::SWAP);
                }),
                details::FILLS_SWAP_ID,
            ),
            (
                edited(fixtures::wallet_take(), |p| payment(p)["receiveToken"] = json!(pm_token)),
                details::PAYMENTS_RECEIVE,
            ),
            (
                edited(fixtures::wallet_take(), |p| payment(p)["receiveRecipient"] = json!(OWNER)),
                details::PAYMENTS_RECEIVE_RECIPIENT,
            ),
            (
                edited(fixtures::wallet_take(), |p| {
                    p["take"]["fills"][0]["taker_recipient"] = json!(OWNER);
                }),
                details::FILLS_TAKER_RECIPIENT,
            ),
        ];
        for (index, (prepared, detail)) in cases.iter().enumerate() {
            assert_eq!(refused(prepared, &intent).detail, *detail, "case {index}");
        }
        // The wallet the caller names, never the one the API echoes.
        let echoed = edited(fixtures::wallet_take(), |p| {
            payment(p)["receiveRecipient"] = json!(OWNER);
            p["take"]["fills"][0]["taker_recipient"] = json!(OWNER);
        });
        assert_eq!(refused(&echoed, &intent).detail, details::FILLS_TAKER_RECIPIENT);
        // A venue take refuses each wallet binding.
        let venue = fixtures::escrow_intent();
        let wallet_fill = filled("destination", json!("wallet"));
        assert_eq!(refused(&wallet_fill, &venue).detail, details::FILLS_DESTINATION);
        let usdc = deployments::chain(&VenueId::Polymarket).unwrap().wallet_token.unwrap();
        let wallet_token = paid("receiveToken", json!(usdc));
        assert_eq!(refused(&wallet_token, &venue).detail, details::PAYMENTS_RECEIVE);
        let wallet_swap = edited(escrow_take(), |p| {
            let swap = fixtures::wallet_take()["take"]["fills"][0]["swap_id"].clone();
            p["take"]["fills"][0]["swap_id"] = swap.clone();
            payment(p)["swapId"] = swap;
        });
        assert_eq!(refused(&wallet_swap, &venue).detail, details::FILLS_SWAP_ID);
        // Only an escrow venue delivers to a wallet.
        for to in ["aster", "lighter"] {
            let intent = TakeIntent { to: to.parse().unwrap(), ..intent.clone() };
            let error = refused(&fixtures::wallet_take(), &intent);
            assert_eq!(
                (error.kind, error.detail),
                (VerificationKind::Unsupported, details::DESTINATION)
            );
        }
    }

    /// `intents.test.ts` "a wallet take's least received is the escrow's floor on all it
    /// delivers, a venue take's none": two fills of 1.000001 delivered in one transfer give
    /// ⌈2 000 002 × 0.9995⌉, not each fill's floor added up. Required-nullable: no tolerance for
    /// an answer without it.
    #[test]
    fn a_wallet_takes_least_received_is_the_escrows_floor_on_all_it_delivers() {
        let usdc = deployments::chain(&VenueId::Polymarket).unwrap().wallet_token.unwrap();
        let take = |least: Value, to: &str| -> Take {
            let mut fill = fixtures::fill("hyperliquid", to, "1000001", "1000001");
            fill["destination"] = json!("wallet");
            let prepared = fixtures::preparation(vec![fill.clone(), fill], vec![]);
            let mut take = prepared["take"].clone();
            take["minimum_received"] = least;
            fixtures::model(&take)
        };
        let wallet = fixtures::wallet_intent();
        let least = |amount: &str| json!({"token": usdc, "amount": amount});
        assert!(check_minimum_received(&take(least("1999002"), "polymarket"), &wallet).is_ok());
        for wrong in [
            least("1999001"),
            least("1999004"),
            json!({"token": OTHER, "amount": "1999002"}),
            Value::Null,
        ] {
            let error = check_minimum_received(&take(wrong, "polymarket"), &wallet).unwrap_err();
            assert_eq!(error.detail, details::MINIMUM_RECEIVED);
        }
        let venue = fixtures::escrow_intent();
        assert!(check_minimum_received(&take(Value::Null, "polymarket"), &venue).is_ok());
        let error =
            check_minimum_received(&take(least("1999002"), "polymarket"), &venue).unwrap_err();
        assert_eq!(error.detail, details::MINIMUM_RECEIVED);
        // HyperEVM delivers all of it.
        let hyperevm = TakeIntent { to: VenueId::Hyperliquid, ..wallet };
        let hl_usdc = deployments::chain(&VenueId::Hyperliquid).unwrap().wallet_token.unwrap();
        let hl = |amount: &str| json!({"token": hl_usdc, "amount": amount});
        assert!(check_minimum_received(&take(hl("2000002"), "hyperliquid"), &hyperevm).is_ok());
        assert!(check_minimum_received(&take(hl("1999002"), "hyperliquid"), &hyperevm).is_err());
    }

    /// Sums near `2^256 - 1` refuse rather than wrap.
    #[test]
    fn amounts_near_the_largest_integer_refuse_rather_than_wrap() {
        let max = U256::MAX.to_string();
        let first = fixtures::fill("hyperliquid", "polymarket", &max, "2000000");
        let mut second = first.clone();
        second["id"] = json!("0192f0a1-b2c3-7d4e-8f90-000000000002");
        second["swap_id"] = json!(format!("0x{}", "cc".repeat(32)));
        second["amount_in"] = json!("1");
        let prepared = fixtures::preparation(vec![first, second], vec![]);
        let intent = TakeIntent { amount_in: Amount::new(U256::MAX), ..fixtures::escrow_intent() };
        assert_eq!(refused(&prepared, &intent).detail, details::AMOUNT_IN);
        let mut out = fixtures::fill("hyperliquid", "polymarket", "1", &max);
        out["destination"] = json!("wallet");
        let mut more = out.clone();
        more["amount_out"] = json!("1");
        let wallet = fixtures::preparation(vec![out, more], vec![]);
        let error = refused(
            &edited(wallet, |p| {
                p["take"]["minimum_received"] = json!({"token": OTHER, "amount": "1"});
            }),
            &fixtures::wallet_intent(),
        );
        assert_eq!(error.detail, details::MINIMUM_RECEIVED);
        // A fee near the largest integer: on the payment, taken out of it; on delivery, added
        // to what the offer delivers, which must not wrap.
        let fee_swap = format!("0xde65f326692b00000000000000000bb8{}", "cc".repeat(16));
        let charged = |amount_in: &str| {
            let mut fill = fixtures::fill("hyperliquid", "polymarket", amount_in, "2000000");
            fill["swap_id"] = json!(fee_swap);
            fill["taker_fee"] = json!("3000");
            let payment = fixtures::pay_take(vec![lock(&fill)], "0");
            fixtures::preparation(vec![fill], vec![fixtures::pay_item(payment)])
        };
        let consenting = TakeIntent { max_fee_bps: Bps::new(100), ..intent.clone() };
        assert!(check(&charged(&max), &consenting).is_ok());
        assert_eq!(refused(&charged("2999"), &consenting).detail, details::FILLS_TAKER_FEE);
        let on_delivery = edited(fixtures::aster_take(), |p| {
            let fill = &mut p["take"]["fills"][0];
            fill["swap_id"] = json!(fee_swap.replace("de65f326692b", "cc909c08081a"));
            fill["taker_fee"] = json!("3000");
            fill["amount_out"] = json!(max);
        });
        let aster = TakeIntent { max_fee_bps: Bps::new(100), ..fixtures::aster_intent() };
        assert_eq!(refused(&on_delivery, &aster).detail, details::FILLS_TAKER_FEE);
        // The largest amount itself is fine when it is the whole take.
        let whole = fixtures::fill("hyperliquid", "polymarket", &max, "2000000");
        let prepared = fixtures::preparation(
            vec![whole.clone()],
            vec![fixtures::pay_item(fixtures::pay_take(vec![lock(&whole)], &max))],
        );
        assert!(check(&prepared, &intent).is_ok());
    }

    /// `take.ts:103-106`: whenever a take delivers into Lighter, whatever pays it, Lighter itself
    /// must say the delivering account is the recipient's.
    #[test]
    fn a_take_into_lighter_needs_lighters_word_on_the_account() {
        let owners = fixtures::lighter_owners();
        let into = |from: &str| TakeIntent {
            to: VenueId::Lighter,
            ..fixtures::intent(from, "lighter", 2_000_000, 2_000_000)
        };
        let check = |prepared: &Value, owners: &LighterOwners| {
            fixtures::take_check(prepared, &into("hyperliquid"), owners, None)
        };
        let prepared = fixtures::lighter_delivery_take();
        assert!(check(&prepared, &owners).is_ok());
        let mut ours = BTreeSet::new();
        ours.insert(731);
        assert_eq!(lighter_accounts(&fixtures::model(&prepared), &into("hyperliquid")), ours);
        // Without Lighter's answer, or with another owner, nothing is signed.
        assert_eq!(check(&prepared, &none()).unwrap_err().detail, details::DESTINATION_OWNER);
        let theirs = LighterOwners::from([(731, MAKER)]);
        assert_eq!(check(&prepared, &theirs).unwrap_err().detail, details::DESTINATION_OWNER);
        for delivery in [json!(null), json!("0x12"), json!("")] {
            let other = edited(prepared.clone(), |p| {
                p["take"]["fills"][0]["delivery_to"] = delivery.clone();
            });
            assert_eq!(
                check(&other, &owners).unwrap_err().detail,
                details::FILLS_DELIVERY_TO,
                "{delivery}"
            );
        }
        // Two fills into two accounts.
        let first = fixtures::lighter_delivery_take()["take"]["fills"][0].clone();
        let mut second = first.clone();
        second["id"] = json!("0192f0a1-b2c3-7d4e-8f90-000000000002");
        second["swap_id"] = json!(format!("0x{}", "cc".repeat(32)));
        second["delivery_to"] = json!("732");
        let split = fixtures::preparation(vec![first, second], vec![]);
        let owners = LighterOwners::from([(731, OWNER), (732, OWNER)]);
        assert_eq!(
            fixtures::take_check(&split, &into("hyperliquid"), &owners, None).unwrap_err().detail,
            details::FILLS_DELIVERY_TO
        );
        // Paid from Lighter or Aster, the destination is checked first, before the payment.
        for from in ["lighter", "aster", "polymarket", "gmx"] {
            let prepared = edited(fixtures::lighter_delivery_take(), |p| {
                p["take"]["fills"][0]["from_venue"] = json!(from);
            });
            let error = fixtures::take_check(&prepared, &into(from), &theirs, None).unwrap_err();
            assert!(
                matches!(error.detail, details::DESTINATION_OWNER | details::TO_SIGN_KIND),
                "{from}: {error}"
            );
            let prepared = edited(prepared, |p| p["to_sign"] = json!([]));
            let error = fixtures::take_check(&prepared, &into(from), &theirs, None).unwrap_err();
            assert_eq!(error.detail, details::DESTINATION_OWNER, "{from}");
        }
    }

    /// `aster.test.ts` "a take into Aster names its own escrow".
    #[test]
    fn a_take_into_aster_names_its_own_domain_and_the_takers_account() {
        let mut fill = fixtures::fill("hyperliquid", "aster", "1000000", "1000000000000000000");
        fill["delivery_to"] = json!(format!("{OWNER:#x}"));
        let prepared = |fill: &Value, escrow: Option<&str>| {
            let mut lock = lock(fill);
            if let Some(escrow) = escrow {
                lock["receiveEscrow"] = json!(escrow);
            }
            fixtures::preparation(
                vec![fill.clone()],
                vec![fixtures::pay_item(fixtures::pay_take(vec![lock], "1000000"))],
            )
        };
        let intent = fixtures::intent("hyperliquid", "aster", 1_000_000, 1_000_000_000_000_000_000);
        assert!(check(&prepared(&fill, None), &intent).is_ok());
        assert_eq!(
            refused(&prepared(&fill, Some(fixtures::ESCROW_HL)), &intent).detail,
            details::PAYMENTS_RECEIVE
        );
        for delivery in [json!(format!("{MAKER:#x}")), json!(null)] {
            let mut other = fill.clone();
            other["delivery_to"] = delivery;
            assert_eq!(
                refused(&prepared(&other, None), &intent).detail,
                details::FILLS_DELIVERY_TO
            );
        }
    }

    /// `aster.test.ts` "a take paid from Aster signs one transfer of its amount to the maker, under
    /// its fill id, its nonce about a minute ahead".
    #[test]
    fn a_take_paid_from_aster_signs_one_transfer_of_its_fill() {
        let intent = fixtures::aster_intent();
        let verified = check(&fixtures::aster_take(), &intent).unwrap();
        // Signable while the nonce stays 20 seconds ahead: 35 seconds, before pay_by's 45.
        assert_eq!(verified.not_after(), Some(NOW + 34));
        let with = |name: &str, value: String| {
            fixtures::aster_take_of(&replaced(fixtures::aster_fields(), name, &value))
        };
        let nonce = |offset: i64| ((NOW + offset) * 1_000_000).to_string();
        let cases = [
            (with("amount", "1.5".into()), details::ASTER_TRANSFER),
            (with("amount", "1.500002".into()), details::ASTER_TRANSFER),
            (with("amount", "1.5000011".into()), details::ASTER_TRANSFER),
            (with("asset", "USDC".into()), details::ASTER_TRANSFER),
            (with("toAddress", format!("{OTHER:#x}")), details::ASTER_TRANSFER),
            (with("user", format!("{OTHER:#x}")), details::ASTER_TRANSFER),
            (with("clientTranId", "0".repeat(32)), details::ASTER_TRANSFER),
            (with("nonce", nonce(10)), details::ASTER_NONCE),
            (with("nonce", nonce(80)), details::ASTER_NONCE),
            (
                edited(fixtures::aster_take(), |p| {
                    p["take"]["fills"][0]["amount_in"] = json!("1500000000000000002");
                }),
                details::FILLS_AMOUNT_IN,
            ),
            (
                edited(fixtures::aster_take(), |p| {
                    p["take"]["fills"][0]["amount_out"] = json!("999999");
                }),
                details::FILLS_AMOUNT_OUT,
            ),
        ];
        for (index, (prepared, detail)) in cases.iter().enumerate() {
            assert_eq!(refused(prepared, &intent).detail, *detail, "case {index}");
        }
        let mut extra = fixtures::aster_fields();
        extra.push(("signer", format!("{OWNER:#x}")));
        assert_eq!(
            refused(&fixtures::aster_take_of(&extra), &intent).detail,
            details::ASTER_TRANSFER
        );
        let elsewhere = edited(fixtures::aster_take(), |p| {
            p["to_sign"][0]["typed_data"]["domain"]["chainId"] = json!(56);
        });
        assert_eq!(refused(&elsewhere, &intent).detail, details::DOMAIN);
        let signer = edited(fixtures::aster_take(), |p| p["to_sign"][0]["signer"] = json!(OTHER));
        assert_eq!(refused(&signer, &intent).detail, details::TO_SIGN_SIGNER);
        let fill = edited(fixtures::aster_take(), |p| {
            p["to_sign"][0]["fill_id"] = json!("0192f0a1-b2c3-7d4e-8f90-000000000000");
        });
        assert_eq!(refused(&fill, &intent).detail, details::FILL_ID);
        let two = edited(fixtures::aster_take(), |p| {
            let item = p["to_sign"][0].clone();
            p["to_sign"] = json!([item, item]);
        });
        assert_eq!(refused(&two, &intent).detail, details::TO_SIGN);
        let fills = edited(fixtures::aster_take(), |p| {
            let mut other = p["take"]["fills"][0].clone();
            other["id"] = json!("0192f0a1-b2c3-7d4e-8f90-000000000002");
            p["take"]["fills"].as_array_mut().unwrap().push(other);
        });
        assert_eq!(refused(&fills, &intent).detail, details::TO_SIGN);
        // A grouped preparation extends pay_by and its nonce together.
        let grouped = |offset: i64| {
            edited(with("nonce", nonce(offset)), |p| {
                p["take"]["fills"][0]["pay_by"] = json!(time(90));
            })
        };
        assert!(check(&grouped(110), &intent).is_ok());
        assert_eq!(refused(&grouped(111), &intent).detail, details::ASTER_NONCE);
        assert_eq!(refused(&with("nonce", nonce(110)), &intent).detail, details::ASTER_NONCE);
        let later = |now: i64| {
            let prepared: TakePreparation = fixtures::model(&grouped(110));
            let reads = TakeReads { lighter_owners: &none(), wallet_funds: None };
            check_take(&prepared, &intent, reads, EscrowEnvironment::Production, at(now))
        };
        assert!(later(89).is_ok());
        assert!(later(90).is_err());
        let late = |now: i64| {
            let prepared: TakePreparation = fixtures::model(&fixtures::aster_take());
            let reads = TakeReads { lighter_owners: &none(), wallet_funds: None };
            check_take(&prepared, &intent, reads, EscrowEnvironment::Production, at(now))
        };
        assert!(late(60).is_err());
    }

    /// `aster.test.ts` "an Aster transfer never pays a take from another venue".
    #[test]
    fn an_aster_transfer_never_pays_a_take_from_another_venue() {
        let item = fixtures::aster_take()["to_sign"][0].clone();
        let prepared = edited(escrow_take(), |p| p["to_sign"] = json!([item]));
        assert_eq!(refused(&prepared, &fixtures::escrow_intent()).detail, details::TO_SIGN_KIND);
    }

    /// `take.test.ts` "a take paid from Lighter checks its one transfer against the reserved fill"
    /// and "… signs the transfer message its terms build, once Lighter confirms both accounts".
    #[test]
    fn a_take_paid_from_lighter_signs_the_message_its_checked_terms_build() {
        let owners = fixtures::lighter_owners();
        let intent = fixtures::lighter_intent();
        let check = |prepared: &Value, intent: &TakeIntent, owners: &LighterOwners| {
            fixtures::take_check(prepared, intent, owners, None)
        };
        let verified = check(&fixtures::lighter_take(), &intent, &owners).unwrap();
        let memo = lighter::fill_memo(testing::FILL.parse().unwrap());
        let transfer = lighter::Transfer {
            nonce: 7,
            from: 42,
            from_route: 0,
            api_key: 254,
            to: 731_033,
            to_route: 0,
            asset: 3,
            amount: 2_001_300,
            fee: 0,
            memo,
        };
        let message = lighter::transfer_message(&transfer, 304);
        assert_eq!(verified.digest(), alloy_primitives::eip191_hash_message(message.as_bytes()));
        let mut expected = BTreeSet::new();
        expected.extend([42, 731_033]);
        assert_eq!(
            lighter_accounts(&fixtures::model(&fixtures::lighter_take()), &intent),
            expected
        );
        let terms = |name: &str, value: Value| {
            edited(fixtures::lighter_take(), |p| {
                p["to_sign"][0]["typed_data"][name] = value.clone();
            })
        };
        let cases = [
            (terms("amount", json!("2001301")), details::LIGHTER_TRANSFER),
            (terms("memo", json!("00".repeat(32))), details::LIGHTER_TRANSFER),
            (terms("from_account_index", json!(43)), details::LIGHTER_TRANSFER),
            // The development key is another environment's registration.
            (terms("api_key_index", json!(253)), details::LIGHTER_KEY),
            (terms("api_key_index", json!(4)), details::LIGHTER_KEY),
            (terms("usdc_fee", json!("1000001")), details::LIGHTER_FEE),
        ];
        for (index, (prepared, detail)) in cases.iter().enumerate() {
            assert_eq!(
                check(prepared, &intent, &owners).unwrap_err().detail,
                *detail,
                "case {index}"
            );
        }
        assert!(check(&terms("usdc_fee", json!("1000000")), &intent, &owners).is_ok());
        let unlinked = TakeIntent { payer_account: None, ..intent.clone() };
        assert_eq!(
            check(&fixtures::lighter_take(), &unlinked, &owners).unwrap_err().detail,
            details::LIGHTER_TRANSFER
        );
        let two = edited(fixtures::lighter_take(), |p| {
            let item = p["to_sign"][0].clone();
            p["to_sign"] = json!([item, item]);
        });
        assert_eq!(check(&two, &intent, &owners).unwrap_err().detail, details::TO_SIGN);
        let aster = TakeIntent { from: VenueId::Aster, ..intent.clone() };
        assert_eq!(
            check(&fixtures::lighter_take(), &aster, &owners).unwrap_err().detail,
            details::TO_SIGN_KIND
        );
        // Lighter's word on both accounts.
        for (owners, detail) in [
            (LighterOwners::from([(42, OWNER), (731_033, OWNER)]), details::LIGHTER_MAKER_ACCOUNT),
            (LighterOwners::from([(42, MAKER), (731_033, MAKER)]), details::LIGHTER_PAYER_ACCOUNT),
            (LighterOwners::from([(42, OWNER)]), details::LIGHTER_MAKER_ACCOUNT),
            (LighterOwners::from([(731_033, MAKER)]), details::LIGHTER_PAYER_ACCOUNT),
        ] {
            assert_eq!(
                check(&fixtures::lighter_take(), &intent, &owners).unwrap_err().detail,
                detail
            );
        }
        // An amount Lighter cannot carry.
        let huge = edited(fixtures::lighter_take(), |p| {
            let units = (u128::from(u64::MAX) + 1).to_string();
            p["take"]["fills"][0]["amount_in"] = json!(units);
            p["to_sign"][0]["typed_data"]["amount"] = json!(units);
        });
        let rich = TakeIntent { amount_in: Amount::new(U256::MAX), ..intent };
        assert_eq!(check(&huge, &rich, &owners).unwrap_err().detail, details::LIGHTER_TRANSFER);
    }

    /// `take.test.ts` "a take paid from Lighter to a wallet is checked against the address and the
    /// kind it asked".
    #[test]
    fn a_take_paid_from_lighter_to_a_wallet_binds_the_address_and_kind() {
        let delivered = edited(fixtures::lighter_take(), |p| {
            let fill = &mut p["take"]["fills"][0];
            fill["destination"] = json!("wallet");
            fill["taker_recipient"] = json!(BYBIT);
            fill["swap_id"] =
                json!(format!("0xfcb0d9ddb285{}{}", "00".repeat(10), "02".repeat(16)));
            p["take"]["minimum_received"] = json!({"token": deployments::chain(&VenueId::Hyperliquid).unwrap().wallet_token, "amount": "2000000"});
        });
        let owners = fixtures::lighter_owners();
        let wallet = TakeIntent { delivery: Delivery::Wallet(BYBIT), ..fixtures::lighter_intent() };
        let check = |prepared: &Value, intent: &TakeIntent| {
            fixtures::take_check(prepared, intent, &owners, None)
        };
        assert!(check(&delivered, &wallet).is_ok());
        let cases = [
            ("destination", json!("venue"), details::FILLS_DESTINATION),
            ("swap_id", json!(format!("0x{}", "02".repeat(32))), details::FILLS_SWAP_ID),
            ("taker_recipient", json!(OWNER), details::FILLS_TAKER_RECIPIENT),
        ];
        for (field, value, detail) in cases {
            let prepared =
                edited(delivered.clone(), |p| p["take"]["fills"][0][field] = value.clone());
            assert_eq!(check(&prepared, &wallet).unwrap_err().detail, detail, "{field}");
        }
        let venue = TakeIntent { delivery: Delivery::Account(OWNER), ..wallet.clone() };
        assert_eq!(check(&delivered, &venue).unwrap_err().detail, details::MINIMUM_RECEIVED);
    }

    #[test]
    fn a_take_from_wallet_funds_needs_a_fresh_read_that_covers_it() {
        let prepared = edited(escrow_take(), |p| {
            p["take"]["funding"] = json!("wallet");
            p["take"]["fills"][0]["funding"] = json!("wallet");
        });
        let intent = TakeIntent { wallet_funding: true, ..fixtures::escrow_intent() };
        let funds: WalletFunding = fixtures::model(&fixtures::wallet_funds());
        let check = |funds: Option<&WalletFunding>| {
            fixtures::take_check(&prepared, &intent, &none(), funds)
        };
        assert!(check(Some(&funds)).is_ok());
        assert_eq!(check(None).unwrap_err().kind, VerificationKind::Changed);
        let short: WalletFunding = fixtures::model(&edited(fixtures::wallet_funds(), |f| {
            f["spendable_wallet"] = json!("999999");
        }));
        assert_eq!(check(Some(&short)).unwrap_err().detail, details::WALLET_FUNDS);
        assert_eq!(refused(&escrow_take(), &intent).detail, details::FUNDING);
    }

    #[test]
    fn a_take_signed_at_once_has_the_routes_window_and_the_at_once_cutoff() {
        let prepared = edited(escrow_take(), |p| {
            p["take"]["fills"][0]["pay_by"] = json!(time(200));
            payment(p)["deadline"] = json!((NOW + 200).to_string());
        });
        let at_once = TakeIntent { at_once: true, ..fixtures::escrow_intent() };
        assert_eq!(refused(&prepared, &fixtures::escrow_intent()).detail, details::FILLS_PAY_BY);
        let verified = check(&prepared, &at_once).unwrap();
        // pay_by 200 s ahead: cutoff 170 s, signable while 30 s more remain.
        assert_eq!(verified.not_after(), Some(NOW + 139));
        let model: TakePreparation = fixtures::model(&prepared);
        assert_eq!(signable_until(&model).unwrap(), NOW + 200);
        assert_eq!(at_once_cutoff(&model).unwrap(), NOW + 170);
    }

    #[test]
    fn an_aster_reissue_ignores_the_held_nonce_until_issued_again() {
        let mut prepared = fixtures::aster_take();
        let model: TakePreparation = fixtures::model(&prepared);
        assert_eq!(signable_until(&model).unwrap(), NOW + 35);
        let TakeSignRequest::Aster(item) = &model.to_sign[0] else { unreachable!() };
        assert_eq!(aster_nonce(item).unwrap(), u64::try_from((NOW + 55) * 1_000_000).unwrap());
        prepared["aster_reissue"] = json!(true);
        let reissued: TakePreparation = fixtures::model(&prepared);
        assert_eq!(signable_until(&reissued).unwrap(), NOW + 45);
        // The request issued again is checked in full.
        assert!(check(&prepared, &fixtures::aster_intent()).is_ok());
    }

    #[test]
    fn a_reissued_aster_request_is_signed_only_while_its_nonce_stays_ahead() {
        let key = PrivateKeySigner::random();
        let payer = key.address();
        let fields = replaced(fixtures::aster_fields(), "user", &format!("{payer:#x}"));
        let prepared = edited(fixtures::aster_take_of(&fields), |p| {
            p["to_sign"][0]["signer"] = json!(payer);
            p["take"]["fills"][0]["payer"] = json!(payer);
            p["aster_reissue"] = json!(true);
        });
        let intent = TakeIntent { payer, ..fixtures::aster_intent() };
        let verified = check(&prepared, &intent).unwrap();
        // The nonce at +55 stays 20 seconds ahead until +35, before pay_by's +45.
        assert_eq!(verified.not_after(), Some(NOW + 34));
        assert!(fixtures::sign_until(key.clone(), &verified, 34).is_ok());
        let late = fixtures::sign_until(key, &verified, 40).unwrap_err();
        assert_eq!((late.kind, late.detail), (VerificationKind::Stale, details::SIGNING_WINDOW));
    }

    #[test]
    fn an_unknown_venue_is_unsupported() {
        let intent = TakeIntent { from: "dydx".parse().unwrap(), ..fixtures::escrow_intent() };
        assert_eq!(refused(&escrow_take(), &intent).kind, VerificationKind::Unsupported);
    }
}
