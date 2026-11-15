//! Deployment and venue registry checks: every chain the API lists is one this release pins.

use super::details::{self, deployment, unsupported};
use crate::{
    deployments,
    error::VerificationError,
    model::{EscrowChain, Settlement, Venue},
    types::{EscrowEnvironment, VenueId},
};

/// Checks `chain`, as the API lists it for `venue`, against the pins: chain id, token, decimals,
/// permit domain, wallet token and escrow (any environment's). Returns the environment of an
/// escrow venue's escrow; `None` for a venue settled inside itself, whose listed escrow is its
/// signing domain's contract.
pub(crate) fn check_registry(
    venue: &VenueId,
    chain: &EscrowChain,
) -> Result<Option<EscrowEnvironment>, VerificationError> {
    let pinned = deployments::chain(venue).ok_or(unsupported(details::VENUE))?;
    let permit = match (&chain.permit, pinned.permit) {
        (None, None) => true,
        (Some(listed), Some(permit)) => {
            listed.name == permit.name && listed.version == permit.version
        }
        _ => false,
    };
    let wallet = chain.wallet.as_ref().is_none_or(|wallet| {
        Some(wallet.token) == pinned.wallet_token && wallet.decimals == pinned.decimals
    });
    if chain.id != pinned.id
        || chain.token != pinned.token
        || chain.decimals != pinned.decimals
        || !permit
        || !wallet
    {
        return Err(deployment(details::REGISTRY));
    }
    match deployments::venue_contract(venue) {
        Some(contract) if chain.escrow == contract => Ok(None),
        Some(_) => Err(deployment(details::ESCROW)),
        None => deployments::escrow_environment(chain.id, chain.escrow)
            .map(Some)
            .ok_or(deployment(details::ESCROW)),
    }
}

