//! The vectors and rejections of `sdk-ts/src/protocol/relay.test.ts`, on Relay orders saved from
//! live quotes (`tests/vectors/relay.json`).

use alloy_primitives::{U256, address};
use serde_json::{Value, json};

use super::*;
use crate::{error::VerificationKind, testing::OTHER, types::VenueId};

const VECTORS: &str =
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/vectors/relay.json"));
const PINS: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/vectors/pins.json"));
const PAYER: Address = address!("000000000000000000000000000000000000dead");
const NOW: i64 = 1_794_658_000;

fn vectors() -> Value {
    serde_json::from_str(VECTORS).unwrap()
}

fn case(name: &str) -> Value {
    vectors()["cases"][name].clone()
}

fn units(value: &Value) -> U256 {
    value.as_str().unwrap().parse().unwrap()
}

/// A case's request as the caller's terms: the recipient is the payer unless named, the floor
/// the account credit or 1.
struct Terms {
    from: MoveEndpoint,
    to: MoveEndpoint,
    recipient: Recipient,
    amount_in: U256,
    min_out: U256,
    credit: Option<U256>,
}

impl Terms {
    fn of(case: &Value) -> Self {
        let request = &case["request"];
        let to: MoveEndpoint = serde_json::from_value(request["to"].clone()).unwrap();
        let recipient = match request["recipient"].as_str() {
            Some(index) if to.venue == MoveVenue::Lighter => {
                Recipient::Lighter(index.parse().unwrap())
            }
            Some(address) => Recipient::Address(address.parse().unwrap()),
            None => Recipient::Address(PAYER),
        };
        let credit = request.get("credit").map(units);
        Self {
            from: serde_json::from_value(request["from"].clone()).unwrap(),
            to,
            recipient,
            amount_in: units(&request["amount_in"]),
            min_out: credit.unwrap_or(U256::from(1u8)),
            credit,
        }
    }

    fn expectation(&self) -> Expectation<'_> {
        Expectation {
            from: &self.from,
            to: &self.to,
            payer: PAYER,
            recipient: self.recipient,
            amount_in: self.amount_in,
            min_out: self.min_out,
            credit: self.credit,
        }
    }
}

fn order_id(case: &Value) -> B256 {
    case["order_id"].as_str().unwrap().parse().unwrap()
}

#[test]
fn order_ids_are_relays() {
    let vectors = vectors();
    let cases = vectors["cases"].as_object().unwrap();
    assert_eq!(cases.len(), 8);
    for (name, case) in cases {
        assert_eq!(relay_order_id(&case["order"]), Ok(order_id(case)), "{name}");
    }
}

#[test]
fn every_hashed_field_changes_the_id() {
    let case = case(LIGHTER_CASE);
    let id = order_id(&case);
    let edits: [fn(&mut Value); 4] = [
        |o| o["salt"] = json!(12),
        |o| o["output"]["payments"][0]["recipient"] = json!("4"),
        |o| o["inputs"][0]["refunds"][0]["deadline"] = json!("1795262576"),
        |o| o["output"]["calls"] = json!(["0x00"]),
    ];
    for edit in edits {
        let mut order = case["order"].clone();
        edit(&mut order);
        assert_ne!(relay_order_id(&order).unwrap(), id);
    }
    let mut order = case["order"].clone();
    order["output"]["payments"][0]["recipient"] = json!("0x03");
    assert_eq!(relay_order_id(&order), Err(differs(details::BRIDGE_ORDER)));
}

#[test]
fn every_requested_order_is_accepted() {
    let vectors = vectors();
    let mut accepted = 0;
    for (name, case) in vectors["cases"].as_object().unwrap() {
        if case.get("request").is_none() {
            continue;
        }
        let terms = Terms::of(case);
        let checked = check_bridge_order(&case["order"], &terms.expectation(), NOW).unwrap();
        assert_eq!(checked.id, order_id(case), "{name}");
        accepted += 1;
    }
    assert_eq!(accepted, 7);
}

