//! Maker checks: capital additions (`FundOffer`, margin moves), signed quote sets, closes and
//! created credentials.

use std::collections::HashSet;

use alloy_primitives::{Address, U256};
use chrono::{DateTime, Utc};

use super::{
    deadline_bound, deadline_within,
    details::{self, at, changed, differs, stale, unsupported},
    differs_unless, escrow, signed, withdrawal,
};
use crate::{
    deployments,
    error::VerificationError,
    model::{
        CredentialCreated, CredentialScope, FundingSource, MakerPreparation, MakerPreparationState,
        Order, ToSign, ToSignKind,
    },
    protocol::{
        eip712::{self, CloseOrders, FundOffer, QuoteSet},
        signing::Verified,
    },
    types::{Amount, ClientId, EscrowEnvironment, OrderId, QuoteIdentityId, VenueId},
};

/// The longest a maker preparation, funding or close may stay signable.
const MAKER_SECONDS: i64 = 3600;
/// The highest quote sequence, a quote set's or a funding's floor: JavaScript's largest exact
/// integer, as the server caps it.
pub(super) const MAX_SEQUENCE: u64 = (1 << 53) - 1;

/// The caller's capital addition a maker preparation is checked against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CapitalRequest {
    pub(crate) venue: VenueId,
    /// Capital to add, in `venue` base units.
    pub(crate) amount: Amount,
    pub(crate) client_request_id: ClientId,
    pub(crate) funding_source: FundingSource,
}

/// Who a maker preparation may name: the linked owner, the delegated identity and its quote
/// signer, the deployment's operator, and the identity's existing pool on the venue, if any.
#[derive(Debug, Clone, Copy)]
pub(crate) struct MakerAuthority<'a> {
    pub(crate) owner: Address,
    pub(crate) quote_signer: Address,
    pub(crate) quote_identity_id: QuoteIdentityId,
    pub(crate) operator: Address,
    pub(crate) existing: Option<&'a Order>,
}

/// Checks a maker preparation (`POST /v1/maker/prepare`) for a delegated quote identity at `now`
/// and returns its payloads, in `to_sign` order: the `FundOffer` of all the capital asked, its
/// deposit only the part the pool's available balance does not cover, from the asked source, and
/// for venue funding one margin move of that deposit. A delegated identity's quotes are never
/// re-signed here, so any `quote_set` request is refused.
pub(crate) fn check_maker_intent(
    prepared: &MakerPreparation,
    request: &CapitalRequest,
    authority: &MakerAuthority<'_>,
    environment: EscrowEnvironment,
    now: DateTime<Utc>,
) -> Result<Vec<Verified>, VerificationError> {
    if deployments::is_venue_settled(&request.venue) {
        return Err(unsupported(details::VENUE));
    }
    let (order, intent) = (&prepared.order, &prepared.intent);
    let identity = authority.quote_identity_id;
    differs_unless([
        (prepared.state == MakerPreparationState::Preparing, details::STATE),
        (prepared.client_request_id == request.client_request_id, details::CLIENT_REQUEST_ID),
        (prepared.venue == request.venue && order.venue == request.venue, details::VENUE),
        (order.owner_address == authority.owner, details::ORDER_OWNER_ADDRESS),
        (order.quote_signer == authority.quote_signer, details::ORDER_QUOTE_SIGNER),
        (order.quote_identity_id == identity, details::ORDER_QUOTE_IDENTITY_ID),
        (intent.amount == request.amount, details::INTENT_AMOUNT),
        (intent.quote_identity_id == identity, details::INTENT_QUOTE_IDENTITY_ID),
        (intent.funding_source == request.funding_source, details::INTENT_FUNDING_SOURCE),
        (prepared.expected_revision == order.revision, details::EXPECTED_REVISION),
    ])?;
    let expires = U256::try_from(prepared.expires_at.timestamp()).unwrap_or(U256::ZERO);
    if !deadline_within(expires, now.timestamp(), MAKER_SECONDS) {
        return Err(stale(details::EXPIRES_AT));
    }
    let available = check_existing(order, request, authority)?;
    let deposit = check_deposit(prepared, request, available)?;
    let kinds: Vec<&ToSignKind> = prepared.to_sign.iter().map(|item| &item.kind).collect();
    let unique = kinds.iter().collect::<HashSet<_>>().len() == kinds.len();
    let moves = !deposit.venue.is_zero();
    let allowed = kinds.iter().all(|kind| match kind {
        ToSignKind::Fund => true,
        ToSignKind::MarginMove => moves,
        _ => false,
    });
    if !unique
        || !allowed
        || !kinds.contains(&&ToSignKind::Fund)
        || kinds.contains(&&ToSignKind::MarginMove) != moves
    {
        return Err(differs(details::TO_SIGN));
    }
    if prepared.sequence.is_some() {
        return Err(differs(details::SEQUENCE));
    }
    prepared
        .to_sign
        .iter()
        .map(|item| match item.kind {
            ToSignKind::MarginMove => {
                check_margin_move(item, &request.venue, deposit.venue, authority.owner, now)
            }
            _ => check_fund(item, order, request, authority, deposit.total, environment, now),
        })
        .collect()
}

