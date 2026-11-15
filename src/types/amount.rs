//! Token amounts in base units.

use std::{fmt, str::FromStr};

use alloy_primitives::{I256, Sign, U256};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use super::VenueId;

/// The most digits a wire amount has: `2^256 - 1` has 78.
const MAX_DIGITS: usize = 78;

/// A token amount in base units (`"1000000"` is 1 USDC).
///
/// On the wire it is a decimal string (`^[0-9]{1,78}$`), never hex. Which token's units depends
/// on the venue: see [`Amount::parse_venue`] and [`VenueId::decimals`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Amount(U256);

impl Amount {
    /// Zero.
    pub const ZERO: Self = Self(U256::ZERO);

    /// An amount of `units` base units.
    pub const fn new(units: U256) -> Self {
        Self(units)
    }

    /// The base units.
    pub const fn get(self) -> U256 {
        self.0
    }

    /// Parses a decimal token amount (`"12.5"`) in `venue`'s token decimals.
    ///
    /// Fails for a venue this SDK version does not know, or a text [`Amount::parse_units`]
    /// refuses.
    pub fn parse_venue(venue: &VenueId, decimal: &str) -> Result<Self, ParseAmountError> {
        let decimals = venue.decimals().ok_or(ParseAmountError(ParseAmountKind::UnknownVenue))?;
        Self::parse_units(decimal, decimals)
    }

    /// Formats the amount as a decimal token amount in `venue`'s token decimals; `None` for a
    /// venue this SDK version does not know.
    pub fn format_venue(&self, venue: &VenueId) -> Option<String> {
        venue.decimals().map(|decimals| self.format_units(decimals))
    }

    /// Parses a decimal token amount (`"12.5"`) with `decimals` decimals, exactly.
    ///
    /// Accepts `digits` or `digits.digits`; refuses more fractional digits than `decimals` and
    /// values of `2^256` base units or more.
    pub fn parse_units(decimal: &str, decimals: u8) -> Result<Self, ParseAmountError> {
        let (whole, fraction) = match decimal.split_once('.') {
            Some((whole, fraction)) => (whole, fraction),
            None => (decimal, ""),
        };
        let digits = |text: &str| !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
        if !digits(whole) || (decimal.contains('.') && !digits(fraction)) {
            return Err(ParseAmountError(ParseAmountKind::Invalid));
        }
        let padding = usize::from(decimals)
            .checked_sub(fraction.len())
            .ok_or(ParseAmountError(ParseAmountKind::TooManyDecimals))?;
        let units = whole
            .bytes()
            .chain(fraction.bytes())
            .chain(std::iter::repeat_n(b'0', padding))
            .try_fold(U256::ZERO, |units, digit| {
                units.checked_mul(U256::from(10u8))?.checked_add(U256::from(digit - b'0'))
            })
            .ok_or(ParseAmountError(ParseAmountKind::Overflow))?;
        Ok(Self(units))
    }

    /// Formats the amount as a decimal token amount with `decimals` decimals, without trailing
    /// fractional zeros (`1500000` with 6 decimals is `"1.5"`).
    pub fn format_units(&self, decimals: u8) -> String {
        let digits = self.0.to_string();
        let decimals = usize::from(decimals);
        if decimals == 0 {
            return digits;
        }
        let padded = format!("{digits:0>width$}", width = decimals + 1);
        let (whole, fraction) = padded.split_at(padded.len() - decimals);
        let fraction = fraction.trim_end_matches('0');
        if fraction.is_empty() { whole.to_owned() } else { format!("{whole}.{fraction}") }
    }

    /// `self + other`, or `None` on overflow.
    pub fn checked_add(self, other: Self) -> Option<Self> {
        self.0.checked_add(other.0).map(Self)
    }

    /// `self - other`, or `None` below zero.
    pub fn checked_sub(self, other: Self) -> Option<Self> {
        self.0.checked_sub(other.0).map(Self)
    }
}

impl From<u64> for Amount {
    fn from(units: u64) -> Self {
        Self(U256::from(units))
    }
}

impl From<u128> for Amount {
    fn from(units: u128) -> Self {
        Self(U256::from(units))
    }
}

impl FromStr for Amount {
    type Err = ParseAmountError;