/// `case`'s order edited by `edit` is refused with `detail`, under `terms` edited by `adjust`.
fn refused(
    name: &str,
    edit: impl FnOnce(&mut Value),
    adjust: impl FnOnce(&mut Terms),
) -> VerificationError {
    let case = case(name);
    let mut order = case["order"].clone();
    edit(&mut order);
    let mut terms = Terms::of(&case);
    adjust(&mut terms);
    check_bridge_order(&order, &terms.expectation(), NOW).unwrap_err()
}

fn differs_at(detail: &'static str, index: usize) -> VerificationError {
    at(differs(detail), index)
}

/// An edit of an order.
type Edit = Box<dyn FnOnce(&mut Value)>;

const PLAIN: &str = "hyperliquid_to_arbitrum";
const CALLS: &str = "hyperliquid_to_aster";
const LIGHTER_CASE: &str = "arbitrum_to_lighter";

#[test]
fn each_tampered_field_is_refused() {
    let keep = |_: &mut Terms| {};
    let cases: Vec<(&str, Edit, VerificationError)> = vec![
        (PLAIN, Box::new(|o| o["solver"] = json!(OTHER)), differs(details::ORDER_SOLVER)),
        (
            PLAIN,
            Box::new(|o| o["solverChainId"] = json!("arbitrum")),
            differs(details::ORDER_SOLVER),
        ),
        (PLAIN, Box::new(|o| o["version"] = json!("v2")), differs(details::ORDER_VERSION)),
        (
            CALLS,
            Box::new(|o| {
                o["output"]["extraData"] =
                    json!(format!("0x{}{}", "22".repeat(12), "33".repeat(20)));
            }),
            differs(details::ORDER_OUTPUT_EXTRA),
        ),
        (
            PLAIN,
            Box::new(|o| o["inputs"][0]["refunds"][1]["extraData"] = json!("0x")),
            differs_at(details::ORDER_REFUND_EXTRA, 1),
        ),
        (
            PLAIN,
            Box::new(|o| o["inputs"][0]["refunds"][0]["recipient"] = json!(OTHER)),
            differs_at(details::ORDER_REFUND, 0),
        ),
        (
            PLAIN,
            Box::new(|o| o["inputs"][0]["refunds"][1]["chainId"] = json!("base")),
            differs_at(details::ORDER_REFUND, 1),
        ),
        (
            PLAIN,
            Box::new(|o| o["output"]["payments"][0]["recipient"] = json!(OTHER)),
            differs(details::ORDER_RECIPIENT),
        ),
        (
            LIGHTER_CASE,
            Box::new(|o| o["output"]["payments"][0]["recipient"] = json!("4")),
            differs(details::ORDER_RECIPIENT),
        ),
        (
            PLAIN,
            Box::new(|o| o["inputs"][0]["payment"]["amount"] = json!("1")),
            differs(details::ORDER_AMOUNT),
        ),
        (
            PLAIN,
            Box::new(|o| {
                o["inputs"][0]["payment"]["currency"] = json!("0x6d1e7cde53ba9467b783cb7c530ce054");
            }),
            differs(details::ORDER_INPUT),
        ),
        (
            PLAIN,
            Box::new(|o| {
                let input = o["inputs"][0].clone();
                o["inputs"].as_array_mut().unwrap().push(input);
            }),
            differs(details::ORDER_INPUTS),
        ),
        (
            PLAIN,
            Box::new(|o| {
                let payment = o["output"]["payments"][0].clone();
                o["output"]["payments"].as_array_mut().unwrap().push(payment);
            }),
            differs(details::ORDER_PAYMENTS),
        ),
        (
            PLAIN,
            Box::new(|o| {
                o["fees"] = json!([{"recipientChainId": "base", "recipient": PAYER,
                    "currencyChainId": "base", "currency": PAYER, "amount": "1"}]);
            }),
            differs(details::ORDER_FEES),
        ),
        (
            PLAIN,
            Box::new(|o| o["output"]["calls"] = case(CALLS)["order"]["output"]["calls"].clone()),
            differs(details::ORDER_CALLS),
        ),
        (
            CALLS,
            Box::new(|o| o["output"]["calls"].as_array_mut().unwrap().truncate(2)),
            differs(details::ORDER_CALLS),
        ),
        (
            CALLS,
            Box::new(|o| o["output"]["payments"][0]["minimumAmount"] = json!("1")),
            differs(details::ORDER_MINIMUM),
        ),
    ];
    for (index, (name, edit, error)) in cases.into_iter().enumerate() {
        assert_eq!(refused(name, edit, keep), error, "case {index} on {name}");
    }
}

