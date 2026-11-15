//! Valid server answers and caller terms for the check tests, at [`NOW`]: each check accepts its
//! fixture unchanged, so a test changes one term and expects that term's refusal. Typed data
//! starts from the server builders' own output (`tests/vectors/server-typed-data.json`).
#![expect(clippy::needless_pass_by_value, reason = "test helpers take owned JSON")]

use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use alloy_primitives::{Address, U256, address, hex};
use alloy_signer_local::PrivateKeySigner;
use chrono::{DateTime, Utc};
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

use super::{
    FILL, HASHLOCK, IDENTITY, MAKER, NOW, OPERATOR, ORDER, OWNER, POOL, SWAP, at, edited,
    json::fixture, sample, time,
};
use crate::{
    Error, deployments,
    error::VerificationError,
    model::{
        EscrowChain, FundingSource, MakerPreparation, Order, QuoteLine, QuoteLinesPreparation,
        TakePreparation, ToSign, Venue, WalletFunding,
    },
    protocol::{
        checks::{
            LighterOwners,
            funding::PlanTerms,
            lines::{self, LinesAuthority, LinesRequest},
            maker::{CapitalRequest, MakerAuthority},
            take::{Delivery, TakeIntent, TakeReads, check_take},
        },
        lighter,
        signing::{SigningKey, Verified},
    },
    transport::Clock,
    types::{
        Amount, ClientId, EscrowEnvironment, FillId, MaxParts, Price, QuoteIdentityId, Revision,
        VenueId,
    },
};

/// A wallet a take delivers to.
pub(crate) const BYBIT: Address = address!("4444444444444444444444444444444444444444");
/// Production's escrow on HyperEVM and Polygon.
pub(crate) const ESCROW_HL: &str = "0x3100b7b49c0f9631c6af25877b5a4e13a47afad6";

/// [`NOW`].
pub(crate) fn now() -> DateTime<Utc> {
    at(0)
}

pub(crate) fn model<T: DeserializeOwned>(value: &Value) -> T {
    serde_json::from_value(value.clone()).unwrap()
}

/// The model fixture `base` (see [`fixture`]) with `patch`'s top-level keys replaced.
fn merged(base: &str, patch: Value) -> Value {
    let mut value = fixture(base);
    let Value::Object(patch) = patch else { unreachable!("patches are objects") };
    value.as_object_mut().unwrap().extend(patch);
    value
}

fn hex_address(address: Address) -> String {
    format!("{address:#x}")
}

// Venues.

fn chain_json(venue: &VenueId, environment: EscrowEnvironment) -> Value {
    let chain = deployments::chain(venue).unwrap();
    let escrow = deployments::contract(venue, environment).unwrap();
    json!({
        "wallet_funding": chain.wallet_token.map(|_| json!({
            "token": chain.token, "symbol": "USDC", "decimals": chain.decimals
        })),
        "wallet": chain.wallet_token.map(|token| json!({
            "token": token, "symbol": "USDC", "decimals": chain.decimals, "minimum": "10000"
        })),
        "id": chain.id,
        "escrow": escrow,
        "token": chain.token,
        "symbol": "USDC",
        "decimals": chain.decimals,
        "permit": chain.permit.map(|permit| json!({"name": permit.name, "version": permit.version})),
    })
}

fn venues_json(environment: EscrowEnvironment) -> Vec<Value> {
    ["hyperliquid", "polymarket", "gmx", "lighter", "aster"]
        .into_iter()
        .map(|id| {
            let venue: VenueId = id.parse().unwrap();
            let settlement = if deployments::is_venue_settled(&venue) { "venue" } else { "escrow" };
            json!({
                "id": id, "name": id, "enabled": true, "settlement": settlement,
                "chain": chain_json(&venue, environment), "margin_move": null,
                "release_minimum": "5000000",
            })
        })
        .collect()
}

/// Every venue, as production lists it.
pub(crate) fn venues() -> Vec<Venue> {
    venues_in(EscrowEnvironment::Production)
}

