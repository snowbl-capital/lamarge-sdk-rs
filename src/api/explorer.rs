//! Explorer reads: transfers and trades, pending and settled.

use futures_core::Stream;
use serde::Deserialize;

use super::{
    Explorer, PageRequest,
    page::{every, limit_query, page_query},
};
use crate::{
    Result,
    model::{ExplorerTransfer, Page, PendingTrade, TradeHistoryRow},
    transport::{Call, ops},
    types::{TakeId, VenueId},
};

/// `call` narrowed to the session's own items when `mine`.
fn scoped(call: Call<'_>, mine: bool) -> Call<'_> {
    if mine { call.query("scope", "mine") } else { call }
}

/// A list answer `{"data": [...]}`.
#[derive(Deserialize)]
struct Data<T> {
    data: Vec<T>,
}

impl<'a> Explorer<'a> {
    /// Narrows a call to the session's own items when the handle has one.
    fn scoped(&self, call: Call<'a>) -> Call<'a> {
        scoped(call, self.auth.is_some())
    }

    /// One page of transfers, newest first (`GET /v1/explorer/transfers`).
    pub async fn transfers(&self, page: &PageRequest) -> Result<Page<ExplorerTransfer>> {
        let call = page_query(self.scoped(Call::new(&ops::GET_EXPLORER_TRANSFERS)), page);
        self.inner.send(call, self.auth).await
    }

    /// Every transfer, newest first, reading pages of `page_size` (clamped to `1..=500`) as
    /// consumed. The stream owns what it needs, so it outlives the handle.
    pub fn transfers_stream(
        &self,
        page_size: u16,
    ) -> impl Stream<Item = Result<ExplorerTransfer>> + Send + Unpin + 'static {
        let mine = self.auth.is_some();
        every(self.inner, self.auth, PageRequest::first(page_size), move || {
            scoped(Call::new(&ops::GET_EXPLORER_TRANSFERS), mine)
        })
    }

    /// Transfers with an execution in flight, newest first, at most `limit` (1 to 500; server
    /// default 100) (`GET /v1/explorer/transfers/pending`).
    pub async fn pending_transfers(&self, limit: Option<u16>) -> Result<Vec<ExplorerTransfer>> {
        let call = limit_query(self.scoped(Call::new(&ops::GET_EXPLORER_PENDING_TRANSFERS)), limit);
        let list: Data<ExplorerTransfer> = self.inner.send(call, self.auth).await?;
        Ok(list.data)
    }

    /// A transfer's public receipt, by its first take (`GET /v1/explorer/transfers/{id}`).
    pub async fn transfer(&self, id: TakeId) -> Result<ExplorerTransfer> {
        let call = Call::new(&ops::GET_EXPLORER_TRANSFER).segment(id);
        self.inner.send(call, None).await
    }

    /// One page of settled trades, newest first, of every pair or only between the venues of
    /// `pair` in either direction (`GET /v1/trades/history`).
    pub async fn trades(
        &self,
        page: &PageRequest,
        pair: Option<(&VenueId, &VenueId)>,
    ) -> Result<Page<TradeHistoryRow>> {
        let mut call = page_query(self.scoped(Call::new(&ops::GET_TRADE_HISTORY)), page);
        if let Some((a, b)) = pair {
            call = call.query("a", a).query("b", b);
        }
        self.inner.send(call, self.auth).await
    }

    /// Signed trades not yet final, newest first, at most `limit` (1 to 500; server default
    /// 100) (`GET /v1/trades/pending`).
    pub async fn pending_trades(&self, limit: Option<u16>) -> Result<Vec<PendingTrade>> {
        let call = limit_query(self.scoped(Call::new(&ops::GET_PENDING_TRADES)), limit);
        let list: Data<PendingTrade> = self.inner.send(call, self.auth).await?;
        Ok(list.data)
    }
}

#[cfg(test)]
mod tests {
    use futures_util::StreamExt as _;
    use serde_json::json;
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::any};

    use super::*;
    use crate::{
        testing::{
            http::{Expect, client, collect_spawned, served, session},
            json::{fixture, page},
        },
        types::PageCursor,
    };

    #[tokio::test]
    async fn a_client_reads_every_users_items_anonymously() {
        let server = MockServer::start().await;
        let client = client(&server);
        let explorer = client.explorer();
        let request = PageRequest::first(20).after(PageCursor::new("c1"));
        let transfers = Expect::get("/v1/explorer/transfers", fixture("explorer_transfer_page"))
            .query(&[("after", "c1"), ("limit", "20")]);
        served(&server, transfers, explorer.transfers(&request)).await.unwrap();
        let pending = Expect::get("/v1/explorer/transfers/pending", json!({"data": []}));
        served(&server, pending, explorer.pending_transfers(None)).await.unwrap();
        let pair = (&VenueId::Lighter, &VenueId::Aster);
        let trades = Expect::get("/v1/trades/history", fixture("trade_history_page")).query(&[
            ("a", "lighter"),
            ("b", "aster"),
            ("limit", "1"),
        ]);
        served(&server, trades, explorer.trades(&PageRequest::first(1), Some(pair))).await.unwrap();
        let rows = json!({"data": [fixture("pending_trade_list/data/0")]});
        let pending = Expect::get("/v1/trades/pending", rows);
        assert_eq!(served(&server, pending, explorer.pending_trades(None)).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_session_reads_its_own_items_but_receipts_stay_public() {
        let server = MockServer::start().await;
        let client = client(&server);
        let explorer = Explorer { inner: client.inner(), auth: Some(session()) };
        let mine = Expect::get("/v1/explorer/transfers", page(&[], None))
            .query(&[("scope", "mine")])
            .bearer();
        served(&server, mine, explorer.transfers(&PageRequest::default())).await.unwrap();
        let pending = Expect::get("/v1/explorer/transfers/pending", json!({"data": []}))
            .query(&[("scope", "mine"), ("limit", "500")])
            .bearer();
        served(&server, pending, explorer.pending_transfers(Some(900))).await.unwrap();
        let trades =
            Expect::get("/v1/trades/history", page(&[], None)).query(&[("scope", "mine")]).bearer();
        served(&server, trades, explorer.trades(&PageRequest::default(), None)).await.unwrap();
        let pending = Expect::get("/v1/trades/pending", json!({"data": []}))
            .query(&[("scope", "mine")])
            .bearer();
        served(&server, pending, explorer.pending_trades(None)).await.unwrap();
        let receipt = fixture("explorer_transfer_page/data/0");
        let id: TakeId = receipt["id"].as_str().unwrap().parse().unwrap();
        let public = Expect::get(format!("/v1/explorer/transfers/{id}"), receipt);
        assert_eq!(served(&server, public, explorer.transfer(id)).await.unwrap().id, id);
    }

    #[tokio::test]
    async fn a_session_transfers_stream_stays_scoped_on_every_page() {
        let server = MockServer::start().await;
        let row = fixture("explorer_transfer_page/data/0");
        Mock::given(any())
            .respond_with(move |request: &wiremock::Request| {
                let after = request.url.query_pairs().any(|(key, _)| key == "after");
                let next = (!after).then_some("p2");
                ResponseTemplate::new(200).set_body_json(page(std::slice::from_ref(&row), next))
            })
            .mount(&server)
            .await;
        let stream =
            Explorer { inner: client(&server).inner(), auth: Some(session()) }.transfers_stream(2);
        assert_eq!(collect_spawned(stream).await.unwrap().len(), 2);
        for request in server.received_requests().await.unwrap() {
            assert!(request.url.query().unwrap().contains("scope=mine"), "{}", request.url);
        }
    }

    #[tokio::test]
    async fn a_transfers_stream_ends_with_the_first_error() {
        let server = MockServer::start().await;
        Mock::given(any()).respond_with(ResponseTemplate::new(404)).mount(&server).await;
        let client = client(&server);
        let items: Vec<Result<ExplorerTransfer>> =
            client.explorer().transfers_stream(5).collect().await;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].as_ref().unwrap_err().status(), Some(404));
    }
}