#[test]
fn terms_the_order_does_not_meet_are_refused() {
    let lower = refused(
        PLAIN,
        |o| o["output"]["payments"][0]["minimumAmount"] = json!("1"),
        |t| t.min_out = U256::from(99_000_000u64),
    );
    assert_eq!(lower, differs(details::ORDER_MINIMUM));
    let credit = U256::from(94u8) * U256::from(10u8).pow(U256::from(18u8));
    let other_credit = refused(CALLS, |_| {}, |t| (t.credit, t.min_out) = (Some(credit), credit));
    assert_eq!(other_credit, differs(details::ORDER_CALLS));
    let no_credit = refused(CALLS, |_| {}, |t| t.credit = None);
    assert_eq!(no_credit, differs(details::ACCOUNT_CREDIT));
    let lighter_as_address = refused(
        LIGHTER_CASE,
        |o| o["inputs"][0]["refunds"].as_array_mut().unwrap().truncate(1),
        |t| t.recipient = Recipient::Address(PAYER),
    );
    assert_eq!(lighter_as_address, differs(details::ORDER_RECIPIENT));
    let aster = refused(PLAIN, |_| {}, |t| t.from = MoveEndpoint::account(&VenueId::Aster));
    assert_eq!(aster, unsupported(details::BRIDGE_ORIGIN));

    let case = case(PLAIN);
    let terms = Terms::of(&case);
    let expired = check_bridge_order(&case["order"], &terms.expectation(), 4_000_000_000);
    assert_eq!(expired.unwrap_err(), stale(details::ORDER_DEADLINE));
    let unreadable = check_bridge_order(&json!({"version": "v1"}), &terms.expectation(), NOW);
    assert_eq!(unreadable.unwrap_err().kind, VerificationKind::Differs);
}

#[test]
fn the_deposit_and_approval_equal_relays() {
    let case = case("arbitrum_to_hyperliquid");
    let origin = relay_endpoint(&Terms::of(&case).from, true).unwrap();
    let built =
        bridge_transactions(order_id(&case), origin, PAYER, U256::from(100_000_000u64)).unwrap();
    let step = |name: &str| case["relay_steps"][name].as_str().unwrap().parse::<Bytes>().unwrap();
    assert_eq!(built.deposit, step("deposit"));
    assert_eq!(built.approve, step("approve"));
    assert_eq!((built.chain_id, Some(built.token)), (42161, origin.token()));
    assert!(bridge_transactions(order_id(&case), &HYPERCORE, PAYER, U256::from(1u8)).is_none());
}

#[test]
fn the_hypercore_authorization_and_send_equal_relays() {
    let case = case(PLAIN);
    let steps = &case["relay_steps"];
    let authorization = &steps["authorization"];
    let nonce = authorization["value"]["nonce"].as_u64().unwrap();
    let relays: TypedData = serde_json::from_value(json!({
        "types": authorization["types"],
        "primaryType": authorization["primaryType"],
        "domain": authorization["domain"],
        "message": authorization["value"],
    }))
    .unwrap();
    let ours = bridge_authorization(order_id(&case), PAYER, nonce);
    assert_eq!(ours.encode_type().unwrap(), relays.encode_type().unwrap());
    assert_eq!(ours.eip712_signing_hash().unwrap(), relays.eip712_signing_hash().unwrap());
    let send = bridge_send_asset(U256::from(100_000_000u64), nonce).typed_data();
    assert_eq!(serde_json::to_value(&send).unwrap()["message"], steps["send_asset"]);
    assert_eq!(bridge_send_asset(U256::from(5u8), 1).amount, "0.000005");
}

