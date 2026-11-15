//! Open string enums: every wire value the SDK knows is a variant, anything else lands in
//! `Unknown` and round-trips unchanged.

use std::fmt;

/// An open enum's unrecognised wire value.
///
/// Only parsing (`FromStr` or serde) constructs it, and never for a value the enum knows, so
/// matching on a known variant is always reliable. Do not match on its contents as a contract: a
/// later SDK version may know the value.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UnknownValue(String);

impl UnknownValue {
    pub(crate) fn new(value: String) -> Self {
        Self(value)
    }

    /// The value as received.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for UnknownValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

impl fmt::Display for UnknownValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Declares an open enum with its wire strings: `#[non_exhaustive]`, a final
/// `Unknown(UnknownValue)`, `as_str`, `is_unknown`, `Display`, infallible `FromStr` and string
/// serde.
/// Each variant's doc starts with its wire string.
macro_rules! open_enum {
    (
        $(#[$meta:meta])*
        pub enum $name:ident {
            $( $(#[$variant_meta:meta])* $variant:ident = $wire:literal, )+
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        #[non_exhaustive]
        pub enum $name {
            $( #[doc = concat!("`", $wire, "`")] $(#[$variant_meta])* $variant, )+
            /// A value this SDK version does not know.
            Unknown($crate::types::UnknownValue),
        }

        impl $name {
            /// The wire string.
            pub fn as_str(&self) -> &str {
                match self {
                    $( Self::$variant => $wire, )+
                    Self::Unknown(value) => value.as_str(),
                }
            }

            /// Whether the value is one this SDK version does not know.
            pub fn is_unknown(&self) -> bool {
                matches!(self, Self::Unknown(_))
            }

            fn known(value: &str) -> Option<Self> {
                match value {
                    $( $wire => Some(Self::$variant), )+
                    _ => None,
                }
            }
        }

        impl ::std::fmt::Display for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl ::std::str::FromStr for $name {
            type Err = ::std::convert::Infallible;

            fn from_str(value: &str) -> ::std::result::Result<Self, Self::Err> {
                Ok(Self::known(value).unwrap_or_else(|| {
                    Self::Unknown($crate::types::UnknownValue::new(value.to_owned()))
                }))
            }
        }

        impl ::serde::Serialize for $name {
            fn serialize<S: ::serde::Serializer>(
                &self,
                serializer: S,
            ) -> ::std::result::Result<S::Ok, S::Error> {
                serializer.serialize_str(self.as_str())
            }
        }

        impl<'de> ::serde::Deserialize<'de> for $name {
            fn deserialize<D: ::serde::Deserializer<'de>>(
                deserializer: D,
            ) -> ::std::result::Result<Self, D::Error> {
                let value = <::std::string::String as ::serde::Deserialize>::deserialize(deserializer)?;
                Ok(Self::known(&value)
                    .unwrap_or_else(|| Self::Unknown($crate::types::UnknownValue::new(value))))
            }
        }

        #[cfg(test)]
        impl $crate::model::conformance::Variant for $name {
            fn is_known(&self) -> bool {
                !self.is_unknown()
            }
        }
    };
}

pub(crate) use open_enum;
