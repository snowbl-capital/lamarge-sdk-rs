//! Event journal reads: snapshots, replay pages and the live subscription.

use std::sync::Arc;

use super::{Events, PublicEvents, page::limit_query};
use crate::{
    Result,
    events::EventStream,
    model::{EventPage, PrivateSnapshot, PublicSnapshot},
    transport::{Call, ops},
    types::Cursor,
};

impl PublicEvents<'_> {
    /// The public books at a journal cursor (`GET /v1/snapshot`).
    pub async fn snapshot(&self) -> Result<PublicSnapshot> {
        self.inner.send(Call::new(&ops::GET_SNAPSHOT), None).await
    }

    /// The public events after `after`, at most `limit` (1 to 500; server default 100)
    /// (`GET /v1/events`).
    pub async fn page(&self, after: Cursor, limit: Option<u16>) -> Result<EventPage> {
        let call = limit_query(Call::new(&ops::GET_EVENTS).query("after", after), limit);
        self.inner.send(call, None).await
    }

    /// The public journal as a live [`EventStream`]. Lazy: nothing is sent before the first
    /// poll. The stream owns what it needs, so it outlives the handle.
    pub fn subscribe(&self) -> EventStream {
        EventStream::new(Arc::clone(self.inner), None)
    }
}

impl Events<'_> {
    /// The reader's orders, fills and quote identities at a journal cursor
    /// (`GET /v1/me/snapshot`).
    pub async fn snapshot(&self) -> Result<PrivateSnapshot> {
        self.inner.send(Call::new(&ops::GET_MY_SNAPSHOT), self.auth).await
    }

    /// The reader's events after `after`, at most `limit` (1 to 500; server default 100)
    /// (`GET /v1/me/events`).
    pub async fn page(&self, after: Cursor, limit: Option<u16>) -> Result<EventPage> {
        let call = limit_query(Call::new(&ops::GET_MY_EVENTS).query("after", after), limit);
        self.inner.send(call, self.auth).await
    }

    /// The reader's journal as a live [`EventStream`]. Lazy: nothing is sent before the first
    /// poll. The stream owns what it needs, so it outlives the handle; it follows renewals of a
    /// session's token and ends on sign-out.
    pub fn subscribe(&self) -> EventStream {
        EventStream::new(Arc::clone(self.inner), self.auth.cloned())
    }
}

#[cfg(test)]
mod tests {
    use wiremock::MockServer;

    use super::*;
    use crate::testing::{
        http::{Expect, client, served, session},
        json::fixture,
    };

    #[tokio::test]
    async fn a_client_reads_the_public_journal_anonymously() {
        let server = MockServer::start().await;
        let client = client(&server);
        let events = client.events();
        let snapshot = Expect::get("/v1/snapshot", fixture("snapshot"));
        assert_eq!(
            served(&server, snapshot, events.snapshot()).await.unwrap().cursor,
            Cursor::new(1007)
        );
        let page = Expect::get("/v1/events", fixture("event_page"))
            .query(&[("after", "1000"), ("limit", "500")]);
        served(&server, page, events.page(Cursor::new(1000), Some(900))).await.unwrap();
    }

    #[tokio::test]
    async fn a_reader_reads_its_own_journal() {
        let server = MockServer::start().await;
        let client = client(&server);
        let events = Events { inner: client.inner(), auth: Some(session()) };
        let snapshot = Expect::get("/v1/me/snapshot", fixture("private_snapshot")).bearer();
        served(&server, snapshot, events.snapshot()).await.unwrap();
        let page =
            Expect::get("/v1/me/events", fixture("event_page")).query(&[("after", "7")]).bearer();
        served(&server, page, events.page(Cursor::new(7), None)).await.unwrap();
    }
}