/// What the pool had available as last read: an existing pool is the identity's and unchanged
/// since; a new one has no quotes or capital yet.
fn check_existing(
    order: &Order,
    request: &CapitalRequest,
    authority: &MakerAuthority<'_>,
) -> Result<U256, VerificationError> {
    let Some(existing) = authority.existing else {
        let new = order.last_sequence == 0 && order.available_to_quote.get().is_zero();
        return if new { Ok(U256::ZERO) } else { Err(differs(details::ORDER)) };
    };
    if order.id != existing.id {
        return Err(differs(details::ORDER_ID));
    }
    if existing.venue != request.venue
        || existing.owner_address != authority.owner
        || existing.quote_signer != authority.quote_signer
        || existing.quote_identity_id != authority.quote_identity_id
    {
        return Err(differs(details::EXISTING_ORDER));
    }
    if order.last_sequence != existing.last_sequence
        || order.available_to_quote != existing.available_to_quote
        || order.revision < existing.revision
    {
        return Err(changed(details::MAKER_CAPITAL));
    }
    Ok(existing.available_to_quote.get())
}

/// A capital addition's deposit, and what of it leaves the venue's margin.
struct Deposit {
    total: U256,
    venue: U256,
}

/// The capital asked is the idle balance plus the deposit, the deposit is only what the pool's
/// spare available balance does not cover, and it comes from the asked source alone.
fn check_deposit(
    prepared: &MakerPreparation,
    request: &CapitalRequest,
    available: U256,
) -> Result<Deposit, VerificationError> {
    let funding = &prepared.funding;
    let amount = request.amount.get();
    let (idle, wallet, venue, existing) = (
        funding.idle_amount.get(),
        funding.wallet_amount.get(),
        funding.venue_amount.get(),
        funding.existing_available.get(),
    );
    let total = wallet.checked_add(venue).ok_or(differs(details::FUNDING_DEPOSIT))?;
    if amount.is_zero() {
        return Err(differs(details::AMOUNT));
    }
    if existing < available {
        return Err(differs(details::FUNDING_EXISTING_AVAILABLE));
    }
    if idle.checked_add(total) != Some(amount) {
        return Err(differs(details::FUNDING_IDLE_AMOUNT));
    }
    match request.funding_source {
        FundingSource::Venue if !wallet.is_zero() => {
            return Err(differs(details::FUNDING_WALLET_AMOUNT));
        }
        FundingSource::Wallet if !venue.is_zero() => {
            return Err(differs(details::FUNDING_VENUE_AMOUNT));
        }
        FundingSource::Venue | FundingSource::Wallet => {}
        FundingSource::Unknown(_) => return Err(unsupported(details::INTENT_FUNDING_SOURCE)),
    }
    if total != amount.saturating_sub(existing - available) {
        return Err(differs(details::FUNDING_DEPOSIT));
    }
    Ok(Deposit { total, venue })
}

/// A margin move of the whole venue deposit, rounded up to whole millionths (the wallet keeps
/// the rest), by the owner to the owner.
fn check_margin_move(
    item: &ToSign,
    venue: &VenueId,
    amount: U256,
    owner: Address,
    now: DateTime<Utc>,
) -> Result<Verified, VerificationError> {
    let moved =
        deployments::venue_units(amount, venue).ok_or(differs(details::MARGIN_MOVE_AMOUNT))?;
    if item.signer != owner {
        return Err(differs(details::MARGIN_MOVE_SIGNER));
    }
    if withdrawal::withdrawal_cost(&item.typed_data, venue)?.get() != moved {
        return Err(differs(details::MARGIN_MOVE_AMOUNT));
    }
    withdrawal::check_withdrawal(&item.typed_data, venue, owner, owner, Amount::new(moved), now)
}

