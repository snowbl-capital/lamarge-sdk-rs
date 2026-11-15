//! Quote models: quote sets, quote lines, drafts, leases and identity cancellations.

use alloy_primitives::{Address, Bytes};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::ToSign;
use crate::types::{Amount, OrderId, Price, QuoteIdentityId, Revision, VenueId};

/// One quote as signed: the taker pays `receive` on `venue` per `supply` of the order's venue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Quote {
    /// The venue the taker pays on.
    pub venue: VenueId,
    /// That venue's chain.
    pub chain_id: u64,
    /// The token paid.
    pub token: Address,
    /// The maker's payment recipient.
    pub recipient: Address,
    /// Price denominator and budget, in the order venue's base units.
    pub supply: Amount,
    /// Price numerator, in `venue` base units.
    pub receive: Amount,
    /// The smallest fill, in the order venue's base units.
    pub min_fill: Amount,
}

/// A quote of a [`QuoteSet`] with what it can still fill.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct OfferedQuote {
    /// The venue the taker pays on.
    pub venue: VenueId,
    /// That venue's chain.
    pub chain_id: u64,
    /// The token paid.
    pub token: Address,
    /// The maker's payment recipient.
    pub recipient: Address,
    /// Price denominator and budget, in the order venue's base units.
    pub supply: Amount,
    /// Price numerator, in `venue` base units.
    pub receive: Amount,
    /// The smallest fill, in the order venue's base units.
    pub min_fill: Amount,
    /// What it can fill now.
    pub available: Amount,
    /// What it offers in this sequence.
    pub offered: Amount,
}

/// An order's signed quotes for one sequence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct QuoteSet {
    /// The sequence.
    pub sequence: u64,
    /// The signed on-chain settlement deadline.
    pub expires_at: DateTime<Utc>,
    /// The quotes, in signed order.
    pub quotes: Vec<OfferedQuote>,
    /// The quote signer's signature (an opaque ERC-1271 signature on some older sets).
    pub signature: Bytes,
    /// When it was stored.
    pub created_at: DateTime<Utc>,
    /// The last admission time.
    pub valid_until: DateTime<Utc>,
    /// The sequence's admission budget; not signed on chain.
    pub quote_size: Amount,
    /// The identity generation it was signed under.
    pub identity_generation: Revision,
    /// Whether it admits takes now: within validity, lease and identity.
    pub active: bool,
}

/// A maker's prices and stock on one venue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct QuoteLine {
    /// The venue.
    pub venue: VenueId,
    /// What the maker pays to receive one unit on `venue`; `None` receives nothing there.
    #[serde(deserialize_with = "super::required")]
    pub buy: Option<Price>,
    /// What the maker asks to deliver one unit on `venue`; `None` delivers nothing there.
    #[serde(deserialize_with = "super::required")]
    pub sell: Option<Price>,
    /// What the identity's pool on `venue` offers to deliver.
    pub stock: Amount,
    /// The smallest fill of the pool's quotes; `None` is one token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_fill: Option<Amount>,
}

/// A quote identity's published lines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct QuoteLines {
    /// The identity.
    pub quote_identity_id: QuoteIdentityId,
    /// Advances at each publication.
    pub revision: Revision,
    /// One line per venue.
    pub lines: Vec<QuoteLine>,
}

/// One pool's quote set to sign for a lines publication.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct QuoteLineDraft {
    /// The pool.
    pub(crate) order_id: OrderId,
    /// The pool's venue.
    pub(crate) venue: VenueId,
    /// The draft, named by its signature.
    pub(crate) draft_id: Uuid,
    /// The sequence it signs.
    pub(crate) sequence: u64,
    /// The last admission time, Unix seconds.
    pub(crate) valid_until: u64,
    /// The signed settlement deadline, Unix seconds.
    pub(crate) expires_at: u64,
    /// The line's stock; not signed on chain.
    pub(crate) quote_size: Amount,
    /// The quotes to sign.
    pub(crate) quotes: Vec<Quote>,
    /// The quote set's typed data.
    pub(crate) to_sign: Vec<ToSign>,
}

/// A prepared lines publication: the normalized lines and the drafts to sign.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct QuoteLinesPreparation {
    /// The preparation.
    pub(crate) id: Uuid,
    /// The identity.
    pub(crate) quote_identity_id: QuoteIdentityId,
    /// The lines' revision it replaces.
    pub(crate) expected_revision: Revision,
    /// The last admission time, Unix seconds.
    pub(crate) valid_until: u64,
    /// The lines, normalized and sorted by venue.
    pub(crate) lines: Vec<QuoteLine>,
    /// One draft per pool to quote.
    pub(crate) drafts: Vec<QuoteLineDraft>,
    /// The pools whose quotes the publication pulls.
    pub(crate) cancels: Vec<OrderId>,
}

/// A quote identity's lease: its quotes admit takes only while it runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Lease {
    /// The identity.
    pub identity_id: QuoteIdentityId,
    /// The identity's generation.
    pub generation: Revision,
    /// When the lease ends unless renewed.
    pub expires_at: DateTime<Utc>,
}

/// The answer to pulling every quote of an identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct IdentityCancellation {
    /// The identity.
    pub identity_id: QuoteIdentityId,
    /// The identity's new generation.
    pub generation: Revision,
    /// How many orders' quotes were pulled.
    pub orders: u64,
}
