//! Pinned deployments: chains, tokens, escrows and signing domains per environment.
//!
//! Copied from `sdk-ts/src/protocol/{protocol,deployments}.ts`; `tests/vectors/pins.json`, generated
//! from the TypeScript objects, must equal these tables (unit test below). A new deployment
//! changes both SDKs together.

use alloy_primitives::{Address, U256, address};

use crate::types::{EscrowEnvironment, VenueId};

/// An EIP-2612 permit domain's name and version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PermitDomain {
    pub(crate) name: &'static str,
    pub(crate) version: &'static str,
}

/// A venue's settlement chain: the escrow chain of an escrow venue, the signing chain of a venue
/// settled inside itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Chain {
    pub(crate) id: u64,
    /// The settlement token (USDC, pUSD, USDT).
    pub(crate) token: Address,
    pub(crate) decimals: u8,
    /// The token's permit domain, if it has one.
    pub(crate) permit: Option<PermitDomain>,
    /// What a wallet delivery pays on this chain, of `decimals` too; none on a venue settled
    /// inside itself.
    pub(crate) wallet_token: Option<Address>,
}

impl Chain {
    /// One millionth of the token: venues move six decimals at most.
    pub(crate) fn step(&self) -> U256 {
        U256::from(10u8).pow(U256::from(self.decimals.saturating_sub(6)))
    }
}

/// HyperEVM and USDC.
pub(crate) const HYPERLIQUID: Chain = Chain {
    id: 999,
    token: address!("b88339cb7199b77e23db6e890353e22632ba630f"),
    decimals: 6,
    permit: Some(PermitDomain { name: "USDC", version: "2" }),
    wallet_token: Some(address!("b88339cb7199b77e23db6e890353e22632ba630f")),
};
/// Polygon and pUSD.
pub(crate) const POLYMARKET: Chain = Chain {
    id: 137,
    token: address!("c011a7e12a19f7b1f670d46f03b03f3342e82dfb"),
    decimals: 6,
    permit: Some(PermitDomain { name: "Polymarket USD", version: "1" }),
    // Native USDC, converted from pUSD.
    wallet_token: Some(address!("3c499c542cef5e3811e1192ce70d8cc03d5c3359")),
};
/// Arbitrum and USDC.
pub(crate) const GMX: Chain = Chain {
    id: 42161,
    token: address!("af88d065e77c8cc2239327c5edb3a432268e5831"),
    decimals: 6,
    permit: Some(PermitDomain { name: "USD Coin", version: "2" }),
    wallet_token: Some(address!("af88d065e77c8cc2239327c5edb3a432268e5831")),
};
/// Lighter's L2 chain id and its USDC on Ethereum.
pub(crate) const LIGHTER: Chain = Chain {
    id: 304,
    token: address!("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"),
    decimals: 6,
    permit: None,
    wallet_token: None,
};
/// BSC and USDT.
pub(crate) const ASTER: Chain = Chain {
    id: 56,
    token: address!("55d398326f99059ff775485246999027b3197955"),
    decimals: 18,
    permit: None,
    wallet_token: None,
};

/// `venue`'s pinned chain; `None` for a venue this SDK does not trade.
pub(crate) fn chain(venue: &VenueId) -> Option<&'static Chain> {
    match venue {
        VenueId::Hyperliquid => Some(&HYPERLIQUID),
        VenueId::Polymarket => Some(&POLYMARKET),
        VenueId::Gmx => Some(&GMX),
        VenueId::Lighter => Some(&LIGHTER),
        VenueId::Aster => Some(&ASTER),
        VenueId::Unknown(_) => None,
    }
}

/// Whether `venue` settles inside itself (no escrow): Lighter and Aster.
pub(crate) fn is_venue_settled(venue: &VenueId) -> bool {
    matches!(venue, VenueId::Lighter | VenueId::Aster)
}

/// A reviewed escrow proxy (v5 UUPS), never an implementation or an address the API supplies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Escrow {
    pub(crate) chain_id: u64,
    pub(crate) address: Address,
    pub(crate) environment: EscrowEnvironment,
}

/// The active escrows, one per environment and escrow chain. Retired proxies are not pinned:
/// nothing new is signed for them.
pub(crate) const ESCROWS: [Escrow; 6] = [
    Escrow {
        chain_id: 999,
        address: address!("3100b7b49c0f9631c6af25877b5a4e13a47afad6"),
        environment: EscrowEnvironment::Production,
    },
    Escrow {
        chain_id: 137,
        address: address!("3100b7b49c0f9631c6af25877b5a4e13a47afad6"),
        environment: EscrowEnvironment::Production,
    },
    Escrow {
        chain_id: 42161,
        address: address!("7b0c3eb38153614452a56bc9295d1bd084a9190a"),
        environment: EscrowEnvironment::Production,
    },
    Escrow {
        chain_id: 999,
        address: address!("948e65c40442d935d4ee990bdbd01bdd825c4873"),
        environment: EscrowEnvironment::Development,
    },
    Escrow {
        chain_id: 137,
        address: address!("a3fca394739ec4b984e908bd7a3c3b72d0ed77d1"),
        environment: EscrowEnvironment::Development,
    },
    Escrow {
        chain_id: 42161,
        address: address!("66e4c25d7e92c3e2765b021a748e740f775ac112"),
        environment: EscrowEnvironment::Development,
    },
];

