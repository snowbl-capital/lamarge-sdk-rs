//! The event journal: events, their previews, pages and snapshots.

use alloy_primitives::Address;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer, ser::SerializeStruct};
use serde_json::Value;

use super::{FillPreview, HeldIn, OrderState, WithdrawalState};
use crate::types::{
    Amount, ClientId, Cursor, FillId, OrderId, QuoteIdentityId, Revision, UnknownValue, VenueId,
    WithdrawalId,
};

/// `from` → `to` books changed (all books when both are absent): reload them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct BookInvalidation {
    /// The paying venue of the changed books.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<VenueId>,
    /// The receiving venue of the changed books.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<VenueId>,
}

/// An order as the event journal shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct OrderPreview {
    /// The order.
    pub id: OrderId,
    /// Its state.
    pub state: OrderState,
    /// The venue it delivers on.
    pub venue: VenueId,
    /// Its unlocked inventory, in `venue` base units.
    pub held: Amount,
    /// Where `held` is.
    pub held_in: HeldIn,
    /// The current quote set's sequence; `None` without one.
    #[serde(deserialize_with = "super::required")]
    pub sequence: Option<u64>,
    /// The cancellation epoch.
    pub cancel_epoch: Revision,
    /// The caller's order id.
    pub client_order_id: ClientId,
}

/// A quote identity's lease as the event journal shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct LeasePreview {
    /// The quote identity.
    pub id: QuoteIdentityId,
    /// The lease generation.
    pub generation: Revision,
    /// When the lease lapses.
    pub lease_expires_at: DateTime<Utc>,
    /// When the identity was revoked.
    #[serde(deserialize_with = "super::required")]
    pub revoked_at: Option<DateTime<Utc>>,
    /// When the identity's credential expires.
    #[serde(deserialize_with = "super::required")]
    pub credential_expires_at: Option<DateTime<Utc>>,
    /// Whether the identity may quote now.
    pub active: bool,
}

/// A withdrawal as the event journal shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WithdrawalPreview {
    /// The withdrawal.
    pub id: WithdrawalId,
    /// The venue withdrawn from.
    pub venue: VenueId,
    /// Its state.
    pub state: WithdrawalState,
    /// The amount, in `venue` base units.
    pub amount: Amount,
    /// The venue's reference for it.
    #[serde(deserialize_with = "super::required")]
    pub venue_ref: Option<String>,
}

/// A balance read as the event journal shows it: reload `GET /v1/balances`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct BalancePreview {
    /// The venue.
    pub venue: VenueId,
    /// The last successful venue read.
    #[serde(deserialize_with = "super::required")]
    pub read_at: Option<DateTime<Utc>>,
    /// The last successful chain read.
    #[serde(deserialize_with = "super::required")]
    pub chain_read_at: Option<DateTime<Utc>>,
    /// Whether the venue read succeeded.
    pub venue_available: bool,
    /// Whether the chain read succeeded.
    pub chain_available: bool,
}

/// A settled public trade.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TradeEvent {
    /// The trade (its fill).
    pub id: FillId,
    /// The paying venue.
    pub from: VenueId,
    /// The receiving venue.
    pub to: VenueId,
    /// The payment, in `from` base units.
    pub amount_in: Amount,
    /// The delivery, in `to` base units.
    pub amount_out: Amount,
    /// When it settled.
    pub settled_at: DateTime<Utc>,
}

/// One journal event. Payloads are previews at publication and may repeat: skip revisions
/// already applied.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "RawEvent")]
#[non_exhaustive]
pub struct Event {
    /// The event's journal position.
    pub cursor: Cursor,
    /// The resource the event is about.
    pub resource_id: String,
    /// The resource's revision.
    pub revision: Revision,
    /// When the change happened.
    pub occurred_at: DateTime<Utc>,
    /// The kind and its payload.
    pub data: EventData,
}

/// An event's payload, by kind.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
#[expect(
    clippy::large_enum_variant,
    reason = "events are read in pages and consumed at once; boxing would allocate per fill"
)]
pub enum EventData {
    /// `book.changed`.
    BookChanged(BookInvalidation),
    /// `order.changed`.
    OrderChanged(OrderPreview),
    /// `fill.changed`.
    FillChanged(FillPreview),
    /// `lease.changed`.
    LeaseChanged(LeasePreview),
    /// `withdrawal.changed`.
    WithdrawalChanged(WithdrawalPreview),
    /// `balance.changed`.
    BalanceChanged(BalancePreview),
    /// `trade`.
    Trade(TradeEvent),
    /// A kind this SDK version does not know, with its raw payload.
    Unknown {
        /// The kind as received.
        kind: UnknownValue,
        /// The payload as received.
        data: Value,
    },
}

impl EventData {
    /// The wire kind (`fill.changed`, …).
    pub fn kind(&self) -> &str {
        match self {
            Self::BookChanged(_) => "book.changed",
            Self::OrderChanged(_) => "order.changed",
            Self::FillChanged(_) => "fill.changed",
            Self::LeaseChanged(_) => "lease.changed",
            Self::WithdrawalChanged(_) => "withdrawal.changed",
            Self::BalanceChanged(_) => "balance.changed",
            Self::Trade(_) => "trade",
            Self::Unknown { kind, .. } => kind.as_str(),
        }
    }