    /// Parses base units as the wire writes them: 1 to 78 decimal digits.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        if text.is_empty() || text.len() > MAX_DIGITS || !text.bytes().all(|b| b.is_ascii_digit()) {
            return Err(ParseAmountError(ParseAmountKind::Invalid));
        }
        Self::parse_units(text, 0)
    }
}

impl fmt::Display for Amount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl Serialize for Amount {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Amount {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?.parse().map_err(de::Error::custom)
    }
}

/// A signed token amount in base units (`"-20000"`), for balances that can be negative.
///
/// On the wire it is a decimal string (`^-?[0-9]{1,78}$`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct SignedAmount(I256);

impl SignedAmount {
    /// Zero.
    pub const ZERO: Self = Self(I256::ZERO);

    /// An amount of `units` base units.
    pub const fn new(units: I256) -> Self {
        Self(units)
    }

    /// The base units.
    pub const fn get(self) -> I256 {
        self.0
    }

    /// Formats the amount as a signed decimal token amount with `decimals` decimals, without
    /// trailing fractional zeros (`-1500000` with 6 decimals is `"-1.5"`).
    pub fn format_units(&self, decimals: u8) -> String {
        let magnitude = Amount(self.0.unsigned_abs()).format_units(decimals);
        if self.0.is_negative() { format!("-{magnitude}") } else { magnitude }
    }
}

impl FromStr for SignedAmount {
    type Err = ParseAmountError;

    /// Parses base units as the wire writes them: an optional `-`, then 1 to 78 decimal digits.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let (sign, digits) = match text.strip_prefix('-') {
            Some(digits) => (Sign::Negative, digits),
            None => (Sign::Positive, text),
        };
        let magnitude: Amount = digits.parse()?;
        I256::checked_from_sign_and_abs(sign, magnitude.0)
            .map(Self)
            .ok_or(ParseAmountError(ParseAmountKind::Overflow))
    }
}

impl fmt::Display for SignedAmount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl Serialize for SignedAmount {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for SignedAmount {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?.parse().map_err(de::Error::custom)
    }
}

/// Why a text is not an amount.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseAmountError(ParseAmountKind);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParseAmountKind {
    Invalid,
    TooManyDecimals,
    Overflow,
    UnknownVenue,
}

impl fmt::Display for ParseAmountError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self.0 {
            ParseAmountKind::Invalid => "not a decimal number",
            ParseAmountKind::TooManyDecimals => "more decimals than the token has",
            ParseAmountKind::Overflow => "2^256 base units or more",
            ParseAmountKind::UnknownVenue => "a venue this SDK version does not know",
        })
    }
}