/// Every venue, as `environment` lists it.
pub(crate) fn venues_in(environment: EscrowEnvironment) -> Vec<Venue> {
    model(&Value::Array(venues_json(environment)))
}

/// Production's venues, edited.
pub(crate) fn venues_with(edit: impl FnOnce(&mut Vec<Value>)) -> Vec<Venue> {
    let mut list = venues_json(EscrowEnvironment::Production);
    edit(&mut list);
    model(&Value::Array(list))
}

/// `venue`'s production chain with `patch`'s keys replaced.
pub(crate) fn chain_with(venue: &str, patch: &Value) -> EscrowChain {
    let mut chain = chain_json(&venue.parse().unwrap(), EscrowEnvironment::Production);
    chain.as_object_mut().unwrap().extend(patch.as_object().unwrap().clone());
    model(&chain)
}

// Takes.

/// A taker fill of `amount_in` on `from` for `amount_out` on `to`, paid by [`OWNER`] to
/// [`MAKER`], delivered into [`OWNER`]'s account, paid within 45 seconds.
pub(crate) fn fill(from: &str, to: &str, amount_in: &str, amount_out: &str) -> Value {
    merged(
        "take_preparation/take/fills/0",
        json!({
            "funding": "venue", "id": FILL, "take_id": FILL, "role": "taker", "order_id": ORDER,
            "sequence": 7, "quote_index": 0, "swap_id": SWAP, "hashlock": HASHLOCK,
            "from_venue": from, "to_venue": to, "amount_in": amount_in, "amount_out": amount_out,
            "taker_fee": "0", "taker_fee_bps": 0, "maker_owner": MAKER, "maker_recipient": MAKER,
            "payer": OWNER, "taker_recipient": OWNER, "destination": "venue", "state": "PREPARED",
            "pay_by": time(45), "t1": time(3600), "t2": time(7200), "delivery_to": null,
        }),
    )
}

/// The escrow payment of `fill`, received as the fill delivers.
pub(crate) fn lock(fill: &Value) -> Value {
    let to: VenueId = fill["to_venue"].as_str().unwrap().parse().unwrap();
    let chain = deployments::chain(&to).unwrap();
    let wallet = fill["destination"] == "wallet";
    let escrow = deployments::contract(&to, EscrowEnvironment::Production).unwrap();
    json!({
        "payer": fill["payer"], "swapId": fill["swap_id"], "recipient": fill["maker_recipient"],
        "amount": fill["amount_in"], "hashlock": fill["hashlock"],
        "refundAt": DateTime::parse_from_rfc3339(fill["t2"].as_str().unwrap()).unwrap().timestamp(),
        "deadline": DateTime::parse_from_rfc3339(fill["pay_by"].as_str().unwrap()).unwrap().timestamp().to_string(),
        "orderId": fill["order_id"], "sequence": fill["sequence"], "amountOut": fill["amount_out"],
        "receiveChainId": chain.id.to_string(),
        "receiveToken": if wallet { chain.wallet_token.unwrap() } else { chain.token },
        "receiveEscrow": escrow, "receiveRecipient": fill["taker_recipient"], "dex": 0,
    })
}

/// A `PayTake` of `payments` by [`OWNER`] in production's HyperEVM escrow.
pub(crate) fn pay_take(payments: Vec<Value>, deposit: &str) -> Value {
    let mut typed = sample("pay_take");
    typed["message"] = json!({"payer": OWNER, "deposit": deposit, "payments": payments});
    typed
}

/// A take of `fills` asking to sign `to_sign`.
pub(crate) fn preparation(fills: Vec<Value>, to_sign: Vec<Value>) -> Value {
    let total = |field: &str| {
        let sum: U256 =
            fills.iter().map(|fill| fill[field].as_str().unwrap().parse::<U256>().unwrap()).sum();
        sum.to_string()
    };
    json!({
        "take": {
            "funding": "venue", "minimum_received": null, "id": FILL, "client_take_id": "take-1",
            "amount_in": total("amount_in"), "amount_out": total("amount_out"), "taker_fee": "0",
            "fills": fills,
        },
        "to_sign": to_sign,
    })
}

