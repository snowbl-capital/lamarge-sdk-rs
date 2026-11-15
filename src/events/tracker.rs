//! Fill state tracking over the event stream.

use std::collections::HashMap;

use super::{Change, Snapshot};
use crate::{
    model::{EventData, FillPreview},
    types::{FillId, Revision},
};

/// The reader's fills, kept current from the [`Change`]s of a private
/// [`EventStream`](super::EventStream). Events repeat and may arrive late: a fill is replaced only
/// by a newer revision.
#[derive(Debug, Clone, Default)]
pub struct FillTracker {
    fills: HashMap<FillId, (Revision, FillPreview)>,
    synced: bool,
}

impl FillTracker {
    /// Applies one change: a private resync replaces every fill; a `fill.changed` event replaces
    /// its fill when its revision is newer than the one held. Public resyncs carry no fills and
    /// change nothing.
    pub fn apply(&mut self, change: &Change) {
        match change {
            Change::Resync(Snapshot::Private(snapshot)) => {
                self.fills.clear();
                for fill in &snapshot.fills {
                    self.update(fill.revision, fill.fill.clone());
                }
                self.synced = true;
            }
            Change::Resync(Snapshot::Public(_)) => {}
            Change::Events(page) => {
                for event in &page.events {
                    if let EventData::FillChanged(preview) = &event.data
                        && self.is_newer(preview.id, event.revision)
                    {
                        self.update(event.revision, preview.clone());
                    }
                }
            }
        }
    }

    /// Whether a private resync has been applied: before it, the tracker holds only fills seen
    /// in events.
    pub fn is_synced(&self) -> bool {
        self.synced
    }

    /// The fill `id`, if held.
    pub fn get(&self, id: FillId) -> Option<&FillPreview> {
        self.fills.get(&id).map(|(_, preview)| preview)
    }

    /// Every fill held, in no particular order.
    pub fn iter(&self) -> impl Iterator<Item = &FillPreview> {
        self.fills.values().map(|(_, preview)| preview)
    }

    fn is_newer(&self, id: FillId, revision: Revision) -> bool {
        self.fills.get(&id).is_none_or(|(held, _)| revision > *held)
    }

    fn update(&mut self, revision: Revision, preview: FillPreview) {
        if self.is_newer(preview.id, revision) {
            self.fills.insert(preview.id, (revision, preview));
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::{
        model::{EventPage, FillState, PrivateSnapshot},
        testing::json::fixture,
    };

    const FILL: &str = "0190f3a4-7b2c-7d10-8e4f-000000000004";
    const OTHER: &str = "0190f3a4-7b2c-7d10-8e4f-0000000000ff";

    fn id(text: &str) -> FillId {
        text.parse().unwrap()
    }

    fn snapshot_json() -> Value {
        fixture("private_snapshot")
    }

    /// A private resync holding the fixture fill (revision 9, `SETTLED`) under `id`.
    fn resync(fill_id: &str) -> Change {
        let mut value = snapshot_json();
        value["fills"][0]["id"] = json!(fill_id);
        Change::Resync(Snapshot::Private(serde_json::from_value(value).unwrap()))
    }

    /// A page with one `fill.changed` event for `FILL` at `revision` in `state`.
    fn fill_event(revision: u64, state: &str) -> Change {
        let mut event: Value = fixture("event_page/events/2");
        event["revision"] = json!(revision.to_string());
        event["data"]["state"] = json!(state);
        let page: EventPage = serde_json::from_value(json!({
            "events": [event],
            "next_cursor": "2000",
            "has_more": false,
            "oldest_cursor": "1",
            "server_time": "2026-11-14T12:00:00Z",
        }))
        .unwrap();
        Change::Events(page)
    }

    #[test]
    fn private_resync_replaces_every_fill() {
        let mut tracker = FillTracker::default();
        tracker.apply(&resync(OTHER));
        assert!(tracker.get(id(OTHER)).is_some());
        tracker.apply(&resync(FILL));
        assert!(tracker.get(id(OTHER)).is_none(), "fills absent from the new snapshot are dropped");
        assert_eq!(tracker.get(id(FILL)).unwrap().state, FillState::Settled);
        assert_eq!(tracker.iter().count(), 1);
    }

    #[test]
    fn a_resync_replaces_a_newer_event_state() {
        let mut tracker = FillTracker::default();
        tracker.apply(&fill_event(50, "REFUNDED"));
        tracker.apply(&resync(FILL));
        assert_eq!(tracker.get(id(FILL)).unwrap().state, FillState::Settled);
    }

    #[test]
    fn events_apply_only_newer_revisions() {
        let mut tracker = FillTracker::default();
        tracker.apply(&resync(FILL));
        tracker.apply(&fill_event(8, "REFUNDED"));
        tracker.apply(&fill_event(9, "REFUNDED"));
        assert_eq!(
            tracker.get(id(FILL)).unwrap().state,
            FillState::Settled,
            "older and equal skipped"
        );
        tracker.apply(&fill_event(10, "REFUNDED"));
        assert_eq!(tracker.get(id(FILL)).unwrap().state, FillState::Refunded);
        tracker.apply(&fill_event(9, "SETTLED"));
        assert_eq!(
            tracker.get(id(FILL)).unwrap().state,
            FillState::Refunded,
            "a late event is skipped"
        );
    }

    #[test]
    fn synced_only_after_a_private_resync() {
        let mut tracker = FillTracker::default();
        assert!(!tracker.is_synced());
        tracker.apply(&fill_event(1, "SETTLED"));
        assert!(!tracker.is_synced());
        assert!(tracker.get(id(FILL)).is_some(), "events before the resync are kept");
        let public = serde_json::from_value(fixture("snapshot")).unwrap();
        tracker.apply(&Change::Resync(Snapshot::Public(public)));
        assert!(!tracker.is_synced());
        assert!(tracker.get(id(FILL)).is_some(), "a public resync changes nothing");
        tracker.apply(&resync(FILL));
        assert!(tracker.is_synced());
    }

    #[test]
    fn malformed_private_snapshots_fail_decoding() {
        let mut missing_fills = snapshot_json();
        missing_fills.as_object_mut().unwrap().remove("fills");
        assert!(serde_json::from_value::<PrivateSnapshot>(missing_fills).is_err());
        let mut missing_revision = snapshot_json();
        missing_revision["fills"][0].as_object_mut().unwrap().remove("revision");
        assert!(serde_json::from_value::<PrivateSnapshot>(missing_revision).is_err());
    }
}