    /// Decodes the payload of `kind` from `data`.
    fn decode<'de, D: Deserializer<'de>>(kind: String, data: D) -> Result<Self, D::Error> {
        Ok(match kind.as_str() {
            "book.changed" => Self::BookChanged(Deserialize::deserialize(data)?),
            "order.changed" => Self::OrderChanged(Deserialize::deserialize(data)?),
            "fill.changed" => Self::FillChanged(Deserialize::deserialize(data)?),
            "lease.changed" => Self::LeaseChanged(Deserialize::deserialize(data)?),
            "withdrawal.changed" => Self::WithdrawalChanged(Deserialize::deserialize(data)?),
            "balance.changed" => Self::BalanceChanged(Deserialize::deserialize(data)?),
            "trade" => Self::Trade(Deserialize::deserialize(data)?),
            _ => Self::Unknown { kind: UnknownValue::new(kind), data: Value::deserialize(data)? },
        })
    }
}

impl Serialize for Event {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut event = serializer.serialize_struct("Event", 6)?;
        event.serialize_field("cursor", &self.cursor)?;
        event.serialize_field("resource_id", &self.resource_id)?;
        event.serialize_field("revision", &self.revision)?;
        event.serialize_field("occurred_at", &self.occurred_at)?;
        event.serialize_field("kind", self.data.kind())?;
        match &self.data {
            EventData::BookChanged(data) => event.serialize_field("data", data)?,
            EventData::OrderChanged(data) => event.serialize_field("data", data)?,
            EventData::FillChanged(data) => event.serialize_field("data", data)?,
            EventData::LeaseChanged(data) => event.serialize_field("data", data)?,
            EventData::WithdrawalChanged(data) => event.serialize_field("data", data)?,
            EventData::BalanceChanged(data) => event.serialize_field("data", data)?,
            EventData::Trade(data) => event.serialize_field("data", data)?,
            EventData::Unknown { data, .. } => event.serialize_field("data", data)?,
        }
        event.end()
    }
}

/// An event as the wire carries it; its `data` is decoded by `kind`.
#[derive(Deserialize)]
struct RawEvent {
    cursor: Cursor,
    resource_id: String,
    revision: Revision,
    occurred_at: DateTime<Utc>,
    kind: String,
    data: Value,
}

impl TryFrom<RawEvent> for Event {
    type Error = serde_json::Error;

    fn try_from(raw: RawEvent) -> Result<Self, Self::Error> {
        Ok(Self {
            cursor: raw.cursor,
            resource_id: raw.resource_id,
            revision: raw.revision,
            occurred_at: raw.occurred_at,
            data: EventData::decode(raw.kind, raw.data)?,
        })
    }
}

/// A page of the event journal, oldest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct EventPage {
    /// The events, in cursor order.
    pub events: Vec<Event>,
    /// Where the next page starts (a journal position, not contiguous per reader).
    pub next_cursor: Cursor,
    /// Whether more events follow now.
    pub has_more: bool,
    /// The oldest cursor the journal still replays.
    pub oldest_cursor: Cursor,
    /// The server's time.
    pub server_time: DateTime<Utc>,
}

/// A quote of a snapshot book: as signed, plus what it can fill now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SnapshotQuote {
    /// The venue the taker pays on.
    pub venue: VenueId,
    /// That venue's chain id.
    pub chain_id: u64,
    /// The payment token.
    pub token: Address,
    /// Where the payment goes.
    pub recipient: Address,
    /// Price denominator and this quote's budget, in the order venue's base units.
    pub supply: Amount,
    /// Price numerator, in `venue` base units.
    pub receive: Amount,
    /// Executable minimum, in the order venue's base units.
    pub min_fill: Amount,
    /// The quote's position in the signed set.
    pub quote_index: u32,
    /// What it can fill now, capped at the fill maximum.
    pub available: Amount,
}

/// An order's quotes in a snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SnapshotBook {
    /// The order.
    pub order_id: OrderId,
    /// The quote set's sequence.
    pub sequence: u64,
    /// The order's revision.
    pub revision: Revision,
    /// The venue the order delivers on.
    pub to: VenueId,
    /// Quotes with capacity, best price first per paying venue.
    pub quotes: Vec<SnapshotQuote>,
    /// The order's available inventory, in `to` base units.
    pub available: Amount,
    /// Last admission time.
    pub valid_until: DateTime<Utc>,
    /// Signed on-chain settlement deadline.
    pub expires_at: DateTime<Utc>,
}

/// The public state at a cursor: books.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PublicSnapshot {
    /// The journal position the snapshot is at; replay from it.
    pub cursor: Cursor,
    /// The public books.
    pub books: Vec<SnapshotBook>,
    /// The server's time.
    pub server_time: DateTime<Utc>,
}