impl std::error::Error for ParseAmountError {}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn parses_exact_decimal_amounts() {
        assert_eq!(Amount::parse_units("1.5", 6).unwrap(), Amount::from(1_500_000u64));
        assert_eq!(Amount::parse_units("0.000001", 6).unwrap(), Amount::from(1u64));
        assert_eq!(Amount::parse_units("42", 0).unwrap(), Amount::from(42u64));
        assert_eq!(Amount::parse_units("007.10", 6).unwrap(), Amount::from(7_100_000u64));
    }

    #[test]
    fn refuses_malformed_or_over_precise_amounts() {
        let kind = |text, decimals| Amount::parse_units(text, decimals).unwrap_err().0;
        assert_eq!(kind("1.0000001", 6), ParseAmountKind::TooManyDecimals);
        assert_eq!(kind("1.5", 0), ParseAmountKind::TooManyDecimals);
        for text in ["", ".5", "1.", "1,5", "-1", "+1", "1e6", " 1", "1.2.3", "0x10"] {
            assert_eq!(kind(text, 6), ParseAmountKind::Invalid, "{text:?}");
        }
        assert_eq!(kind("1", 78), ParseAmountKind::Overflow);
    }

    #[test]
    fn venue_amounts_use_the_venue_decimals() {
        let usdt = Amount::parse_venue(&VenueId::Aster, "1").unwrap();
        assert_eq!(usdt.get(), U256::from(10u8).pow(U256::from(18u8)));
        assert_eq!(
            Amount::parse_venue(&VenueId::Hyperliquid, "1").unwrap(),
            Amount::from(1_000_000u64)
        );
        assert_eq!(usdt.format_venue(&VenueId::Aster).as_deref(), Some("1"));
        let unknown: VenueId = "orderly".parse().unwrap();
        let kind = |venue, text| Amount::parse_venue(venue, text).unwrap_err().0;
        assert_eq!(kind(&unknown, "1"), ParseAmountKind::UnknownVenue);
        assert_eq!(kind(&VenueId::Gmx, "0.0000001"), ParseAmountKind::TooManyDecimals);
        assert_eq!(usdt.format_venue(&unknown), None);
    }

    #[test]
    fn formats_without_trailing_zeros() {
        assert_eq!(Amount::from(1_500_000u64).format_units(6), "1.5");
        assert_eq!(Amount::from(1_000_000u64).format_units(6), "1");
        assert_eq!(Amount::from(1u64).format_units(6), "0.000001");
        assert_eq!(Amount::ZERO.format_units(6), "0");
        assert_eq!(Amount::from(123u64).format_units(0), "123");
    }

    #[test]
    fn wire_form_is_bounded_decimal_text() {
        let max = "115792089237316195423570985008687907853269984665640564039457584007913129639935";
        assert_eq!(max.parse::<Amount>().unwrap().get(), U256::MAX);
        let over = "115792089237316195423570985008687907853269984665640564039457584007913129639936";
        assert!(over.parse::<Amount>().is_err());
        assert!(format!("0{max}").parse::<Amount>().is_err(), "79 digits");
        assert!("1.5".parse::<Amount>().is_err());
        assert!("".parse::<Amount>().is_err());
    }

    #[test]
    fn serializes_as_decimal_string_never_hex() {
        let amount = Amount::from(255u64);
        assert_eq!(serde_json::to_string(&amount).unwrap(), "\"255\"");
        assert_eq!(serde_json::from_str::<Amount>("\"255\"").unwrap(), amount);
        assert!(serde_json::from_str::<Amount>("\"0xff\"").is_err());
        assert!(serde_json::from_str::<Amount>("255").is_err());
    }

    #[test]
    fn signed_amounts_are_signed_decimal_text() {
        let locked: SignedAmount = serde_json::from_str("\"-20000\"").unwrap();
        assert!(locked < SignedAmount::ZERO);
        assert_eq!(locked.to_string(), "-20000");
        assert_eq!(serde_json::to_string(&locked).unwrap(), "\"-20000\"");
        assert_eq!(locked.format_units(6), "-0.02");
        assert_eq!("1500000".parse::<SignedAmount>().unwrap().format_units(6), "1.5");
        let min = format!("-{}", U256::from(1u8) << 255);
        assert_eq!(min.parse::<SignedAmount>().unwrap().get(), I256::MIN);
        assert!(min.trim_start_matches('-').parse::<SignedAmount>().is_err(), "above I256::MAX");
        for bad in ["", "-", "+1", "--1", "1.5", "0x10", " 1"] {
            assert!(bad.parse::<SignedAmount>().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn checked_arithmetic() {
        let max = Amount::new(U256::MAX);
        assert_eq!(max.checked_add(Amount::from(1u64)), None);
        assert_eq!(Amount::ZERO.checked_sub(Amount::from(1u64)), None);
        assert_eq!(Amount::from(3u64).checked_sub(Amount::from(1u64)), Some(Amount::from(2u64)));
        assert_eq!(Amount::from(3u64).checked_add(Amount::from(1u64)), Some(Amount::from(4u64)));
    }

    fn any_u256() -> impl Strategy<Value = U256> {
        any::<[u64; 4]>().prop_map(U256::from_limbs)
    }

    proptest! {
        #[test]
        fn parse_inverts_format(units in any_u256(), decimals in 0u8..=77) {
            let amount = Amount::new(units);
            prop_assert_eq!(Amount::parse_units(&amount.format_units(decimals), decimals).unwrap(), amount);
        }

        #[test]
        fn format_inverts_parse_on_canonical_text(text in "(0|[1-9][0-9]{0,40})(\\.[0-9]{0,17}[1-9])?") {
            let amount = Amount::parse_units(&text, 18).unwrap();
            prop_assert_eq!(amount.format_units(18), text);
        }

        #[test]
        fn wire_text_round_trips(units in any_u256()) {
            let amount = Amount::new(units);
            let json = serde_json::to_string(&amount).unwrap();
            prop_assert_eq!(serde_json::from_str::<Amount>(&json).unwrap(), amount);
        }
    }
}
