//! Bounded numbers: quote prices, basis points and take parts.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

/// Price units per USDC: prices have eight decimals.
const PRICE_SCALE: u32 = 100_000_000;
const PRICE_MIN: u32 = 90_000_000;
const PRICE_MAX: u32 = 110_000_000;

/// A quote price: USDC per unit of the venue's token, from 0.90 to 1.10 with at most eight
/// decimals.
///
/// Parsed from and serialized as the wire's decimal string (`"1.0005"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Price(u32);

impl Price {
    /// A price of `units` hundred-millionths of a USDC; `None` outside 0.90..=1.10.
    pub const fn from_units(units: u32) -> Option<Self> {
        if units >= PRICE_MIN && units <= PRICE_MAX { Some(Self(units)) } else { None }
    }

    /// The price in hundred-millionths of a USDC (`1.0005` is `100_050_000`).
    pub const fn units(self) -> u32 {
        self.0
    }
}

impl FromStr for Price {
    type Err = ParsePriceError;

    /// Parses `^[01](\.[0-9]{1,8})?$` within 0.90..=1.10.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
        let whole = match whole {
            "0" => 0,
            "1" => PRICE_SCALE,
            _ => return Err(ParsePriceError),
        };
        let fraction_ok = fraction.len() <= 8 && fraction.bytes().all(|b| b.is_ascii_digit());
        if !fraction_ok || (text.contains('.') && fraction.is_empty()) {
            return Err(ParsePriceError);
        }
        let fraction = fraction
            .bytes()
            .chain(std::iter::repeat(b'0'))
            .take(8)
            .fold(0, |units, digit| units * 10 + u32::from(digit - b'0'));
        Self::from_units(whole + fraction).ok_or(ParsePriceError)
    }
}

impl fmt::Display for Price {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let whole = self.0 / PRICE_SCALE;
        let fraction = format!("{:08}", self.0 % PRICE_SCALE);
        let fraction = fraction.trim_end_matches('0');
        if fraction.is_empty() { write!(f, "{whole}") } else { write!(f, "{whole}.{fraction}") }
    }
}

impl Serialize for Price {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Price {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?.parse().map_err(de::Error::custom)
    }
}

/// A text that is not a price from 0.90 to 1.10 with at most eight decimals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParsePriceError;

impl fmt::Display for ParsePriceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("not a price from 0.90 to 1.10 with at most eight decimals")
    }
}

impl std::error::Error for ParsePriceError {}

/// A signed decimal number as the API writes it (`"-0.0015"`): USD amounts and basis points of
/// results. Kept as text, without arithmetic.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct Decimal(String);

impl Decimal {
    /// The text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for Decimal {
    type Err = ParseDecimalError;

    /// Parses `^-?[0-9]+(\.[0-9]+)?$`.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let unsigned = text.strip_prefix('-').unwrap_or(text);
        let (whole, fraction) = unsigned.split_once('.').unwrap_or((unsigned, "0"));
        let digits = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
        if digits(whole) && digits(fraction) {
            Ok(Self(text.to_owned()))
        } else {
            Err(ParseDecimalError)
        }
    }
}

impl fmt::Display for Decimal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Decimal {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?.parse().map_err(de::Error::custom)
    }
}

/// A text that is not a signed decimal number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseDecimalError;

impl fmt::Display for ParseDecimalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("not a signed decimal number")
    }
}

impl std::error::Error for ParseDecimalError {}

/// Basis points, `0..=10_000`; serialized as an integer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize)]
#[serde(transparent)]
pub struct Bps(u16);

impl Bps {
    /// Zero basis points.
    pub const ZERO: Self = Self(0);

    /// `bps` basis points; `None` above 10 000.
    pub const fn new(bps: u16) -> Option<Self> {
        if bps <= 10_000 { Some(Self(bps)) } else { None }
    }

    /// The basis points.
    pub const fn get(self) -> u16 {
        self.0
    }
}

impl fmt::Display for Bps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} bps", self.0)
    }
}

impl<'de> Deserialize<'de> for Bps {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let bps = u16::deserialize(deserializer)?;
        Self::new(bps).ok_or_else(|| de::Error::custom("basis points above 10000"))
    }
}