/// The `pay_take` request of `typed_data` for the take [`FILL`].
pub(crate) fn pay_item(typed_data: Value) -> Value {
    json!({"kind": "pay_take", "signer": OWNER, "fill_id": FILL, "typed_data": typed_data})
}

/// A take of 2 USDC from Hyperliquid for 2 USDC on Polymarket, into the taker's account.
pub(crate) fn escrow_take() -> Value {
    let fill = fill("hyperliquid", "polymarket", "2000000", "2000000");
    let payment = pay_take(vec![lock(&fill)], "0");
    preparation(vec![fill], vec![pay_item(payment)])
}

/// A fill id no fixture takes.
pub(crate) const UNKNOWN_FILL: &str = "0192f0a1-b2c3-7d4e-8f90-000000000000";

/// The first payment of an escrow take's `PayTake`.
pub(crate) fn payment(prepared: &mut Value) -> &mut Value {
    &mut prepared["to_sign"][0]["typed_data"]["message"]["payments"][0]
}

/// [`escrow_take`] with its fill's `field` set to `value`.
pub(crate) fn filled(field: &str, value: Value) -> Value {
    edited(escrow_take(), |p| p["take"]["fills"][0][field] = value)
}

/// [`escrow_take`] with its request's `field` set to `value`.
pub(crate) fn requested(field: &str, value: Value) -> Value {
    edited(escrow_take(), |p| p["to_sign"][0][field] = value)
}

/// [`escrow_take`] with its `PayTake`'s `field` set to `value`.
pub(crate) fn paying(field: &str, value: Value) -> Value {
    edited(escrow_take(), |p| p["to_sign"][0]["typed_data"]["message"][field] = value)
}

/// [`escrow_take`] with its payment's `field` set to `value`.
pub(crate) fn paid(field: &str, value: Value) -> Value {
    edited(escrow_take(), |p| payment(p)[field] = value)
}

/// The caller's terms of a take of `amount_in` from `from` for at least `min_out` on `to`.
pub(crate) fn intent(from: &str, to: &str, amount_in: u128, min_out: u128) -> TakeIntent {
    TakeIntent {
        from: from.parse().unwrap(),
        to: to.parse().unwrap(),
        amount_in: Amount::from(amount_in),
        min_out: Amount::from(min_out),
        max_parts: MaxParts::MAX,
        max_fee_bps: None,
        wallet_funding: false,
        delivery: Delivery::Account(OWNER),
        payer: OWNER,
        payer_account: None,
        at_once: false,
    }
}

/// [`escrow_take`]'s terms.
pub(crate) fn escrow_intent() -> TakeIntent {
    intent("hyperliquid", "polymarket", 2_000_000, 2_000_000)
}

/// A take of [`escrow_take`]'s fill delivered to [`BYBIT`]'s wallet on Polygon, under a wallet
/// swap id, with its least received.
pub(crate) fn wallet_take() -> Value {
    let mut fill = fill("hyperliquid", "polymarket", "2000000", "2000000");
    fill["destination"] = json!("wallet");
    fill["taker_recipient"] = json!(BYBIT);
    fill["swap_id"] = json!(format!("0x1b4c4374729e{}{}", "00".repeat(10), "cd".repeat(16)));
    let payment = pay_take(vec![lock(&fill)], "0");
    let mut prepared = preparation(vec![fill], vec![pay_item(payment)]);
    let usdc = deployments::chain(&VenueId::Polymarket).unwrap().wallet_token.unwrap();
    prepared["take"]["minimum_received"] = json!({"token": usdc, "amount": "1999000"});
    prepared
}

