//! Identifiers, cursors and signatures.

use std::{fmt, str::FromStr};

use alloy_primitives::{B256, hex};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use uuid::Uuid;

const CLIENT_ID_MAX: usize = 64;

/// A caller-chosen idempotency key: 1 to 64 characters of `[A-Za-z0-9_.:-]`.
///
/// Persist it with the whole request it names before the first attempt and retry with both; the
/// server refuses its reuse with other parameters.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct ClientId(String);

impl ClientId {
    /// Validates `id`.
    pub fn new(id: impl Into<String>) -> Result<Self, InvalidClientId> {
        let id = id.into();
        let allowed = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b':' | b'-');
        if id.is_empty() || id.len() > CLIENT_ID_MAX || !id.bytes().all(allowed) {
            return Err(InvalidClientId);
        }
        Ok(Self(id))
    }

    /// A fresh time-ordered id (a UUID v7 as 32 hex digits).
    pub fn generate() -> Self {
        Self(Uuid::now_v7().simple().to_string())
    }

    /// The id.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ClientId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for ClientId {
    type Err = InvalidClientId;

    fn from_str(id: &str) -> Result<Self, Self::Err> {
        Self::new(id)
    }
}

impl<'de> Deserialize<'de> for ClientId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::new(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

/// A text that is not 1 to 64 characters of `[A-Za-z0-9_.:-]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidClientId;

impl fmt::Display for InvalidClientId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("client id must be 1 to 64 characters of [A-Za-z0-9_.:-]")
    }
}

impl std::error::Error for InvalidClientId {}

/// Declares a `u64` carried on the wire as a decimal string.
macro_rules! decimal_u64 {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
        pub struct $name(u64);

        impl $name {
            /// Wraps `value`.
            pub const fn new(value: u64) -> Self {
                Self(value)
            }

            /// The value.
            pub const fn get(self) -> u64 {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }

        impl FromStr for $name {
            type Err = std::num::ParseIntError;

            fn from_str(text: &str) -> Result<Self, Self::Err> {
                text.parse().map(Self)
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.collect_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                String::deserialize(deserializer)?.parse().map_err(de::Error::custom)
            }
        }
    };
}

decimal_u64! {
    /// A position in the event journal; keep it lossless (decimal string on the wire).
    Cursor
}

decimal_u64! {
    /// A resource's revision: newer state has a higher revision (decimal string on the wire).
    Revision
}

decimal_u64! {
    /// A block number (decimal string on the wire).
    BlockNumber
}

/// A balance's portfolio change counter (`<count>:<microseconds>` on the wire): a later
/// observation compares greater.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PortfolioRevision {
    count: u64,
    micros: i64,
}

impl fmt::Display for PortfolioRevision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.count, self.micros)
    }
}

impl FromStr for PortfolioRevision {
    type Err = std::num::ParseIntError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let (count, micros) = text.split_once(':').unwrap_or((text, ""));
        Ok(Self { count: count.parse()?, micros: micros.parse()? })
    }
}

impl Serialize for PortfolioRevision {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for PortfolioRevision {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?.parse().map_err(de::Error::custom)
    }
}

/// An opaque history or transfer page cursor, passed back as received.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PageCursor(String);

impl PageCursor {
    /// A cursor from a saved `next_cursor`.
    pub fn new(cursor: impl Into<String>) -> Self {
        Self(cursor.into())
    }

    /// The cursor text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PageCursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Declares a resource id wrapping a UUID.
macro_rules! uuid_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            /// Wraps `id`.
            pub const fn new(id: Uuid) -> Self {
                Self(id)
            }

            /// The UUID.
            pub const fn as_uuid(&self) -> &Uuid {
                &self.0
            }
        }

        impl From<Uuid> for $name {
            fn from(id: Uuid) -> Self {
                Self(id)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }

        impl FromStr for $name {
            type Err = uuid::Error;

            fn from_str(text: &str) -> Result<Self, Self::Err> {
                text.parse().map(Self)
            }
        }
    };
}