#[test]
fn hypercore_payloads_sign_for_the_payer_until_the_deadline() {
    let case = case(PLAIN);
    let terms = Terms::of(&case);
    let checked = check_bridge_order(&case["order"], &terms.expectation(), NOW).unwrap();
    let [authorization, send] = hypercore_payloads(checked, &terms.expectation(), 7).unwrap();
    let deadline = i64::from(checked.deadline) - 30;
    assert_eq!((authorization.not_after(), send.not_after()), (Some(deadline), Some(deadline)));
    let expected = bridge_authorization(checked.id, PAYER, 7).eip712_signing_hash().unwrap();
    assert_eq!(authorization.digest(), expected);
    assert_eq!(
        hypercore_payloads(checked, &terms.expectation(), 0).unwrap_err(),
        differs(details::NONCE)
    );
    let wallet = Terms::of(&self::case("arbitrum_to_hyperliquid"));
    let refused = hypercore_payloads(checked, &wallet.expectation(), 7).unwrap_err();
    assert_eq!(refused, unsupported(details::BRIDGE_ORIGIN));
}

#[test]
fn destination_calls_equal_the_orders() {
    for venue in [MoveVenue::Aster, MoveVenue::Polymarket, MoveVenue::Gmx] {
        let case = case(&format!("hyperliquid_to_{venue}"));
        let credit = units(&case["request"]["credit"]);
        let calls: Vec<Bytes> = case["order"]["output"]["calls"]
            .as_array()
            .unwrap()
            .iter()
            .map(|call| call.as_str().unwrap().parse().unwrap())
            .collect();
        assert_eq!(destination_calls(&venue, PAYER, credit), Some(calls), "{venue}");
    }
    assert_eq!(destination_calls(&MoveVenue::Gmx, PAYER, U256::ZERO), None);
    assert_eq!(destination_calls(&MoveVenue::Hyperliquid, PAYER, U256::from(1u8)), None);
}

#[test]
fn units_convert_with_pinned_decimals() {
    let hyperliquid = relay_endpoint(&MoveEndpoint::account(&VenueId::Hyperliquid), true).unwrap();
    assert_eq!(
        to_relay_units(U256::from(1_000_000u64), hyperliquid),
        Some(U256::from(100_000_000u64))
    );
    assert_eq!(from_relay_units(U256::from(99_999_999u64), hyperliquid), U256::from(999_999u64));
    let bsc = relay_endpoint(&MoveEndpoint::account(&VenueId::Aster), false).unwrap();
    let one = U256::from(10u8).pow(U256::from(18u8));
    assert_eq!((to_relay_units(one, bsc), from_relay_units(one, bsc)), (Some(one), one));
    assert_eq!(to_relay_units(U256::MAX, hyperliquid), None);
    let pusd = MoveEndpoint::wallet(137, address!("c011a7e12a19f7b1f670d46f03b03f3342e82dfb"));
    let native = MoveEndpoint::wallet(137, address!("3c499c542cef5e3811e1192ce70d8cc03d5c3359"));
    assert_ne!(
        relay_endpoint(&pusd, true).unwrap().currency,
        relay_endpoint(&native, true).unwrap().currency
    );
    let mainnet = MoveEndpoint::wallet(1, address!("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"));
    assert_eq!(relay_endpoint(&mainnet, true), Err(unsupported(details::BRIDGE_WALLET)));
    let lighter = MoveEndpoint::account(&VenueId::Lighter);
    assert_eq!(relay_endpoint(&lighter, true), Err(unsupported(details::BRIDGE_ORIGIN)));
    assert_eq!(relay_endpoint(&lighter, false), Ok(&LIGHTER));
}

