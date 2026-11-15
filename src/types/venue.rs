//! Venues, setup steps and escrow environments.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::open_enum::open_enum;

open_enum! {
    /// A trading venue, as `GET /v1/venues` names it.
    pub enum VenueId {
        /// Hyperliquid (USDC on HyperEVM).
        Hyperliquid = "hyperliquid",
        /// Polymarket (pUSD on Polygon).
        Polymarket = "polymarket",
        /// GMX Account (USDC on Arbitrum).
        Gmx = "gmx",
        /// Lighter (USDC, settled inside the venue).
        Lighter = "lighter",
        /// Aster (USDT, settled inside the venue).
        Aster = "aster",
    }
}

impl VenueId {
    /// Decimals of the venue's token: 6 for USDC and pUSD, 18 for Aster's USDT; `None` for a
    /// venue this SDK version does not know.
    pub fn decimals(&self) -> Option<u8> {
        match self {
            Self::Hyperliquid | Self::Polymarket | Self::Gmx | Self::Lighter => Some(6),
            Self::Aster => Some(18),
            Self::Unknown(_) => None,
        }
    }
}

open_enum! {
    /// A venue account setup step.
    pub enum SetupStepKind {
        /// A token permit for the escrow.
        Permit = "permit",
        /// An agent approval (Aster).
        Agent = "agent",
        /// An API key registration (Lighter).
        ApiKey = "api_key",
    }
}

/// Which set of escrow deployments a server uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EscrowEnvironment {
    /// The production escrows.
    Production,
    /// The development escrows.
    Development,
}

impl EscrowEnvironment {
    /// `"production"` or `"development"`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Production => "production",
            Self::Development => "development",
        }
    }
}

impl fmt::Display for EscrowEnvironment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_values_never_parse_to_unknown() {
        for (text, venue) in [
            ("hyperliquid", VenueId::Hyperliquid),
            ("polymarket", VenueId::Polymarket),
            ("gmx", VenueId::Gmx),
            ("lighter", VenueId::Lighter),
            ("aster", VenueId::Aster),
        ] {
            let parsed: VenueId = text.parse().unwrap();
            assert_eq!(parsed, venue);
            assert!(!parsed.is_unknown());
            assert_eq!(serde_json::from_str::<VenueId>(&format!("\"{text}\"")).unwrap(), venue);
            assert_eq!(serde_json::to_string(&venue).unwrap(), format!("\"{text}\""));
        }
    }

    #[test]
    fn unknown_values_round_trip() {
        let parsed: VenueId = serde_json::from_str("\"orderly\"").unwrap();
        assert!(parsed.is_unknown());
        assert_eq!(parsed.as_str(), "orderly");
        assert_eq!(parsed.to_string(), "orderly");
        assert_eq!(serde_json::to_string(&parsed).unwrap(), "\"orderly\"");
        assert_eq!(parsed.decimals(), None);
        // Case matters on the wire: a differently cased id is not the known venue.
        assert!("Hyperliquid".parse::<VenueId>().unwrap().is_unknown());
    }

    #[test]
    fn setup_steps_use_snake_case() {
        assert_eq!("api_key".parse::<SetupStepKind>().unwrap(), SetupStepKind::ApiKey);
        assert_eq!(SetupStepKind::ApiKey.as_str(), "api_key");
        assert!("apiKey".parse::<SetupStepKind>().unwrap().is_unknown());
    }
}