/// The pinned escrow of `environment` on `chain_id`.
pub(crate) fn escrow(chain_id: u64, environment: EscrowEnvironment) -> Option<Address> {
    ESCROWS
        .iter()
        .find(|escrow| escrow.chain_id == chain_id && escrow.environment == environment)
        .map(|escrow| escrow.address)
}

/// The environment whose pinned escrow on `chain_id` is `address`.
pub(crate) fn escrow_environment(chain_id: u64, address: Address) -> Option<EscrowEnvironment> {
    ESCROWS
        .iter()
        .find(|escrow| escrow.chain_id == chain_id && escrow.address == address)
        .map(|escrow| escrow.environment)
}

/// The contract a venue settled inside itself signs quote sets for, on its own chain, and nothing
/// is ever sent to: Lighter's zkLighter contract, Aster's vault.
pub(crate) fn venue_contract(venue: &VenueId) -> Option<Address> {
    match venue {
        VenueId::Lighter => Some(address!("3b4d794a66304f130a4db8f2551b0070dfcf5ca7")),
        VenueId::Aster => Some(address!("128463a60784c4d3f46c23af3f65ed859ba87974")),
        _ => None,
    }
}

/// The contract `venue`'s escrow requests name: its [`venue_contract`], else `environment`'s
/// escrow on its chain.
pub(crate) fn contract(venue: &VenueId, environment: EscrowEnvironment) -> Option<Address> {
    venue_contract(venue).or_else(|| escrow(chain(venue)?.id, environment))
}

/// What an environment registers on venues: registering another environment's slot replaces it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Slots {
    pub(crate) aster_agent_name: &'static str,
    pub(crate) lighter_key_index: u8,
}

/// `environment`'s Aster agent name and Lighter API key index.
pub(crate) const fn slots(environment: EscrowEnvironment) -> Slots {
    match environment {
        EscrowEnvironment::Production => {
            Slots { aster_agent_name: "LaMarge", lighter_key_index: 254 }
        }
        EscrowEnvironment::Development => {
            Slots { aster_agent_name: "LaMargeDev", lighter_key_index: 253 }
        }
    }
}

/// Seconds a quote set outlasts its `valid_until` (`FILL_MAKER_TIMEOUT`), and the latest `t1`.
pub(crate) const MAKER_TIMEOUT_SECONDS: u64 = 3600;
/// The latest a take's payment refunds, `t2`: the escrow's `MAX_LOCK_DURATION`.
pub(crate) const TAKER_TIMEOUT_SECONDS: u64 = 86_400;
/// The most seconds to a route's second take's `pay_by` when signed at once.
pub(crate) const ROUTE_PAY_WINDOW_SECONDS: u64 = 210;
/// A take signed at once with its withdrawal is accepted until this long before its `pay_by`.
pub(crate) const AT_ONCE_SEND_MARGIN_SECONDS: u64 = 30;
/// Left before that cutoff for each request of a take signed at once.
pub(crate) const AT_ONCE_SIGNING_SECONDS: u64 = 30;
/// The least share of a Polygon wallet delivery's pUSD its recipient receives in native USDC.
pub(crate) const WALLET_SWAP_MIN_BPS: u16 = 9_995;
/// The allowance a permit grants, once.
pub(crate) const UNLIMITED_ALLOWANCE: U256 = U256::MAX;
/// The most Lighter charges a taker's account on top of a transfer: 1 USDC.
pub(crate) const LIGHTER_MAX_TAKER_FEE: U256 = U256::from_limbs([1_000_000, 0, 0, 0]);
/// USDC on `HyperCore`, as `SendAsset` names it.
pub(crate) const HYPERLIQUID_USDC: &str = "USDC:0x6d1e7cde53ba9467b783cb7c530ce054";
/// USDC's `HyperCore` system address: a `SendAsset` there withdraws to the sender's HyperEVM
/// address.
pub(crate) const HYPERLIQUID_BRIDGE: Address = address!("2000000000000000000000000000000000000000");
/// The contract of Polymarket's withdrawal signing domain.
pub(crate) const POLYMARKET_WITHDRAW_CONTRACT: Address =
    address!("dca4af75705dbb50f62437045aff9921947917d2");