/// [`wallet_take`]'s terms.
pub(crate) fn wallet_intent() -> TakeIntent {
    TakeIntent { delivery: Delivery::Wallet(BYBIT), ..escrow_intent() }
}

/// A take from Hyperliquid into the Lighter account `731`, whose owner [`lighter_owners`] says
/// is [`OWNER`].
pub(crate) fn lighter_delivery_take() -> Value {
    let mut fill = fill("hyperliquid", "lighter", "2000000", "2000000");
    fill["delivery_to"] = json!("731");
    let payment = pay_take(vec![lock(&fill)], "0");
    preparation(vec![fill], vec![pay_item(payment)])
}

/// The owners Lighter reports: [`OWNER`]'s accounts 42 and 731, [`MAKER`]'s 731033.
pub(crate) fn lighter_owners() -> LighterOwners {
    LighterOwners::from([(42, OWNER), (731, OWNER), (731_033, MAKER)])
}

/// The fill memo of [`FILL`], in hex.
pub(crate) fn memo() -> String {
    hex::encode(lighter::fill_memo(FILL.parse::<FillId>().unwrap()))
}

/// A take of 2.0013 USDC from Lighter account 42 for 2 USDC on Hyperliquid.
pub(crate) fn lighter_take() -> Value {
    let fill = fill("lighter", "hyperliquid", "2001300", "2000000");
    let terms = json!({
        "api_key_index": 254, "from_account_index": 42, "to_account_index": 731_033,
        "amount": "2001300", "usdc_fee": "0", "memo": memo(), "nonce": 7,
    });
    let item =
        json!({"kind": "lighter_transfer", "signer": OWNER, "fill_id": FILL, "typed_data": terms});
    preparation(vec![fill], vec![item])
}

/// [`lighter_take`]'s terms.
pub(crate) fn lighter_intent() -> TakeIntent {
    TakeIntent { payer_account: Some(42), ..intent("lighter", "hyperliquid", 2_001_300, 2_000_000) }
}

/// An Aster transfer request of `fields` (name, value), in order.
pub(crate) fn aster_request(fields: &[(&str, String)], chain_id: u64) -> Value {
    let msg: Vec<String> = fields.iter().map(|(name, value)| format!("{name}={value}")).collect();
    let mut typed = sample("aster_transfer");
    typed["domain"]["chainId"] = json!(chain_id);
    typed["message"] = json!({"msg": msg.join("&")});
    typed
}

/// [`aster_take`]'s transfer fields: 1.500001 USDT to [`MAKER`] under [`FILL`], its nonce 55
/// seconds ahead.
pub(crate) fn aster_fields() -> Vec<(&'static str, String)> {
    vec![
        ("asset", "USDT".to_owned()),
        ("amount", "1.500001".to_owned()),
        ("toAddress", hex_address(MAKER)),
        ("clientTranId", FILL.replace('-', "")),
        ("nonce", ((NOW + 55) * 1_000_000).to_string()),
        ("user", hex_address(OWNER)),
    ]
}

/// The Aster take of `fields`.
pub(crate) fn aster_take_of(fields: &[(&str, String)]) -> Value {
    let fill = fill("aster", "hyperliquid", "1500000000000000001", "1000000");
    let item = json!({
        "kind": "aster_transfer", "signer": OWNER, "fill_id": FILL,
        "typed_data": aster_request(fields, 1666),
    });
    preparation(vec![fill], vec![item])
}

/// A take of 1.500000000000000001 USDT from Aster for 1 USDC on Hyperliquid.
pub(crate) fn aster_take() -> Value {
    aster_take_of(&aster_fields())
}

/// [`aster_take`]'s terms.
pub(crate) fn aster_intent() -> TakeIntent {
    intent("aster", "hyperliquid", 1_500_000_000_000_000_001, 1_000_000)
}