uuid_id! {
    /// A transfer intent id.
    TransferId
}
uuid_id! {
    /// A take id.
    TakeId
}
uuid_id! {
    /// A fill id.
    FillId
}
uuid_id! {
    /// A sign-in session id.
    SessionId
}
uuid_id! {
    /// A maker preparation id.
    PreparationId
}
uuid_id! {
    /// A quote identity id (a maker credential's identity).
    QuoteIdentityId
}
uuid_id! {
    /// A close request id.
    CloseId
}
uuid_id! {
    /// A withdrawal id.
    WithdrawalId
}

/// An order id (bytes32, `0x`-hex on the wire).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OrderId(B256);

impl OrderId {
    /// Wraps `id`.
    pub const fn new(id: B256) -> Self {
        Self(id)
    }

    /// The 32 bytes.
    pub const fn as_b256(&self) -> &B256 {
        &self.0
    }
}

impl fmt::Display for OrderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#x}", self.0)
    }
}

impl FromStr for OrderId {
    type Err = hex::FromHexError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        text.parse().map(Self)
    }
}

/// A 65-byte signature `r ‖ s ‖ v` with `v` 27 or 28; `0x`-hex on the wire.
///
/// `Debug` does not show the bytes.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct SignatureBytes([u8; 65]);

impl SignatureBytes {
    /// Wraps `r ‖ s ‖ v`, normalizing `v` 0 or 1 to 27 or 28; `None` for any other `v`.
    pub const fn new(mut bytes: [u8; 65]) -> Option<Self> {
        match bytes[64] {
            0 | 1 => {
                bytes[64] += 27;
                Some(Self(bytes))
            }
            27 | 28 => Some(Self(bytes)),
            _ => None,
        }
    }

    /// The 65 bytes.
    pub const fn as_bytes(&self) -> &[u8; 65] {
        &self.0
    }
}

impl fmt::Debug for SignatureBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SignatureBytes(****)")
    }
}

impl Serialize for SignatureBytes {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&hex::encode_prefixed(self.0))
    }
}

impl<'de> Deserialize<'de> for SignatureBytes {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        let digits = text.strip_prefix("0x").ok_or_else(|| de::Error::custom("missing 0x"))?;
        let bytes = hex::decode_to_array::<_, 65>(digits).map_err(de::Error::custom)?;
        Self::new(bytes).ok_or_else(|| de::Error::custom("signature v must be 0, 1, 27 or 28"))
    }
}

/// A fill's claim secret: whoever holds it can release the payment to the maker before `t2`.
///
/// `Debug` does not show it; [`expose_secret`](Self::expose_secret) gives the bytes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ClaimSecret(B256);

impl ClaimSecret {
    /// The 32 bytes.
    pub const fn expose_secret(&self) -> &B256 {
        &self.0
    }
}

