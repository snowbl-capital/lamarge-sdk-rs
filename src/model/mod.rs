//! Response models of the API operations the SDK maps.
//!
//! Response structs are `#[non_exhaustive]` with public fields; unknown fields are ignored and
//! unknown enum values land in an `Unknown` variant, so server additions do not break decoding.
//! `typed_data` fields are kept as served, for inspection: the SDK checks them before signing,
//! and a caller never builds one.

mod account;
pub(crate) mod credential;
mod event;
mod explorer;
mod fill;
mod history;
mod info;
mod maker;
mod market;
mod moves;
mod order;
mod page;
mod quote;
mod setup;
mod take;
mod transfer;
mod wallet;
mod withdrawal;

#[cfg(test)]
pub(crate) mod conformance;

pub use account::*;
pub use credential::{CredentialCreated, CredentialRotated, CredentialScope};
pub(crate) use credential::{CredentialInput, RotationBody, expires_at};
pub use event::*;
pub use explorer::*;
pub use fill::*;
pub use history::*;
pub use info::*;
pub use maker::*;
pub use market::*;
pub use moves::*;
pub use order::*;
pub use page::*;
pub use quote::*;
pub use setup::*;
pub use take::*;
pub use transfer::*;
pub use wallet::*;
pub use withdrawal::*;

use serde::{Deserialize, Deserializer};

/// For a required, nullable field: `#[serde(deserialize_with = "required")]` on an `Option`
/// refuses a missing key and reads `null` as `None`. (An `Option` field is otherwise optional.)
pub(crate) fn required<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::deserialize(deserializer)
}