/// Signs `verified` with `key`, by a signer that starts at [`NOW`] and returns at `NOW + done`.
pub(crate) fn sign_until(
    key: PrivateKeySigner,
    verified: &Verified,
    done: i64,
) -> Result<(), VerificationError> {
    let calls = AtomicUsize::new(0);
    let clock: Clock =
        Arc::new(move || at(if calls.fetch_add(1, Ordering::SeqCst) == 0 { 0 } else { done }));
    let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
    match runtime.block_on(SigningKey::new(key).sign(verified, &clock)) {
        Ok(_) => Ok(()),
        Err(Error::Verification(error)) => Err(error),
        Err(other) => unreachable!("{other}"),
    }
}

/// Checks `prepared` against `intent` at [`NOW`] in production with `owners` and `funds`.
pub(crate) fn take_check(
    prepared: &Value,
    intent: &TakeIntent,
    owners: &LighterOwners,
    funds: Option<&WalletFunding>,
) -> Result<Verified, VerificationError> {
    let prepared: TakePreparation = model(prepared);
    let reads = TakeReads { lighter_owners: owners, wallet_funds: funds };
    check_take(&prepared, intent, reads, EscrowEnvironment::Production, now())
}

/// [`OWNER`]'s wallet funds on Hyperliquid: 5 USDC spendable, observed ten seconds ago.
pub(crate) fn wallet_funds() -> Value {
    json!({
        "venue": "hyperliquid", "chain_id": 999, "escrow": ESCROW_HL,
        "token": deployments::chain(&VenueId::Hyperliquid).unwrap().token, "symbol": "USDC",
        "decimals": 6, "address": OWNER, "wallet": "5000000", "available": "1000000",
        "committed_wallet": "0", "committed_available": "0", "spendable_wallet": "4000000",
        "spendable_available": "1000000", "block": 1, "observed_at": time(-10),
        "payment_setup": {"address": OWNER, "authorized": true, "pending": false, "hash": null, "permit": null},
    })
}

// Setup.

/// A required setup `step` asking to sign `request`.
fn setup_step(step: &str, request: Value) -> Value {
    json!({
        "step": step, "status": "required", "reason": null, "expires_at": null,
        "needed_for": ["pay"], "to_sign": request, "hash": null,
    })
}

/// The Hyperliquid permit step: the unlimited permit to production's escrow, ten minutes.
pub(crate) fn permit_step() -> Value {
    let request = json!({"kind": "permit", "signer": OWNER, "ref": "permit-1", "typed_data": sample("permit")});
    setup_step("permit", request)
}

/// Aster's agent approval step: LaMarge's agent, from one address, its nonce [`NOW`].
pub(crate) fn approval_step() -> Value {
    approval_step_of(sample("aster_approval")["message"]["msg"].as_str().unwrap())
}

/// Aster's agent approval step signing `msg`, under the vector's nonce.
pub(crate) fn approval_step_of(msg: &str) -> Value {
    let mut typed = sample("aster_approval");
    typed["message"]["msg"] = json!(msg);
    let request = json!({
        "kind": "aster_approval", "signer": OWNER, "ref": "1790000000000000", "typed_data": typed,
        "expires_at": time(60),
    });
    setup_step("agent", request)
}

/// The agent approval message of `fields` (name, value), in order.
pub(crate) fn approval_message(fields: &[(&str, String)]) -> String {
    fields.iter().map(|(name, value)| format!("{name}={value}")).collect::<Vec<_>>().join("&")
}

/// The vector's agent approval fields.
pub(crate) fn approval_fields() -> Vec<(&'static str, String)> {
    let msg = sample("aster_approval")["message"]["msg"].as_str().unwrap().to_owned();
    let names = [
        "user",
        "nonce",
        "agentName",
        "agentAddress",
        "expired",
        "signatureChainId",
        "canSpotTrade",
        "canPerpTrade",
        "canWithdraw",
        "ipWhitelist",
    ];
    names
        .into_iter()
        .zip(msg.split('&'))
        .map(|(name, pair)| (name, pair.split_once('=').unwrap().1.to_owned()))
        .collect()
}