/// `value` rounded up to whole millionths of `venue`'s token: what the venue moves for it.
/// `None` for an unknown venue or above `2^256 - 1`.
pub(crate) fn venue_units(value: U256, venue: &VenueId) -> Option<U256> {
    let step = chain(venue)?.step();
    let remainder = value % step;
    if remainder.is_zero() { Some(value) } else { value.checked_add(step - remainder) }
}

/// A venue's decimal token amount (`"12.5"`, six decimals at most) in base units of a token with
/// `decimals`, exactly. `None` for any other text.
pub(crate) fn decimal_units(text: &str, decimals: u8) -> Option<U256> {
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    let digits = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
    if !digits(whole) || (text.contains('.') && (!digits(fraction) || fraction.len() > 6)) {
        return None;
    }
    crate::types::Amount::parse_units(text, decimals).ok().map(crate::types::Amount::get)
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;

    const ENVIRONMENTS: [EscrowEnvironment; 2] =
        [EscrowEnvironment::Production, EscrowEnvironment::Development];

    const PINS: &str =
        include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/vectors/pins.json"));

    fn venue(name: &str) -> VenueId {
        name.parse().unwrap()
    }

    fn address_of(value: &Value) -> Option<Address> {
        value.as_str().map(|text| text.parse().unwrap())
    }

    /// `pins.json` is written from the TypeScript SDK's objects (CONTRIBUTING.md).
    #[test]
    fn tables_equal_the_typescript_pins() {
        let pins: Value = serde_json::from_str(PINS).unwrap();

        let chains = pins["chains"].as_object().unwrap();
        assert_eq!(chains.len(), 5);
        for (name, pinned) in chains {
            let ours = chain(&venue(name)).expect(name);
            assert_eq!(ours.id, pinned["id"].as_u64().unwrap(), "{name}");
            assert_eq!(Some(ours.token), address_of(&pinned["token"]), "{name}");
            assert_eq!(u64::from(ours.decimals), pinned["decimals"].as_u64().unwrap(), "{name}");
            assert_eq!(Some(ours.decimals), venue(name).decimals(), "{name}");
            let permit = ours.permit.map(|permit| (permit.name, permit.version));
            let pinned_permit = pinned["permit"].as_object().map(|permit| {
                (permit["name"].as_str().unwrap(), permit["version"].as_str().unwrap())
            });
            assert_eq!(permit, pinned_permit, "{name}");
            assert_eq!(ours.wallet_token, address_of(&pinned["wallet_token"]), "{name}");
        }

        let escrows = pins["escrows"].as_array().unwrap();
        assert_eq!(escrows.len(), ESCROWS.len());
        for (ours, pinned) in ESCROWS.iter().zip(escrows) {
            assert_eq!(ours.chain_id, pinned["chain_id"].as_u64().unwrap());
            assert_eq!(Some(ours.address), address_of(&pinned["address"]));
            assert_eq!(ours.environment.as_str(), pinned["environment"].as_str().unwrap());
        }

        let domains = pins["venue_domains"].as_array().unwrap();
        assert_eq!(domains.len(), 2);
        for pinned in domains {
            let name = pinned["venue"].as_str().unwrap();
            assert_eq!(chain(&venue(name)).unwrap().id, pinned["chain_id"].as_u64().unwrap());
            assert_eq!(venue_contract(&venue(name)), address_of(&pinned["address"]));
            assert!(is_venue_settled(&venue(name)));
        }
        let settled: Vec<&str> =
            pins["venue_settled"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
        for name in chains.keys() {
            assert_eq!(is_venue_settled(&venue(name)), settled.contains(&name.as_str()), "{name}");
        }

        let slots_pinned = pins["environment_slots"].as_object().unwrap();
        assert_eq!(slots_pinned.len(), ENVIRONMENTS.len());
        for environment in ENVIRONMENTS {
            let pinned = &slots_pinned[environment.as_str()];
            let ours = slots(environment);
            assert_eq!(ours.aster_agent_name, pinned["aster_agent_name"].as_str().unwrap());
            assert_eq!(
                u64::from(ours.lighter_key_index),
                pinned["lighter_key_index"].as_u64().unwrap()
            );
        }

        let constants = &pins["constants"];
        let number = |name: &str| constants[name].as_u64().unwrap();
        let units = |name: &str| constants[name].as_str().unwrap().parse::<U256>().unwrap();
        assert_eq!(MAKER_TIMEOUT_SECONDS, number("maker_timeout_seconds"));
        assert_eq!(TAKER_TIMEOUT_SECONDS, number("taker_timeout_seconds"));
        assert_eq!(ROUTE_PAY_WINDOW_SECONDS, number("route_pay_window_seconds"));
        assert_eq!(AT_ONCE_SEND_MARGIN_SECONDS, number("at_once_send_margin_seconds"));
        assert_eq!(AT_ONCE_SIGNING_SECONDS, number("at_once_signing_seconds"));
        assert_eq!(u64::from(WALLET_SWAP_MIN_BPS), number("wallet_swap_min_bps"));
        assert_eq!(UNLIMITED_ALLOWANCE, units("unlimited_allowance"));
        assert_eq!(LIGHTER_MAX_TAKER_FEE, units("lighter_max_taker_fee"));
        assert_eq!(crate::transport::LIGHTER_API, constants["lighter_api"].as_str().unwrap());
        assert_eq!(crate::protocol::aster::CHAIN_ID, number("aster_chain_id"));
        assert_eq!(crate::protocol::aster::TRANSFER_CHAIN_ID, number("aster_transfer_chain_id"));
        assert_eq!(u64::from(crate::protocol::lighter::USDC_ASSET), number("lighter_usdc_asset"));
        assert_eq!(
            u64::from(crate::protocol::lighter::COLLATERAL_ROUTE),
            number("lighter_collateral_route")
        );
        let tags = constants["fee_tags"].as_object().unwrap();
        for (name, tag) in crate::protocol::fees::TAGS_FOR_PINS {
            let pinned = tags[name].as_str().unwrap();
            assert_eq!(format!("0x{}", alloy_primitives::hex::encode(tag)), pinned, "{name}");
        }
    }

    #[test]
    fn each_environment_has_one_escrow_per_escrow_chain() {
        for environment in ENVIRONMENTS {
            for venue in [VenueId::Hyperliquid, VenueId::Polymarket, VenueId::Gmx] {
                let id = chain(&venue).unwrap().id;
                let address = escrow(id, environment).unwrap();
                assert_eq!(escrow_environment(id, address), Some(environment));
                assert_eq!(contract(&venue, environment), Some(address));
            }
            for venue in [VenueId::Lighter, VenueId::Aster] {
                assert_eq!(escrow(chain(&venue).unwrap().id, environment), None);
                assert_eq!(contract(&venue, environment), venue_contract(&venue));
            }
        }
        // Production's proxy is the same address on HyperEVM and Polygon, never on Arbitrum.
        let production = escrow(999, EscrowEnvironment::Production).unwrap();
        assert_eq!(escrow(137, EscrowEnvironment::Production), Some(production));
        assert_eq!(escrow_environment(42161, production), None);
        // Retired proxies sign nothing new.
        let retired = address!("ea5ba9e1c0f5dde572bc08f3af6a3e8b9db73734");
        assert_eq!(escrow_environment(999, retired), None);
        assert_eq!(escrow_environment(56, production), None);
    }

    #[test]
    fn unknown_venues_are_not_pinned() {
        let orderly = venue("orderly");
        assert!(chain(&orderly).is_none());
        assert!(venue_contract(&orderly).is_none());
        assert!(venue_contract(&VenueId::Hyperliquid).is_none());
        assert!(contract(&orderly, EscrowEnvironment::Production).is_none());
        assert!(!is_venue_settled(&orderly));
        assert_eq!(venue_units(U256::from(1u8), &orderly), None);
    }

    #[test]
    fn venue_units_round_up_to_millionths() {
        let usdt = U256::from(10u8).pow(U256::from(12u8));
        assert_eq!(ASTER.step(), usdt);
        assert_eq!(HYPERLIQUID.step(), U256::from(1u8));
        assert_eq!(venue_units(U256::from(1u8), &VenueId::Aster), Some(usdt));
        assert_eq!(venue_units(usdt, &VenueId::Aster), Some(usdt));
        assert_eq!(
            venue_units(usdt + U256::from(1u8), &VenueId::Aster),
            Some(usdt * U256::from(2u8))
        );
        assert_eq!(venue_units(U256::from(7u8), &VenueId::Polymarket), Some(U256::from(7u8)));
        assert_eq!(venue_units(U256::MAX, &VenueId::Aster), None);
    }

    #[test]
    fn decimal_units_are_exact_with_six_decimals_at_most() {
        assert_eq!(decimal_units("12.5", 6), Some(U256::from(12_500_000u64)));
        assert_eq!(decimal_units("0.000001", 6), Some(U256::from(1u8)));
        assert_eq!(decimal_units("10.11", 18), Some(U256::from(10_110_000_000_000_000_000u128)));
        assert_eq!(decimal_units("7", 6), Some(U256::from(7_000_000u64)));
        assert_eq!(decimal_units("2", 18), Some(U256::from(2_000_000_000_000_000_000u128)));
        assert_eq!(decimal_units("1", 6), Some(LIGHTER_MAX_TAKER_FEE));
        for text in ["", "1.", ".5", "1.0000001", "-1", "1e6", "1,5", " 1", "0x10", "1.2.3"] {
            assert_eq!(decimal_units(text, 18), None, "{text:?}");
        }
    }
}