impl fmt::Debug for ClaimSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ClaimSecret(****)")
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn client_id_charset_and_length() {
        assert!(ClientId::new("take-1:retry_2.a").is_ok());
        assert!(ClientId::new("a".repeat(64)).is_ok());
        assert_eq!(ClientId::new("a".repeat(65)), Err(InvalidClientId));
        assert_eq!(ClientId::new(""), Err(InvalidClientId));
        for bad in ["a b", "a/b", "é", "a\n", "a#b"] {
            assert_eq!(ClientId::new(bad), Err(InvalidClientId), "{bad:?}");
        }
        assert!(serde_json::from_str::<ClientId>("\"a/b\"").is_err());
        assert_eq!(serde_json::from_str::<ClientId>("\"ok\"").unwrap().as_str(), "ok");
    }

    #[test]
    fn generated_client_ids_are_valid_and_distinct() {
        let (a, b) = (ClientId::generate(), ClientId::generate());
        assert_ne!(a, b);
        assert_eq!(a.as_str().len(), 32);
        assert_eq!(ClientId::new(a.as_str()).unwrap(), a);
    }

    #[test]
    fn cursors_are_lossless_decimal_strings() {
        let cursor = Cursor::new(u64::MAX);
        let json = serde_json::to_string(&cursor).unwrap();
        assert_eq!(json, "\"18446744073709551615\"");
        assert_eq!(serde_json::from_str::<Cursor>(&json).unwrap(), cursor);
        assert!(serde_json::from_str::<Cursor>("7").is_err(), "a JSON number loses precision");
        assert!("-7".parse::<Revision>().is_err());
        assert!(Revision::new(2) > Revision::new(1));
    }

    #[test]
    fn portfolio_revisions_order_by_count_then_time() {
        let parse = |text: &str| text.parse::<PortfolioRevision>().unwrap();
        assert_eq!(parse("12:1731585600000000").to_string(), "12:1731585600000000");
        assert!(parse("12:5") > parse("12:4"));
        assert!(parse("13:0") > parse("12:9"));
        for bad in ["12", "12:", ":5", "a:1", "1:2:3"] {
            assert!(bad.parse::<PortfolioRevision>().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn uuid_ids_are_transparent() {
        let text = "0192f1c2-3d4e-7f00-8a1b-2c3d4e5f6a7b";
        let id: TransferId = serde_json::from_str(&format!("\"{text}\"")).unwrap();
        assert_eq!(id.to_string(), text);
        assert_eq!(serde_json::to_string(&id).unwrap(), format!("\"{text}\""));
        assert!(serde_json::from_str::<TakeId>("\"not-a-uuid\"").is_err());
    }

    #[test]
    fn order_ids_are_prefixed_hex() {
        let text = format!("0x{}", "ab".repeat(32));
        let id: OrderId = text.parse().unwrap();
        assert_eq!(id.to_string(), text);
        assert_eq!(serde_json::to_string(&id).unwrap(), format!("\"{text}\""));
        assert!("0xabcd".parse::<OrderId>().is_err());
    }

    fn signature(v: u8) -> [u8; 65] {
        let mut bytes = [0x11; 65];
        bytes[64] = v;
        bytes
    }

    #[test]
    fn signature_v_is_normalized_and_bounded() {
        assert_eq!(SignatureBytes::new(signature(0)).unwrap().as_bytes()[64], 27);
        assert_eq!(SignatureBytes::new(signature(1)).unwrap().as_bytes()[64], 28);
        assert_eq!(SignatureBytes::new(signature(28)).unwrap().as_bytes()[64], 28);
        assert!(SignatureBytes::new(signature(2)).is_none());
        assert!(SignatureBytes::new(signature(29)).is_none());
    }

    #[test]
    fn signature_wire_form_and_redacted_debug() {
        let signature = SignatureBytes::new(signature(27)).unwrap();
        let json = serde_json::to_string(&signature).unwrap();
        assert_eq!(json, format!("\"0x{}1b\"", "11".repeat(64)));
        assert_eq!(serde_json::from_str::<SignatureBytes>(&json).unwrap(), signature);
        assert!(!format!("{signature:?}").contains("1111"));
        assert!(
            serde_json::from_str::<SignatureBytes>(&format!("\"{}1b\"", "11".repeat(64))).is_err()
        );
        assert!(
            serde_json::from_str::<SignatureBytes>(&format!("\"0x{}\"", "11".repeat(64))).is_err()
        );
    }

    proptest! {
        #[test]
        fn client_id_accepts_exactly_its_pattern(id in "[A-Za-z0-9_.:-]{1,64}") {
            prop_assert!(ClientId::new(id).is_ok());
        }

        #[test]
        fn client_id_refuses_other_characters(prefix in "[a-z]{0,10}", bad in "[^A-Za-z0-9_.:-]") {
            let id = format!("{prefix}{bad}");
            prop_assert!(ClientId::new(id).is_err());
        }

        #[test]
        fn revision_text_round_trips(value: u64) {
            prop_assert_eq!(Revision::new(value).to_string().parse::<Revision>().unwrap().get(), value);
        }
    }
}