/// The public key the registration fixture registers.
pub(crate) const LIGHTER_KEY: [u8; 40] = [7; 40];

/// Lighter's key registration step: production's key 254 on account 42 at nonce 9.
pub(crate) fn registration_step() -> Value {
    let message = lighter::registration_message(42, 254, &LIGHTER_KEY, 9);
    let request =
        json!({"kind": "lighter_api_key", "signer": OWNER, "ref": "9", "message": message});
    setup_step("api_key", request)
}

/// The wallet permit a transfer from Hyperliquid wallet funds asks for.
pub(crate) fn wallet_permit() -> Value {
    json!({
        "id": "0190f3a4-7b2c-7d10-8e4f-000000000003",
        "to_sign": {"kind": "permit", "signer": OWNER, "typed_data": sample("permit")},
    })
}

// Transfers.

/// A plan funding 10 USDC from Hyperliquid: a permit, 7 USDC out of margin, the payment.
pub(crate) fn funding_plan() -> Value {
    let request = |kind: &str| json!({"kind": kind, "signer": OWNER, "chain_id": 999});
    json!({
        "observed_at": time(-1), "venue": "hyperliquid", "owner": OWNER, "amount": "10000000",
        "deposit": "7000000", "withdrawal_amount": "7000000", "withdrawal_to": "signer",
        "payment_setup": "required", "signed_at_once": false,
        "requests": [request("permit"), request("margin_move"), request("pay_take")],
    })
}

/// [`funding_plan`]'s terms.
pub(crate) fn plan_terms() -> PlanTerms {
    PlanTerms {
        from: VenueId::Hyperliquid,
        payer: OWNER,
        amount_in: Amount::from(10_000_000u64),
        wallet_funding: false,
    }
}

// Maker.

/// [`OWNER`]'s new Hyperliquid pool for [`IDENTITY`], quoted by [`MAKER`].
pub(crate) fn pool() -> Value {
    merged(
        "maker_preparation/order",
        json!({
            "preparation_id": null, "capital_pool": true, "id": POOL, "venue": "hyperliquid",
            "owner_address": OWNER, "state": "preparing", "revision": "1", "available_to_quote": "0",
            "last_sequence": 0, "quote_set": null, "margin_move": null,
            "quote_identity_id": IDENTITY, "quote_signer": MAKER,
        }),
    )
}

/// The funding of [`pool`] with `amount`, depositing `deposit`.
pub(crate) fn fund_item(amount: &str, deposit: &str, floor: u64) -> Value {
    let mut typed = sample("fund_offer");
    typed["domain"]["chainId"] = json!(999);
    typed["domain"]["verifyingContract"] = json!(ESCROW_HL);
    typed["message"] = json!({
        "owner": OWNER, "offerId": POOL, "amount": amount, "deposit": deposit, "quoteSigner": MAKER,
        "operator": OPERATOR, "minQuoteSequence": floor, "nonce": "4", "deadline": (NOW + 3600).to_string(),
    });
    json!({"kind": "fund", "signer": OWNER, "typed_data": typed})
}

/// [`maker_preparation`] with its `FundOffer`'s `field` set to `value`.
pub(crate) fn funded(field: &str, value: Value) -> Value {
    edited(maker_preparation(), |p| p["to_sign"][0]["typed_data"]["message"][field] = value)
}

/// A preparation adding 10 USDC to a new pool from [`OWNER`]'s wallet.
pub(crate) fn maker_preparation() -> Value {
    json!({
        "id": "0192f1c2-3d4e-7f00-8a1b-2c3d4e5f6a40", "client_request_id": "request-1",
        "state": "preparing", "venue": "hyperliquid", "order": pool(),
        "to_sign": [fund_item("10000000", "10000000", 1)], "sequence": null,
        "expires_at": time(3600), "expected_revision": "1",
        "intent": {"amount": "10000000", "quote_identity_id": IDENTITY, "funding_source": "wallet"},
        "funding": {"existing_available": "0", "wallet_amount": "10000000", "venue_amount": "0", "idle_amount": "0"},
    })
}