/// An order of the reader in a private snapshot: its preview and more.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SnapshotOrder {
    /// The order as events show it.
    #[serde(flatten)]
    pub order: OrderPreview,
    /// The quote identity that quotes it.
    pub quote_identity_id: QuoteIdentityId,
    /// The order's revision.
    pub revision: Revision,
    /// The funded amount, in its venue's base units.
    pub amount: Amount,
}

/// A fill of the reader in a private snapshot: its preview and revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SnapshotFill {
    /// The fill as events show it.
    #[serde(flatten)]
    pub fill: FillPreview,
    /// The fill's revision.
    pub revision: Revision,
}

/// A quote identity of the reader in a private snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SnapshotIdentity {
    /// The quote identity.
    pub id: QuoteIdentityId,
    /// The lease generation.
    pub generation: Revision,
    /// When the lease lapses.
    pub lease_expires_at: DateTime<Utc>,
    /// When the identity was revoked.
    #[serde(deserialize_with = "super::required")]
    pub revoked_at: Option<DateTime<Utc>>,
    /// When the identity's credential expires.
    #[serde(deserialize_with = "super::required")]
    pub credential_expires_at: Option<DateTime<Utc>>,
    /// The identity's revision.
    pub revision: Revision,
}

/// The reader's state at a cursor: orders, fills and quote identities (books are empty).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PrivateSnapshot {
    /// The journal position the snapshot is at; replay from it.
    pub cursor: Cursor,
    /// Empty in a private snapshot.
    pub books: Vec<SnapshotBook>,
    /// The server's time.
    pub server_time: DateTime<Utc>,
    /// The reader's orders.
    pub orders: Vec<SnapshotOrder>,
    /// The reader's fills.
    pub fills: Vec<SnapshotFill>,
    /// The reader's quote identities.
    pub quote_identities: Vec<SnapshotIdentity>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::testing::json::fixture;

    fn fill_event() -> Value {
        fixture("event_page/events/2")
    }

    #[test]
    fn every_kind_decodes_to_its_variant() {
        let page: EventPage = serde_json::from_value(fixture("event_page")).unwrap();
        let kinds: Vec<&str> = page.events.iter().map(|event| event.data.kind()).collect();
        assert_eq!(
            kinds,
            [
                "book.changed",
                "order.changed",
                "fill.changed",
                "lease.changed",
                "withdrawal.changed",
                "balance.changed",
                "trade"
            ]
        );
    }

    #[test]
    fn events_round_trip() {
        let page: EventPage = serde_json::from_value(fixture("event_page")).unwrap();
        let again: EventPage =
            serde_json::from_value(serde_json::to_value(&page).unwrap()).unwrap();
        assert_eq!(again, page);
    }

    #[test]
    fn data_before_kind_is_decoded_by_kind() {
        let text = r#"{"data":{"from":"gmx"},"cursor":"5","resource_id":"gmx","revision":"1",
            "occurred_at":"2026-11-14T12:00:00Z","kind":"book.changed"}"#;
        let event: Event = serde_json::from_str(text).unwrap();
        let EventData::BookChanged(books) = event.data else { panic!("{event:?}") };
        assert_eq!((books.from, books.to), (Some(VenueId::Gmx), None));
        assert_eq!(event.cursor, Cursor::new(5));
    }

    #[test]
    fn a_known_kind_with_a_bad_payload_fails() {
        let mut event = fill_event();
        event["data"]["state"] = json!(7);
        assert!(serde_json::from_value::<Event>(event).is_err());
        let mut event = fill_event();
        event["data"].as_object_mut().unwrap().remove("role");
        assert!(serde_json::from_value::<Event>(event).is_err());
    }

    #[test]
    fn unknown_kinds_keep_their_payload() {
        let mut event = fill_event();
        event["kind"] = json!("route.changed");
        let decoded: Event = serde_json::from_value(event.clone()).unwrap();
        let EventData::Unknown { kind, data } = &decoded.data else { panic!("{decoded:?}") };
        assert_eq!(kind.as_str(), "route.changed");
        assert_eq!(data, &event["data"]);
        assert_eq!(serde_json::to_value(&decoded).unwrap(), event);
    }

    #[test]
    fn missing_envelope_keys_fail() {
        for key in ["cursor", "resource_id", "revision", "occurred_at", "kind", "data"] {
            let mut event = fill_event();
            event.as_object_mut().unwrap().remove(key);
            assert!(serde_json::from_value::<Event>(event).is_err(), "{key}");
        }
    }

    #[test]
    fn snapshot_rows_flatten_their_preview() {
        let fill: SnapshotFill =
            serde_json::from_value(fixture("private_snapshot/fills/0")).unwrap();
        let expected: FillPreview =
            serde_json::from_value(fixture("event_page/events/2/data")).unwrap();
        assert_eq!((fill.fill, fill.revision), (expected, Revision::new(9)));
        let order: SnapshotOrder =
            serde_json::from_value(fixture("private_snapshot/orders/0")).unwrap();
        let expected: OrderPreview =
            serde_json::from_value(fixture("private_snapshot/orders/0")).unwrap();
        assert_eq!(order.order, expected);
    }
}