/// `pins.json` is written from the TypeScript SDK's objects (CONTRIBUTING.md).
#[test]
fn relay_pins_equal_the_typescript_pins() {
    let pins: Value = serde_json::from_str(PINS).unwrap();
    let relay = &pins["relay"];
    let address = |value: &Value| value.as_str().unwrap().parse::<Address>().unwrap();
    assert_eq!(address(&relay["depository"]), DEPOSITORY);
    assert_eq!(address(&relay["hyperliquid_depository"]), HYPERLIQUID_DEPOSITORY);
    assert_eq!(address(&relay["router"]), ROUTER);
    assert_eq!(address(&relay["solver"]), SOLVER);
    assert_eq!(relay["solver_chain_id"], SOLVER_CHAIN_ID);
    let deposits = &relay["venue_deposits"];
    assert_eq!(address(&deposits["polymarket"]), POLYMARKET_DEPOSIT);
    assert_eq!(address(&deposits["gmx"]), GMX_ROUTER);
    assert_eq!(address(&deposits["gmx_vault"]), GMX_VAULT);
    assert_eq!(address(&deposits["aster"]), ASTER_VAULT);
    assert_eq!(Some(ASTER_VAULT), deployments::venue_contract(&VenueId::Aster));

    let same = |ours: &RelayEndpoint, pinned: &Value| {
        let vm = match ours.vm {
            Vm::Evm => "evm",
            Vm::Hyperliquid => "hyperliquid",
            Vm::Lighter => "lighter",
        };
        assert_eq!(
            json!({"chain": ours.chain, "chain_id": ours.chain_id, "currency": ours.currency,
                "decimals": ours.decimals, "units": ours.units, "vm": vm}),
            *pinned
        );
    };
    let wallets = relay["wallets"].as_array().unwrap();
    assert_eq!(wallets.len(), WALLETS.len());
    for (ours, pinned) in WALLETS.iter().zip(wallets) {
        same(ours, pinned);
    }
    let accounts = relay["accounts"].as_object().unwrap();
    assert_eq!(accounts.len(), 5);
    let listed = |name: &str, venue: &str| {
        relay[name].as_array().unwrap().iter().any(|v| v.as_str() == Some(venue))
    };
    for (name, pinned) in accounts {
        let venue: MoveVenue = name.parse().unwrap();
        same(account(&venue).unwrap(), pinned);
        assert_eq!(is_origin(&venue), listed("origin_venues", name), "{name}");
        assert_eq!(is_call_venue(&venue), listed("call_venues", name), "{name}");
    }
    assert!(account(&MoveVenue::Wallet).is_none());
    let minimums = relay["account_minimums"].as_object().unwrap();
    for (name, _) in accounts {
        let pinned = minimums.get(name).map_or(U256::ZERO, units);
        assert_eq!(account_minimum(&name.parse().unwrap()), pinned, "{name}");
    }
    // The wallet tokens are the pinned chains' tokens.
    for chain in
        [deployments::HYPERLIQUID, deployments::POLYMARKET, deployments::GMX, deployments::ASTER]
    {
        assert!(WALLETS.iter().any(|w| w.chain_id == chain.id && w.token() == Some(chain.token)));
    }
    assert!(WALLETS.iter().any(|w| w.token() == deployments::POLYMARKET.wallet_token));
}

#[test]
fn part_floors_round_up_and_nonces_are_fresh() {
    let floor = |part: u64, min_out: u64, amount_in: u64| {
        part_floor(U256::from(part), U256::from(min_out), U256::from(amount_in))
    };
    assert_eq!(floor(1, 2, 3), Some(U256::from(1u8)));
    assert_eq!(floor(3, 2, 3), Some(U256::from(2u8)));
    assert_eq!(floor(9_000, 9_900, 10_000), Some(U256::from(8_910u64)));
    assert_eq!(floor(1, 1, 0), None);
    assert_eq!(part_floor(U256::MAX, U256::from(2u8), U256::from(1u8)), None);

    let now = 1_794_657_763_758;
    let nonce = |n: i64| check_bridge_nonce(u64::try_from(n).unwrap(), now);
    assert_eq!(nonce(now), Ok(u64::try_from(now).unwrap()));
    assert!(nonce(now - 1_799_999).is_ok() && nonce(now + 299_999).is_ok());
    for refused in [now - 1_800_000, now + 300_000, 0] {
        assert_eq!(nonce(refused), Err(differs(details::NONCE)));
    }
}