/// [`maker_preparation`] funded by one margin move of the 10 USDC out of Hyperliquid.
pub(crate) fn margin_preparation() -> Value {
    let mut prepared = maker_preparation();
    let mut send = sample("send_asset_withdrawal");
    send["message"]["amount"] = json!("10");
    let margin = json!({"kind": "margin_move", "signer": OWNER, "typed_data": send});
    prepared["to_sign"] = json!([margin, fund_item("10000000", "10000000", 1)]);
    prepared["intent"]["funding_source"] = json!("venue");
    prepared["funding"]["wallet_amount"] = json!("0");
    prepared["funding"]["venue_amount"] = json!("10000000");
    prepared
}

/// [`maker_preparation`]'s request.
pub(crate) fn capital_request(funding_source: FundingSource) -> CapitalRequest {
    CapitalRequest {
        venue: VenueId::Hyperliquid,
        amount: Amount::from(10_000_000u64),
        client_request_id: ClientId::new("request-1").unwrap(),
        funding_source,
    }
}

/// The delegated identity's authority, over `existing`.
pub(crate) fn authority(existing: Option<&Order>) -> MakerAuthority<'_> {
    MakerAuthority {
        owner: OWNER,
        quote_signer: MAKER,
        quote_identity_id: IDENTITY.parse::<QuoteIdentityId>().unwrap(),
        operator: OPERATOR,
        existing,
    }
}

/// The close of orders `0x44…` and `0x55…` by [`OWNER`].
pub(crate) fn close_items() -> Value {
    json!([{"kind": "close_orders", "signer": OWNER, "typed_data": sample("close_orders")}])
}

pub(crate) fn to_sign(value: &Value) -> Vec<ToSign> {
    model(value)
}

// Quote lines.

/// The last admission time of the lines fixture.
pub(crate) const VALID_UNTIL: u64 = 253_402_214_400;

/// The maker's Hyperliquid and Polymarket pools, live, at sequence 1.
pub(crate) fn line_pools() -> Vec<Value> {
    [("hyperliquid", "aa"), ("polymarket", "bb")]
        .into_iter()
        .map(|(venue, byte)| {
            let mut pool = pool();
            pool["id"] = json!(format!("0x{}", byte.repeat(32)));
            pool["venue"] = json!(venue);
            pool["state"] = json!("live");
            pool["last_sequence"] = json!(1);
            pool
        })
        .collect()
}

/// Nico's lines: buy and sell on both venues, a hundred USDC of stock on Polymarket and a
/// thousand on Hyperliquid.
pub(crate) fn nico() -> Vec<Value> {
    vec![
        json!({"venue": "hyperliquid", "buy": "0.9994", "sell": "1.0002", "stock": "1000000000"}),
        json!({"venue": "polymarket", "buy": "1", "sell": "1.0014", "stock": "100000000"}),
    ]
}

/// The maker's recipients: [`OWNER`] on every venue it buys on.
pub(crate) fn recipients() -> BTreeMap<VenueId, Address> {
    ["hyperliquid", "polymarket", "lighter"]
        .into_iter()
        .map(|venue| (venue.parse().unwrap(), OWNER))
        .collect()
}

