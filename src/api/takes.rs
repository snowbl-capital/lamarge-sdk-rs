//! Take reads, cancellation and take history.

use futures_core::Stream;

use super::{
    PageRequest, Takes,
    page::{every, history},
};
use crate::{
    Result,
    model::{Page, TakeHistoryRow, TakePreparation},
    transport::{Call, ops},
    types::TakeId,
};

/// The most takes a take history page returns.
const TAKE_PAGE_MAX: u16 = 100;

impl Takes<'_> {
    /// A take and what it still needs signed.
    pub async fn get(&self, id: TakeId) -> Result<TakePreparation> {
        self.inner.send(Call::new(&ops::GET_TAKE).segment(id), self.auth).await
    }

    /// Cancels a take until its signature is accepted: its unsigned fills expire, releasing
    /// their inventory. Repeats succeed.
    pub async fn cancel(&self, id: TakeId) -> Result<()> {
        self.inner.send(Call::new(&ops::DELETE_TAKE).segment(id), self.auth).await
    }

    /// One page of the session's takes, newest first; at most 100 per page (a larger limit is
    /// lowered).
    pub async fn history(&self, page: &PageRequest) -> Result<Page<TakeHistoryRow>> {
        history(self.inner, self.auth, &ops::GET_TAKE_HISTORY, &capped(page)).await
    }

    /// Every take, newest first, reading pages of `page_size` (clamped to `1..=100`) as
    /// consumed. The stream owns what it needs, so it outlives the handle.
    pub fn history_stream(
        &self,
        page_size: u16,
    ) -> impl Stream<Item = Result<TakeHistoryRow>> + Send + Unpin + 'static {
        let first = PageRequest::first(page_size.min(TAKE_PAGE_MAX));
        every(self.inner, self.auth, first, || Call::new(&ops::GET_TAKE_HISTORY))
    }
}

/// `page` with its limit lowered to what take history allows.
fn capped(page: &PageRequest) -> PageRequest {
    let limited = match page.limit() {
        Some(limit) => PageRequest::first(limit.min(TAKE_PAGE_MAX)),
        None => PageRequest::default(),
    };
    match page.cursor() {
        Some(cursor) => limited.after(cursor.clone()),
        None => limited,
    }
}

#[cfg(test)]
mod tests {
    use wiremock::MockServer;

    use super::*;
    use crate::{
        testing::{
            http::{Expect, client, collect_spawned, served, session},
            json::{fixture, page},
        },
        types::PageCursor,
    };

    #[tokio::test]
    async fn every_call_carries_the_credential_and_history_pages_hold_at_most_100() {
        let server = MockServer::start().await;
        let client = client(&server);
        let takes = Takes { inner: client.inner(), auth: Some(session()) };
        let preparation = fixture("take_preparation");
        let id: TakeId = preparation["take"]["id"].as_str().unwrap().parse().unwrap();
        let one = Expect::get(format!("/v1/takes/{id}"), preparation).bearer();
        served(&server, one, takes.get(id)).await.unwrap();
        let cancel = Expect::delete(format!("/v1/takes/{id}"), None).bearer();
        served(&server, cancel, takes.cancel(id)).await.unwrap();
        let rows = page(&[fixture("take_history_page/data/0")], None);
        let request = PageRequest::first(500).after(PageCursor::new("t1"));
        let capped = Expect::get("/v1/takes/history", rows.clone())
            .query(&[("after", "t1"), ("limit", "100")])
            .bearer();
        served(&server, capped, takes.history(&request)).await.unwrap();
        let streamed = Expect::get("/v1/takes/history", rows).query(&[("limit", "100")]).bearer();
        served(&server, streamed, collect_spawned(takes.history_stream(300))).await.unwrap();
    }
}