/// The owner's funding of `order` with all the capital asked, `deposit`, the identity's quote
/// signer and the operator, its quote floor above the order's last sequence, within the hour.
fn check_fund(
    item: &ToSign,
    order: &Order,
    request: &CapitalRequest,
    authority: &MakerAuthority<'_>,
    deposit: U256,
    environment: EscrowEnvironment,
    now: DateTime<Utc>,
) -> Result<Verified, VerificationError> {
    let owner = authority.owner;
    let (parsed, domain) =
        escrow::check_escrow_typed::<FundOffer>(&item.typed_data, &order.venue, environment)?;
    let fund = &parsed.message;
    let floor = fund.minQuoteSequence;
    differs_unless([
        (item.signer == owner, details::FUND_SIGNER),
        (fund.owner == owner, details::FUND_OWNER),
        (fund.offerId == *order.id.as_b256(), details::FUND_OFFER_ID),
        (fund.amount == request.amount.get(), details::FUND_AMOUNT),
        (fund.deposit == deposit, details::FUND_DEPOSIT),
        (fund.quoteSigner == authority.quote_signer, details::FUND_QUOTE_SIGNER),
        (fund.operator == authority.operator, details::FUND_OPERATOR),
        (floor > order.last_sequence && floor <= MAX_SEQUENCE, details::FUND_MIN_QUOTE_SEQUENCE),
    ])?;
    if !deadline_within(fund.deadline, now.timestamp(), MAKER_SECONDS) {
        return Err(stale(details::DEADLINE));
    }
    let not_after = Some(deadline_bound(fund.deadline));
    Verified::typed(eip712::typed(fund, domain), parsed.digest, owner, not_after)
}

/// The terms of one quote, as the maker set them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct QuoteTerms {
    /// The venue the taker pays on.
    pub(crate) venue: VenueId,
    pub(crate) recipient: Address,
    pub(crate) supply: U256,
    pub(crate) receive: U256,
    pub(crate) min_fill: U256,
}

/// Checks a quote set to sign for `pool` (its id, venue, owner and quote signer): exactly the
/// `requested` quotes, in order (each position is the index a fill names), at `sequence`,
/// settling by `expires_at`, in the pool's pinned signing domain; signable before `valid_until`,
/// its last admission time (Unix seconds).
pub(crate) fn check_signed_quote(
    item: &ToSign,
    pool: &Order,
    requested: &[QuoteTerms],
    sequence: u64,
    valid_until: u64,
    expires_at: u64,
    environment: EscrowEnvironment,
) -> Result<Verified, VerificationError> {
    let (parsed, domain) =
        escrow::check_escrow_typed::<QuoteSet>(&item.typed_data, &pool.venue, environment)?;
    let set = &parsed.message;
    differs_unless([
        (item.kind == ToSignKind::QuoteSet, details::KIND),
        (item.signer == pool.quote_signer, details::TO_SIGN_SIGNER),
        (set.owner == pool.owner_address, details::OWNER),
        (set.offerId == *pool.id.as_b256(), details::OFFER_ID),
        (set.sequence == sequence, details::SEQUENCE),
        (set.expiresAt == expires_at, details::EXPIRES_AT),
        (!requested.is_empty() && set.quotes.len() == requested.len(), details::QUOTES),
    ])?;
    for (index, (quote, expected)) in set.quotes.iter().zip(requested).enumerate() {
        let chain = deployments::chain(&expected.venue);
        let venue = quote.venue.parse::<VenueId>().ok();
        differs_unless([
            (
                chain.is_some()
                    && venue.as_ref() == Some(&expected.venue)
                    && expected.venue != pool.venue,
                details::QUOTES_VENUE,
            ),
            (
                chain.is_some_and(|chain| quote.chainId == U256::from(chain.id)),
                details::QUOTES_CHAIN_ID,
            ),
            (chain.is_some_and(|chain| quote.token == chain.token), details::QUOTES_TOKEN),
            (quote.recipient == expected.recipient, details::QUOTES_RECIPIENT),
            (quote.supply == expected.supply && !quote.supply.is_zero(), details::QUOTES_SUPPLY),
            (
                quote.receive == expected.receive && !quote.receive.is_zero(),
                details::QUOTES_RECEIVE,
            ),
            (
                quote.minFill == expected.min_fill && quote.minFill <= quote.supply,
                details::QUOTES_MIN_FILL,
            ),
        ])
        .map_err(|error| at(error, index))?;
    }
    let not_after = Some(signed(valid_until).saturating_sub(1));
    Verified::typed(eip712::typed(set, domain), parsed.digest, pool.quote_signer, not_after)
}