/// The preparation the server builds for `lines`: one draft per pool with stock, each quoting
/// every other venue with a buy price; `edit` changes a pool's quotes before they are signed.
pub(crate) fn lines_preparation(lines: &[Value], edit: impl Fn(&str, &mut Vec<Value>)) -> Value {
    let price = |value: &Value| value.as_str().map(|text| text.parse::<Price>().unwrap());
    let drafts: Vec<Value> = line_pools()
        .iter()
        .enumerate()
        .filter_map(|(index, pool)| {
            let venue = pool["venue"].as_str().unwrap();
            let line = lines.iter().find(|line| line["venue"] == venue)?;
            if line["stock"] == "0" {
                return None;
            }
            let stock: U256 = line["stock"].as_str().unwrap().parse().unwrap();
            let supply: VenueId = venue.parse().unwrap();
            let mut quotes: Vec<Value> = lines
                .iter()
                .filter(|other| other["venue"] != venue && !other["buy"].is_null())
                .map(|other| {
                    let paying: VenueId = other["venue"].as_str().unwrap().parse().unwrap();
                    let receive = lines::line_receive(
                        stock,
                        price(&line["sell"]).unwrap(),
                        price(&other["buy"]).unwrap(),
                        &paying,
                        &supply,
                    )
                    .unwrap();
                    let chain = deployments::chain(&paying).unwrap();
                    json!({
                        "venue": paying.as_str(), "chain_id": chain.id, "token": chain.token,
                        "recipient": OWNER, "supply": stock.to_string(), "receive": receive.to_string(),
                        "min_fill": "1000000",
                    })
                })
                .collect();
            edit(venue, &mut quotes);
            let expires_at = VALID_UNTIL + 3601;
            let mut typed = sample("quote_set");
            let chain = deployments::chain(&supply).unwrap();
            typed["domain"]["chainId"] = json!(chain.id);
            typed["domain"]["verifyingContract"] =
                json!(deployments::escrow(chain.id, EscrowEnvironment::Production).unwrap());
            let signed: Vec<Value> = quotes
                .iter()
                .map(|quote| {
                    json!({
                        "venue": quote["venue"], "chainId": quote["chain_id"], "token": quote["token"],
                        "recipient": quote["recipient"], "supply": quote["supply"],
                        "receive": quote["receive"], "minFill": quote["min_fill"],
                    })
                })
                .collect();
            typed["message"] = json!({
                "owner": OWNER, "offerId": pool["id"], "sequence": 2, "expiresAt": expires_at, "quotes": signed,
            });
            Some(json!({
                "order_id": pool["id"], "venue": venue,
                "draft_id": format!("0192f1c2-3d4e-7f00-8a1b-2c3d4e5f6a6{index}"), "sequence": 2,
                "valid_until": VALID_UNTIL, "expires_at": expires_at, "quote_size": line["stock"],
                "quotes": quotes, "to_sign": [{"kind": "quote_set", "signer": MAKER, "typed_data": typed}],
            }))
        })
        .collect();
    json!({
        "id": "0192f1c2-3d4e-7f00-8a1b-2c3d4e5f6a61", "quote_identity_id": IDENTITY,
        "expected_revision": "3", "valid_until": VALID_UNTIL, "lines": lines, "drafts": drafts,
        "cancels": [],
    })
}

/// Checks `prepared` against `lines` with the fixture's pools and recipients, at [`NOW`].
pub(crate) fn lines_check(
    prepared: &Value,
    lines: &[Value],
    recipients: &BTreeMap<VenueId, Address>,
) -> Result<Vec<Verified>, VerificationError> {
    let prepared: QuoteLinesPreparation = model(prepared);
    let lines: Vec<QuoteLine> = model(&Value::Array(lines.to_vec()));
    let pools: Vec<Order> = model(&Value::Array(line_pools()));
    let request = LinesRequest {
        lines: &lines,
        valid_until: VALID_UNTIL,
        expected_revision: Revision::new(3),
    };
    let authority = LinesAuthority {
        identity: IDENTITY.parse().unwrap(),
        quote_signer: MAKER,
        pools: &pools,
        recipients,
    };
    lines::check_prepared_lines(
        &prepared,
        &request,
        &authority,
        EscrowEnvironment::Production,
        now(),
    )
}

/// A maker preparation as served.
pub(crate) fn maker(value: &Value) -> MakerPreparation {
    model(value)
}

/// `value` as a JSON object, to remove or add keys.
pub(crate) fn object(value: &mut Value) -> &mut Map<String, Value> {
    value.as_object_mut().unwrap()
}
