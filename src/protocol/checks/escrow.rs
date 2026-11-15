//! Escrow typed-data and signing-domain checks.

use alloy_sol_types::{Eip712Domain, SolStruct};
use serde_json::Value;

use super::details::{self, differs, unsupported};
use crate::{
    deployments,
    error::VerificationError,
    protocol::eip712::{self, Parsed, QuoteSet, Schema},
    types::{EscrowEnvironment, VenueId},
};

/// The domain `venue`'s escrow requests are signed in: `environment`'s escrow on the venue's
/// chain, or the signing domain of a venue settled inside itself. Never another environment's
/// escrow, a retired proxy, or an address the API supplies.
pub(crate) fn signing_domain(
    venue: &VenueId,
    environment: EscrowEnvironment,
) -> Result<Eip712Domain, VerificationError> {
    let chain = deployments::chain(venue).ok_or(unsupported(details::VENUE))?;
    let contract = deployments::contract(venue, environment).ok_or(unsupported(details::VENUE))?;
    Ok(eip712::lamarge_domain(chain.id, contract))
}

/// Reads `raw` as an escrow request of schema `T` on `venue`, signed in its pinned domain
/// ([`signing_domain`]); a venue settled inside itself signs quote sets only. Returns the parsed
/// request and the pinned domain to sign it in.
pub(crate) fn check_escrow_typed<T: Schema>(
    raw: &Value,
    venue: &VenueId,
    environment: EscrowEnvironment,
) -> Result<(Parsed<T>, Eip712Domain), VerificationError> {
    if deployments::is_venue_settled(venue) && T::PRIMARY != QuoteSet::NAME {
        return Err(differs(details::PRIMARY_TYPE));
    }
    let domain = signing_domain(venue, environment)?;
    Ok((eip712::parse_in::<T>(raw, &domain)?, domain))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{
        protocol::eip712::{FundOffer, PayTake},
        testing::{fixtures, sample},
    };

    const PRODUCTION: EscrowEnvironment = EscrowEnvironment::Production;
    const DEVELOPMENT: EscrowEnvironment = EscrowEnvironment::Development;

    fn with_contract(name: &str, chain: u64, contract: &str) -> Value {
        let mut raw = sample(name);
        raw["domain"]["chainId"] = json!(chain);
        raw["domain"]["verifyingContract"] = json!(contract);
        raw
    }

    /// `deployments.test.ts` "each environment's pinned escrow may be named, on its own chain":
    /// here only the client's own environment's (a delta from TypeScript).
    #[test]
    fn only_the_environments_escrow_on_the_venues_chain_is_accepted() {
        let production = sample("fund_offer");
        let polymarket = VenueId::Polymarket;
        assert!(check_escrow_typed::<FundOffer>(&production, &polymarket, PRODUCTION).is_ok());
        let error = check_escrow_typed::<FundOffer>(&production, &polymarket, DEVELOPMENT);
        assert_eq!(error.unwrap_err().detail, details::DOMAIN);
        let development =
            with_contract("fund_offer", 137, "0xa3fca394739ec4b984e908bd7a3c3b72d0ed77d1");
        assert!(check_escrow_typed::<FundOffer>(&development, &polymarket, DEVELOPMENT).is_ok());
        // Another chain's escrow of the same environment, a retired proxy, the venue on its
        // chain id but the other venue.
        for raw in [
            with_contract("fund_offer", 137, "0x948e65c40442d935d4ee990bdbd01bdd825c4873"),
            with_contract("fund_offer", 137, "0xea5ba9e1c0f5dde572bc08f3af6a3e8b9db73734"),
            with_contract("fund_offer", 999, "0x3100b7b49c0f9631c6af25877b5a4e13a47afad6"),
        ] {
            let error = check_escrow_typed::<FundOffer>(&raw, &polymarket, PRODUCTION).unwrap_err();
            assert_eq!(error.detail, details::DOMAIN);
        }
    }

    /// `aster.test.ts` "Aster signs only quote sets, in its own domain"; `deployments.test.ts` "a
    /// signing bundle is refused whole when one … names an unpinned domain".
    #[test]
    fn a_venue_settled_inside_itself_signs_only_quote_sets_in_its_domain() {
        let lighter = sample("quote_set_lighter");
        assert!(check_escrow_typed::<QuoteSet>(&lighter, &VenueId::Lighter, PRODUCTION).is_ok());
        assert!(check_escrow_typed::<QuoteSet>(&lighter, &VenueId::Lighter, DEVELOPMENT).is_ok());
        let escrow = with_contract("quote_set_lighter", 304, fixtures::ESCROW_HL);
        let error = check_escrow_typed::<QuoteSet>(&escrow, &VenueId::Lighter, PRODUCTION);
        assert_eq!(error.unwrap_err().detail, details::DOMAIN);
        // Nothing is ever funded or paid in a venue's own domain.
        let fund = with_contract("fund_offer", 304, "0x3b4d794a66304f130a4db8f2551b0070dfcf5ca7");
        let error = check_escrow_typed::<FundOffer>(&fund, &VenueId::Lighter, PRODUCTION);
        assert_eq!(error.unwrap_err().detail, details::PRIMARY_TYPE);
        let pay = with_contract("pay_take", 56, "0x128463a60784c4d3f46c23af3f65ed859ba87974");
        let error = check_escrow_typed::<PayTake>(&pay, &VenueId::Aster, PRODUCTION);
        assert_eq!(error.unwrap_err().detail, details::PRIMARY_TYPE);
        // Nor is a quote set of an escrow venue signed in a venue domain.
        let quote = with_contract("quote_set", 999, "0x3b4d794a66304f130a4db8f2551b0070dfcf5ca7");
        let error = check_escrow_typed::<QuoteSet>(&quote, &VenueId::Hyperliquid, PRODUCTION);
        assert_eq!(error.unwrap_err().detail, details::DOMAIN);
    }

    #[test]
    fn an_unknown_venue_has_no_domain() {
        let venue: VenueId = "dydx".parse().unwrap();
        let error = signing_domain(&venue, PRODUCTION).unwrap_err();
        assert_eq!(error.kind, crate::error::VerificationKind::Unsupported);
    }
}