/// Checks a close (`POST /v1/closes`) of orders among `ids` by `master` at `now`: one
/// `CloseOrders` in the HyperEVM escrow's domain (LaMarge's operator authorization), by and for
/// the master, of distinct asked orders, within the hour.
pub(crate) fn check_close_orders(
    items: &[ToSign],
    ids: &[OrderId],
    master: Address,
    environment: EscrowEnvironment,
    now: DateTime<Utc>,
) -> Result<Verified, VerificationError> {
    let [item] = items else {
        return Err(differs(details::TO_SIGN));
    };
    let (parsed, domain) = escrow::check_escrow_typed::<CloseOrders>(
        &item.typed_data,
        &VenueId::Hyperliquid,
        environment,
    )?;
    let close = &parsed.message;
    let distinct = close.orders.iter().collect::<HashSet<_>>().len() == close.orders.len();
    let asked = close.orders.iter().all(|order| ids.iter().any(|id| id.as_b256() == order));
    differs_unless([
        (item.kind == ToSignKind::CloseOrders, details::KIND),
        (item.signer == master, details::TO_SIGN_SIGNER),
        (close.account == master, details::ACCOUNT),
        (!close.orders.is_empty() && distinct && asked, details::ORDERS),
    ])?;
    if !deadline_within(close.deadline, now.timestamp(), MAKER_SECONDS) {
        return Err(stale(details::DEADLINE));
    }
    let not_after = Some(deadline_bound(close.deadline));
    Verified::typed(eip712::typed(close, domain), parsed.digest, master, not_after)
}