/// Checks the venues the API lists (`GET /v1/venues`) and returns the environment their escrows
/// belong to. Each pinned venue lists a pinned chain, settled as pinned, unless it is disabled
/// and lists none; a venue this release does not know is skipped (nothing here signs for it).
/// Every escrow belongs to one environment, `named` when the client names one, and at least one
/// escrow venue is enabled.
pub(crate) fn check_deployment(
    venues: &[Venue],
    named: Option<EscrowEnvironment>,
) -> Result<EscrowEnvironment, VerificationError> {
    let mut environment = None;
    let mut enabled_escrow = false;
    for venue in venues {
        if deployments::chain(&venue.id).is_none() {
            continue;
        }
        let Some(chain) = &venue.chain else {
            if venue.enabled {
                return Err(deployment(details::REGISTRY));
            }
            continue;
        };
        let escrow = check_registry(&venue.id, chain)?;
        let settlement = if escrow.is_some() { Settlement::Escrow } else { Settlement::Venue };
        if venue.settlement.as_ref() != Some(&settlement) {
            return Err(deployment(details::SETTLEMENT));
        }
        if let Some(escrow) = escrow {
            if environment.is_some_and(|known| known != escrow) {
                return Err(deployment(details::ENVIRONMENT));
            }
            environment = Some(escrow);
            enabled_escrow |= venue.enabled;
        }
    }
    let environment =
        environment.filter(|_| enabled_escrow).ok_or(deployment(details::NO_ESCROW_VENUE))?;
    if named.is_some_and(|named| named != environment) {
        return Err(deployment(details::ENVIRONMENT));
    }
    Ok(environment)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{error::VerificationKind, testing::fixtures};

    fn refused(venues: &[Venue], named: Option<EscrowEnvironment>) -> &'static str {
        let error = check_deployment(venues, named).unwrap_err();
        assert_eq!(error.kind, VerificationKind::Deployment);
        error.detail
    }

    fn edited(venue: &str, edit: impl FnOnce(&mut serde_json::Value)) -> Vec<Venue> {
        fixtures::venues_with(|list| {
            let row = list.iter_mut().find(|row| row["id"] == venue).unwrap();
            edit(row);
        })
    }

    /// `session.ts` `checkDeployment`: the environment comes from the escrows the API names.
    #[test]
    fn the_environment_comes_from_the_listed_escrows() {
        assert_eq!(
            check_deployment(&fixtures::venues(), None).unwrap(),
            EscrowEnvironment::Production
        );
        assert_eq!(
            check_deployment(&fixtures::venues(), Some(EscrowEnvironment::Production)).unwrap(),
            EscrowEnvironment::Production
        );
        assert_eq!(
            refused(&fixtures::venues(), Some(EscrowEnvironment::Development)),
            details::ENVIRONMENT
        );
        let development = fixtures::venues_in(EscrowEnvironment::Development);
        assert_eq!(check_deployment(&development, None).unwrap(), EscrowEnvironment::Development);
        // One escrow of each environment.
        let mixed = edited("polymarket", |row| {
            row["chain"]["escrow"] = json!("0xa3fca394739ec4b984e908bd7a3c3b72d0ed77d1");
        });
        assert_eq!(refused(&mixed, None), details::ENVIRONMENT);
    }

    #[test]
    fn a_disabled_venue_may_list_no_chain_and_an_unknown_venue_is_skipped() {
        let disabled = edited("gmx", |row| {
            row["enabled"] = json!(false);
            row["chain"] = json!(null);
            row["settlement"] = json!(null);
        });
        assert!(check_deployment(&disabled, None).is_ok());
        let missing = edited("gmx", |row| row["chain"] = json!(null));
        assert_eq!(refused(&missing, None), details::REGISTRY);
        let unknown = fixtures::venues_with(|list| {
            let mut row = list[0].clone();
            row["id"] = json!("dydx");
            row["chain"]["id"] = json!(1);
            list.push(row);
        });
        assert!(check_deployment(&unknown, None).is_ok());
    }

    #[test]
    fn settlement_is_as_pinned_and_an_escrow_venue_is_enabled() {
        let settled = edited("lighter", |row| row["settlement"] = json!("escrow"));
        assert_eq!(refused(&settled, None), details::SETTLEMENT);
        let escrowed = edited("hyperliquid", |row| row["settlement"] = json!("venue"));
        assert_eq!(refused(&escrowed, None), details::SETTLEMENT);
        let none = fixtures::venues_with(|list| {
            for row in list.iter_mut().filter(|row| row["settlement"] == "escrow") {
                row["enabled"] = json!(false);
            }
        });
        assert_eq!(refused(&none, None), details::NO_ESCROW_VENUE);
    }

    /// `consents.test.ts` "a listed permit domain and wallet token are the pinned ones, and none
    /// on a venue settled inside itself", `deployments.test.ts` (escrows of either environment,
    /// a venue settled inside itself keeps its one domain).
    #[test]
    fn a_listed_chain_is_the_pinned_one() {
        let accepted = [
            ("hyperliquid", json!({})),
            ("lighter", json!({"permit": null, "wallet": null})),
            ("polymarket", json!({"wallet": null})),
            ("hyperliquid", json!({"escrow": "0x948e65c40442d935d4ee990bdbd01bdd825c4873"})),
        ];
        for (venue, patch) in accepted {
            let chain = fixtures::chain_with(venue, &patch);
            assert!(check_registry(&venue.parse().unwrap(), &chain).is_ok(), "{venue} {patch}");
        }
        let usdc = "0x3c499c542cef5e3811e1192ce70d8cc03d5c3359";
        let wallet = |token: &str, decimals: u8| json!({"wallet": {"token": token, "symbol": "USDC", "decimals": decimals, "minimum": "10000"}});
        let refused = [
            ("hyperliquid", json!({"permit": {"name": "USDC", "version": "1"}}), details::REGISTRY),
            ("hyperliquid", json!({"permit": null}), details::REGISTRY),
            ("lighter", json!({"permit": {"name": "USDC", "version": "2"}}), details::REGISTRY),
            (
                "polymarket",
                wallet("0xc011a7e12a19f7b1f670d46f03b03f3342e82dfb", 6),
                details::REGISTRY,
            ),
            ("polymarket", wallet(usdc, 18), details::REGISTRY),
            ("lighter", wallet("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48", 6), details::REGISTRY),
            ("hyperliquid", json!({"id": 137}), details::REGISTRY),
            ("hyperliquid", json!({"decimals": 18}), details::REGISTRY),
            ("hyperliquid", json!({"token": usdc}), details::REGISTRY),
            // A retired proxy, another chain's escrow, a venue domain elsewhere.
            (
                "hyperliquid",
                json!({"escrow": "0xea5ba9e1c0f5dde572bc08f3af6a3e8b9db73734"}),
                details::ESCROW,
            ),
            (
                "polymarket",
                json!({"escrow": "0x948e65c40442d935d4ee990bdbd01bdd825c4873"}),
                details::ESCROW,
            ),
            (
                "aster",
                json!({"escrow": "0x3100b7b49c0f9631c6af25877b5a4e13a47afad6"}),
                details::ESCROW,
            ),
        ];
        for (venue, patch, detail) in refused {
            let chain = fixtures::chain_with(venue, &patch);
            let error = check_registry(&venue.parse().unwrap(), &chain).unwrap_err();
            assert_eq!(error.detail, detail, "{venue} {patch}");
        }
    }
}