/// The most parts (fills) a take may split into, `1..=8`; defaults to 8.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct MaxParts(u8);

impl MaxParts {
    /// The server's limit.
    pub const MAX: Self = Self(8);

    /// `parts` parts; `None` outside `1..=8`.
    pub const fn new(parts: u8) -> Option<Self> {
        if parts >= 1 && parts <= 8 { Some(Self(parts)) } else { None }
    }

    /// The number of parts.
    pub const fn get(self) -> u8 {
        self.0
    }
}

impl Default for MaxParts {
    fn default() -> Self {
        Self::MAX
    }
}

impl<'de> Deserialize<'de> for MaxParts {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let parts = u8::deserialize(deserializer)?;
        Self::new(parts).ok_or_else(|| de::Error::custom("parts outside 1..=8"))
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn decimals_are_signed_decimal_text() {
        for text in ["0", "-0.0015", "12.5", "-5"] {
            assert_eq!(text.parse::<Decimal>().unwrap().as_str(), text);
        }
        for bad in ["", "-", ".5", "1.", "+1", "1e3", "1,5", "--1"] {
            assert_eq!(bad.parse::<Decimal>(), Err(ParseDecimalError), "{bad:?}");
        }
        assert!(serde_json::from_str::<Decimal>("1.5").is_err(), "a JSON number");
    }

    #[test]
    fn price_bounds() {
        assert!("0.89999999".parse::<Price>().is_err());
        assert_eq!("0.9".parse::<Price>().unwrap().units(), 90_000_000);
        assert_eq!("1.1".parse::<Price>().unwrap().units(), 110_000_000);
        assert!("1.10000001".parse::<Price>().is_err());
        assert_eq!("1".parse::<Price>().unwrap().units(), 100_000_000);
        assert_eq!("1.0005".parse::<Price>().unwrap().units(), 100_050_000);
        assert_eq!(Price::from_units(89_999_999), None);
        assert_eq!(Price::from_units(110_000_001), None);
    }

    #[test]
    fn price_refuses_off_pattern_text() {
        for text in ["1.", ".9", "01", "1.000000001", "0.9x", "+1", "1,0", "", "2", "0.95e0"] {
            assert!(text.parse::<Price>().is_err(), "{text:?}");
        }
    }

    #[test]
    fn price_wire_form() {
        let price: Price = serde_json::from_str("\"0.99950000\"").unwrap();
        assert_eq!(price.to_string(), "0.9995");
        assert_eq!(serde_json::to_string(&price).unwrap(), "\"0.9995\"");
        assert_eq!(
            serde_json::to_string(&Price::from_units(100_000_000).unwrap()).unwrap(),
            "\"1\""
        );
        assert!(serde_json::from_str::<Price>("1").is_err());
    }

    proptest! {
        #[test]
        fn price_text_round_trips(units in PRICE_MIN..=PRICE_MAX) {
            let price = Price::from_units(units).unwrap();
            prop_assert_eq!(price.to_string().parse::<Price>().unwrap(), price);
        }
    }

    #[test]
    fn bps_bounds() {
        assert_eq!(Bps::new(10_000).map(Bps::get), Some(10_000));
        assert_eq!(Bps::new(10_001), None);
        assert_eq!(serde_json::to_string(&Bps::new(25).unwrap()).unwrap(), "25");
        assert_eq!(serde_json::from_str::<Bps>("100").unwrap().get(), 100);
        assert!(serde_json::from_str::<Bps>("10001").is_err());
    }

    #[test]
    fn max_parts_bounds() {
        assert_eq!(MaxParts::new(0), None);
        assert_eq!(MaxParts::new(1).map(MaxParts::get), Some(1));
        assert_eq!(MaxParts::new(8).map(MaxParts::get), Some(8));
        assert_eq!(MaxParts::new(9), None);
        assert_eq!(MaxParts::default().get(), 8);
        assert_eq!(serde_json::to_string(&MaxParts::default()).unwrap(), "8");
        assert!(serde_json::from_str::<MaxParts>("0").is_err());
        assert!(serde_json::from_str::<MaxParts>("9").is_err());
    }
}