/// Checks a created credential (`POST /v1/credentials`) echoes the quote signer and scopes asked.
pub(crate) fn check_created_credential(
    created: &CredentialCreated,
    quote_signer: Address,
    scopes: &[CredentialScope],
) -> Result<(), VerificationError> {
    if created.quote_signer != quote_signer {
        return Err(differs(details::CREDENTIAL_QUOTE_SIGNER));
    }
    if created.scopes != scopes {
        return Err(differs(details::CREDENTIAL_SCOPES));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::{
        error::VerificationKind,
        testing::{
            self, MAKER, NOW, OPERATOR, OTHER, OWNER, POOL, edited,
            fixtures::{self, funded},
            json::fixture,
        },
    };

    fn check_with(
        prepared: &Value,
        request: &CapitalRequest,
        existing: Option<&Order>,
    ) -> Result<Vec<Verified>, VerificationError> {
        let authority = fixtures::authority(existing);
        check_maker_intent(
            &fixtures::maker(prepared),
            request,
            &authority,
            EscrowEnvironment::Production,
            fixtures::now(),
        )
    }

    fn check(prepared: &Value) -> Result<Vec<Verified>, VerificationError> {
        check_with(prepared, &fixtures::capital_request(FundingSource::Wallet), None)
    }

    fn fund(prepared: &mut Value) -> &mut Value {
        &mut prepared["to_sign"][0]["typed_data"]["message"]
    }

    fn fund_domain(prepared: &mut Value) -> &mut Value {
        &mut prepared["to_sign"][0]["typed_data"]["domain"]
    }

    /// Development's escrow on HyperEVM.
    const DEV_HL: &str = "0x948e65c40442d935d4ee990bdbd01bdd825c4873";

    /// `intents.test.ts` "financial provisioning permits only the requested authority, domain,
    /// capital and request id"; `maker.test.ts` "malicious operator authority is rejected before
    /// the first master signature", "funding against an unpinned escrow never reaches the
    /// financial key".
    #[test]
    fn a_funding_binds_only_the_requested_authority_domain_capital_and_request() {
        let verified = check(&fixtures::maker_preparation()).unwrap();
        assert_eq!(verified.len(), 1);
        assert_eq!(verified[0].not_after(), Some(NOW + 3570));
        let prepared = |edit: fn(&mut Value)| edited(fixtures::maker_preparation(), edit);
        let item = fixtures::maker_preparation()["to_sign"][0].clone();
        let max = U256::MAX.to_string();
        let cases = [
            (funded("operator", json!(OWNER)), details::FUND_OPERATOR),
            (funded("quoteSigner", json!(OPERATOR)), details::FUND_QUOTE_SIGNER),
            (funded("amount", json!("10000001")), details::FUND_AMOUNT),
            (funded("deposit", json!("9999999")), details::FUND_DEPOSIT),
            (funded("owner", json!(OTHER)), details::FUND_OWNER),
            (funded("offerId", json!(testing::ORDER)), details::FUND_OFFER_ID),
            (funded("minQuoteSequence", json!(0)), details::FUND_MIN_QUOTE_SEQUENCE),
            (funded("minQuoteSequence", json!(1u64 << 53)), details::FUND_MIN_QUOTE_SEQUENCE),
            (funded("deadline", json!((NOW + 7200).to_string())), details::DEADLINE),
            (funded("deadline", json!(NOW.to_string())), details::DEADLINE),
            (prepared(|p| fund_domain(p)["verifyingContract"] = json!(OWNER)), details::DOMAIN),
            (prepared(|p| fund_domain(p)["chainId"] = json!(137)), details::DOMAIN),
            (prepared(|p| fund_domain(p)["verifyingContract"] = json!(DEV_HL)), details::DOMAIN),
            (
                prepared(|p| {
                    p["to_sign"][0]["typed_data"]["types"]["FundOffer"][2]["type"] =
                        json!("uint64");
                }),
                details::TYPES,
            ),
            (prepared(|p| p["to_sign"][0]["signer"] = json!(OPERATOR)), details::FUND_SIGNER),
            (prepared(|p| p["client_request_id"] = json!("another")), details::CLIENT_REQUEST_ID),
            (
                edited(fixtures::maker_preparation(), |p| p["to_sign"] = json!([item, item])),
                details::TO_SIGN,
            ),
            (prepared(|p| p["to_sign"] = json!([])), details::TO_SIGN),
            (prepared(|p| p["state"] = json!("accepted")), details::STATE),
            (prepared(|p| p["venue"] = json!("polymarket")), details::VENUE),
            (
                prepared(|p| p["order"]["owner_address"] = json!(OTHER)),
                details::ORDER_OWNER_ADDRESS,
            ),
            (prepared(|p| p["order"]["quote_signer"] = json!(OTHER)), details::ORDER_QUOTE_SIGNER),
            (
                prepared(|p| p["order"]["quote_identity_id"] = json!(testing::FILL)),
                details::ORDER_QUOTE_IDENTITY_ID,
            ),
            (prepared(|p| p["intent"]["amount"] = json!("1")), details::INTENT_AMOUNT),
            (
                prepared(|p| p["intent"]["quote_identity_id"] = json!(testing::FILL)),
                details::INTENT_QUOTE_IDENTITY_ID,
            ),
            (
                prepared(|p| p["intent"]["funding_source"] = json!("venue")),
                details::INTENT_FUNDING_SOURCE,
            ),
            (prepared(|p| p["expected_revision"] = json!("2")), details::EXPECTED_REVISION),
            (prepared(|p| p["expires_at"] = json!(testing::time(3631))), details::EXPIRES_AT),
            (prepared(|p| p["order"]["last_sequence"] = json!(3)), details::ORDER),
            (prepared(|p| p["sequence"] = json!(2)), details::SEQUENCE),
            (prepared(|p| p["funding"]["venue_amount"] = json!("1")), details::FUNDING_IDLE_AMOUNT),
            // A deposit beyond the largest integer refuses rather than wraps.
            (
                edited(fixtures::maker_preparation(), |p| {
                    p["funding"]["wallet_amount"] = json!(max);
                    p["funding"]["venue_amount"] = json!("1");
                }),
                details::FUNDING_DEPOSIT,
            ),
        ];
        for (index, (prepared, detail)) in cases.iter().enumerate() {
            assert_eq!(check(prepared).unwrap_err().detail, *detail, "case {index}");
        }
        // The operator comes from the caller: the API's never authorizes another.
        let elsewhere = MakerAuthority { operator: OTHER, ..fixtures::authority(None) };
        let error = check_maker_intent(
            &fixtures::maker(&fixtures::maker_preparation()),
            &fixtures::capital_request(FundingSource::Wallet),
            &elsewhere,
            EscrowEnvironment::Production,
            fixtures::now(),
        );
        assert_eq!(error.unwrap_err().detail, details::FUND_OPERATOR);
    }

    /// `intents.test.ts` "a deposit takes the available balance first and only the rest from its
    /// source".
    #[test]
    fn a_deposit_takes_the_available_balance_first() {
        let idle = |patch: &Value, deposit: &str| {
            edited(fixtures::maker_preparation(), |p| {
                let mut funding = json!({"existing_available": "4000000", "idle_amount": "4000000", "wallet_amount": "6000000"});
                fixtures::object(&mut funding).extend(patch.as_object().unwrap().clone());
                fixtures::object(&mut p["funding"]).extend(funding.as_object().unwrap().clone());
                fund(p)["deposit"] = json!(deposit);
            })
        };
        assert!(check(&idle(&json!({}), "6000000")).is_ok());
        let cases = [
            (json!({"existing_available": "3000000"}), "6000000", details::FUNDING_DEPOSIT),
            (
                json!({"idle_amount": "0", "wallet_amount": "10000000"}),
                "10000000",
                details::FUNDING_DEPOSIT,
            ),
            (
                json!({"idle_amount": "3000000", "wallet_amount": "7000000"}),
                "7000000",
                details::FUNDING_DEPOSIT,
            ),
            (
                json!({"idle_amount": "4000000", "wallet_amount": "7000000"}),
                "7000000",
                details::FUNDING_IDLE_AMOUNT,
            ),
        ];
        for (patch, deposit, detail) in cases {
            assert_eq!(check(&idle(&patch, deposit)).unwrap_err().detail, detail, "{patch}");
        }
    }

    /// A top-up of the identity's pool: what it had available stays its own, and capital that
    /// moved since the pool was read is reviewed again.
    #[test]
    fn a_top_up_binds_the_pool_as_it_was_read() {
        let live = |edit: fn(&mut Value)| -> Order {
            let mut pool = fixtures::pool();
            pool["state"] = json!("live");
            pool["available_to_quote"] = json!("4000000");
            edit(&mut pool);
            fixtures::model(&pool)
        };
        let prepared = edited(fixtures::maker_preparation(), |p| {
            p["order"]["available_to_quote"] = json!("4000000");
            p["funding"]["existing_available"] = json!("4000000");
        });
        let request = fixtures::capital_request(FundingSource::Wallet);
        let existing = live(|_| {});
        assert!(check_with(&prepared, &request, Some(&existing)).is_ok());
        assert_eq!(check(&prepared).unwrap_err().detail, details::ORDER);
        let below =
            edited(prepared.clone(), |p| p["funding"]["existing_available"] = json!("3999999"));
        assert_eq!(
            check_with(&below, &request, Some(&existing)).unwrap_err().detail,
            details::FUNDING_EXISTING_AVAILABLE
        );
        let moved = live(|pool| pool["available_to_quote"] = json!("5000000"));
        let error = check_with(&prepared, &request, Some(&moved)).unwrap_err();
        assert_eq!((error.kind, error.detail), (VerificationKind::Changed, details::MAKER_CAPITAL));
        let newer = live(|pool| pool["revision"] = json!("2"));
        assert_eq!(
            check_with(&prepared, &request, Some(&newer)).unwrap_err().detail,
            details::MAKER_CAPITAL
        );
        let other = live(|pool| pool["id"] = json!(testing::ORDER));
        assert_eq!(
            check_with(&prepared, &request, Some(&other)).unwrap_err().detail,
            details::ORDER_ID
        );
        let foreign = live(|pool| pool["quote_signer"] = json!(OTHER));
        assert_eq!(
            check_with(&prepared, &request, Some(&foreign)).unwrap_err().detail,
            details::EXISTING_ORDER
        );
    }

    #[test]
    fn venue_funding_moves_the_whole_deposit_out_of_margin_once() {
        let request = fixtures::capital_request(FundingSource::Venue);
        let verified = check_with(&fixtures::margin_preparation(), &request, None).unwrap();
        assert_eq!(verified.len(), 2);
        let served = eip712::parse::<crate::protocol::hyperliquid::SendAsset>(
            &fixtures::margin_preparation()["to_sign"][0]["typed_data"],
        )
        .unwrap();
        assert_eq!(verified[0].digest(), served.digest);
        let cases = [
            (
                edited(fixtures::margin_preparation(), |p| {
                    p["to_sign"][0]["signer"] = json!(OTHER);
                }),
                details::MARGIN_MOVE_SIGNER,
            ),
            (
                edited(fixtures::margin_preparation(), |p| {
                    p["to_sign"][0]["typed_data"]["message"]["amount"] = json!("9");
                }),
                details::MARGIN_MOVE_AMOUNT,
            ),
            (
                edited(fixtures::margin_preparation(), |p| {
                    p["to_sign"][0]["typed_data"]["message"]["destination"] =
                        json!(format!("{OTHER:#x}"));
                }),
                details::DESTINATION,
            ),
            (
                edited(fixtures::margin_preparation(), |p| {
                    p["funding"]["wallet_amount"] = json!("1");
                }),
                details::FUNDING_IDLE_AMOUNT,
            ),
            (
                edited(fixtures::margin_preparation(), |p| {
                    let fund = p["to_sign"][1].clone();
                    p["to_sign"] = json!([fund]);
                }),
                details::TO_SIGN,
            ),
        ];
        for (index, (prepared, detail)) in cases.iter().enumerate() {
            assert_eq!(
                check_with(prepared, &request, None).unwrap_err().detail,
                *detail,
                "case {index}"
            );
        }
        let wallet_with_move = edited(fixtures::margin_preparation(), |p| {
            p["intent"]["funding_source"] = json!("wallet");
        });
        let error =
            check_with(&wallet_with_move, &fixtures::capital_request(FundingSource::Wallet), None);
        assert_eq!(error.unwrap_err().detail, details::FUNDING_VENUE_AMOUNT);
        let venue_from_wallet = edited(fixtures::maker_preparation(), |p| {
            p["intent"]["funding_source"] = json!("venue");
        });
        assert_eq!(
            check_with(&venue_from_wallet, &request, None).unwrap_err().detail,
            details::FUNDING_WALLET_AMOUNT
        );
    }

    /// A delegated identity's quotes are never re-signed by a capital addition;
    /// `intents.test.ts`'s browser top-up is deferred.
    #[test]
    fn a_delegated_addition_signs_no_quote_set() {
        let quote = json!({"kind": "quote_set", "signer": MAKER, "typed_data": testing::sample("quote_set")});
        let prepared = edited(fixtures::maker_preparation(), |p| {
            p["to_sign"].as_array_mut().unwrap().push(quote);
            p["sequence"] = json!(12);
        });
        assert_eq!(check(&prepared).unwrap_err().detail, details::TO_SIGN);
        // Lighter and Aster pools are settled inside the venue: not funded here.
        let lighter = CapitalRequest {
            venue: VenueId::Lighter,
            ..fixtures::capital_request(FundingSource::Wallet)
        };
        assert_eq!(
            check_with(&fixtures::maker_preparation(), &lighter, None).unwrap_err().kind,
            VerificationKind::Unsupported
        );
    }

    fn close(items: &Value, ids: &[&str]) -> Result<Verified, VerificationError> {
        let ids: Vec<OrderId> = ids.iter().map(|id| id.parse().unwrap()).collect();
        check_close_orders(
            &fixtures::to_sign(items),
            &ids,
            OWNER,
            EscrowEnvironment::Production,
            fixtures::now(),
        )
    }

    #[test]
    fn a_close_names_only_asked_orders_of_the_master_within_the_hour() {
        let both = [POOL, "0x5555555555555555555555555555555555555555555555555555555555555555"];
        let verified = close(&fixtures::close_items(), &both).unwrap();
        assert_eq!(verified.not_after(), Some(NOW + 3570));
        let message = |name: &str, value: Value| {
            edited(fixtures::close_items(), |items| {
                items[0]["typed_data"]["message"][name] = value.clone();
            })
        };
        let cases = [
            (message("account", json!(OTHER)), details::ACCOUNT),
            (message("orders", json!([])), details::ORDERS),
            (message("orders", json!([POOL, POOL])), details::ORDERS),
            (message("orders", json!([testing::ORDER])), details::ORDERS),
            (message("deadline", json!((NOW + 3631).to_string())), details::DEADLINE),
            (
                edited(fixtures::close_items(), |items| items[0]["signer"] = json!(OTHER)),
                details::TO_SIGN_SIGNER,
            ),
            (
                edited(fixtures::close_items(), |items| items[0]["kind"] = json!("fund")),
                details::KIND,
            ),
            (
                edited(fixtures::close_items(), |items| {
                    items[0]["typed_data"]["domain"]["chainId"] = json!(137);
                }),
                details::DOMAIN,
            ),
        ];
        for (index, (items, detail)) in cases.iter().enumerate() {
            assert_eq!(close(items, &both).unwrap_err().detail, *detail, "case {index}");
        }
        assert_eq!(close(&fixtures::close_items(), &[POOL]).unwrap_err().detail, details::ORDERS);
        let two = edited(fixtures::close_items(), |items| {
            let item = items[0].clone();
            items.as_array_mut().unwrap().push(item);
        });
        assert_eq!(close(&two, &both).unwrap_err().detail, details::TO_SIGN);
    }

    fn pool_order() -> Order {
        let mut pool = fixtures::pool();
        pool["last_sequence"] = json!(11);
        fixtures::model(&pool)
    }

    /// The vector's quote set for [`POOL`], as the maker asked it.
    fn quote_item() -> Value {
        let mut typed = testing::sample("quote_set");
        typed["message"]["owner"] = json!(OWNER);
        json!({"kind": "quote_set", "signer": MAKER, "typed_data": typed})
    }

    fn quote_terms() -> Vec<QuoteTerms> {
        ["polymarket", "gmx"]
            .into_iter()
            .map(|venue| QuoteTerms {
                venue: venue.parse().unwrap(),
                recipient: OWNER,
                supply: U256::from(500_000_000u64),
                receive: U256::from(500_250_000u64),
                min_fill: U256::from(10_000_000u64),
            })
            .collect()
    }

    fn signed_quote(item: &Value, requested: &[QuoteTerms]) -> Result<Verified, VerificationError> {
        let item: ToSign = fixtures::model(item);
        check_signed_quote(
            &item,
            &pool_order(),
            requested,
            12,
            1_790_000_000,
            1_790_003_601,
            EscrowEnvironment::Production,
        )
    }

    /// `protocol.ts` `checkSignedQuote`: the entire set, in order, each quote as asked.
    #[test]
    fn a_quote_set_is_exactly_the_asked_quotes_in_order() {
        assert!(signed_quote(&quote_item(), &quote_terms()).is_ok());
        let quote = |index: usize, name: &str, value: Value| {
            edited(quote_item(), |item| {
                item["typed_data"]["message"]["quotes"][index][name] = value.clone();
            })
        };
        let cases = [
            (quote(1, "venue", json!("polymarket")), details::QUOTES_VENUE),
            (quote(1, "chainId", json!(137)), details::QUOTES_CHAIN_ID),
            (quote(1, "token", json!(OTHER)), details::QUOTES_TOKEN),
            (quote(1, "recipient", json!(OTHER)), details::QUOTES_RECIPIENT),
            (quote(1, "supply", json!("500000001")), details::QUOTES_SUPPLY),
            (quote(1, "receive", json!("1")), details::QUOTES_RECEIVE),
            (quote(1, "minFill", json!("1")), details::QUOTES_MIN_FILL),
        ];
        for (prepared, detail) in &cases {
            let error = signed_quote(prepared, &quote_terms()).unwrap_err();
            assert_eq!((error.detail, error.index), (*detail, Some(1)));
        }
        let message = |name: &str, value: Value| {
            edited(quote_item(), |item| item["typed_data"]["message"][name] = value.clone())
        };
        let cases = [
            (message("owner", json!(OTHER)), details::OWNER),
            (message("offerId", json!(testing::ORDER)), details::OFFER_ID),
            (message("sequence", json!(13)), details::SEQUENCE),
            (message("expiresAt", json!(1_790_003_602)), details::EXPIRES_AT),
            (message("quotes", json!([])), details::QUOTES),
            (edited(quote_item(), |item| item["signer"] = json!(OWNER)), details::TO_SIGN_SIGNER),
            (edited(quote_item(), |item| item["kind"] = json!("fund")), details::KIND),
        ];
        for (index, (prepared, detail)) in cases.iter().enumerate() {
            assert_eq!(
                signed_quote(prepared, &quote_terms()).unwrap_err().detail,
                *detail,
                "case {index}"
            );
        }
        let mut reversed = quote_terms();
        reversed.reverse();
        assert_eq!(
            signed_quote(&quote_item(), &reversed).unwrap_err().detail,
            details::QUOTES_VENUE
        );
        assert_eq!(
            signed_quote(&quote_item(), &quote_terms()[..1]).unwrap_err().detail,
            details::QUOTES
        );
        assert_eq!(signed_quote(&quote_item(), &[]).unwrap_err().detail, details::QUOTES);
        // A quote never receives on the pool's own venue, nor on a venue this release lacks.
        let mut own = quote_terms();
        own[0].venue = VenueId::Hyperliquid;
        let own_item = quote(0, "venue", json!("hyperliquid"));
        assert_eq!(signed_quote(&own_item, &own).unwrap_err().detail, details::QUOTES_VENUE);
        let zero = quote_terms()
            .into_iter()
            .map(|terms| QuoteTerms { supply: U256::ZERO, min_fill: U256::ZERO, ..terms })
            .collect::<Vec<_>>();
        let zero_item = edited(quote_item(), |item| {
            for quote in item["typed_data"]["message"]["quotes"].as_array_mut().unwrap() {
                quote["supply"] = json!("0");
                quote["minFill"] = json!("0");
            }
        });
        assert_eq!(signed_quote(&zero_item, &zero).unwrap_err().detail, details::QUOTES_SUPPLY);
    }

    #[test]
    fn a_created_credential_echoes_the_signer_and_scopes_asked() {
        let created: CredentialCreated =
            serde_json::from_value(fixture("credential_created")).unwrap();
        let scopes = created.scopes.clone();
        assert!(check_created_credential(&created, created.quote_signer, &scopes).is_ok());
        let error = check_created_credential(&created, OTHER, &scopes).unwrap_err();
        assert_eq!(error.detail, details::CREDENTIAL_QUOTE_SIGNER);
        let error =
            check_created_credential(&created, created.quote_signer, &scopes[1..]).unwrap_err();
        assert_eq!(error.detail, details::CREDENTIAL_SCOPES);
    }
}
